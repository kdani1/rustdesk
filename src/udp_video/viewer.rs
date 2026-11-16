//! Viewer end of the UDP video lane in shadow mode (M19 second half).
//!
//! - The viewer advertises `LoginRequest.udp_video_version` ([`advertised_version`]) on
//!   Windows, for remote control, unless `enable-udp-video` is `N`.
//! - On the host's offer, [`ViewerLane::on_offer`] checks it and starts a [`UdpVideoClient`]:
//!   a socket bound to the session's own local TCP address (never the wildcard), connected to
//!   the host's offered address, and one receive thread. The thread sends HELLO every 20 ms for
//!   up to 1 s; on HELLO_ACK the io_loop sends READY over TCP. Then it pings every 200 ms,
//!   reassembles shadow frames, parses each complete one for its display and pts (on the
//!   thread, so the io_loop does no full-frame parse for a copy it never shows) and hands it,
//!   stamped, through a tokio mpsc to a `select!` branch of the io_loop, the path the live lane
//!   (M20) will use.
//! - The io_loop pairs UDP and TCP copies by pts ([`super::shadow::ShadowMatcher`]) and logs a
//!   `UDPVIDEO viewer` line every 5 s. UDP copies are never decoded or shown. Both copies are
//!   stamped before any parse in the io_loop: the TCP copy when `peer.next()` returns it
//!   ([`ViewerLane::note_tcp_rx`]), the UDP copy at the entry of its branch.
//!
//! Dropping the client (STOP from the host, a refused handshake, the end of the io_loop round)
//! stops the thread within 100 ms and closes the socket.

use super::{
    crypto::{Role, SECRET_LEN},
    fragment::{Event, LossStats, ReassemblyConfig},
    lane::{DropStats, LaneReceiver, LaneSender, Received},
    net::{self, LaneClock, ReadTimeout},
    packet::{Inner, PathClass, VIDEO_OVERHEAD},
    shadow::{Samples, ShadowMatcher},
};
use base::{
    config::keys,
    message_proto::{
        message, udp_video_control, udp_video_offer, Message, Misc, UdpVideoControl, UdpVideoOffer,
        VideoFrame,
    },
};
use hbb_common::{
    config::LocalConfig, log, protobuf::Message as _, rendezvous_proto::ConnType,
    tokio::sync::mpsc,
};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const HELLO_EVERY: Duration = Duration::from_millis(20);
const HELLO_FOR: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_millis(200);
const WINDOW: Duration = Duration::from_secs(5);
const RECV_BUF: usize = 2048;
/// Shadow frames are unpaced and share the link with the full TCP copy, so a big frame can
/// take well over the live lane's 50 ms to arrive whole. A slow but complete frame must still
/// be delivered and paired (a late one shows as a negative ahead sample), not counted as lost.
const SHADOW_FRAME_TIMEOUT: Duration = Duration::from_millis(500);

/// The lane version to advertise in the login request, 0 for none.
pub fn advertised_version(conn_type: ConnType) -> u32 {
    if cfg!(windows) && conn_type == ConnType::DEFAULT_CONN && enabled() {
        super::host::LANE_VERSION
    } else {
        0
    }
}

/// Viewer option `enable-udp-video`: on unless "N". It only advertises; the host decides.
pub fn enabled() -> bool {
    LocalConfig::get_option(keys::OPTION_ENABLE_UDP_VIDEO) != "N"
}

/// A complete shadow frame, stamped and parsed by the receive thread.
pub struct ShadowFrame {
    pub data: Vec<u8>,
    /// (display, pts) of the video frame inside, if it parsed and has a pts.
    pub key: Option<(i32, i64)>,
    pub completed_at: Instant,
}

