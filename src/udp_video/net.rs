//! Sockets and addresses of the UDP video lane (M19 second half).
//!
//! - LAN gate and address normalization for the offer (review fix: `try_into_v4`, no
//!   link-local IPv6 in phase 1, no unspecified address).
//! - Socket setup: bound to one interface address, never the wildcard, with
//!   `SIO_UDP_CONNRESET` off on Windows (otherwise one ICMP port-unreachable makes the next
//!   `recv` fail with WSAECONNRESET) and larger kernel buffers.
//! - The lane clock: microseconds since the lane started, mod 2^32 (the `send_us` field).

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

/// Receive buffer the viewer asks for (Moonlight uses a similar size): an IDR at 4K arrives
/// as hundreds of datagrams in one burst.
pub const VIEWER_RCVBUF: usize = 4 * 1024 * 1024;
/// Send buffer the host asks for, so a burst never waits in `send`.
pub const HOST_SNDBUF: usize = 2 * 1024 * 1024;

fn is_lan_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

/// RFC 1918, link-local and unique-local addresses (v4-mapped too). CGNAT is not LAN. Same rule
/// as `platform::windows_qos::is_lan`, here for every platform.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_lan_v4(v4),
            None => {
                let s0 = v6.segments()[0];
                (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80
            }
        },
    }
}

fn is_link_local_v6(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
}

/// The address a lane endpoint may use, normalized, or why not: v4-mapped IPv6 becomes IPv4,
/// an unspecified address is refused (KCP streams report one), link-local IPv6 is refused in
/// phase 1 (its scope id does not travel in the offer), and the address must be on the LAN.
pub fn lane_ip(ip: IpAddr) -> Result<IpAddr, &'static str> {
    let ip = hbb_common::try_into_v4(SocketAddr::new(ip, 0)).ip();
    if ip.is_unspecified() {
        return Err("unspecified address");
    }
    if ip.is_loopback() {
        return Err("loopback address");
    }
    if is_link_local_v6(ip) {
        return Err("link-local IPv6");
    }
    if !is_lan(ip) {
        return Err("not a LAN address");
    }
    Ok(ip)
}

/// The IP of an offer's `ip` bytes (4 or 16).
pub fn ip_from_bytes(b: &[u8]) -> Option<IpAddr> {
    match b.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(b).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(b).ok()?)),
        _ => None,
    }
}

pub fn ip_to_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

/// A blocking UDP socket bound to `(ip, 0)` with the lane's socket options. Option failures
/// are logged by the caller from the returned notes; only the bind itself can fail.
pub fn bind(ip: IpAddr, rcvbuf: usize, sndbuf: usize) -> io::Result<(UdpSocket, Vec<String>)> {
    let socket = UdpSocket::bind(SocketAddr::new(ip, 0))?;
    let mut notes = Vec::new();
    #[cfg(windows)]
    {
        if let Err(e) = win::connreset_off(&socket) {
            notes.push(format!("SIO_UDP_CONNRESET: {e}"));
        }
        if rcvbuf > 0 {
            if let Err(e) = win::set_buf(&socket, win::SO_RCVBUF, rcvbuf) {
                notes.push(format!("SO_RCVBUF: {e}"));
            }
        }
        if sndbuf > 0 {
            if let Err(e) = win::set_buf(&socket, win::SO_SNDBUF, sndbuf) {
                notes.push(format!("SO_SNDBUF: {e}"));
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (rcvbuf, sndbuf);
    Ok((socket, notes))
}

/// Read timeouts the receive loops switch between: short while a frame is being reassembled
/// (its loss deadline is near), long when idle.
pub const RECV_TIMEOUT_BUSY: Duration = Duration::from_millis(5);
pub const RECV_TIMEOUT_IDLE: Duration = Duration::from_millis(100);

/// Sets the read timeout only when it changes (one syscall saved per datagram).
pub struct ReadTimeout(Option<Duration>);

impl ReadTimeout {
    pub fn new() -> Self {
        Self(None)
    }

    pub fn set(&mut self, socket: &UdpSocket, t: Duration) {
        if self.0 != Some(t) && socket.set_read_timeout(Some(t)).is_ok() {
            self.0 = Some(t);
        }
    }
}

/// Whether a `recv` error is only the read timeout.
pub fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Microseconds since the lane started, mod 2^32. Only differences are used.
#[derive(Clone, Copy)]
pub struct LaneClock(Instant);

impl LaneClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }

    pub fn us(&self, at: Instant) -> u32 {
        at.saturating_duration_since(self.0).as_micros() as u32
    }

    pub fn now_us(&self) -> u32 {
        self.us(Instant::now())
    }
}

/// `a - b` for two lane-clock values, as a signed number (valid within +-35 minutes).
pub fn us_diff(a: u32, b: u32) -> i64 {
    a.wrapping_sub(b) as i32 as i64
}

#[cfg(windows)]
mod win {
    use std::{ffi::c_void, io, net::UdpSocket, os::windows::io::AsRawSocket};

    // mswsock.h: _WSAIOW(IOC_VENDOR, 12)
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    const SOL_SOCKET: i32 = 0xffff;
    pub const SO_SNDBUF: i32 = 0x1001;
    pub const SO_RCVBUF: i32 = 0x1002;

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
    }

    fn last_error() -> io::Error {
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    pub fn connreset_off(socket: &UdpSocket) -> io::Result<()> {
        let off: u32 = 0;
        let mut returned = 0u32;
        let rc = unsafe {
            WSAIoctl(
                socket.as_raw_socket() as usize,
                SIO_UDP_CONNRESET,
                &off as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }

    pub fn set_buf(socket: &UdpSocket, name: i32, bytes: usize) -> io::Result<()> {
        let v = bytes.min(i32::MAX as usize) as i32;
        let rc = unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                SOL_SOCKET,
                name,
                &v as *const i32 as *const u8,
                std::mem::size_of::<i32>() as i32,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn lane_ip_gate() {
        let v4: IpAddr = "192.168.1.20".parse().unwrap();
        assert_eq!(lane_ip(v4), Ok(v4));
        let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 7).to_ipv6_mapped());
        assert_eq!(lane_ip(mapped), Ok("10.0.0.7".parse().unwrap()));
        assert!(lane_ip("0.0.0.0".parse().unwrap()).is_err());
        assert!(lane_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)).is_err());
        assert!(lane_ip("fe80::1".parse().unwrap()).is_err());
        assert!(lane_ip("8.8.8.8".parse().unwrap()).is_err());
        assert!(lane_ip("100.64.0.1".parse().unwrap()).is_err());
        assert!(lane_ip("127.0.0.1".parse().unwrap()).is_err());
        assert_eq!(
            lane_ip("fd00::5".parse().unwrap()),
            Ok("fd00::5".parse().unwrap())
        );
    }

    #[test]
    fn ip_bytes_round_trip() {
        for ip in ["192.168.0.2", "fd12::9"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert_eq!(ip_from_bytes(&ip_to_bytes(ip)), Some(ip));
        }
        assert_eq!(ip_from_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn lane_clock_diff_wraps() {
        assert_eq!(us_diff(5, u32::MAX - 4), 10);
        assert_eq!(us_diff(10, 15), -5);
    }
}
