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

/// The viewer's address for an internet lane (M24), normalized, or why not: a unicast address
/// that is not on the LAN (that path has its own gate), not loopback, unspecified, link-local
/// or multicast. CGNAT and other shared ranges count as internet (a tunnel or a carrier NAT:
/// the WAN datagram size is the safe one there).
pub fn wan_ip(ip: IpAddr) -> Result<IpAddr, &'static str> {
    let ip = hbb_common::try_into_v4(SocketAddr::new(ip, 0)).ip();
    if ip.is_unspecified() {
        return Err("unspecified address");
    }
    if ip.is_loopback() {
        return Err("loopback address");
    }
    if ip.is_multicast() || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()) {
        return Err("multicast address");
    }
    if is_link_local_v6(ip) {
        return Err("link-local IPv6");
    }
    if is_lan(ip) {
        return Err("a LAN address");
    }
    Ok(ip)
}

/// The wildcard address of `ip`'s family: an internet lane socket is bound to it (the session's
/// own local address can be a KCP stream's unspecified one, and the route to the viewer is the
/// system's choice).
pub fn any_of_family(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
    }
}

/// 100.64.0.0/10 (RFC 6598): carrier-grade NAT and overlay networks (Tailscale).
pub fn is_shared_v4(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
}

/// How long a lane end waits for the rendezvous server's DNS lookup.
pub const DNS_FOR: Duration = Duration::from_millis(1000);

/// The rendezvous server's UDP address in `peer`'s family, for the NAT mapping query; `peer` is
/// the other end's session address. Blocking DNS, called on a lane thread and bounded by
/// `DNS_FOR` (the lookup runs on a helper thread; a late answer is dropped). None for an empty
/// server, IPv6 (no NAT to learn there: the lane socket's local port is its public port), a peer
/// in 100.64.0.0/10 (an overlay or in-carrier path: the port the rendezvous server sees is that
/// of another path, the local port is the one this path reaches), or no answer in time.
pub fn rendezvous_udp(server: &str, peer: IpAddr) -> Option<SocketAddr> {
    use std::net::ToSocketAddrs;
    if server.is_empty() || peer.is_ipv6() || is_shared_v4(peer) {
        return None;
    }
    let host = hbb_common::socket_client::check_port(server, hbb_common::config::RENDEZVOUS_PORT);
    let v4 = peer.is_ipv4();
    if let Ok(a) = host.parse::<SocketAddr>() {
        return (a.is_ipv4() == v4).then_some(a);
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("udp-video-dns".to_owned())
        .spawn(move || {
            let found = host
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.find(|a| a.is_ipv4() == v4));
            let _ = tx.send(found);
        })
        .ok()?;
    rx.recv_timeout(DNS_FOR).ok().flatten()
}

/// How long a lane end asks the rendezvous server for its public port.
pub const NAT_QUERY_FOR: Duration = Duration::from_millis(1000);
const NAT_QUERY_EVERY: Duration = Duration::from_millis(150);

/// The public UDP port of `socket` (unconnected) as the rendezvous server sees it: the reply to
/// a `TestNatRequest`, the query the session's own UDP punch uses. Asked every 150 ms for up to
/// `wait`; only a reply from `server` counts. None when the server does not answer.
pub fn nat_port(socket: &UdpSocket, server: SocketAddr, wait: Duration) -> Option<u16> {
    use hbb_common::{
        protobuf::Message as _,
        rendezvous_proto::{rendezvous_message, RendezvousMessage, TestNatRequest},
    };
    let mut req = RendezvousMessage::new();
    req.set_test_nat_request(TestNatRequest::default());
    let req = req.write_to_bytes().ok()?;
    let started = Instant::now();
    let mut next = started;
    let mut buf = [0u8; 1500];
    let mut timeout = ReadTimeout::new();
    loop {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(started);
        if elapsed >= wait {
            return None;
        }
        if now >= next {
            let _ = socket.send_to(&req, server);
            next = now + NAT_QUERY_EVERY;
        }
        let left = next
            .saturating_duration_since(now)
            .min(wait - elapsed)
            .max(Duration::from_millis(1));
        timeout.set(socket, left);
        let Ok((n, src)) = socket.recv_from(&mut buf) else {
            continue;
        };
        if hbb_common::try_into_v4(src) != hbb_common::try_into_v4(server) {
            continue;
        }
        if let Ok(RendezvousMessage {
            union: Some(rendezvous_message::Union::TestNatResponse(r)),
            ..
        }) = RendezvousMessage::parse_from_bytes(&buf[..n])
        {
            if let Some(port) = u16::try_from(r.port).ok().filter(|p| *p != 0) {
                return Some(port);
            }
        }
    }
}

