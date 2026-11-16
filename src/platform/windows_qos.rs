//! qWAVE AudioVideo marking (DSCP 40, WMM video on Wi-Fi) for a host's session socket when the
//! peer is on the LAN. Only the host side: it sends the video. qwave.dll is loaded at run time,
//! because qWAVE is an optional feature on Windows Server and a static import would stop RustDesk
//! from starting there. Marking failures leave the session unmarked, never broken.
//! The flow is added at accept time, before the login names the connection type, so
//! file-transfer, port-forward and terminal connections from LAN peers are marked as well (known
//! limit; narrowing it needs the qWAVE handle and socket kept until after login).

use base::config::keys::OPTION_ENABLE_LAN_QOS;
use hbb_common::{
    config::Config,
    log,
    tcp::FramedStream,
    tokio::{
        self,
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::TcpStream,
    },
    Stream,
};
use std::{
    ffi::c_void,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::windows::io::AsRawSocket,
    pin::Pin,
    ptr::null,
    sync::OnceLock,
    task::{Context, Poll},
    time::Duration,
};
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

const LOG_INTERVAL: Duration = Duration::from_secs(300);
// qos2.h
const QOS_TRAFFIC_TYPE_AUDIO_VIDEO: i32 = 3;
const QOS_NON_ADAPTIVE_FLOW: u32 = 2;

#[repr(C)]
struct QosVersion {
    major: u16,
    minor: u16,
}

type CreateFn = unsafe extern "system" fn(*const QosVersion, *mut *mut c_void) -> i32;
type AddFn =
    unsafe extern "system" fn(*mut c_void, usize, *const c_void, i32, u32, *mut u32) -> i32;
type CloseFn = unsafe extern "system" fn(*mut c_void) -> i32;

struct Api {
    create: CreateFn,
    add: AddFn,
    close: CloseFn,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load() }).as_ref()
}

unsafe fn load() -> Option<Api> {
    let name: Vec<u16> = "qwave.dll\0".encode_utf16().collect();
    let module = LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
    if module.is_null() {
        log::info!("qwave.dll not available, LAN QoS marking off");
        return None;
    }
    macro_rules! sym {
        ($n:literal) => {{
            let f = GetProcAddress(module, concat!($n, "\0").as_ptr() as _);
            if f.is_null() {
                log::warn!("qwave.dll lacks {}", $n);
                return None;
            }
            std::mem::transmute(f)
        }};
    }
    Some(Api {
        create: sym!("QOSCreateHandle"),
        add: sym!("QOSAddSocketToFlow"),
        close: sym!("QOSCloseHandle"),
    })
}

fn is_lan_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

/// RFC 1918, link-local and unique-local addresses (also v4-mapped). CGNAT is not LAN.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_lan_v4(v4),
            // fc00::/7 unique local, fe80::/10 link local (the std helpers need Rust 1.84)
            None => {
                let s0 = v6.segments()[0];
                (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80
            }
        },
    }
}

// The qWAVE handle as an integer, so the stream wrapper stays Send + Sync.
struct QosFlow(isize);

impl QosFlow {
    fn add(socket: u64) -> io::Result<Self> {
        let api = api().ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no qwave.dll"))?;
        let version = QosVersion { major: 1, minor: 0 };
        let mut handle = std::ptr::null_mut();
        unsafe {
            if (api.create)(&version, &mut handle) == 0 {
                return Err(io::Error::last_os_error());
            }
            let flow = Self(handle as isize);
            let mut flow_id = 0u32;
            if (api.add)(
                handle,
                socket as usize,
                null(),
                QOS_TRAFFIC_TYPE_AUDIO_VIDEO,
                QOS_NON_ADAPTIVE_FLOW,
                &mut flow_id,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(flow)
        }
    }
}

impl Drop for QosFlow {
    // Closing the handle removes its flows; the socket is never named again, since its value may
    // already belong to another socket. A short synchronous RPC to the qWAVE service.
    fn drop(&mut self) {
        if let Some(api) = api() {
            unsafe {
                (api.close)(self.0 as *mut c_void);
            }
        }
    }
}

// Field order matters: the flow is dropped, and its handle closed, before the socket.
struct QosTcpStream {
    _flow: QosFlow,
    stream: TcpStream,
}

impl AsyncRead for QosTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for QosTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// The host's session stream: qWAVE-marked when the option is on (default) and the peer is on
/// the LAN, otherwise exactly `Stream::from`.
pub async fn host_stream(stream: TcpStream, local: SocketAddr, peer: SocketAddr) -> Option<Stream> {
    if Config::get_option(OPTION_ENABLE_LAN_QOS) == "N" || !is_lan(peer.ip()) {
        return Some(Stream::from(stream, local));
    }
    let started = std::time::Instant::now();
    // qWAVE calls are synchronous RPCs (the first may start the service). The blocking task owns
    // the stream, so the socket stays open while qWAVE names it, even if this future is dropped.
    let joined = tokio::task::spawn_blocking(move || {
        let flow = QosFlow::add(stream.as_raw_socket());
        (flow, stream)
    })
    .await;
    match joined {
        Ok((Ok(flow), stream)) => {
            // Throttled: a LAN peer controls how often connections are accepted.
            hbb_common::throttled_log!(
                LOG_INTERVAL,
                info,
                "qWAVE AudioVideo flow added for {peer} in {:?}",
                started.elapsed()
            );
            Some(Stream::Tcp(FramedStream::from(
                QosTcpStream {
                    _flow: flow,
                    stream,
                },
                local,
            )))
        }
        Ok((Err(err), stream)) => {
            // No qwave.dll was already reported once by load().
            if err.kind() != io::ErrorKind::Unsupported {
                hbb_common::throttled_log!(LOG_INTERVAL, warn, "qWAVE marking failed: {err}");
            }
            Some(Stream::from(stream, local))
        }
        Err(err) => {
            // The task (and with it the socket) is gone.
            log::warn!("qWAVE marking for {peer} did not complete: {err}");
            None
        }
    }
}