/// What the receive thread measured in one window.
#[derive(Default)]
pub struct RecvWindow {
    pub span: Duration,
    pub loss: LossStats,
    pub drops: DropStats,
    pub rtt: Samples,
    /// Arrival of each frame's first fragment minus the host's `send_us`, stamped right before
    /// it sealed and sent the frame's first datagram, both lane clocks: an unknown constant
    /// plus the one-way delay of one datagram, independent of frame size. Only its variation
    /// is reported.
    pub transit: Samples,
    /// First to last fragment of a frame.
    pub assembly: Samples,
    pub pings: u64,
    pub pongs: u64,
    pub recv_errors: u64,
}

pub enum ViewerEvent {
    HelloAck { after: Duration, hellos: u32 },
    HandshakeFailed(String),
    Frame(ShadowFrame),
    Window(Box<RecvWindow>),
}

/// Socket and receive thread of one lane.
pub struct UdpVideoClient {
    lane_id: u64,
    stop: Arc<AtomicBool>,
}

impl Drop for UdpVideoClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What a valid offer gives the viewer.
#[derive(Debug, PartialEq, Eq)]
pub struct Accepted {
    pub local: IpAddr,
    pub host: SocketAddr,
    pub secret: [u8; SECRET_LEN],
    pub every: u32,
}

/// Checks an offer against what this build speaks and the session's own local address.
pub fn check_offer(offer: &UdpVideoOffer, local: SocketAddr) -> Result<Accepted, String> {
    if offer.version != super::host::LANE_VERSION {
        return Err(format!("version {} unsupported", offer.version));
    }
    if offer.mode.enum_value() != Ok(udp_video_offer::Mode::ModeShadow) {
        return Err(format!("mode {:?} unsupported", offer.mode));
    }
    let secret: [u8; SECRET_LEN] = offer
        .secret
        .as_ref()
        .try_into()
        .map_err(|_| format!("secret of {} bytes", offer.secret.len()))?;
    let max = offer.max_datagram as usize;
    if max <= VIDEO_OVERHEAD || max > RECV_BUF {
        return Err(format!("max_datagram {max}"));
    }
    let host_ip = net::ip_from_bytes(&offer.ip).ok_or("bad host address")?;
    let host_ip = net::lane_ip(host_ip).map_err(|e| format!("host address: {e}"))?;
    let port = u16::try_from(offer.port)
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| format!("port {}", offer.port))?;
    let local_ip = net::lane_ip(local.ip()).map_err(|e| format!("local address {local}: {e}"))?;
    if local_ip.is_ipv4() != host_ip.is_ipv4() {
        return Err("address families differ".to_owned());
    }
    Ok(Accepted {
        local: local_ip,
        host: SocketAddr::new(host_ip, port),
        secret,
        every: offer.shadow_every.max(1),
    })
}

impl UdpVideoClient {
    pub fn start(
        a: &Accepted,
        lane_id: u64,
        tx: mpsc::UnboundedSender<ViewerEvent>,
    ) -> Result<(Self, SocketAddr), String> {
        let (socket, notes) = net::bind(a.local, net::VIEWER_RCVBUF, 0)
            .map_err(|e| format!("bind {}: {e}", a.local))?;
        for n in notes {
            log::warn!("UDPVIDEO viewer socket option failed: {n}");
        }
        socket
            .connect(a.host)
            .map_err(|e| format!("connect {}: {e}", a.host))?;
        let local = socket.local_addr().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let secret = a.secret;
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("udp-video-rx".to_owned())
            .spawn(move || recv_loop(socket, secret, lane_id, tx, &stop2))
            .map_err(|e| format!("spawn: {e}"))?;
        Ok((Self { lane_id, stop }, local))
    }
}

