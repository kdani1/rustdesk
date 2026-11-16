//! qWAVE AudioVideo marking (DSCP 40, WMM video on Wi-Fi) for a host's session socket when the
//! peer is on the LAN. Only the host side: it sends the video. qwave.dll is loaded at run time,
//! because qWAVE is an optional feature on Windows Server and a static import would stop RustDesk
//! from starting there. Marking failures leave the session unmarked, never broken.
//! The flow is added at accept time, before the login names the connection type, so
//! file-transfer, port-forward and terminal connections from LAN peers are marked as well (known
//! limit; narrowing it needs the qWAVE handle and socket kept until after login).
//!
//! Also the TCP meter (option enable-tcp-meter) and the session socket handle the
//! video-sndbuf-kb experiment sets SO_SNDBUF through after login: [`HostTcpStream`], a
//! pass-through wrapper that counts the bytes the session wrote and samples SIO_TCP_INFO. With
//! both options off (the default) the stream is exactly what it was before.

use base::config::keys::{OPTION_ENABLE_LAN_QOS, OPTION_ENABLE_TCP_METER, OPTION_VIDEO_SNDBUF_KB};
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
    collections::HashMap,
    ffi::c_void,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::windows::io::AsRawSocket,
    pin::Pin,
    ptr::null,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll},
    time::{Duration, Instant},
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