/// Drops whatever waits in the socket's receive queue (extra NAT replies): `connect` does not
/// flush it.
pub fn drain(socket: &UdpSocket) {
    if socket.set_nonblocking(true).is_ok() {
        let mut buf = [0u8; 1500];
        while socket.recv_from(&mut buf).is_ok() {}
        let _ = socket.set_nonblocking(false);
    }
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
    // Android viewers (Quest): the default receive buffer (about 200 KB) is smaller than one
    // key frame burst, so ask for the same sizes as on Windows (capped by net.core.rmem_max).
    #[cfg(target_os = "android")]
    {
        if rcvbuf > 0 {
            if let Err(e) = unix::set_buf(&socket, hbb_common::libc::SO_RCVBUF, rcvbuf) {
                notes.push(format!("SO_RCVBUF: {e}"));
            }
        }
        if sndbuf > 0 {
            if let Err(e) = unix::set_buf(&socket, hbb_common::libc::SO_SNDBUF, sndbuf) {
                notes.push(format!("SO_SNDBUF: {e}"));
            }
        }
    }
    #[cfg(not(any(windows, target_os = "android")))]
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

    /// Time since the lane started.
    pub fn elapsed(&self) -> Duration {
        self.0.elapsed()
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

#[cfg(target_os = "android")]
mod unix {
    use hbb_common::libc;
    use std::{io, net::UdpSocket, os::unix::io::AsRawFd};

    pub fn set_buf(socket: &UdpSocket, name: libc::c_int, bytes: usize) -> io::Result<()> {
        let v = bytes.min(libc::c_int::MAX as usize) as libc::c_int;
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                name,
                &v as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
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
    fn latency_wan_ip_gate() {
        let v4: IpAddr = "8.8.8.8".parse().unwrap();
        assert_eq!(wan_ip(v4), Ok(v4));
        let mapped = IpAddr::V6(Ipv4Addr::new(1, 2, 3, 4).to_ipv6_mapped());
        assert_eq!(wan_ip(mapped), Ok("1.2.3.4".parse().unwrap()));
        assert!(wan_ip("100.64.0.1".parse().unwrap()).is_ok());
        assert!(wan_ip("2001:db8::1".parse().unwrap()).is_ok());
        for bad in [
            "192.168.1.20",
            "10.0.0.1",
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "fd00::5",
            "::",
            "::1",
        ] {
            assert!(wan_ip(bad.parse().unwrap()).is_err(), "{bad}");
        }
        assert_eq!(
            any_of_family("1.2.3.4".parse().unwrap()),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
        assert!(any_of_family("2001:db8::1".parse().unwrap()).is_ipv6());
    }

    /// A stand-in rendezvous server answers the NAT query with a port; a foreign source's
    /// answer is ignored, and a silent server gives None after the wait.
    #[test]
    fn latency_nat_port_query() {
        use hbb_common::{
            protobuf::Message as _,
            rendezvous_proto::{rendezvous_message, RendezvousMessage, TestNatResponse},
        };
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
        let lane = UdpSocket::bind("127.0.0.1:0").unwrap();
        let lane_addr = lane.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            let (n, src) = server.recv_from(&mut buf).unwrap();
            let req = RendezvousMessage::parse_from_bytes(&buf[..n]).unwrap();
            assert!(matches!(
                req.union,
                Some(rendezvous_message::Union::TestNatRequest(_))
            ));
            let answer = |port| {
                let mut m = RendezvousMessage::new();
                m.set_test_nat_response(TestNatResponse {
                    port,
                    ..Default::default()
                });
                m.write_to_bytes().unwrap()
            };
            foreign.send_to(&answer(1), src).unwrap();
            server.send_to(&answer(4242), src).unwrap();
            src
        });
        assert_eq!(
            nat_port(&lane, server_addr, Duration::from_secs(2)),
            Some(4242)
        );
        assert_eq!(t.join().unwrap(), lane_addr);
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let started = Instant::now();
        assert_eq!(
            nat_port(&lane, silent.local_addr().unwrap(), Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(rendezvous_udp("", "1.2.3.4".parse().unwrap()), None);
        assert_eq!(
            rendezvous_udp("127.0.0.1", "2001:db8::1".parse().unwrap()),
            None
        );
        assert_eq!(
            rendezvous_udp("127.0.0.1", "1.2.3.4".parse().unwrap()),
            Some(
                format!("127.0.0.1:{}", hbb_common::config::RENDEZVOUS_PORT)
                    .parse()
                    .unwrap()
            )
        );
        // Through the lookup thread.
        assert_eq!(
            rendezvous_udp("localhost:21116", "1.2.3.4".parse().unwrap()),
            Some("127.0.0.1:21116".parse().unwrap())
        );
    }

    #[test]
    fn latency_wan_overlay_peer_skips_nat_query() {
        for ip in ["100.64.0.1", "100.101.2.3", "100.127.255.254"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_shared_v4(ip), "{ip}");
            assert!(wan_ip(ip).is_ok(), "{ip} is still an internet lane peer");
            assert_eq!(rendezvous_udp("127.0.0.1", ip), None, "{ip}");
        }
        for ip in ["100.63.255.255", "100.128.0.1", "81.2.3.4"] {
            assert!(!is_shared_v4(ip.parse().unwrap()), "{ip}");
        }
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
