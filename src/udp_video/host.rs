//! Host end of the UDP video lane in shadow mode (M19 second half).
//!
//! The connection offers a lane after a remote-control login when every gate passes
//! ([`offer_gate`]). [`UdpVideoHost`] owns the socket and two threads:
//!
//! - receive thread: answers the viewer's HELLO (only from the session's peer IP, only if it
//!   opens with the lane keys) with HELLO_ACK and then `connect()`s the socket to the viewer's
//!   address; answers PING with PONG. Blocking `recv` with a 100 ms timeout.
//! - sender thread: takes shadow jobs from a bounded queue, serializes the frame once, seals
//!   and sends its datagrams back to back (no pacing in shadow mode), and logs a
//!   `UDPVIDEO host` line every 5 s.
//!
//! The connection's video path calls [`ShadowTap::offer`] with every video item before its TCP
//! send. While no lane is active that is one relaxed atomic load. The tap never blocks: a full
//! queue drops the copy (`qdrop`).
//!
//! Dropping `UdpVideoHost` stops both threads (they exit within 100 ms; nothing joins them on
//! the async runtime) and closes the socket when the last thread lets go of it.

use super::{
    crypto::{Role, SECRET_LEN},
    fragment::FrameMeta,
    lane::{LaneReceiver, LaneSender, Received},
    net::{self, LaneClock},
    packet::{Inner, PathClass, FLAG_KEY, FLAG_SHADOW},
    shadow::Samples,
};
use base::message_proto::{
    message, misc, udp_video_offer, video_frame, Message, UdpVideoOffer, VideoFrame,
};
use hbb_common::{log, protobuf::Message as _};
use std::{
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Lane version this build speaks.
pub const LANE_VERSION: u32 = 1;
/// From the offer to the viewer's READY.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Default of `udp-video-shadow-every`: half the frames, so the extra Wi-Fi load stays at half
/// the video bitrate.
pub const DEFAULT_SHADOW_EVERY: u32 = 2;
const QUEUE: usize = 4;
const LOG_EVERY: Duration = Duration::from_secs(5);

/// What the connection knows about its transport when it starts, for the offer gate.
#[derive(Clone, Copy, Debug)]
pub struct Gate {
    /// The session came in on a direct TCP path (intranet, direct IP access, TCP punch).
    pub direct_tcp: bool,
    pub secured: bool,
    /// The session stream's local address (the host IP the viewer reached).
    pub local: SocketAddr,
    /// The viewer's address as the connection keys on it.
    pub peer: SocketAddr,
}

/// The host IP to bind and offer and the viewer IP HELLO must come from, or why there is no
/// offer. `video_ack_required` (web client) and the connection type are checked by the caller.
pub fn offer_gate(g: &Gate) -> Result<(IpAddr, IpAddr), String> {
    if !g.direct_tcp {
        return Err("transport is not direct TCP".to_owned());
    }
    if !g.secured {
        return Err("session stream not encrypted".to_owned());
    }
    let local = net::lane_ip(g.local.ip()).map_err(|e| format!("host address {}: {e}", g.local))?;
    let peer = net::lane_ip(g.peer.ip()).map_err(|e| format!("peer address {}: {e}", g.peer))?;
    if local.is_ipv4() != peer.is_ipv4() {
        return Err("address families differ".to_owned());
    }
    Ok((local, peer))
}

/// Counters the tap and both threads share; the sender thread logs and resets the windowed
/// ones.
#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    peer: Mutex<Option<SocketAddr>>,
    hellos: AtomicU64,
    pings: AtomicU64,
    /// Datagrams refused before or by the crypto (foreign source, bad tag, replay).
    refused: AtomicU64,
    skipped: AtomicU64,
    qdrop: AtomicU64,
}

struct Job {
    msg: Arc<Message>,
    display: u8,
    epoch: u8,
    flags: u8,
    queued: Instant,
}

#[derive(Default)]
struct TapInner {
    tx: Option<SyncSender<Job>>,
    shared: Option<Arc<Shared>>,
    every: u32,
    /// Video items seen per display while active, for the every-Nth choice.
    seen: Vec<(i32, u32)>,
    epoch: u8,
}

/// The connection's hook in front of every TCP video send (whole or split session).
#[derive(Default)]
pub struct ShadowTap {
    active: AtomicBool,
    inner: Mutex<TapInner>,
}

fn has_key_frame(vf: &VideoFrame) -> bool {
    use video_frame::Union::*;
    match vf.union.as_ref() {
        Some(Vp8s(f)) | Some(Vp9s(f)) | Some(Av1s(f)) | Some(H264s(f)) | Some(H265s(f)) => {
            f.frames.iter().any(|e| e.key)
        }
        _ => false,
    }
}