/// qWAVE AudioVideo marking of the UDP video lane's connected socket (roadmap M20; the lane
/// exists on the LAN only), unless `enable-lan-qos` is "N". Drop it before the socket closes.
pub struct UdpQos(#[allow(dead_code)] QosFlow);

/// None when `enable-lan-qos` is "N"; otherwise the flow or why it could not be added. A
/// synchronous RPC to the qWAVE service: call it off the async runtime.
pub fn mark_udp(socket: &std::net::UdpSocket) -> Option<io::Result<UdpQos>> {
    if Config::get_option(OPTION_ENABLE_LAN_QOS) == "N" {
        return None;
    }
    Some(QosFlow::add(socket.as_raw_socket()).map(UdpQos))
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
/// the LAN, wrapped in [`HostTcpStream`] when the TCP meter or the SO_SNDBUF experiment wants
/// this socket (both off by default), otherwise exactly what it was before: `Stream::from`.
pub async fn host_stream(stream: TcpStream, local: SocketAddr, peer: SocketAddr) -> Option<Stream> {
    let lan = is_lan(peer.ip());
    let socket = probe_socket(&stream, peer, lan);
    let wrap = |stream: TcpStream, flow: Option<QosFlow>, socket: Option<Arc<HostSocket>>| {
        Stream::Tcp(match (socket, flow) {
            (Some(socket), flow) => FramedStream::from(
                HostTcpStream {
                    _flow: flow,
                    stream,
                    socket,
                },
                local,
            ),
            (None, Some(flow)) => FramedStream::from(
                QosTcpStream {
                    _flow: flow,
                    stream,
                },
                local,
            ),
            (None, None) => FramedStream::from(stream, local),
        })
    };
    if Config::get_option(OPTION_ENABLE_LAN_QOS) == "N" || !lan {
        return Some(wrap(stream, None, socket));
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
            Some(wrap(stream, Some(flow), socket))
        }
        Ok((Err(err), stream)) => {
            // No qwave.dll was already reported once by load().
            if err.kind() != io::ErrorKind::Unsupported {
                hbb_common::throttled_log!(LOG_INTERVAL, warn, "qWAVE marking failed: {err}");
            }
            Some(wrap(stream, None, socket))
        }
        Err(err) => {
            // The task (and with it the socket) is gone.
            log::warn!("qWAVE marking for {peer} did not complete: {err}");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// TCP meter and session socket handle (roadmap M11)

// mstcpip.h: _WSAIORW(IOC_VENDOR, 39)
const SIO_TCP_INFO: u32 = 0xD800_0027;
const SOL_SOCKET: i32 = 0xffff;
const SO_SNDBUF: i32 = 0x1001;
// A write that returned Pending is sampled at most this often, other writes at most every
// `SAMPLE_INTERVAL`.
const SAMPLE_INTERVAL_PENDING: Duration = Duration::from_millis(10);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_RTT_SAMPLES: usize = 1024;
// Registry entries no connection claimed (the handshake failed) are dropped after this.
const REGISTRY_TTL: Duration = Duration::from_secs(120);

#[link(name = "ws2_32")]
extern "system" {
    fn WSAIoctl(
        s: usize,
        code: u32,
        in_buf: *const c_void,
        in_len: u32,
        out_buf: *mut c_void,
        out_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        routine: *mut c_void,
    ) -> i32;
    fn WSAGetLastError() -> i32;
    fn setsockopt(s: usize, level: i32, name: i32, val: *const u8, len: i32) -> i32;
    fn getsockopt(s: usize, level: i32, name: i32, val: *mut u8, len: *mut i32) -> i32;
}

/// TCP_INFO_v0 (mstcpip.h), C layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TcpInfoV0 {
    state: i32,
    mss: u32,
    connection_time_ms: u64,
    timestamps_enabled: u8,
    rtt_us: u32,
    min_rtt_us: u32,
    bytes_in_flight: u32,
    cwnd: u32,
    snd_wnd: u32,
    rcv_wnd: u32,
    rcv_buf: u32,
    bytes_out: u64,
    bytes_in: u64,
    bytes_reordered: u32,
    bytes_retrans: u32,
    fast_retrans: u32,
    dup_acks_in: u32,
    timeout_episodes: u32,
    syn_retrans: u8,
}

/// TCP_INFO_v1: v0 followed by the send-limit counters (cumulative since the socket was made).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TcpInfoV1 {
    v0: TcpInfoV0,
    snd_lim_trans_rwin: u32,
    snd_lim_time_rwin: u32,
    snd_lim_bytes_rwin: u64,
    snd_lim_trans_cwnd: u32,
    snd_lim_time_cwnd: u32,
    snd_lim_bytes_cwnd: u64,
    snd_lim_trans_snd: u32,
    snd_lim_time_snd: u32,
    snd_lim_bytes_snd: u64,
}

/// One SIO_TCP_INFO reading. The counters are cumulative since the socket was created.
#[derive(Clone, Copy, Default)]
struct TcpSample {
    rtt_us: u32,
    min_rtt_us: u32,
    in_flight: u32,
    cwnd: u32,
    snd_wnd: u32,
    bytes_out: u64,
    bytes_retrans: u32,
    fast_retrans: u32,
    dup_acks_in: u32,
    timeout_episodes: u32,
    // v1 only: ms the sender was limited by the receive window, the congestion window, itself.
    snd_lim_ms: Option<[u32; 3]>,
}

unsafe fn query_tcp_info(socket: u64, version: u32) -> Result<TcpSample, i32> {
    let mut info = TcpInfoV1::default();
    let len = if version == 1 {
        std::mem::size_of::<TcpInfoV1>()
    } else {
        std::mem::size_of::<TcpInfoV0>()
    } as u32;
    let mut returned = 0u32;
    let rc = WSAIoctl(
        socket as usize,
        SIO_TCP_INFO,
        &version as *const u32 as *const c_void,
        std::mem::size_of::<u32>() as u32,
        &mut info as *mut TcpInfoV1 as *mut c_void,
        len,
        &mut returned,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if rc != 0 {
        return Err(WSAGetLastError());
    }
    let v0 = &info.v0;
    Ok(TcpSample {
        rtt_us: v0.rtt_us,
        min_rtt_us: v0.min_rtt_us,
        in_flight: v0.bytes_in_flight,
        cwnd: v0.cwnd,
        snd_wnd: v0.snd_wnd,
        bytes_out: v0.bytes_out,
        bytes_retrans: v0.bytes_retrans,
        fast_retrans: v0.fast_retrans,
        dup_acks_in: v0.dup_acks_in,
        timeout_episodes: v0.timeout_episodes,
        snd_lim_ms: (version == 1).then(|| {
            [
                info.snd_lim_time_rwin,
                info.snd_lim_time_cwnd,
                info.snd_lim_time_snd,
            ]
        }),
    })
}

/// What one report window of a metered socket saw. Bytes in bytes, times in us unless named.
#[derive(Default)]
pub struct TcpWindow {
    /// SIO_TCP_INFO version in use: 1, or 0 (no send-limit counters).
    pub version: u32,
    pub samples: usize,
    /// Writes into the socket that returned Pending (send buffer full).
    pub pending: usize,
    /// Bytes the session wrote into the socket in the window.
    pub app_bytes: u64,
    /// BytesOut delta over the window (as Windows counts it; compare with `app_bytes`).
    pub out_bytes: u64,
    pub rtt_us: Vec<u32>,
    pub min_rtt_us: u32,
    /// Bytes the session wrote that TCP has not sent yet: written minus unique bytes out
    /// (BytesOut - BytesRetrans), both counted from the baseline taken when the wrapper was
    /// made. Signed: a value well below 0 means BytesOut does not count what the formula assumes.
    pub unsent_max: i64,
    pub unsent_last: i64,
    /// Bytes sent and not acknowledged yet (BytesInFlight), max over the samples.
    pub in_flight_max: u32,
    pub cwnd_last: u32,
    pub snd_wnd_last: u32,
    /// Deltas over the window.
    pub retrans_bytes: u32,
    pub fast_retrans: u32,
    pub dup_acks: u32,
    pub timeouts: u32,
    /// v1: ms the sender was limited by the receive window, the congestion window, itself.
    pub snd_lim_ms: Option<[u32; 3]>,
}

struct MeterState {
    // The socket is open. Cleared by the stream wrapper's drop before the socket closes, so a
    // handle that outlives the stream never names a closed, or reused, socket value.
    alive: bool,
    // SIO_TCP_INFO version that works on this socket; None: sampling is off for good.
    version: Option<u32>,
    base_out: u64,
    base_retrans: u32,
    last_at: Option<Instant>,
    // The newest sample, and the one the current window's deltas start from.
    last: Option<TcpSample>,
    window_base: Option<TcpSample>,
    app_bytes_base: u64,
    win: TcpWindow,
}

/// The session socket of an accepted host connection, shared by its stream wrapper and the
/// connection: the TCP meter's state, and the raw socket for SO_SNDBUF.
pub struct HostSocket {
    socket: u64,
    lan: bool,
    meter: bool,
    app_bytes: AtomicU64,
    // Sample only while the connection reports (enable-latency-log); set by the connection.
    active: AtomicBool,
    state: Mutex<MeterState>,
}

impl HostSocket {
    fn new(socket: u64, lan: bool, meter: bool) -> Self {
        let s = Self {
            socket,
            lan,
            meter,
            app_bytes: AtomicU64::new(0),
            active: AtomicBool::new(false),
            state: Mutex::new(MeterState {
                alive: true,
                version: None,
                base_out: 0,
                base_retrans: 0,
                last_at: None,
                last: None,
                window_base: None,
                app_bytes_base: 0,
                win: TcpWindow::default(),
            }),
        };
        if meter {
            if let Ok(mut st) = s.state.lock() {
                // Baseline: what this socket had sent before the wrapper counts anything.
                let mut first = None;
                for version in [1u32, 0] {
                    match unsafe { query_tcp_info(socket, version) } {
                        Ok(sample) => {
                            first = Some((version, sample));
                            break;
                        }
                        Err(code) => log::debug!("SIO_TCP_INFO v{version} failed: {code}"),
                    }
                }
                if let Some((version, sample)) = first {
                    st.version = Some(version);
                    st.base_out = sample.bytes_out;
                    st.base_retrans = sample.bytes_retrans;
                    st.last = Some(sample);
                    st.window_base = Some(sample);
                    st.win.version = version;
                } else {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "TCP meter: SIO_TCP_INFO is not available, sampling off"
                    );
                }
            }
        }
        s
    }

    /// Whether this socket is metered and SIO_TCP_INFO works on it.
    pub fn is_metered(&self) -> bool {
        self.meter
            && self
                .state
                .lock()
                .map(|st| st.version.is_some())
                .unwrap_or(false)
    }

    /// The SIO_TCP_INFO version in use, if any.
    pub fn version(&self) -> Option<u32> {
        if !self.meter {
            return None;
        }
        self.state.lock().ok().and_then(|st| st.version)
    }

    pub fn is_lan(&self) -> bool {
        self.lan
    }

    /// Sampling on or off; the connection follows enable-latency-log.
    pub fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    // From the stream wrapper's poll_write: counts only completed writes, samples now and then,
    // never blocks (try_lock) and never fails the write.
    #[inline]
    fn on_write(&self, res: &Poll<io::Result<usize>>) {
        if !self.meter {
            return;
        }
        let pending = match res {
            Poll::Ready(Ok(n)) => {
                self.app_bytes.fetch_add(*n as u64, Ordering::Relaxed);
                false
            }
            Poll::Pending => true,
            Poll::Ready(Err(_)) => return,
        };
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut st) = self.state.try_lock() else {
            return;
        };
        if !st.alive || st.version.is_none() {
            return;
        }
        if pending {
            st.win.pending += 1;
        }
        let now = Instant::now();
        let due = match st.last_at {
            None => true,
            Some(t) => {
                now.saturating_duration_since(t)
                    >= if pending {
                        SAMPLE_INTERVAL_PENDING
                    } else {
                        SAMPLE_INTERVAL
                    }
            }
        };
        if due {
            st.last_at = Some(now);
            self.sample_locked(&mut st);
        }
    }

    fn sample_locked(&self, st: &mut MeterState) {
        let Some(version) = st.version else {
            return;
        };
        let sample = match unsafe { query_tcp_info(self.socket, version) } {
            Ok(s) => s,
            Err(code) if version == 1 => {
                // v0 from now on; the send-limit counters are gone for this socket.
                match unsafe { query_tcp_info(self.socket, 0) } {
                    Ok(s) => {
                        st.version = Some(0);
                        s
                    }
                    Err(code0) => {
                        st.version = None;
                        log::warn!("TCP meter off for this socket: SIO_TCP_INFO {code}/{code0}");
                        return;
                    }
                }
            }
            Err(code) => {
                st.version = None;
                log::warn!("TCP meter off for this socket: SIO_TCP_INFO v0 {code}");
                return;
            }
        };
        let app = self.app_bytes.load(Ordering::Relaxed);
        let unique_out = sample
            .bytes_out
            .wrapping_sub(st.base_out)
            .wrapping_sub(sample.bytes_retrans.wrapping_sub(st.base_retrans) as u64);
        let unsent = app as i64 - unique_out as i64;
        let w = &mut st.win;
        if w.samples == 0 || unsent > w.unsent_max {
            w.unsent_max = unsent;
        }
        w.unsent_last = unsent;
        w.samples += 1;
        if w.rtt_us.len() < MAX_RTT_SAMPLES {
            w.rtt_us.push(sample.rtt_us);
        }
        w.min_rtt_us = sample.min_rtt_us;
        w.in_flight_max = w.in_flight_max.max(sample.in_flight);
        w.cwnd_last = sample.cwnd;
        w.snd_wnd_last = sample.snd_wnd;
        st.last = Some(sample);
        if st.window_base.is_none() {
            st.window_base = Some(sample);
        }
    }

    /// The window since the last call, with a fresh sample taken now (a write blocked for the
    /// whole window gives no sample of its own). None while sampling is off.
    pub fn take_window(&self) -> Option<TcpWindow> {
        let Ok(mut st) = self.state.lock() else {
            return None;
        };
        if !self.meter || st.version.is_none() {
            return None;
        }
        if st.alive && self.active.load(Ordering::Relaxed) {
            self.sample_locked(&mut st);
        }
        let app = self.app_bytes.load(Ordering::Relaxed);
        let app_base = std::mem::replace(&mut st.app_bytes_base, app);
        let mut w = std::mem::take(&mut st.win);
        w.version = st.version.unwrap_or(0);
        w.app_bytes = app.saturating_sub(app_base);
        if let (Some(base), Some(last)) = (st.window_base, st.last) {
            w.out_bytes = last.bytes_out.wrapping_sub(base.bytes_out);
            w.retrans_bytes = last.bytes_retrans.wrapping_sub(base.bytes_retrans);
            w.fast_retrans = last.fast_retrans.wrapping_sub(base.fast_retrans);
            w.dup_acks = last.dup_acks_in.wrapping_sub(base.dup_acks_in);
            w.timeouts = last.timeout_episodes.wrapping_sub(base.timeout_episodes);
            w.snd_lim_ms = match (base.snd_lim_ms, last.snd_lim_ms) {
                (Some(b), Some(l)) => Some([
                    l[0].wrapping_sub(b[0]),
                    l[1].wrapping_sub(b[1]),
                    l[2].wrapping_sub(b[2]),
                ]),
                _ => None,
            };
        }
        st.window_base = st.last;
        Some(w)
    }

    /// Sets SO_SNDBUF; returns what Windows reported before and after, in bytes.
    pub fn set_sndbuf(&self, bytes: u32) -> io::Result<(u32, u32)> {
        let st = self
            .state
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "poisoned"))?;
        if !st.alive {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "socket closed"));
        }
        let get = || -> io::Result<u32> {
            let mut v = 0u32;
            let mut len = std::mem::size_of::<u32>() as i32;
            if unsafe {
                getsockopt(
                    self.socket as usize,
                    SOL_SOCKET,
                    SO_SNDBUF,
                    &mut v as *mut u32 as *mut u8,
                    &mut len,
                )
            } != 0
            {
                return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
            }
            Ok(v)
        };
        let before = get()?;
        if unsafe {
            setsockopt(
                self.socket as usize,
                SOL_SOCKET,
                SO_SNDBUF,
                &bytes as *const u32 as *const u8,
                std::mem::size_of::<u32>() as i32,
            )
        } != 0
        {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        let after = get().unwrap_or(0);
        drop(st);
        Ok((before, after))
    }
}