fn recv_loop(
    socket: UdpSocket,
    secret: [u8; SECRET_LEN],
    lane_id: u64,
    tx: mpsc::UnboundedSender<ViewerEvent>,
    stop: &AtomicBool,
) {
    #[cfg(windows)]
    let _boost = crate::platform::windows::viewer_thread_boost("udp_video", None);
    let mut lane_tx = LaneSender::new(&secret, lane_id, Role::Viewer, PathClass::Lan);
    // Shadow frames are measured, never decoded: no gate after a loss, every complete frame
    // counts, also a slow one.
    let config = ReassemblyConfig {
        gate_after_loss: false,
        frame_timeout: SHADOW_FRAME_TIMEOUT,
        ..Default::default()
    };
    let mut lane_rx = LaneReceiver::new(&secret, lane_id, Role::Viewer, config);
    let clock = LaneClock::new();
    let started = Instant::now();
    let mut acked = false;
    let mut hellos = 0u32;
    let mut next_hello = started;
    let mut next_ping = started;
    let mut ping_id = 0u32;
    let mut win = RecvWindow::default();
    let mut win_start = started;
    let mut timeout = ReadTimeout::new();
    let mut buf = vec![0u8; RECV_BUF];
    let mut error_logged = false;
    let send = |lane_tx: &mut LaneSender, inner: &Inner| {
        if let Ok(d) = lane_tx.seal(inner) {
            let _ = socket.send(&d);
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if !acked {
            if now.duration_since(started) >= HELLO_FOR {
                let _ = tx.send(ViewerEvent::HandshakeFailed(format!(
                    "no HELLO_ACK in {} ms after {hellos} HELLO",
                    HELLO_FOR.as_millis()
                )));
                return;
            }
            if now >= next_hello {
                send(&mut lane_tx, &Inner::Hello { lane_id });
                hellos += 1;
                next_hello = now + HELLO_EVERY;
            }
        } else if now >= next_ping {
            ping_id = ping_id.wrapping_add(1);
            send(
                &mut lane_tx,
                &Inner::Ping {
                    id: ping_id,
                    send_us: clock.now_us(),
                },
            );
            win.pings += 1;
            next_ping = now + PING_EVERY;
        }
        let busy = !acked || lane_rx.next_deadline().is_some();
        timeout.set(
            &socket,
            if busy {
                net::RECV_TIMEOUT_BUSY
            } else {
                net::RECV_TIMEOUT_IDLE
            },
        );
        match socket.recv(&mut buf) {
            Ok(n) => match lane_rx.on_datagram(&mut buf[..n], Instant::now()) {
                Ok(Received::HelloAck { lane_id: l }) if l == lane_id && !acked => {
                    acked = true;
                    next_ping = Instant::now();
                    let ev = ViewerEvent::HelloAck {
                        after: started.elapsed(),
                        hellos,
                    };
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Ok(Received::Pong { echo_us, .. }) => {
                    win.pongs += 1;
                    win.rtt.push(net::us_diff(clock.now_us(), echo_us));
                }
                // Video fragments surface through poll; refused datagrams are in DropStats.
                Ok(_) | Err(_) => {}
            },
            Err(e) if net::is_timeout(&e) => {}
            Err(e) => {
                win.recv_errors += 1;
                if !error_logged {
                    error_logged = true;
                    log::warn!("UDPVIDEO viewer recv error: {e}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for ev in lane_rx.poll(Instant::now()) {
            if let Event::Frame(f) = ev {
                win.transit
                    .push(net::us_diff(clock.us(f.first_fragment_at), f.send_us));
                win.assembly
                    .push_duration(f.completed_at.saturating_duration_since(f.first_fragment_at));
                let key = parse_video(&f.data).and_then(|vf| {
                    crate::client::latency::frame_pts(&vf).map(|pts| (vf.display, pts))
                });
                let frame = ShadowFrame {
                    data: f.data,
                    key,
                    completed_at: f.completed_at,
                };
                if tx.send(ViewerEvent::Frame(frame)).is_err() {
                    return;
                }
            }
        }
        let now = Instant::now();
        if now.duration_since(win_start) >= WINDOW {
            let (loss, drops) = lane_rx.take_stats();
            win.loss = loss;
            win.drops = drops;
            win.span = now.duration_since(win_start);
            win_start = now;
            if tx
                .send(ViewerEvent::Window(Box::new(std::mem::take(&mut win))))
                .is_err()
            {
                return;
            }
        }
    }
}

fn control(kind: udp_video_control::Kind, lane_id: u64, reason: &str) -> Message {
    let mut misc = Misc::new();
    misc.set_udp_video_control(UdpVideoControl {
        kind: kind.into(),
        lane_id,
        reason: reason.to_owned(),
        ..Default::default()
    });
    let mut msg = Message::new();
    msg.set_misc(misc);
    msg
}

struct Round {
    /// The session stream's local address, when the session may take a lane.
    local: Option<SocketAddr>,
    tx: mpsc::UnboundedSender<ViewerEvent>,
}

/// The io_loop's lane state across rounds.
#[derive(Default)]
pub struct ViewerLane {
    round: Option<Round>,
    client: Option<UdpVideoClient>,
    matcher: ShadowMatcher,
    peer: String,
    /// When `peer.next()` returned the TCP message being handled.
    tcp_rx_at: Option<Instant>,
}

impl ViewerLane {
    /// A new io_loop round. `local` is the session stream's local address when the session is
    /// direct TCP (no KCP, no WebRTC, no relay) and encrypted, else None (an offer is refused).
    pub fn begin_round(
        &mut self,
        peer: &str,
        local: Option<SocketAddr>,
    ) -> mpsc::UnboundedReceiver<ViewerEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.client = None;
        self.matcher = ShadowMatcher::default();
        self.peer = peer.to_owned();
        self.round = Some(Round { local, tx });
        rx
    }

    pub fn end_round(&mut self) {
        self.close("session round ended");
        self.round = None;
    }

    fn close(&mut self, why: &str) {
        if let Some(c) = self.client.take() {
            log::info!(
                "UDPVIDEO viewer peer={} closed lane={:016x}: {why}",
                self.peer,
                c.lane_id
            );
        }
    }

    /// The host's offer; returns the message to send back when it is refused.
    pub fn on_offer(&mut self, offer: &UdpVideoOffer) -> Option<Message> {
        let peer = self.peer.clone();
        let refuse = |why: String| {
            log::info!(
                "UDPVIDEO viewer peer={peer} refused lane={:016x}: {why}",
                offer.lane_id
            );
            Some(control(
                udp_video_control::Kind::KindRequestStop,
                offer.lane_id,
                &format!("viewer refused: {why}"),
            ))
        };
        if !enabled() {
            return refuse("enable-udp-video is N".to_owned());
        }
        let Some(round) = &self.round else {
            return refuse("no session round".to_owned());
        };
        let Some(local) = round.local else {
            return refuse("session is not direct encrypted TCP".to_owned());
        };
        let a = match check_offer(offer, local) {
            Ok(a) => a,
            Err(why) => return refuse(why),
        };
        let tx = round.tx.clone();
        self.close("replaced by a new offer");
        match UdpVideoClient::start(&a, offer.lane_id, tx) {
            Ok((client, bound)) => {
                log::info!(
                    "UDPVIDEO viewer peer={} accepted lane={:016x} mode=shadow host={} local={} every={}",
                    self.peer,
                    offer.lane_id,
                    a.host,
                    bound,
                    a.every
                );
                self.client = Some(client);
                self.matcher = ShadowMatcher::default();
                None
            }
            Err(why) => refuse(why),
        }
    }

    /// Lane control from the host.
    pub fn on_control(&mut self, c: &UdpVideoControl) {
        let ours = self.client.as_ref().map(|c| c.lane_id);
        if ours != Some(c.lane_id) {
            return;
        }
        if c.kind.enum_value() == Ok(udp_video_control::Kind::KindStop) {
            let why = format!("host stopped: {}", c.reason);
            self.close(&why);
        }
    }

    /// An event of the receive thread; returns a message to send to the host, if any.
    pub fn on_event(&mut self, ev: ViewerEvent) -> Option<Message> {
        let lane_id = self.client.as_ref()?.lane_id;
        match ev {
            ViewerEvent::HelloAck { after, hellos } => {
                log::info!(
                    "UDPVIDEO viewer peer={} ready lane={lane_id:016x} after_ms={} hellos={hellos}",
                    self.peer,
                    after.as_millis()
                );
                Some(control(udp_video_control::Kind::KindReady, lane_id, ""))
            }
            ViewerEvent::HandshakeFailed(why) => {
                log::warn!(
                    "UDPVIDEO viewer peer={} handshake failed lane={lane_id:016x}: {why} \
                     (UDP from this viewer to the host, or back, is blocked)",
                    self.peer
                );
                self.client = None;
                Some(control(
                    udp_video_control::Kind::KindRequestStop,
                    lane_id,
                    &why,
                ))
            }
            ViewerEvent::Frame(f) => {
                // Branch entry: the copy was parsed on the receive thread.
                let app = Instant::now();
                if let Some((display, pts)) = f.key {
                    self.matcher.on_udp(display, pts, f.completed_at, app);
                }
                None
            }
            ViewerEvent::Window(w) => {
                self.log_window(lane_id, *w);
                None
            }
        }
    }

    /// The time `peer.next()` returned the TCP message about to be parsed and handled.
    #[inline]
    pub fn note_tcp_rx(&mut self, at: Instant) {
        self.tcp_rx_at = Some(at);
    }

    /// The TCP copy of a frame, as the io_loop handles it. Only while a lane exists. Stamped
    /// with [`Self::note_tcp_rx`], taken before the message was parsed, like the UDP copy.
    #[inline]
    pub fn on_tcp_frame(&mut self, vf: &VideoFrame) {
        let at = self.tcp_rx_at.take();
        if self.client.is_some() {
            if let Some(pts) = crate::client::latency::frame_pts(vf) {
                self.matcher
                    .on_tcp(vf.display, pts, at.unwrap_or_else(Instant::now));
            }
        }
    }

    fn log_window(&mut self, lane_id: u64, mut w: RecvWindow) {
        let mut m = self.matcher.take_window();
        let l = &w.loss;
        let d = &w.drops;
        log::info!(
            "UDPVIDEO viewer peer={} lane={lane_id:016x} win_s={:.1} frames={} lost={} frag_miss={} dgrams={}/{} loss_pct={:.2} reorder={}/{} dup={} late={} bad={} drop={}/{}/{}/{} matched={} tcp_first={} unpaired={} ahead_net_ms={} ahead_app_ms={} owd_var_ms={} asm_ms={} rtt_ms={} pings={}/{} rx_err={}",
            self.peer,
            w.span.as_secs_f64(),
            l.frames_completed,
            l.frames_lost,
            l.fragments_missing,
            l.datagrams_received,
            l.datagrams_expected,
            l.datagram_loss() * 100.0,
            l.reorder_events,
            l.reorder_max,
            l.fragments_duplicate,
            l.fragments_late,
            l.fragments_invalid,
            d.malformed,
            d.replayed,
            d.unauthenticated,
            d.bad_inner,
            m.matched,
            m.tcp_first,
            m.udp_unpaired,
            m.ahead_net.ms4(),
            m.ahead_app.ms4(),
            w.transit.above_min().ms3(),
            w.assembly.ms3(),
            w.rtt.ms3(),
            w.pings,
            w.pongs,
            w.recv_errors,
        );
    }
}

fn parse_video(data: &[u8]) -> Option<VideoFrame> {
    match Message::parse_from_bytes(data).ok()?.union? {
        message::Union::VideoFrame(vf) => Some(vf),
        _ => None,
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use crate::udp_video::host::{ShadowTap, UdpVideoHost};

    fn offer() -> UdpVideoOffer {
        UdpVideoOffer {
            version: 1,
            mode: udp_video_offer::Mode::ModeShadow.into(),
            lane_id: 7,
            secret: vec![9u8; SECRET_LEN].into(),
            ip: vec![192, 168, 1, 5].into(),
            port: 40000,
            max_datagram: 1400,
            shadow_every: 2,
            ..Default::default()
        }
    }

    #[test]
    fn check_offer_rules() {
        let local: SocketAddr = "192.168.1.9:50123".parse().unwrap();
        let a = check_offer(&offer(), local).unwrap();
        assert_eq!(a.local, "192.168.1.9".parse::<IpAddr>().unwrap());
        assert_eq!(a.host, "192.168.1.5:40000".parse().unwrap());
        assert_eq!(a.every, 2);
        let bad = |f: &dyn Fn(&mut UdpVideoOffer)| {
            let mut o = offer();
            f(&mut o);
            check_offer(&o, local).is_err()
        };
        assert!(bad(&|o| o.version = 2));
        assert!(bad(&|o| o.mode = udp_video_offer::Mode::ModeLive.into()));
        assert!(bad(&|o| o.secret = vec![1u8; 16].into()));
        assert!(bad(&|o| o.ip = vec![8, 8, 8, 8].into()));
        assert!(bad(&|o| o.ip = vec![1, 2, 3].into()));
        assert!(bad(&|o| o.port = 0));
        assert!(bad(&|o| o.port = 70000));
        assert!(bad(&|o| o.max_datagram = 9000));
        assert!(check_offer(&offer(), "0.0.0.0:1".parse().unwrap()).is_err());
        assert!(check_offer(&offer(), "[fd00::1]:1".parse().unwrap()).is_err());
    }

    fn next_event(rx: &mut mpsc::UnboundedReceiver<ViewerEvent>) -> ViewerEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(ev) => return ev,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("no event: {e:?}"),
            }
        }
    }

    /// Host and viewer ends over loopback (the LAN gate is bypassed: it refuses loopback):
    /// HELLO, HELLO_ACK, READY, then every 2nd frame arrives whole as the same message.
    #[test]
    fn loopback_handshake_and_shadow_frames() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let tap = Arc::new(ShadowTap::default());
        let (mut host, offer) = UdpVideoHost::start(1, tap.clone(), lo, lo, 2).unwrap();
        let a = Accepted {
            local: lo,
            host: SocketAddr::new(lo, offer.port as u16),
            secret: offer.secret.as_ref().try_into().unwrap(),
            every: offer.shadow_every,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_client, _) = UdpVideoClient::start(&a, offer.lane_id, tx).unwrap();
        match next_event(&mut rx) {
            ViewerEvent::HelloAck { hellos, .. } => assert!(hellos >= 1),
            _ => panic!("expected HELLO_ACK"),
        }
        assert!(host.on_ready(offer.lane_id.wrapping_add(1)).is_err());
        host.on_ready(offer.lane_id).unwrap();
        let sent: Vec<_> = (0..6)
            .map(|pts| crate::udp_video::host::latency_tests::video(0, pts * 1000, pts == 0))
            .collect();
        for m in &sent {
            tap.offer(m);
            // The queue holds 4; give the sender thread time so nothing is dropped.
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut got = Vec::new();
        while got.len() < 3 {
            if let ViewerEvent::Frame(f) = next_event(&mut rx) {
                // Parsed on the receive thread.
                assert_eq!(f.key, Some((0, got.len() as i64 * 2000)));
                got.push(Message::parse_from_bytes(&f.data).unwrap());
            }
        }
        assert_eq!(got[0], *sent[0]);
        assert_eq!(got[1], *sent[2]);
        assert_eq!(got[2], *sent[4]);
        assert!(host.check().is_ok());
    }
}