impl ShadowTap {
    /// A video channel item (VideoFrame or SwitchDisplay), before its TCP send.
    #[inline]
    pub fn offer(&self, msg: &Arc<Message>) {
        if self.active.load(Ordering::Relaxed) {
            self.offer_active(msg);
        }
    }

    fn offer_active(&self, msg: &Arc<Message>) {
        let Ok(mut t) = self.inner.lock() else {
            return;
        };
        match &msg.union {
            Some(message::Union::VideoFrame(vf)) => {
                let every = t.every.max(1);
                let i = match t.seen.iter().position(|(d, _)| *d == vf.display) {
                    Some(i) => i,
                    None => {
                        t.seen.push((vf.display, 0));
                        t.seen.len() - 1
                    }
                };
                let n = t.seen[i].1;
                t.seen[i].1 = n.wrapping_add(1);
                let pick = n % every == 0;
                let Some(shared) = t.shared.clone() else {
                    return;
                };
                if !pick {
                    shared.skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let job = Job {
                    msg: msg.clone(),
                    display: vf.display.clamp(0, u8::MAX as i32) as u8,
                    epoch: t.epoch,
                    flags: FLAG_SHADOW | if has_key_frame(vf) { FLAG_KEY } else { 0 },
                    queued: Instant::now(),
                };
                if let Some(tx) = &t.tx {
                    if let Err(TrySendError::Full(_)) = tx.try_send(job) {
                        shared.qdrop.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Some(message::Union::Misc(m))
                if matches!(m.union, Some(misc::Union::SwitchDisplay(_))) =>
            {
                // The viewer will hold frames of a newer epoch until it read the SwitchDisplay
                // (M20); in shadow mode it only travels along.
                t.epoch = t.epoch.wrapping_add(1);
            }
            _ => {}
        }
    }

    fn activate(&self, tx: SyncSender<Job>, shared: Arc<Shared>, every: u32) {
        if let Ok(mut t) = self.inner.lock() {
            *t = TapInner {
                tx: Some(tx),
                shared: Some(shared),
                every,
                ..Default::default()
            };
            self.active.store(true, Ordering::Relaxed);
        }
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
        if let Ok(mut t) = self.inner.lock() {
            *t = TapInner::default();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Offered,
    Active,
}

/// One connection's lane: socket, threads, and the tap it feeds while active.
pub struct UdpVideoHost {
    conn_id: i32,
    lane_id: u64,
    every: u32,
    state: State,
    offered_at: Instant,
    tap: Arc<ShadowTap>,
    shared: Arc<Shared>,
    job_tx: Option<SyncSender<Job>>,
}

impl UdpVideoHost {
    /// Binds the lane socket on `local`, starts the threads, and returns the offer to send.
    pub fn start(
        conn_id: i32,
        tap: Arc<ShadowTap>,
        local: IpAddr,
        viewer: IpAddr,
        every: u32,
    ) -> Result<(Self, UdpVideoOffer), String> {
        let (socket, notes) = net::bind(local, 0, net::HOST_SNDBUF)
            .map_err(|e| format!("bind {local}: {e}"))?;
        for n in notes {
            log::warn!("UDPVIDEO host conn={conn_id} socket option failed: {n}");
        }
        let port = socket
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?
            .port();
        let secret: [u8; SECRET_LEN] =
            hbb_common::sodiumoxide::crypto::secretbox::gen_key().0;
        let lane_id: u64 = hbb_common::rand::random();
        let socket = Arc::new(socket);
        let shared = Arc::new(Shared::default());
        let sender = Arc::new(Mutex::new(LaneSender::new(
            &secret,
            lane_id,
            Role::Host,
            PathClass::Lan,
        )));
        let clock = LaneClock::new();
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(QUEUE);
        {
            let (socket, sender, shared) = (socket.clone(), sender.clone(), shared.clone());
            std::thread::Builder::new()
                .name("udp-video-host-rx".to_owned())
                .spawn(move || {
                    recv_loop(
                        &socket, &sender, &shared, &secret, lane_id, viewer, conn_id, clock,
                    )
                })
                .map_err(|e| format!("spawn: {e}"))?;
        }
        let tx_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("udp-video-host-tx".to_owned())
            .spawn(move || {
                send_loop(&socket, &sender, &tx_shared, job_rx, conn_id, lane_id, clock)
            });
        if let Err(e) = spawned {
            // The receive thread is already running: stop it.
            shared.stop.store(true, Ordering::Relaxed);
            return Err(format!("spawn: {e}"));
        }
        let offer = UdpVideoOffer {
            version: LANE_VERSION,
            mode: udp_video_offer::Mode::ModeShadow.into(),
            lane_id,
            secret: secret.to_vec().into(),
            ip: net::ip_to_bytes(local).into(),
            port: port as u32,
            max_datagram: PathClass::Lan.max_datagram() as u32,
            shadow_every: every,
            ..Default::default()
        };
        log::info!(
            "UDPVIDEO host conn={conn_id} offer lane={lane_id:016x} mode=shadow addr={} every={every} viewer={viewer} installed={}",
            SocketAddr::new(local, port),
            installed()
        );
        Ok((
            Self {
                conn_id,
                lane_id,
                every,
                state: State::Offered,
                offered_at: Instant::now(),
                tap,
                shared,
                job_tx: Some(job_tx),
            },
            offer,
        ))
    }

    pub fn lane_id(&self) -> u64 {
        self.lane_id
    }

    /// The viewer's READY: start copying frames. Err when it does not fit this lane.
    pub fn on_ready(&mut self, lane_id: u64) -> Result<(), String> {
        if lane_id != self.lane_id {
            return Err(format!("READY for another lane {lane_id:016x}"));
        }
        if self.state == State::Active {
            return Ok(());
        }
        let peer = self.shared.peer.lock().ok().and_then(|p| *p);
        let Some(peer) = peer else {
            return Err("READY before any HELLO".to_owned());
        };
        let Some(tx) = self.job_tx.take() else {
            return Err("lane already used".to_owned());
        };
        self.tap.activate(tx, self.shared.clone(), self.every);
        self.state = State::Active;
        log::info!(
            "UDPVIDEO host conn={} active lane={:016x} mode=shadow peer={} every={} after_ms={}",
            self.conn_id,
            self.lane_id,
            peer,
            self.every,
            self.offered_at.elapsed().as_millis()
        );
        Ok(())
    }

    /// Called every second: Err(reason) when the handshake timed out.
    pub fn check(&self) -> Result<(), String> {
        if self.state != State::Offered || self.offered_at.elapsed() < HANDSHAKE_TIMEOUT {
            return Ok(());
        }
        let hellos = self.shared.hellos.load(Ordering::Relaxed);
        if hellos == 0 {
            Err(format!(
                "no HELLO from the viewer in {} s: inbound UDP to this host looks blocked \
                 (installed={}; the installer's \"{} Service\" firewall rule allows the \
                 installed exe on every protocol, so suspect a third-party firewall, a \
                 portable exe without a rule, or a network that drops UDP; refused={})",
                HANDSHAKE_TIMEOUT.as_secs(),
                installed(),
                crate::get_app_name(),
                self.shared.refused.load(Ordering::Relaxed)
            ))
        } else {
            Err(format!(
                "{hellos} HELLO answered but no READY over TCP in {} s \
                 (HELLO_ACK lost on the way back?)",
                HANDSHAKE_TIMEOUT.as_secs()
            ))
        }
    }
}

impl Drop for UdpVideoHost {
    fn drop(&mut self) {
        self.tap.deactivate();
        self.shared.stop.store(true, Ordering::Relaxed);
        // `job_tx`, if still here, goes with self; the sender thread sees the channel close.
        log::info!(
            "UDPVIDEO host conn={} closed lane={:016x} state={:?}",
            self.conn_id,
            self.lane_id,
            self.state
        );
    }
}

fn installed() -> bool {
    #[cfg(windows)]
    {
        crate::platform::is_installed()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn recv_loop(
    socket: &UdpSocket,
    sender: &Mutex<LaneSender>,
    shared: &Shared,
    secret: &[u8; SECRET_LEN],
    lane_id: u64,
    viewer: IpAddr,
    conn_id: i32,
    clock: LaneClock,
) {
    let mut rx = LaneReceiver::new(secret, lane_id, Role::Host, Default::default());
    let _ = socket.set_read_timeout(Some(net::RECV_TIMEOUT_IDLE));
    let mut buf = [0u8; 2048];
    let mut connected = false;
    let mut first_error_logged = false;
    while !shared.stop.load(Ordering::Relaxed) {
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if net::is_timeout(&e) => continue,
            Err(e) => {
                if !first_error_logged {
                    first_error_logged = true;
                    log::warn!("UDPVIDEO host conn={conn_id} recv error: {e}");
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        if !connected && hbb_common::try_into_v4(src).ip() != viewer {
            shared.refused.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let reply = match rx.on_datagram(&mut buf[..n], Instant::now()) {
            Ok(Received::Hello { lane_id: l }) if l == lane_id => {
                shared.hellos.fetch_add(1, Ordering::Relaxed);
                if !connected {
                    if let Err(e) = socket.connect(src) {
                        log::warn!("UDPVIDEO host conn={conn_id} connect {src}: {e}");
                        continue;
                    }
                    connected = true;
                    if let Ok(mut p) = shared.peer.lock() {
                        *p = Some(src);
                    }
                    log::info!(
                        "UDPVIDEO host conn={conn_id} HELLO from {src} lane={lane_id:016x} at_us={}",
                        clock.now_us()
                    );
                }
                Inner::HelloAck { lane_id }
            }
            Ok(Received::Ping { id, send_us }) => {
                shared.pings.fetch_add(1, Ordering::Relaxed);
                Inner::Pong {
                    id,
                    echo_us: send_us,
                }
            }
            Ok(_) => continue,
            Err(_) => {
                shared.refused.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let sealed = sender.lock().ok().and_then(|mut s| s.seal(&reply).ok());
        if let Some(d) = sealed {
            if let Err(e) = socket.send(&d) {
                log::debug!("UDPVIDEO host conn={conn_id} reply send: {e}");
            }
        }
    }
}

#[derive(Default)]
struct Window {
    frames: u64,
    keys: u64,
    dgrams: u64,
    bytes: u64,
    errors: u64,
    first_error: Option<String>,
    send: Samples,
    lag: Samples,
}

fn send_loop(
    socket: &UdpSocket,
    sender: &Mutex<LaneSender>,
    shared: &Shared,
    jobs: Receiver<Job>,
    conn_id: i32,
    lane_id: u64,
    clock: LaneClock,
) {
    // Next lane frame id per display: consecutive over the frames actually sent, so a copy the
    // tap dropped is not counted as loss by the viewer.
    let mut ids: Vec<(u8, u32)> = Vec::new();
    let mut w = Window::default();
    let mut last_log = Instant::now();
    loop {
        match jobs.recv_timeout(Duration::from_secs(1)) {
            Ok(job) => send_job(socket, sender, &mut ids, &mut w, job, clock),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if last_log.elapsed() >= LOG_EVERY {
            log_window(conn_id, lane_id, shared, &mut w, last_log.elapsed());
            last_log = Instant::now();
        }
    }
    if w.frames > 0 {
        log_window(conn_id, lane_id, shared, &mut w, last_log.elapsed());
    }
}

fn send_job(
    socket: &UdpSocket,
    sender: &Mutex<LaneSender>,
    ids: &mut Vec<(u8, u32)>,
    w: &mut Window,
    job: Job,
    clock: LaneClock,
) {
    let begin = Instant::now();
    w.lag.push_duration(begin.saturating_duration_since(job.queued));
    let bytes = match job.msg.write_to_bytes() {
        Ok(b) => b,
        Err(e) => {
            w.errors += 1;
            w.first_error.get_or_insert_with(|| format!("serialize: {e}"));
            return;
        }
    };
    let id = match ids.iter_mut().find(|(d, _)| *d == job.display) {
        Some((_, next)) => {
            let id = *next;
            *next = next.wrapping_add(1);
            id
        }
        None => {
            ids.push((job.display, 1));
            0
        }
    };
    let meta = FrameMeta {
        frame_id: id,
        display: job.display,
        epoch: job.epoch,
        flags: job.flags,
        send_us: clock.us(begin),
    };
    let dgrams = match sender.lock() {
        Ok(mut s) => s.frame(&bytes, meta),
        Err(_) => return,
    };
    let dgrams = match dgrams {
        Ok(d) => d,
        Err(e) => {
            w.errors += 1;
            w.first_error.get_or_insert_with(|| format!("frame: {e:?}"));
            return;
        }
    };
    for d in &dgrams {
        match socket.send(d) {
            Ok(_) => {
                w.dgrams += 1;
                w.bytes += d.len() as u64;
            }
            Err(e) => {
                w.errors += 1;
                w.first_error.get_or_insert_with(|| e.to_string());
            }
        }
    }
    w.frames += 1;
    if job.flags & FLAG_KEY != 0 {
        w.keys += 1;
    }
    w.send.push_duration(begin.elapsed());
}

fn log_window(conn_id: i32, lane_id: u64, shared: &Shared, w: &mut Window, span: Duration) {
    let take = |c: &AtomicU64| c.swap(0, Ordering::Relaxed);
    log::info!(
        "UDPVIDEO host conn={} lane={:016x} win_s={:.1} frames={} keys={} skipped={} qdrop={} dgrams={} kB={} send_ms={} lag_ms={} err={} pings={} refused={}{}",
        conn_id,
        lane_id,
        span.as_secs_f64(),
        w.frames,
        w.keys,
        take(&shared.skipped),
        take(&shared.qdrop),
        w.dgrams,
        w.bytes / 1024,
        w.send.ms3(),
        w.lag.ms3(),
        w.errors,
        take(&shared.pings),
        take(&shared.refused),
        w.first_error
            .as_ref()
            .map(|e| format!(" first_err=\"{e}\""))
            .unwrap_or_default()
    );
    *w = Window::default();
}

#[cfg(test)]
pub(crate) mod latency_tests {
    use super::*;
    use base::message_proto::{EncodedVideoFrame, EncodedVideoFrames, Misc, SwitchDisplay};

    pub(crate) fn video(display: i32, pts: i64, key: bool) -> Arc<Message> {
        let mut frames = EncodedVideoFrames::new();
        frames.frames.push(EncodedVideoFrame {
            data: vec![0x42; 3000 + pts as usize].into(),
            key,
            pts,
            ..Default::default()
        });
        let mut vf = VideoFrame::new();
        vf.set_h265s(frames);
        vf.display = display;
        let mut msg = Message::new();
        msg.set_video_frame(vf);
        Arc::new(msg)
    }

    fn gate(local: &str, peer: &str) -> Gate {
        Gate {
            direct_tcp: true,
            secured: true,
            local: local.parse().unwrap(),
            peer: peer.parse().unwrap(),
        }
    }

    #[test]
    fn offer_gate_rules() {
        let ok = gate("192.168.1.5:21118", "192.168.1.9:50000");
        assert_eq!(
            offer_gate(&ok),
            Ok(("192.168.1.5".parse().unwrap(), "192.168.1.9".parse().unwrap()))
        );
        let mapped = gate("[::ffff:10.0.0.2]:21118", "10.0.0.3:1");
        assert_eq!(
            offer_gate(&mapped).map(|v| v.0),
            Ok("10.0.0.2".parse().unwrap())
        );
        assert!(offer_gate(&Gate {
            direct_tcp: false,
            ..ok
        })
        .is_err());
        assert!(offer_gate(&Gate {
            secured: false,
            ..ok
        })
        .is_err());
        assert!(offer_gate(&gate("0.0.0.0:1", "192.168.1.9:1")).is_err());
        assert!(offer_gate(&gate("192.168.1.5:1", "8.8.8.8:1")).is_err());
        assert!(offer_gate(&gate("[fe80::1]:1", "[fe80::2]:1")).is_err());
        assert!(offer_gate(&gate("192.168.1.5:1", "[fd00::2]:1")).is_err());
    }

    #[test]
    fn tap_every_nth_per_display_and_epoch() {
        let tap = ShadowTap::default();
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Arc::new(Shared::default());
        // Inactive: nothing is taken.
        tap.offer(&video(0, 1, true));
        tap.activate(tx, shared.clone(), 2);
        for pts in 0..6 {
            tap.offer(&video(0, pts, pts == 0));
            tap.offer(&video(1, pts, false));
        }
        let mut sd = Misc::new();
        sd.set_switch_display(SwitchDisplay::new());
        let mut m = Message::new();
        m.set_misc(sd);
        tap.offer(&Arc::new(m));
        tap.offer(&video(0, 6, false));
        let jobs: Vec<Job> = rx.try_iter().collect();
        let got: Vec<(u8, i64, u8, u8)> = jobs
            .iter()
            .map(|j| {
                let Some(message::Union::VideoFrame(vf)) = &j.msg.union else {
                    panic!("not a video frame")
                };
                let pts = crate::client::latency::frame_pts(vf).unwrap();
                (j.display, pts, j.flags, j.epoch)
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0, FLAG_SHADOW | FLAG_KEY, 0),
                (1, 0, FLAG_SHADOW, 0),
                (0, 2, FLAG_SHADOW, 0),
                (1, 2, FLAG_SHADOW, 0),
                (0, 4, FLAG_SHADOW, 0),
                (1, 4, FLAG_SHADOW, 0),
                (0, 6, FLAG_SHADOW, 1),
            ]
        );
        assert_eq!(shared.skipped.load(Ordering::Relaxed), 6);
        tap.deactivate();
        tap.offer(&video(0, 8, false));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tap_full_queue_drops() {
        let tap = ShadowTap::default();
        let (tx, _rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared::default());
        tap.activate(tx, shared.clone(), 1);
        for pts in 0..3 {
            tap.offer(&video(0, pts, false));
        }
        assert_eq!(shared.qdrop.load(Ordering::Relaxed), 2);
    }
}