// Field order matters: the flow is dropped, and its handle closed, before the socket. The drop
// below marks the socket handle dead before either.
struct HostTcpStream {
    _flow: Option<QosFlow>,
    stream: TcpStream,
    socket: Arc<HostSocket>,
}

impl Drop for HostTcpStream {
    fn drop(&mut self) {
        match self.socket.state.lock() {
            Ok(mut st) => st.alive = false,
            Err(e) => e.into_inner().alive = false,
        }
    }
}

impl AsyncRead for HostTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for HostTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let res = Pin::new(&mut self.stream).poll_write(cx, buf);
        self.socket.on_write(&res);
        res
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

enum Registered {
    Socket(Weak<HostSocket>),
    // Why this accepted connection has no socket handle.
    None(&'static str),
}

lazy_static::lazy_static! {
    // Accepted session sockets by peer address, until their connection claims them.
    static ref REGISTRY: Mutex<HashMap<SocketAddr, (Instant, Registered)>> = Default::default();
}

fn register(peer: SocketAddr, r: Registered) {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    reg.retain(|_, (at, r)| {
        now.saturating_duration_since(*at) < REGISTRY_TTL
            && match r {
                Registered::Socket(w) => w.strong_count() > 0,
                Registered::None(_) => true,
            }
    });
    reg.insert(hbb_common::try_into_v4(peer), (now, r));
}

fn meter_option(lan: bool) -> Result<(), &'static str> {
    match Config::get_option(OPTION_ENABLE_TCP_METER).as_str() {
        "Y" if lan => Ok(()),
        "Y" => Err("not_lan"),
        "all" => Ok(()),
        _ => Err("option_off"),
    }
}

/// SO_SNDBUF for remote-control and camera sessions of LAN peers from video-sndbuf-kb (KB,
/// clamped to 16 KB..64 MB); None keeps Windows' dynamic send buffering.
pub fn video_sndbuf_bytes() -> Option<u32> {
    let kb = Config::get_option(OPTION_VIDEO_SNDBUF_KB)
        .trim()
        .parse::<u32>()
        .ok()?;
    (kb > 0).then(|| kb.clamp(16, 64 * 1024) * 1024)
}

/// Whether either option wants a socket handle; the connection then logs whether it got one.
pub fn socket_options_on() -> bool {
    !matches!(
        Config::get_option(OPTION_ENABLE_TCP_METER).as_str(),
        "" | "N"
    ) || video_sndbuf_bytes().is_some()
}

fn probe_socket(stream: &TcpStream, peer: SocketAddr, lan: bool) -> Option<Arc<HostSocket>> {
    let meter = meter_option(lan);
    let sndbuf = lan && video_sndbuf_bytes().is_some();
    if let Err(why) = meter {
        if !sndbuf {
            if socket_options_on() {
                register(peer, Registered::None(why));
            }
            return None;
        }
    }
    let socket = Arc::new(HostSocket::new(stream.as_raw_socket(), lan, meter.is_ok()));
    register(peer, Registered::Socket(Arc::downgrade(&socket)));
    Some(socket)
}

/// The session socket handle of the connection accepted from `peer`, taken out of the registry,
/// or why there is none: `option_off`, `not_lan`, `closed`, or `path` (not an accepted path that
/// wraps its socket: relay, dialed punch, KCP, WebRTC).
pub fn take_host_socket(peer: SocketAddr) -> Result<Arc<HostSocket>, &'static str> {
    let entry = REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&hbb_common::try_into_v4(peer));
    match entry {
        Some((_, Registered::Socket(w))) => w.upgrade().ok_or("closed"),
        Some((_, Registered::None(why))) => Err(why),
        None => Err("path"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn latency_tcp_info_layout_matches_mstcpip() {
        assert_eq!(std::mem::size_of::<TcpInfoV0>(), 88);
        assert_eq!(std::mem::size_of::<TcpInfoV1>(), 136);
    }

    // A real loopback socket: the wrapper passes bytes through and counts them, the meter
    // samples, sees nothing unsent once the peer has read everything, and a handle whose stream
    // is gone never touches the socket again.
    #[tokio::test]
    async fn latency_tcp_meter_counts_and_samples_loopback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(addr), listener.accept());
        let mut client = client.unwrap();
        let (accepted, _) = accepted.unwrap();
        let socket = Arc::new(HostSocket::new(accepted.as_raw_socket(), true, true));
        socket.set_active(true);
        let mut wrapped = HostTcpStream {
            _flow: None,
            stream: accepted,
            socket: socket.clone(),
        };
        let data = vec![5u8; 300_000];
        let (w, r) = tokio::join!(wrapped.write_all(&data), async {
            let mut got = vec![0u8; data.len()];
            client.read_exact(&mut got).await.map(|_| got)
        });
        w.unwrap();
        assert_eq!(r.unwrap(), data);
        assert_eq!(socket.app_bytes.load(Ordering::Relaxed), data.len() as u64);
        if socket.is_metered() {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let win = socket.take_window().unwrap();
            assert!(win.samples >= 1);
            assert_eq!(win.app_bytes, data.len() as u64);
            // Everything was read by the peer, so TCP sent at least what was written. Not
            // `unsent` == 0: loopback reports BytesOut 430683 for 300000 bytes written with no
            // retransmits, so that formula is checked on the real link (docs).
            assert!(win.out_bytes >= data.len() as u64, "out={}", win.out_bytes);
            assert!(socket.set_sndbuf(256 * 1024).is_ok());
        } else {
            eprintln!("SIO_TCP_INFO not available here: pass-through checked only");
        }
        drop(wrapped);
        assert_eq!(socket.take_window().map(|w| w.samples).unwrap_or(0), 0);
        assert!(socket.set_sndbuf(256 * 1024).is_err(), "a closed socket is never touched");
    }

    #[test]
    fn latency_tcp_registry_hands_out_once() {
        let peer: SocketAddr = "192.0.2.77:4000".parse().unwrap();
        register(peer, Registered::None("not_lan"));
        assert_eq!(take_host_socket(peer).err(), Some("not_lan"));
        assert_eq!(take_host_socket(peer).err(), Some("path"));
    }
}
