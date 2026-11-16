//! Host end of the UDP video lane: shadow probe (M19) and live video (M20).
//!
//! The connection offers a lane after a remote-control login when every gate passes
//! ([`offer_gate`]). [`UdpVideoHost`] owns the socket and two threads:
//!
//! - receive thread: answers the viewer's HELLO (only from the session's peer IP, only if it
//!   opens with the lane keys) with HELLO_ACK and then `connect()`s the socket to the viewer's
//!   address; answers PING with PONG; live mode: takes the viewer's ACKs (send window, round
//!   trip for the QoS signal) and notes when the viewer was last heard. Blocking `recv` with a
//!   100 ms timeout.
//! - sender thread: takes jobs from a bounded queue, serializes the frame once, seals and sends
//!   its datagrams (shadow: back to back; live: in bursts paced by a token bucket and a
//!   high-resolution timer), and logs a `UDPVIDEO host` line every 5 s.
//!
//! Both threads seal with one `LaneSender` and send each datagram while still holding its lock
//! ([`seal_send`]), one datagram at a time: the wire order is then the seq order, so a PONG sent
//! in the middle of a frame does not overtake the rest of the frame (which the viewer would count
//! as reorder, and, past the 1024-seq replay window, as replayed and lost fragments).
//!
//! The connection's video path asks [`VideoTap::route`] about every video item before its TCP
//! send. While no lane is active and no marker is pending that is one relaxed atomic load and
//! the item goes to TCP. Shadow: the item goes to TCP and every Nth frame is also copied to
//! UDP. Live: video frames go to UDP only, everything else (SwitchDisplay) to TCP; the ACTIVE,
//! EPOCH and STOP markers are written to TCP in the order of the video stream, so the viewer
//! knows exactly which frames came which way and which SwitchDisplay starts a new epoch. The tap never blocks: a full queue drops the frame
//! (`qdrop`; in live mode the viewer sees the gap as a loss and asks for a key frame).
//!
//! Live mode keeps the capture loop's "newest frame wins" back-pressure: the connection no
//! longer reports a UDP frame as fetched when it takes it off the video channel; the sender
//! does, once the frame is out and at most [`WINDOW_FRAMES`] frames of that display are not yet
//! settled by the viewer's ACKs (or after [`WINDOW_WAIT_MAX`]).
//!
//! Dropping `UdpVideoHost` stops both threads (they exit within 100 ms; nothing joins them on
//! the async runtime) and closes the socket when the last thread lets go of it.

use super::{
    crypto::{Role, SECRET_LEN},
    fec::{self, Shard},
    fragment::{fragment, FrameMeta},
    lane::{LaneReceiver, LaneSender, Received},
    net::{self, LaneClock},
    pacing::{self, Pacer, Waiter},
    packet::{Ack, Inner, PathClass, ACK_NO_FRAME, FLAG_KEY, FLAG_RECOVERY_POINT, FLAG_SHADOW},
    rfi,
    shadow::Samples,
};
use base::message_proto::{
    message, misc, udp_video_control, udp_video_offer, video_frame, Message, Misc, UdpVideoControl,
    UdpVideoFeedback, UdpVideoLastFrame, UdpVideoOffer, VideoFrame,
};
use hbb_common::{log, protobuf::Message as _};
use std::{
    collections::VecDeque,
    io,
    net::{IpAddr, SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

/// Lane (wire and offer) version of a shadow offer.
pub const LANE_VERSION: u32 = 1;
/// Offer version of a live lane: v2 has the EPOCH marker (M20 review). The first M20 build
/// offered live lanes as v1 without it; this viewer refuses those.
pub const LIVE_LANE_VERSION: u32 = 2;
/// `LoginRequest.udp_video_version` of a viewer that takes live offers of [`LIVE_LANE_VERSION`]
/// (1: shadow only; 2: the first M20 build, which bumps its epoch on every SwitchDisplay and is
/// offered no live lane).
pub const VIEWER_LIVE_VERSION: u32 = 3;
/// `LoginRequest.udp_video_version` of a viewer that takes FEC parity shards (M21): it is
/// offered `fec_percent` (`udp-video-fec-percent`); older viewers get lanes without parity.
pub const VIEWER_FEC_VERSION: u32 = 4;
/// `LoginRequest.udp_video_version` of a viewer that resumes a broken display at a
/// `RECOVERY_POINT` frame too (M22): only its lanes get reference frame invalidation.
pub const VIEWER_RFI_VERSION: u32 = 5;
/// From the offer to the viewer's READY.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Default of `udp-video-shadow-every`: half the frames, so the extra Wi-Fi load stays at half
/// the video bitrate.
pub const DEFAULT_SHADOW_EVERY: u32 = 2;
/// Default of `udp-video-timeout-ms`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Range of `udp-video-timeout-ms`.
pub const TIMEOUT_RANGE_MS: (u64, u64) = (200, 10_000);
/// Default of `udp-video-burst`.
pub const DEFAULT_BURST: usize = pacing::DEFAULT_BURST_PACKETS;
/// Frames of a display sent but not settled by the viewer, at most, before the sender stops
/// reporting new ones as fetched (roadmap: 2-3 unacknowledged frames).
pub const WINDOW_FRAMES: u32 = 3;
/// A frame held back by the window counts as fetched after this anyway (lost ACKs must not
/// stall the capture loop; silence ends the lane after `timeout`).
pub const WINDOW_WAIT_MAX: Duration = Duration::from_millis(100);
/// This many key frames for losses (the viewer's requests and tail losses) within `KF_WINDOW`
/// end a live lane: sustained loss.
pub const KF_MAX: usize = 4;
pub const KF_WINDOW: Duration = Duration::from_secs(5);
/// Added to the QoS delay sample when the viewer lost frames since the last sample.
pub const LOSS_PENALTY_MS: u32 = 150;
/// After a live lane fell back to TCP, the next offers come this long after each fallback;
/// then the session stays on TCP.
pub const RETRY_BACKOFF: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(120)];
/// Datagram send errors in a row that end a live lane.
const SEND_ERRORS_MAX: u32 = 5;
const QUEUE: usize = 4;
/// Only reached when the capture loop's fetched wait timed out; a drop is a loss.
const LIVE_QUEUE: usize = 8;
/// Lanes are offered on LAN paths only.
const PATH: PathClass = PathClass::Lan;
/// Live: a display's newest frame not settled by the viewer's ACKs this long (plus the ACK round
/// trip baseline) after it went out, with nothing newer sent, is a tail loss: no later frame
/// shows the viewer the gap, so the host asks the capture loop for a key frame itself.
pub const TAIL_LOSS_AFTER: Duration = Duration::from_millis(200);
const LOG_EVERY: Duration = Duration::from_secs(5);
/// The QoS baseline (minimum ACK round trip) is the lower of this window's and the last one's.
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// RFI: lane frame ids remembered per lane with their pts (all displays), to name the first lost
/// frame to the encoder. 2 s at 60 fps; a loss reported later is repaired by a key frame.
const RFI_IDS: usize = 128;

static LIVE_LANES: AtomicUsize = AtomicUsize::new(0);

/// Live lanes that are active now, all connections: the `LATENCY server` line's `qos_sig`.
pub fn live_lanes() -> usize {
    LIVE_LANES.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Shadow,
    Live,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Shadow => "shadow",
            Mode::Live => "live",
        }
    }
}

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

/// The lane mode the host option asks for: "live" live; "shadow" or "Y" shadow (M19 used `Y`
/// for the probe, so a host set up for it does not go live with a newer build); anything else
/// none.
pub fn mode_from_option(v: &str) -> Option<Mode> {
    match v {
        "live" => Some(Mode::Live),
        "shadow" | "Y" => Some(Mode::Shadow),
        _ => None,
    }
}

/// Settings read from the host options at each offer.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub mode: Mode,
    pub every: u32,
    /// Datagrams per burst, 0 = unpaced.
    pub burst: usize,
    pub timeout: Duration,
    /// FEC parity in percent of each block's data shards (M21); 0 = no FEC. The caller sets it
    /// to 0 for a viewer older than [`VIEWER_FEC_VERSION`].
    pub fec_percent: u32,
    /// Live: losses are repaired by reference frame invalidation where the encoder can (M22).
    /// The caller sets it for a viewer of [`VIEWER_RFI_VERSION`] with `enable-nvenc-rfi=Y`.
    pub rfi: bool,
}

impl Params {
    /// From the option strings, each falling back to its default when empty or invalid.
    pub fn from_options(
        mode: Mode,
        every: &str,
        burst: &str,
        timeout_ms: &str,
        fec_percent: &str,
    ) -> Self {
        Self {
            mode,
            every: every
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=60).contains(n))
                .unwrap_or(DEFAULT_SHADOW_EVERY),
            burst: burst
                .parse::<usize>()
                .ok()
                .filter(|n| *n <= 64)
                .unwrap_or(DEFAULT_BURST),
            timeout: timeout_ms
                .parse::<u64>()
                .ok()
                .filter(|n| (TIMEOUT_RANGE_MS.0..=TIMEOUT_RANGE_MS.1).contains(n))
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_TIMEOUT),
            fec_percent: fec_percent
                .parse::<u32>()
                .ok()
                .filter(|n| *n <= fec::MAX_PERCENT)
                .unwrap_or(fec::DEFAULT_PERCENT),
            rfi: false,
        }
    }
}

/// Calls back into the server for frames that go over UDP in live mode.
pub struct LiveHooks {
    /// `notify_video_frame_fetched(display, conn, queued)`: the capture loop may go on.
    pub fetched: Box<dyn Fn(usize, Instant) + Send + Sync>,
    /// `latency_stats::on_video_send`: the `send` field of the `LATENCY server` line.
    pub sent: Box<dyn Fn(&Message, Duration) + Send + Sync>,
    /// A key frame of `display`, as for the viewer's request (`refresh_video_display`): called
    /// by the sender thread on a tail loss.
    pub keyframe: Box<dyn Fn(usize) + Send + Sync>,
}

/// Reports a live frame as fetched when dropped, whichever way: sent and inside the window,
/// held past `WINDOW_WAIT_MAX`, dropped from a full queue, or left behind by a lane that ended.
/// So the capture loop never waits for a frame the lane lost track of.
struct Fetched {
    hooks: Arc<LiveHooks>,
    display: usize,
    queued: Instant,
}

impl Drop for Fetched {
    fn drop(&mut self) {
        (self.hooks.fetched)(self.display, self.queued);
    }
}

/// How a lost frame is repaired: what the connection asks the display's capture loop for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// A key frame (the refresh path: M13 forced key frame or a capture restart).
    KeyFrame(usize),
    /// Reference frame invalidation from the frame with this pts on (M22); the capture loop
    /// falls back to a key frame when its encoder cannot.
    Rfi { display: usize, from_pts: i64 },
}

/// Where the connection writes a video channel item, and the markers to write before it.
#[derive(Debug, Default)]
pub struct Route {
    /// Lane control markers (ACTIVE, STOP) to write to TCP first, in this order.
    pub before: Vec<Message>,
    /// Write the item to TCP. False: it went to the UDP lane.
    pub tcp: bool,
}

impl Route {
    pub const TCP: Route = Route {
        before: Vec::new(),
        tcp: true,
    };
}

#[derive(Default)]
struct DispWin {
    display: u8,
    last_sent: Option<u32>,
    settled: Option<u32>,
    lost_total: Option<u32>,
    pending: Option<(Fetched, Instant)>,
    /// When the newest id went out (or was dropped by a full queue): the tail loss clock.
    newest_at: Option<Instant>,
    /// The newest id a tail loss key frame was already asked for.
    tail_fixed: Option<u32>,
}

impl DispWin {
    /// Frames sent and not settled. Frame ids start at 0 on a live lane.
    fn inflight(&self) -> u32 {
        match (self.last_sent, self.settled) {
            (None, _) => 0,
            (Some(s), None) => s.wrapping_add(1),
            (Some(s), Some(t)) => (s.wrapping_sub(t) as i32).max(0) as u32,
        }
    }

    /// `id` is the newest frame of the display (sent, or dropped by a full queue). Never moves
    /// back: the sender may still finish an older id after the tap dropped a newer one.
    fn on_newest(&mut self, id: u32, now: Instant) {
        if self.last_sent.map_or(true, |s| id_after(id, s)) {
            self.last_sent = Some(id);
        }
        self.newest_at = Some(now);
    }
}

/// Live mode: the send window and the round trip of the ACKs.
#[derive(Default)]
struct Live {
    displays: Vec<DispWin>,
    /// Frames that waited for the window, and how long.
    win_wait: Samples,
    win_timeouts: u64,
    inflight_max: u32,
    /// ACK round trip minus the viewer's hold time, microseconds, this log window.
    rtt: Samples,
    base_cur: Option<u32>,
    base_prev: Option<u32>,
    base_since: Option<Instant>,
    /// Largest excess over the baseline and frames the viewer lost since the last QoS sample.
    excess_max: u32,
    lost: u64,
    /// Frames the viewer reported lost, this log window.
    lost_win: u64,
}

impl Live {
    fn disp(&mut self, display: u8) -> &mut DispWin {
        let i = match self.displays.iter().position(|d| d.display == display) {
            Some(i) => i,
            None => {
                self.displays.push(DispWin {
                    display,
                    ..Default::default()
                });
                self.displays.len() - 1
            }
        };
        &mut self.displays[i]
    }

    fn baseline(&self) -> Option<u32> {
        match (self.base_cur, self.base_prev) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn on_rtt(&mut self, rtt_us: u32, now: Instant) {
        self.rtt.push(rtt_us as i64);
        let since = *self.base_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= BASE_WINDOW {
            self.base_prev = self.base_cur.take();
            self.base_since = Some(now);
        }
        self.base_cur = Some(self.base_cur.map_or(rtt_us, |b| b.min(rtt_us)));
        if let Some(base) = self.baseline() {
            self.excess_max = self.excess_max.max(rtt_us.saturating_sub(base));
        }
    }
}

/// `a` comes after `b` in wrapping frame id order.
fn id_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Counters the tap and both threads share; the sender thread logs and resets the windowed
/// ones.
struct Shared {
    stop: AtomicBool,
    clock: LaneClock,
    peer: Mutex<Option<SocketAddr>>,
    hellos: AtomicU64,
    pings: AtomicU64,
    acks: AtomicU64,
    /// Datagrams refused before or by the crypto (foreign source, bad tag, replay).
    refused: AtomicU64,
    skipped: AtomicU64,
    qdrop: AtomicU64,
    /// Live: frames the tap dropped since the lane started (never reset; the per-frame
    /// feedback's fallback rule takes them off the viewer's lost frames).
    qdrop_live: AtomicU64,
    kf_req: AtomicU64,
    /// Live, M22: loss reports turned into an RFI request (a subset of `kf_req`), and frames
    /// flagged RECOVERY_POINT for this viewer.
    rfi_req: AtomicU64,
    rp: AtomicU64,
    /// Live: key frames asked for on a tail loss; `tail_new` is drained by `check` into the
    /// sustained-loss count, `tail_kf` by the log line.
    tail_kf: AtomicU64,
    tail_new: AtomicU64,
    /// Lane clock, ms + 1, of the last datagram from the viewer that opened (0: none yet).
    last_rx_ms: AtomicU64,
    /// Set by the sender when the socket keeps failing: the lane must end.
    fatal: Mutex<Option<String>>,
    live: Mutex<Live>,
    /// FEC percentage set by the adaptive FEC of the per-frame feedback (M23); `FEC_UNSET`:
    /// the lane's `Params::fec_percent`.
    fec_now: AtomicU32,
}

/// `Shared::fec_now` before any adaptive change.
const FEC_UNSET: u32 = u32::MAX;

impl Shared {
    fn new(clock: LaneClock) -> Self {
        Self {
            stop: AtomicBool::new(false),
            clock,
            peer: Mutex::new(None),
            hellos: AtomicU64::new(0),
            pings: AtomicU64::new(0),
            acks: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            qdrop: AtomicU64::new(0),
            qdrop_live: AtomicU64::new(0),
            kf_req: AtomicU64::new(0),
            rfi_req: AtomicU64::new(0),
            rp: AtomicU64::new(0),
            tail_kf: AtomicU64::new(0),
            tail_new: AtomicU64::new(0),
            last_rx_ms: AtomicU64::new(0),
            fatal: Mutex::new(None),
            live: Mutex::new(Live::default()),
            fec_now: AtomicU32::new(FEC_UNSET),
        }
    }

    /// The FEC percentage for the next frame.
    fn fec_percent(&self, configured: u32) -> u32 {
        match self.fec_now.load(Ordering::Relaxed) {
            FEC_UNSET => configured,
            p => p,
        }
    }

    fn heard(&self) {
        let ms = self.clock.elapsed().as_millis() as u64 + 1;
        self.last_rx_ms.store(ms, Ordering::Relaxed);
    }

    /// How long since the viewer was last heard, if ever.
    fn silent_for(&self) -> Option<Duration> {
        match self.last_rx_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(
                self.clock
                    .elapsed()
                    .saturating_sub(Duration::from_millis(ms - 1)),
            ),
        }
    }

    /// The viewer's ACK: settle frames, release a frame held by the window, take a round trip.
    fn on_ack(&self, a: &Ack, now_us: u32, now: Instant) {
        self.acks.fetch_add(1, Ordering::Relaxed);
        let Ok(mut live) = self.live.lock() else {
            return;
        };
        let d = live.disp(a.display);
        if a.has_settled && d.settled.map_or(true, |s| id_after(a.settled, s)) {
            // Never past what was sent: a forged-free but confused ACK cannot open the window.
            let settled = match d.last_sent {
                Some(s) if id_after(a.settled, s) => s,
                _ => a.settled,
            };
            d.settled = Some(settled);
        }
        let lost = match d.lost_total {
            Some(prev) => a.lost_total.wrapping_sub(prev) as u64,
            None => a.lost_total as u64,
        };
        d.lost_total = Some(a.lost_total);
        let released = if d.inflight() <= WINDOW_FRAMES {
            d.pending.take()
        } else {
            None
        };
        live.lost += lost;
        live.lost_win += lost;
        if let Some((fetched, since)) = released {
            live.win_wait
                .push_duration(now.saturating_duration_since(since));
            drop(fetched);
        }
        if a.hold_us != ACK_NO_FRAME {
            let rtt = net::us_diff(now_us, a.echo_send_us) - a.hold_us as i64;
            if (0..=u32::MAX as i64).contains(&rtt) {
                live.on_rtt(rtt as u32, now);
            }
        }
    }
}

struct Job {
    msg: Arc<Message>,
    display: u8,
    epoch: u8,
    flags: u8,
    queued: Instant,
    /// Live: the lane frame id the tap gave it. Shadow: None, the sender numbers what it sends.
    frame_id: Option<u32>,
    /// Live: reported as fetched when the window allows (or when dropped).
    fetched: Option<Fetched>,
}

enum TapMode {
    Off,
    Shadow {
        tx: SyncSender<Job>,
        shared: Arc<Shared>,
        every: u32,
        /// Video items seen per display while active, for the every-Nth choice.
        seen: Vec<(i32, u32)>,
    },
    Live {
        tx: SyncSender<Job>,
        shared: Arc<Shared>,
        hooks: Arc<LiveHooks>,
        lane_id: u64,
        /// Next lane frame id per display, from 0.
        ids: Vec<(u8, u32)>,
        /// The ACTIVE marker went out (with the first UDP frame).
        announced: bool,
        /// M22: the lane's reference frame invalidation state (None: losses get key frames).
        rfi: Option<RfiTap>,
    },
}

/// M22, per live lane: the pts of the recent lane frames, and per display the pts of the first
/// frame its viewer lost while it still waits for a key frame or a recovery point.
#[derive(Default)]
struct RfiTap {
    /// (display, lane frame id, pts), oldest first, at most `RFI_IDS`.
    sent: VecDeque<(u8, u32, i64)>,
    /// (display, pts of the first lost frame).
    pending: Vec<(u8, i64)>,
}

impl RfiTap {
    /// A frame given to the lane. A key frame repairs every earlier loss of its display, so the
    /// frames before it are forgotten (also the pts of an earlier capture loop, which restarts
    /// with a key frame and pts from 0).
    fn on_frame(&mut self, display: u8, frame_id: u32, pts: i64, key: bool) {
        if key {
            self.sent.retain(|(d, _, _)| *d != display);
        }
        self.sent.push_back((display, frame_id, pts));
        while self.sent.len() > RFI_IDS {
            self.sent.pop_front();
        }
    }

    /// The viewer lost frames of `display` from `frame_id` on: that frame's pts, now pending.
    fn on_loss(&mut self, display: u8, frame_id: u32) -> Option<i64> {
        let pts = self
            .sent
            .iter()
            .rev()
            .find(|(d, id, _)| *d == display && *id == frame_id)
            .map(|(_, _, pts)| *pts)?;
        match self.pending.iter_mut().find(|(d, _)| *d == display) {
            Some((_, p)) => *p = (*p).min(pts),
            None => self.pending.push((display, pts)),
        }
        Some(pts)
    }

    fn take_pending(&mut self, display: u8) -> Option<i64> {
        let i = self.pending.iter().position(|(d, _)| *d == display)?;
        Some(self.pending.swap_remove(i).1)
    }

    /// The flags a frame of `display` with `pts` gets: a key frame ends the wait; a recovery
    /// frame ends it when it references nothing from the first lost frame on (Some: the pending
    /// pts it ended, to restore if the frame is not sent).
    fn route(&mut self, display: u8, pts: i64, key: bool) -> (u8, Option<i64>) {
        if key {
            self.take_pending(display);
            return (FLAG_KEY, None);
        }
        let Some(first_lost) = self
            .pending
            .iter()
            .find(|(d, _)| *d == display)
            .map(|(_, p)| *p)
        else {
            return (0, None);
        };
        match rfi::recovery_bound(display as usize, pts) {
            Some(bound) if bound <= first_lost => {
                self.take_pending(display);
                (FLAG_RECOVERY_POINT, Some(first_lost))
            }
            _ => (0, None),
        }
    }

    fn restore(&mut self, display: u8, pts: i64) {
        if !self.pending.iter().any(|(d, _)| *d == display) {
            self.pending.push((display, pts));
        }
    }
}

/// The pts of a video frame (its first encoded frame's).
fn frame_pts(vf: &VideoFrame) -> Option<i64> {
    use video_frame::Union::*;
    match vf.union.as_ref() {
        Some(Vp8s(f)) | Some(Vp9s(f)) | Some(Av1s(f)) | Some(H264s(f)) | Some(H265s(f)) => {
            f.frames.first().map(|e| e.pts)
        }
        _ => None,
    }
}

struct TapInner {
    mode: TapMode,
    epoch: u8,
    /// Markers to write before the next video item.
    pending: Vec<Message>,
}

impl Default for TapInner {
    fn default() -> Self {
        Self {
            mode: TapMode::Off,
            epoch: 0,
            pending: Vec::new(),
        }
    }
}

/// What ending a lane left to do: displays that had UDP frames (their TCP stream restarts with
/// a key frame), and whether the STOP marker waits in the tap for the next video item.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StopInfo {
    pub displays: Vec<i32>,
    pub in_band: bool,
    /// The lane was live and active (worth a retry later).
    pub was_live: bool,
}

/// The connection's hook in front of every TCP video send (whole or split session).
#[derive(Default)]
pub struct VideoTap {
    /// A mode is on or a marker is pending: `route` takes the lock.
    busy: AtomicBool,
    inner: Mutex<TapInner>,
}

/// Any key frame in it (the lane's KEY flag; the viewer's TCP restart after a live lane).
pub(crate) fn has_key_frame(vf: &VideoFrame) -> bool {
    use video_frame::Union::*;
    match vf.union.as_ref() {
        Some(Vp8s(f)) | Some(Vp9s(f)) | Some(Av1s(f)) | Some(H264s(f)) | Some(H265s(f)) => {
            f.frames.iter().any(|e| e.key)
        }
        _ => false,
    }
}

fn control_msg(c: UdpVideoControl) -> Message {
    let mut misc = Misc::new();
    misc.set_udp_video_control(c);
    let mut msg = Message::new();
    msg.set_misc(misc);
    msg
}

impl VideoTap {
    /// A video channel item (VideoFrame or SwitchDisplay), before its TCP send.
    #[inline]
    pub fn route(&self, msg: &Arc<Message>, queued: Instant) -> Route {
        if self.busy.load(Ordering::Relaxed) {
            self.route_busy(msg, queued)
        } else {
            Route::TCP
        }
    }

    fn route_busy(&self, msg: &Arc<Message>, queued: Instant) -> Route {
        let Ok(mut guard) = self.inner.lock() else {
            return Route::TCP;
        };
        let t = &mut *guard;
        let mut route = Route {
            before: std::mem::take(&mut t.pending),
            tcp: true,
        };
        match &msg.union {
            Some(message::Union::VideoFrame(vf)) => match &mut t.mode {
                TapMode::Off => {}
                TapMode::Shadow {
                    tx,
                    shared,
                    every,
                    seen,
                } => {
                    let every = (*every).max(1);
                    let i = match seen.iter().position(|(d, _)| *d == vf.display) {
                        Some(i) => i,
                        None => {
                            seen.push((vf.display, 0));
                            seen.len() - 1
                        }
                    };
                    let n = seen[i].1;
                    seen[i].1 = n.wrapping_add(1);
                    if n % every != 0 {
                        shared.skipped.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let job = Job {
                            msg: msg.clone(),
                            display: vf.display.clamp(0, u8::MAX as i32) as u8,
                            epoch: t.epoch,
                            flags: FLAG_SHADOW | if has_key_frame(vf) { FLAG_KEY } else { 0 },
                            queued,
                            frame_id: None,
                            fetched: None,
                        };
                        if let Err(TrySendError::Full(_)) = tx.try_send(job) {
                            shared.qdrop.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                TapMode::Live {
                    tx,
                    shared,
                    hooks,
                    lane_id,
                    ids,
                    announced,
                    rfi,
                } => {
                    if !*announced {
                        *announced = true;
                        route.before.push(control_msg(UdpVideoControl {
                            kind: udp_video_control::Kind::KindActive.into(),
                            lane_id: *lane_id,
                            epoch: t.epoch as u32,
                            ..Default::default()
                        }));
                    }
                    let display = vf.display.clamp(0, u8::MAX as i32) as u8;
                    let frame_id = match ids.iter_mut().find(|(d, _)| *d == display) {
                        Some((_, next)) => {
                            let id = *next;
                            *next = next.wrapping_add(1);
                            id
                        }
                        None => {
                            ids.push((display, 1));
                            0
                        }
                    };
                    let key = has_key_frame(vf);
                    let (flags, ended) = match (rfi.as_mut(), frame_pts(vf)) {
                        (Some(r), Some(pts)) => {
                            r.on_frame(display, frame_id, pts, key);
                            r.route(display, pts, key)
                        }
                        _ => (if key { FLAG_KEY } else { 0 }, None),
                    };
                    let job = Job {
                        msg: msg.clone(),
                        display,
                        epoch: t.epoch,
                        flags,
                        queued,
                        frame_id: Some(frame_id),
                        fetched: Some(Fetched {
                            hooks: hooks.clone(),
                            display: vf.display.max(0) as usize,
                            queued,
                        }),
                    };
                    // A refused job drops its `Fetched`, which reports the frame as fetched;
                    // its id is never sent, so the viewer sees a loss when a later frame comes,
                    // and the tail loss check repairs it when none does.
                    if tx.try_send(job).is_err() {
                        shared.qdrop.fetch_add(1, Ordering::Relaxed);
                        shared.qdrop_live.fetch_add(1, Ordering::Relaxed);
                        if let Ok(mut live) = shared.live.lock() {
                            live.disp(display).on_newest(frame_id, Instant::now());
                        }
                        // A recovery point that is not sent repairs nothing.
                        if let (Some(r), Some(first_lost)) = (rfi.as_mut(), ended) {
                            r.restore(display, first_lost);
                        }
                    } else if ended.is_some() {
                        shared.rp.fetch_add(1, Ordering::Relaxed);
                    }
                    route.tcp = false;
                }
            },
            Some(message::Union::Misc(m))
                if matches!(m.union, Some(misc::Union::SwitchDisplay(_))) =>
            {
                // Live: the viewer holds UDP frames of the new epoch until it read this
                // SwitchDisplay on TCP. Shadow: it only travels along. The connection also
                // sends SwitchDisplay outside the video channel (display switch, message query),
                // which never passes here, so an active live lane writes EPOCH right before
                // this one: the viewer bumps its epoch only for a SwitchDisplay after that
                // marker. Before ACTIVE no marker is needed: ACTIVE carries the bumped epoch.
                t.epoch = t.epoch.wrapping_add(1);
                if let TapMode::Live {
                    lane_id,
                    announced: true,
                    ..
                } = &t.mode
                {
                    route.before.push(control_msg(UdpVideoControl {
                        kind: udp_video_control::Kind::KindEpoch.into(),
                        lane_id: *lane_id,
                        epoch: t.epoch as u32,
                        ..Default::default()
                    }));
                }
            }
            _ => {}
        }
        self.update_busy(t);
        route
    }

    fn update_busy(&self, t: &TapInner) {
        let busy = !matches!(t.mode, TapMode::Off) || !t.pending.is_empty();
        self.busy.store(busy, Ordering::Relaxed);
    }

    fn activate_shadow(&self, tx: SyncSender<Job>, shared: Arc<Shared>, every: u32) {
        if let Ok(mut t) = self.inner.lock() {
            t.mode = TapMode::Shadow {
                tx,
                shared,
                every,
                seen: Vec::new(),
            };
            t.epoch = 0;
            self.update_busy(&t);
        }
    }

    fn activate_live(
        &self,
        tx: SyncSender<Job>,
        shared: Arc<Shared>,
        hooks: Arc<LiveHooks>,
        lane_id: u64,
        rfi: bool,
    ) {
        if let Ok(mut t) = self.inner.lock() {
            t.mode = TapMode::Live {
                tx,
                shared,
                hooks,
                lane_id,
                ids: Vec::new(),
                announced: false,
                rfi: rfi.then(RfiTap::default),
            };
            self.update_busy(&t);
        }
    }

    /// M22: the viewer lost frames of `display` from lane frame `frame_id` on. The frame's pts
    /// (now this display's first lost frame until a key frame or a recovery point for it), or
    /// None when the lane has no RFI or no longer knows the frame.
    fn rfi_loss(&self, display: u8, frame_id: u32) -> Option<i64> {
        let mut t = self.inner.lock().ok()?;
        match &mut t.mode {
            TapMode::Live { rfi: Some(r), .. } => r.on_loss(display, frame_id),
            _ => None,
        }
    }

    /// Ends the mode. A live lane that announced itself leaves a STOP marker for the next video
    /// item, with the id of the last frame given to UDP per display.
    fn stop(&self, reason: &str) -> StopInfo {
        let Ok(mut t) = self.inner.lock() else {
            return StopInfo::default();
        };
        let mut info = StopInfo::default();
        if let TapMode::Live {
            lane_id,
            ids,
            announced,
            ..
        } = &t.mode
        {
            info.was_live = *announced;
            if *announced {
                let last_frames = ids
                    .iter()
                    .map(|(d, next)| UdpVideoLastFrame {
                        display: *d as i32,
                        frame_id: next.wrapping_sub(1),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>();
                info.displays = ids.iter().map(|(d, _)| *d as i32).collect();
                info.in_band = true;
                let marker = control_msg(UdpVideoControl {
                    kind: udp_video_control::Kind::KindStop.into(),
                    lane_id: *lane_id,
                    reason: reason.to_owned(),
                    last_frames,
                    in_band: true,
                    ..Default::default()
                });
                t.pending.push(marker);
            }
        }
        t.mode = TapMode::Off;
        self.update_busy(&t);
        info
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Offered,
    Active,
    Stopped,
}

/// One connection's lane: socket, threads, and the tap it feeds while active.
pub struct UdpVideoHost {
    conn_id: i32,
    lane_id: u64,
    params: Params,
    state: State,
    offered_at: Instant,
    tap: Arc<VideoTap>,
    shared: Arc<Shared>,
    job_tx: Option<SyncSender<Job>>,
    hooks: Option<Arc<LiveHooks>>,
    kf_reqs: VecDeque<Instant>,
    live_counted: bool,
}

impl UdpVideoHost {
    /// Binds the lane socket on `local`, starts the threads, and returns the offer to send.
    /// Live mode needs `hooks`.
    pub fn start(
        conn_id: i32,
        tap: Arc<VideoTap>,
        local: IpAddr,
        viewer: IpAddr,
        params: Params,
        hooks: Option<Arc<LiveHooks>>,
    ) -> Result<(Self, UdpVideoOffer), String> {
        if params.mode == Mode::Live && hooks.is_none() {
            return Err("live mode without hooks".to_owned());
        }
        let (socket, notes) =
            net::bind(local, 0, net::HOST_SNDBUF).map_err(|e| format!("bind {local}: {e}"))?;
        for n in notes {
            log::warn!("UDPVIDEO host conn={conn_id} socket option failed: {n}");
        }
        let port = socket
            .local_addr()
            .map_err(|e| format!("local_addr: {e}"))?
            .port();
        let secret: [u8; SECRET_LEN] = hbb_common::sodiumoxide::crypto::secretbox::gen_key().0;
        let lane_id: u64 = hbb_common::rand::random();
        let socket = Arc::new(socket);
        let clock = LaneClock::new();
        let shared = Arc::new(Shared::new(clock));
        let sender = Arc::new(Mutex::new(LaneSender::new(
            &secret,
            lane_id,
            Role::Host,
            PATH,
        )));
        let queue = match params.mode {
            Mode::Shadow => QUEUE,
            Mode::Live => LIVE_QUEUE,
        };
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(queue);
        {
            let (socket, sender, shared) = (socket.clone(), sender.clone(), shared.clone());
            std::thread::Builder::new()
                .name("udp-video-host-rx".to_owned())
                .spawn(move || {
                    recv_loop(
                        &socket,
                        &sender,
                        &shared,
                        &secret,
                        lane_id,
                        viewer,
                        conn_id,
                        params.mode,
                    )
                })
                .map_err(|e| format!("spawn: {e}"))?;
        }
        let tx_shared = shared.clone();
        let tx_hooks = hooks.clone();
        let spawned = std::thread::Builder::new()
            .name("udp-video-host-tx".to_owned())
            .spawn(move || {
                send_loop(
                    &socket, &sender, &tx_shared, job_rx, conn_id, lane_id, params, tx_hooks,
                )
            });
        if let Err(e) = spawned {
            // The receive thread is already running: stop it.
            shared.stop.store(true, Ordering::Relaxed);
            return Err(format!("spawn: {e}"));
        }
        let mode = match params.mode {
            Mode::Shadow => udp_video_offer::Mode::ModeShadow,
            Mode::Live => udp_video_offer::Mode::ModeLive,
        };
        let offer = UdpVideoOffer {
            version: match params.mode {
                Mode::Shadow => LANE_VERSION,
                Mode::Live => LIVE_LANE_VERSION,
            },
            mode: mode.into(),
            lane_id,
            secret: secret.to_vec().into(),
            ip: net::ip_to_bytes(local).into(),
            port: port as u32,
            max_datagram: PATH.max_datagram() as u32,
            shadow_every: params.every,
            timeout_ms: params.timeout.as_millis() as u32,
            fec_percent: params.fec_percent,
            ..Default::default()
        };
        log::info!(
            "UDPVIDEO host conn={conn_id} offer lane={lane_id:016x} mode={} addr={} every={} burst={} timeout_ms={} fec={} rfi={} viewer={viewer} installed={}",
            params.mode.label(),
            SocketAddr::new(local, port),
            params.every,
            params.burst,
            params.timeout.as_millis(),
            params.fec_percent,
            params.rfi,
            installed()
        );
        Ok((
            Self {
                conn_id,
                lane_id,
                params,
                state: State::Offered,
                offered_at: Instant::now(),
                tap,
                shared,
                job_tx: Some(job_tx),
                hooks,
                kf_reqs: VecDeque::new(),
                live_counted: false,
            },
            offer,
        ))
    }

    pub fn lane_id(&self) -> u64 {
        self.lane_id
    }

    #[cfg(test)]
    pub(crate) fn acks_seen(&self) -> u64 {
        self.shared.acks.load(Ordering::Relaxed)
    }

    pub fn mode(&self) -> Mode {
        self.params.mode
    }

    /// Live and carrying video now.
    pub fn is_live_active(&self) -> bool {
        self.params.mode == Mode::Live && self.state == State::Active
    }

    /// The viewer's READY: start copying (shadow) or carrying (live) frames. Err when it does
    /// not fit this lane.
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
        match (self.params.mode, &self.hooks) {
            (Mode::Live, Some(hooks)) => {
                self.tap.activate_live(
                    tx,
                    self.shared.clone(),
                    hooks.clone(),
                    self.lane_id,
                    self.params.rfi,
                );
                LIVE_LANES.fetch_add(1, Ordering::Relaxed);
                self.live_counted = true;
                // The viewer is heard from now on (pings, ACKs); silence counts from here.
                self.shared.heard();
            }
            _ => self
                .tap
                .activate_shadow(tx, self.shared.clone(), self.params.every),
        }
        self.state = State::Active;
        log::info!(
            "UDPVIDEO host conn={} active lane={:016x} mode={} peer={} every={} after_ms={}",
            self.conn_id,
            self.lane_id,
            self.params.mode.label(),
            peer,
            self.params.every,
            self.offered_at.elapsed().as_millis()
        );
        Ok(())
    }

    /// Called every second: Err(reason) when the lane must end (handshake timeout; live: the
    /// viewer silent for `timeout`, the socket failing, sustained tail losses).
    pub fn check(&mut self) -> Result<(), String> {
        match self.state {
            State::Offered if self.offered_at.elapsed() >= HANDSHAKE_TIMEOUT => {}
            State::Active if self.params.mode == Mode::Live => {
                if let Some(e) = self.shared.fatal.lock().ok().and_then(|f| f.clone()) {
                    return Err(e);
                }
                let tails = self.shared.tail_new.swap(0, Ordering::Relaxed);
                if tails > 0 {
                    if let Some(n) = self.count_kf(tails as usize, Instant::now()) {
                        return Err(format!(
                            "sustained loss: {n} key frames in {} s (last: tail losses)",
                            KF_WINDOW.as_secs()
                        ));
                    }
                }
                if let Some(silent) = self.shared.silent_for() {
                    if silent >= self.params.timeout {
                        return Err(format!(
                            "no datagram from the viewer in {} ms",
                            silent.as_millis()
                        ));
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
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

    /// The viewer's loss report. Ok(Some(repair)): what to ask the capture loop of that display
    /// for. Err: sustained loss, the lane must end.
    pub fn on_feedback(&mut self, f: &UdpVideoFeedback) -> Result<Option<Repair>, String> {
        if f.lane_id != self.lane_id || !self.is_live_active() || !f.request_keyframe {
            return Ok(None);
        }
        self.shared.kf_req.fetch_add(1, Ordering::Relaxed);
        if let Some(n) = self.count_kf(1, Instant::now()) {
            return Err(format!(
                "sustained loss: {n} key frames in {} s (last: request for display {} frames {}..={})",
                KF_WINDOW.as_secs(),
                f.display,
                f.lost_from,
                f.lost_to
            ));
        }
        let display = f.display.max(0) as usize;
        if self.params.rfi {
            let lane_display = f.display.clamp(0, u8::MAX as i32) as u8;
            if let Some(from_pts) = self.tap.rfi_loss(lane_display, f.lost_from) {
                self.shared.rfi_req.fetch_add(1, Ordering::Relaxed);
                return Ok(Some(Repair::Rfi { display, from_pts }));
            }
        }
        Ok(Some(Repair::KeyFrame(display)))
    }

    /// Live with RFI (M22).
    pub fn rfi(&self) -> bool {
        self.params.rfi
    }

    /// Counts `n` key frames for losses; Some(count within `KF_WINDOW`) when that is sustained
    /// loss.
    fn count_kf(&mut self, n: usize, now: Instant) -> Option<usize> {
        for _ in 0..n.min(KF_MAX) {
            self.kf_reqs.push_back(now);
        }
        while let Some(t) = self.kf_reqs.front() {
            if now.saturating_duration_since(*t) > KF_WINDOW {
                self.kf_reqs.pop_front();
            } else {
                break;
            }
        }
        (self.kf_reqs.len() >= KF_MAX).then_some(self.kf_reqs.len())
    }

    /// The delay sample for `video_qos` while the lane is live: the TCP probe's own delay plus
    /// the largest queueing excess the ACK round trips showed since the last sample, plus
    /// `LOSS_PENALTY_MS` if the viewer lost frames since; the flag says whether it was added.
    /// None when not live (TCP decides).
    pub fn qos_delay(&self, tcp_ms: u32) -> Option<(u32, bool)> {
        if !self.is_live_active() {
            return None;
        }
        let mut live = self.shared.live.lock().ok()?;
        let excess_ms = std::mem::take(&mut live.excess_max) / 1000;
        let lost = std::mem::take(&mut live.lost) > 0;
        Some((
            tcp_ms
                .saturating_add(excess_ms)
                .saturating_add(if lost { LOSS_PENALTY_MS } else { 0 }),
            lost,
        ))
    }

    /// Live: frames the tap dropped on a full queue since the lane started.
    pub fn qdrop_total(&self) -> u64 {
        self.shared.qdrop_live.load(Ordering::Relaxed)
    }

    /// The FEC percentage the lane started with (0: no parity, e.g. a viewer below lane v4).
    pub fn fec_configured(&self) -> u32 {
        self.params.fec_percent
    }

    /// Adaptive FEC (M23): the parity percentage from the next frame on. Refused (false) for a
    /// lane without parity: its viewer may not take parity datagrams at all.
    pub fn set_fec_percent(&self, percent: u32) -> bool {
        if self.params.fec_percent == 0 {
            return false;
        }
        self.shared
            .fec_now
            .store(percent.min(fec::MAX_PERCENT), Ordering::Relaxed);
        true
    }

    /// Ends the lane (the threads stop when it is dropped). Idempotent.
    pub fn shutdown(&mut self, why: &str) -> StopInfo {
        if self.state == State::Stopped {
            return StopInfo::default();
        }
        let active = self.state == State::Active;
        self.state = State::Stopped;
        let info = if active {
            self.tap.stop(why)
        } else {
            StopInfo::default()
        };
        if self.live_counted {
            self.live_counted = false;
            LIVE_LANES.fetch_sub(1, Ordering::Relaxed);
        }
        self.shared.stop.store(true, Ordering::Relaxed);
        self.job_tx = None;
        info
    }
}

impl Drop for UdpVideoHost {
    fn drop(&mut self) {
        let state = self.state;
        self.shutdown("lane dropped");
        log::info!(
            "UDPVIDEO host conn={} closed lane={:016x} mode={} state={:?}",
            self.conn_id,
            self.lane_id,
            self.params.mode.label(),
            state
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
    mode: Mode,
) {
    // Live: qWAVE AudioVideo marking (DSCP 40, WMM video) once the socket is connected; dropped
    // at the end of this function, while the socket is still open.
    #[cfg(windows)]
    let mut _qos: Option<crate::platform::windows_qos::UdpQos> = None;
    #[cfg(not(windows))]
    let _ = mode;
    let clock = shared.clock;
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
        let now = Instant::now();
        let received = rx.on_datagram(&mut buf[..n], now);
        if received.is_ok() && connected {
            shared.heard();
        }
        let reply = match received {
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
                    #[cfg(windows)]
                    if mode == Mode::Live {
                        match crate::platform::windows_qos::mark_udp(socket) {
                            Some(Ok(q)) => {
                                _qos = Some(q);
                                log::info!(
                                    "UDPVIDEO host conn={conn_id} qWAVE AudioVideo flow added"
                                );
                            }
                            Some(Err(e)) => {
                                log::warn!("UDPVIDEO host conn={conn_id} qWAVE marking failed: {e}")
                            }
                            None => {}
                        }
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
            Ok(Received::Ack(a)) => {
                shared.on_ack(&a, clock.us(now), now);
                continue;
            }
            Ok(_) => continue,
            Err(_) => {
                shared.refused.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if let Err(e) = seal_send(sender, &reply, |d| socket.send(d)) {
            log::debug!("UDPVIDEO host conn={conn_id} reply send: {e}");
        }
    }
}

/// Seals one datagram and sends it before letting go of the sender, so that the seq order of
/// the two threads' datagrams is their wire order. Returns the bytes sent.
fn seal_send(
    sender: &Mutex<LaneSender>,
    inner: &Inner,
    send: impl FnOnce(&[u8]) -> io::Result<usize>,
) -> Result<usize, String> {
    let mut s = sender
        .lock()
        .map_err(|_| "lane sender lock poisoned".to_owned())?;
    let d = s.seal(inner).map_err(|e| format!("seal: {e:?}"))?;
    send(&d).map_err(|e| e.to_string())
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
    /// Live: time spent waiting for the pacer per frame.
    paced: Samples,
    /// FEC (M21): frames sent with parity, parity datagrams among `dgrams`, frames too large
    /// for 4 blocks sent without, and the encode time per protected frame.
    fec_frames: u64,
    fec_dgrams: u64,
    fec_off: u64,
    fec_enc: Samples,
}

impl Window {
    /// The frame's shards with parity at `percent` (0: none), counted. Parity is encoded lazily
    /// as the shards are taken (each block's after its data shards), so the first datagram does
    /// not wait for it; pass the iterator to `protected` once it is drained.
    fn protect<'a>(
        &mut self,
        parts: Vec<(super::packet::VideoHeader, &'a [u8])>,
        percent: u32,
    ) -> fec::Protect<'a> {
        let shards = fec::Protect::new(parts, percent);
        if shards.blocks() > 0 {
            self.fec_frames += 1;
        } else if percent > 0 {
            self.fec_off += 1;
        }
        shards
    }

    /// Records the encode time of a drained `protect` iterator.
    fn protected(&mut self, shards: &fec::Protect<'_>) {
        if shards.blocks() > 0 {
            self.fec_enc.push_duration(shards.encode_time());
        }
    }
}

/// Live pacing state of the sender thread.
struct Pacing {
    burst: usize,
    pacer: Pacer,
    waiter: Waiter,
    /// Frame interval estimate from the jobs' queue times (EWMA).
    interval: Duration,
    last_queued: Option<Instant>,
    /// Bytes sent since `rate_since`, for the bitrate term.
    rate_bytes: u64,
    rate_since: Instant,
    rate_bps: u64,
    /// Datagram send errors in a row.
    errors_in_row: u32,
}

impl Pacing {
    fn new(burst: usize) -> Self {
        let now = Instant::now();
        Self {
            burst,
            pacer: Pacer::new(0.0, burst.max(1) * PATH.max_datagram(), now),
            waiter: Waiter::new(),
            interval: Duration::from_micros(16_667),
            last_queued: None,
            rate_bytes: 0,
            rate_since: now,
            rate_bps: 0,
            errors_in_row: 0,
        }
    }

    /// Updates the rate for a frame of `bytes` queued at `queued`.
    fn on_frame(&mut self, bytes: usize, queued: Instant, now: Instant) {
        if let Some(last) = self.last_queued {
            let dt = queued.saturating_duration_since(last);
            if dt > Duration::ZERO {
                let dt = dt.clamp(Duration::from_millis(4), Duration::from_millis(100));
                self.interval = (self.interval * 7 + dt) / 8;
            }
        }
        self.last_queued = Some(queued);
        let span = now.saturating_duration_since(self.rate_since);
        if span >= Duration::from_secs(1) {
            self.rate_bps = (self.rate_bytes as f64 * 8.0 / span.as_secs_f64()) as u64;
            self.rate_bytes = 0;
            self.rate_since = now;
        }
        self.rate_bytes += bytes as u64;
        if self.burst > 0 {
            let rate = pacing::target_rate(self.rate_bps, bytes, self.interval);
            self.pacer.set_rate(rate, now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    socket: &UdpSocket,
    sender: &Mutex<LaneSender>,
    shared: &Shared,
    jobs: Receiver<Job>,
    conn_id: i32,
    lane_id: u64,
    params: Params,
    hooks: Option<Arc<LiveHooks>>,
) {
    let clock = shared.clock;
    // Shadow: next lane frame id per display, consecutive over the frames actually sent, so a
    // copy the tap dropped is not counted as loss by the viewer. Live ids come from the tap.
    let mut ids: Vec<(u8, u32)> = Vec::new();
    let mut w = Window::default();
    let mut pacing = (params.mode == Mode::Live).then(|| Pacing::new(params.burst));
    if let Some(p) = &pacing {
        log::info!(
            "UDPVIDEO host conn={conn_id} lane={lane_id:016x} pacing burst={} hires_timer={}",
            p.burst,
            p.waiter.high_resolution()
        );
    }
    let mut last_log = Instant::now();
    loop {
        // A frame held by the window is checked every 5 ms for WINDOW_WAIT_MAX.
        let wait = if pacing.is_some() {
            Duration::from_millis(5)
        } else {
            Duration::from_secs(1)
        };
        match jobs.recv_timeout(wait) {
            Ok(mut job) => match (&mut pacing, job.frame_id) {
                (Some(p), Some(frame_id)) => {
                    let fetched = job.fetched.take();
                    // Before the first datagram: an ACK for this frame may come back while its
                    // last bursts are still being paced, and must not be clamped below it.
                    if let Ok(mut live) = shared.live.lock() {
                        let d = live.disp(job.display);
                        if d.last_sent.map_or(true, |s| id_after(frame_id, s)) {
                            d.last_sent = Some(frame_id);
                        }
                    }
                    let sent = send_live(
                        sender,
                        p,
                        &mut w,
                        &job,
                        frame_id,
                        clock,
                        shared.fec_percent(params.fec_percent),
                        |d| socket.send(d),
                    );
                    if let (true, Some(hooks)) = (sent.is_some(), &hooks) {
                        (hooks.sent)(&job.msg, sent.unwrap_or_default());
                    }
                    if p.errors_in_row >= SEND_ERRORS_MAX {
                        if let Ok(mut f) = shared.fatal.lock() {
                            f.get_or_insert_with(|| {
                                format!(
                                    "{} datagram sends failed in a row: {}",
                                    p.errors_in_row,
                                    w.first_error.clone().unwrap_or_default()
                                )
                            });
                        }
                    }
                    on_live_sent(shared, job.display, frame_id, fetched);
                }
                _ => send_job(
                    sender,
                    &mut ids,
                    &mut w,
                    job,
                    clock,
                    shared.fec_percent(params.fec_percent),
                    |d| socket.send(d),
                ),
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if pacing.is_some() {
            expire_window(shared);
            if let Some(hooks) = &hooks {
                for display in tail_losses(shared, Instant::now()) {
                    shared.tail_kf.fetch_add(1, Ordering::Relaxed);
                    shared.tail_new.fetch_add(1, Ordering::Relaxed);
                    (hooks.keyframe)(display as usize);
                }
            }
        }
        if last_log.elapsed() >= LOG_EVERY {
            log_window(
                conn_id,
                lane_id,
                params.mode,
                shared,
                &mut w,
                last_log.elapsed(),
            );
            last_log = Instant::now();
        }
    }
    if w.frames > 0 {
        log_window(
            conn_id,
            lane_id,
            params.mode,
            shared,
            &mut w,
            last_log.elapsed(),
        );
    }
    // A frame still held by the window is reported fetched as the lane state goes.
    if let Ok(mut live) = shared.live.lock() {
        for d in live.displays.iter_mut() {
            d.pending = None;
        }
    }
}

/// A live frame is out: report it fetched now if the window allows, else hold it.
fn on_live_sent(shared: &Shared, display: u8, frame_id: u32, fetched: Option<Fetched>) {
    let Ok(mut live) = shared.live.lock() else {
        return;
    };
    let d = live.disp(display);
    d.on_newest(frame_id, Instant::now());
    let inflight = d.inflight();
    // An older held frame goes now: only the newest waits.
    let older = d.pending.take();
    if inflight > WINDOW_FRAMES {
        d.pending = fetched.map(|f| (f, Instant::now()));
    } else {
        drop(fetched);
    }
    live.inflight_max = live.inflight_max.max(inflight);
    if let Some((f, since)) = older {
        live.win_wait.push_duration(since.elapsed());
        drop(f);
    }
}

/// Displays with a tail loss: the newest frame went out `TAIL_LOSS_AFTER` (plus the ACK round
/// trip baseline) ago, nothing newer followed, and the viewer has not settled it. Each newest id
/// is reported once; the key frame that repairs it is a newer id.
fn tail_losses(shared: &Shared, now: Instant) -> Vec<u8> {
    let Ok(mut live) = shared.live.lock() else {
        return Vec::new();
    };
    let after = TAIL_LOSS_AFTER + Duration::from_micros(live.baseline().unwrap_or(0) as u64);
    let mut out = Vec::new();
    for d in live.displays.iter_mut() {
        let (Some(newest), Some(at)) = (d.last_sent, d.newest_at) else {
            continue;
        };
        let unsettled = d.settled.map_or(true, |s| id_after(newest, s));
        if unsettled && d.tail_fixed != Some(newest) && now.saturating_duration_since(at) >= after
        {
            d.tail_fixed = Some(newest);
            out.push(d.display);
        }
    }
    out
}

/// Releases frames the window held longer than `WINDOW_WAIT_MAX`.
fn expire_window(shared: &Shared) {
    let Ok(mut live) = shared.live.lock() else {
        return;
    };
    let mut expired = Vec::new();
    for d in live.displays.iter_mut() {
        if d.pending
            .as_ref()
            .map_or(false, |(_, since)| since.elapsed() >= WINDOW_WAIT_MAX)
        {
            expired.extend(d.pending.take());
        }
    }
    for (f, since) in expired {
        live.win_timeouts += 1;
        live.win_wait.push_duration(since.elapsed());
        drop(f);
    }
}

/// Seals and sends one live frame in paced bursts, with parity at `fec_percent` (0: none).
/// Returns the send time (first datagram to last, pacing included) if the frame got to the
/// socket.
#[allow(clippy::too_many_arguments)]
fn send_live(
    sender: &Mutex<LaneSender>,
    p: &mut Pacing,
    w: &mut Window,
    job: &Job,
    frame_id: u32,
    clock: LaneClock,
    fec_percent: u32,
    mut send: impl FnMut(&[u8]) -> io::Result<usize>,
) -> Option<Duration> {
    let begin = Instant::now();
    w.lag
        .push_duration(begin.saturating_duration_since(job.queued));
    let bytes = match job.msg.write_to_bytes() {
        Ok(b) => b,
        Err(e) => {
            w.errors += 1;
            w.first_error
                .get_or_insert_with(|| format!("serialize: {e}"));
            return None;
        }
    };
    let meta = FrameMeta {
        frame_id,
        display: job.display,
        epoch: job.epoch,
        flags: job.flags,
        send_us: clock.now_us(),
    };
    let parts = match fragment(&bytes, meta, PATH.max_fragment_payload()) {
        Ok(p) => p,
        Err(e) => {
            w.errors += 1;
            w.first_error
                .get_or_insert_with(|| format!("fragment: {e:?}"));
            return None;
        }
    };
    let mut shards = w.protect(parts, fec_percent);
    // The pacer spreads the whole frame, parity included, within half a frame interval.
    p.on_frame(shards.wire_len(), job.queued, begin);
    // Unpaced: one datagram at a time, so no parity is encoded ahead of the data before it.
    let burst = if p.burst == 0 { 1 } else { p.burst };
    let mut paced = Duration::ZERO;
    let mut any_sent = false;
    let mut chunk: Vec<Shard> = Vec::with_capacity(burst);
    loop {
        chunk.clear();
        chunk.extend(shards.by_ref().take(burst));
        if chunk.is_empty() {
            break;
        }
        if p.burst > 0 {
            let chunk_bytes = chunk
                .iter()
                .map(|s| s.payload_len() + super::packet::VIDEO_OVERHEAD)
                .sum();
            let now = Instant::now();
            let delay = p.pacer.schedule(chunk_bytes, now);
            if delay > Duration::ZERO {
                p.waiter.wait_until(now + delay);
                paced += delay;
            }
        }
        for shard in &chunk {
            match seal_send(sender, &shard.inner(), &mut send) {
                Ok(n) => {
                    w.dgrams += 1;
                    w.fec_dgrams += shard.is_parity() as u64;
                    w.bytes += n as u64;
                    p.errors_in_row = 0;
                    any_sent = true;
                }
                Err(e) => {
                    w.errors += 1;
                    p.errors_in_row += 1;
                    w.first_error.get_or_insert(e);
                }
            }
        }
    }
    w.protected(&shards);
    w.frames += 1;
    if job.flags & FLAG_KEY != 0 {
        w.keys += 1;
    }
    let took = begin.elapsed();
    w.send.push_duration(took);
    w.paced.push_duration(paced);
    any_sent.then_some(took)
}

#[allow(clippy::too_many_arguments)]
fn send_job(
    sender: &Mutex<LaneSender>,
    ids: &mut Vec<(u8, u32)>,
    w: &mut Window,
    job: Job,
    clock: LaneClock,
    fec_percent: u32,
    mut send: impl FnMut(&[u8]) -> io::Result<usize>,
) {
    let begin = Instant::now();
    w.lag
        .push_duration(begin.saturating_duration_since(job.queued));
    let bytes = match job.msg.write_to_bytes() {
        Ok(b) => b,
        Err(e) => {
            w.errors += 1;
            w.first_error
                .get_or_insert_with(|| format!("serialize: {e}"));
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
    // Stamped after serialization, right before the first datagram is sealed and sent: the
    // viewer measures one-way delay from here to its first fragment's arrival.
    let meta = FrameMeta {
        frame_id: id,
        display: job.display,
        epoch: job.epoch,
        flags: job.flags,
        send_us: clock.now_us(),
    };
    let parts = match fragment(&bytes, meta, PATH.max_fragment_payload()) {
        Ok(p) => p,
        Err(e) => {
            w.errors += 1;
            w.first_error
                .get_or_insert_with(|| format!("fragment: {e:?}"));
            return;
        }
    };
    let mut shards = w.protect(parts, fec_percent);
    for shard in shards.by_ref() {
        match seal_send(sender, &shard.inner(), &mut send) {
            Ok(n) => {
                w.dgrams += 1;
                w.fec_dgrams += shard.is_parity() as u64;
                w.bytes += n as u64;
            }
            Err(e) => {
                w.errors += 1;
                w.first_error.get_or_insert(e);
            }
        }
    }
    w.protected(&shards);
    w.frames += 1;
    if job.flags & FLAG_KEY != 0 {
        w.keys += 1;
    }
    w.send.push_duration(begin.elapsed());
}

fn log_window(
    conn_id: i32,
    lane_id: u64,
    mode: Mode,
    shared: &Shared,
    w: &mut Window,
    span: Duration,
) {
    let take = |c: &AtomicU64| c.swap(0, Ordering::Relaxed);
    let live = if mode == Mode::Live {
        match shared.live.lock() {
            Ok(mut guard) => {
                let l = &mut *guard;
                let base = l.baseline();
                let s = format!(
                    " paced_ms={} win_wait_ms={} win_to={} inflight_max={} acks={} ack_rtt_ms={} base_ms={} vlost={} kf_req={} tail_kf={} rfi_req={} rp={}",
                    w.paced.ms3(),
                    l.win_wait.ms3(),
                    l.win_timeouts,
                    l.inflight_max,
                    take(&shared.acks),
                    l.rtt.ms3(),
                    base.map_or("-".to_owned(), |b| format!("{:.1}", b as f64 / 1000.0)),
                    l.lost_win,
                    take(&shared.kf_req),
                    take(&shared.tail_kf),
                    take(&shared.rfi_req),
                    take(&shared.rp),
                );
                l.win_wait = Samples::default();
                l.rtt = Samples::default();
                l.win_timeouts = 0;
                l.inflight_max = 0;
                l.lost_win = 0;
                s
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };
    let fec = if w.fec_frames + w.fec_off > 0 {
        // The adaptive percentage (M23), once it changed the lane's.
        let adapted = match shared.fec_now.load(Ordering::Relaxed) {
            FEC_UNSET => String::new(),
            p => format!(" fec_pct={p}"),
        };
        format!(
            " fec_frames={} fec_dgrams={} fec_off={} fec_ms={}{}",
            w.fec_frames,
            w.fec_dgrams,
            w.fec_off,
            w.fec_enc.ms3(),
            adapted
        )
    } else {
        String::new()
    };
    log::info!(
        "UDPVIDEO host conn={} lane={:016x} mode={} win_s={:.1} frames={} keys={} skipped={} qdrop={} dgrams={} kB={} send_ms={} lag_ms={} err={} pings={} refused={}{}{}{}",
        conn_id,
        lane_id,
        mode.label(),
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
        fec,
        live,
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
    use base::message_proto::{EncodedVideoFrame, EncodedVideoFrames, SwitchDisplay};

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

    pub(crate) fn switch_display() -> Arc<Message> {
        let mut sd = Misc::new();
        sd.set_switch_display(SwitchDisplay::new());
        let mut m = Message::new();
        m.set_misc(sd);
        Arc::new(m)
    }

    /// Live hooks that count fetched reports per display.
    pub(crate) fn counting_hooks() -> (Arc<LiveHooks>, Arc<Mutex<Vec<usize>>>) {
        let fetched = Arc::new(Mutex::new(Vec::new()));
        let f2 = fetched.clone();
        let hooks = Arc::new(LiveHooks {
            fetched: Box::new(move |d, _| f2.lock().unwrap().push(d)),
            sent: Box::new(|_, _| {}),
            keyframe: Box::new(|_| {}),
        });
        (hooks, fetched)
    }

    fn control_of(m: &Message) -> &UdpVideoControl {
        match &m.union {
            Some(message::Union::Misc(misc)) => match &misc.union {
                Some(misc::Union::UdpVideoControl(c)) => c,
                _ => panic!("not a lane control"),
            },
            _ => panic!("not a misc"),
        }
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
            Ok((
                "192.168.1.5".parse().unwrap(),
                "192.168.1.9".parse().unwrap()
            ))
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
    fn latency_params_and_mode_options() {
        assert_eq!(mode_from_option("live"), Some(Mode::Live));
        assert_eq!(mode_from_option("shadow"), Some(Mode::Shadow));
        // M19's value keeps meaning shadow: a newer build never goes live without a new opt-in.
        assert_eq!(mode_from_option("Y"), Some(Mode::Shadow));
        assert_eq!(mode_from_option(""), None);
        assert_eq!(mode_from_option("N"), None);
        let p = Params::from_options(Mode::Live, "", "", "", "");
        assert_eq!(
            (p.every, p.burst, p.timeout, p.fec_percent),
            (DEFAULT_SHADOW_EVERY, DEFAULT_BURST, DEFAULT_TIMEOUT, 20)
        );
        let p = Params::from_options(Mode::Live, "5", "0", "300", "0");
        assert_eq!(
            (p.every, p.burst, p.timeout, p.fec_percent),
            (5, 0, Duration::from_millis(300), 0)
        );
        let p = Params::from_options(Mode::Live, "61", "65", "100", "101");
        assert_eq!(
            (p.every, p.burst, p.timeout, p.fec_percent),
            (DEFAULT_SHADOW_EVERY, DEFAULT_BURST, DEFAULT_TIMEOUT, 20)
        );
        assert_eq!(
            Params::from_options(Mode::Live, "", "", "", "35").fec_percent,
            35
        );
    }

    #[test]
    fn tap_every_nth_per_display_and_epoch() {
        let tap = VideoTap::default();
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Arc::new(Shared::new(LaneClock::new()));
        let now = Instant::now();
        // Inactive: nothing is taken, everything goes to TCP.
        assert!(tap.route(&video(0, 1, true), now).tcp);
        tap.activate_shadow(tx, shared.clone(), 2);
        for pts in 0..6 {
            assert!(tap.route(&video(0, pts, pts == 0), now).tcp);
            assert!(tap.route(&video(1, pts, false), now).tcp);
        }
        assert!(tap.route(&switch_display(), now).tcp);
        tap.route(&video(0, 6, false), now);
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
        // A shadow lane leaves no marker behind.
        assert_eq!(tap.stop("test"), StopInfo::default());
        let r = tap.route(&video(0, 8, false), now);
        assert!(r.tcp && r.before.is_empty());
        assert!(rx.try_recv().is_err());
        assert!(!tap.busy.load(Ordering::Relaxed));
    }

    /// Live: the first UDP frame brings the ACTIVE marker (in the video order, on TCP), frames
    /// go to UDP only with ids from 0 per display, SwitchDisplay stays on TCP and bumps the
    /// epoch, and stopping leaves an in-band STOP with the last ids for the next item, which
    /// goes to TCP again.
    #[test]
    fn latency_live_tap_routes_and_markers() {
        let tap = VideoTap::default();
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Arc::new(Shared::new(LaneClock::new()));
        let (hooks, fetched) = counting_hooks();
        let now = Instant::now();
        tap.activate_live(tx, shared, hooks, 77, false);
        let r = tap.route(&video(0, 0, true), now);
        assert!(!r.tcp);
        assert_eq!(r.before.len(), 1);
        let c = control_of(&r.before[0]);
        assert_eq!(
            (c.kind.enum_value(), c.lane_id, c.epoch),
            (Ok(udp_video_control::Kind::KindActive), 77, 0)
        );
        let r = tap.route(&video(0, 1, false), now);
        assert!(!r.tcp && r.before.is_empty());
        // The SwitchDisplay stays on TCP, right after an EPOCH marker with the new epoch.
        let r = tap.route(&switch_display(), now);
        assert!(r.tcp);
        assert_eq!(r.before.len(), 1);
        let c = control_of(&r.before[0]);
        assert_eq!(
            (c.kind.enum_value(), c.lane_id, c.epoch),
            (Ok(udp_video_control::Kind::KindEpoch), 77, 1)
        );
        assert!(!tap.route(&video(1, 2, true), now).tcp);
        assert!(!tap.route(&video(0, 3, false), now).tcp);
        let jobs: Vec<(u8, Option<u32>, u8, u8)> = rx
            .try_iter()
            .map(|j| (j.display, j.frame_id, j.epoch, j.flags))
            .collect();
        assert_eq!(
            jobs,
            vec![
                (0, Some(0), 0, FLAG_KEY),
                (0, Some(1), 0, 0),
                (1, Some(0), 1, FLAG_KEY),
                (0, Some(2), 1, 0),
            ]
        );
        // The jobs (and their fetched guards) were dropped with the iterator: all reported.
        assert_eq!(fetched.lock().unwrap().len(), 4);
        let info = tap.stop("test stop");
        assert!(info.in_band && info.was_live);
        assert_eq!(info.displays, vec![0, 1]);
        assert!(tap.busy.load(Ordering::Relaxed));
        let r = tap.route(&video(0, 4, true), now);
        assert!(r.tcp);
        assert_eq!(r.before.len(), 1);
        let c = control_of(&r.before[0]);
        assert_eq!(c.kind.enum_value(), Ok(udp_video_control::Kind::KindStop));
        assert!(c.in_band);
        let last: Vec<(i32, u32)> = c
            .last_frames
            .iter()
            .map(|l| (l.display, l.frame_id))
            .collect();
        assert_eq!(last, vec![(0, 2), (1, 0)]);
        assert!(!tap.busy.load(Ordering::Relaxed));
    }

    /// M22: a lane with RFI maps the viewer's lost frame id to its pts, and flags a recovery
    /// frame RECOVERY_POINT only when it references nothing from that frame on; a key frame ends
    /// the wait too, and a lane without RFI never flags one.
    #[test]
    fn latency_live_rfi_recovery_point_per_lane() {
        // Display 7: the registry is shared with the other tests of the binary.
        let tap = VideoTap::default();
        let (tx, rx) = mpsc::sync_channel(64);
        let shared = Arc::new(Shared::new(LaneClock::new()));
        let (hooks, _) = counting_hooks();
        let now = Instant::now();
        tap.activate_live(tx, shared.clone(), hooks, 3, true);
        // Lane ids 0..=4 carry pts 100..=104.
        for pts in 100..=104 {
            assert!(!tap.route(&video(7, pts, pts == 100), now).tcp);
        }
        assert_eq!(tap.rfi_loss(7, 9), None);
        assert_eq!(tap.rfi_loss(7, 3), Some(103));
        // A recovery frame of another lane's later loss (bound 104 > 103) is not ours.
        rfi::register(7, 105, 104);
        tap.route(&video(7, 105, false), now);
        // Ours: references nothing from 103 on.
        rfi::register(7, 106, 103);
        tap.route(&video(7, 106, false), now);
        // Only once: the wait has ended.
        rfi::register(7, 107, 100);
        tap.route(&video(7, 107, false), now);
        // A new loss ends with a key frame, not with a recovery point registered later.
        assert_eq!(tap.rfi_loss(7, 7), Some(107));
        tap.route(&video(7, 108, true), now);
        rfi::register(7, 109, 107);
        tap.route(&video(7, 109, false), now);
        // Frames before a key frame are forgotten: a loss among them gets a key frame.
        assert_eq!(tap.rfi_loss(7, 7), None);
        assert_eq!(tap.rfi_loss(7, 9), Some(109));
        let flags: Vec<(Option<u32>, u8)> = rx.try_iter().map(|j| (j.frame_id, j.flags)).collect();
        assert_eq!(
            flags,
            vec![
                (Some(0), FLAG_KEY),
                (Some(1), 0),
                (Some(2), 0),
                (Some(3), 0),
                (Some(4), 0),
                (Some(5), 0),
                (Some(6), FLAG_RECOVERY_POINT),
                (Some(7), 0),
                (Some(8), FLAG_KEY),
                (Some(9), 0),
            ]
        );
        assert_eq!(shared.rp.load(Ordering::Relaxed), 1);
        rfi::reset_display(7);

        // Without RFI: no mapping, no flag.
        let tap = VideoTap::default();
        let (tx, rx) = mpsc::sync_channel(64);
        let (hooks, _) = counting_hooks();
        tap.activate_live(tx, Arc::new(Shared::new(LaneClock::new())), hooks, 4, false);
        tap.route(&video(8, 200, true), now);
        assert_eq!(tap.rfi_loss(8, 0), None);
        rfi::register(8, 201, 0);
        tap.route(&video(8, 201, false), now);
        let flags: Vec<u8> = rx.try_iter().map(|j| j.flags).collect();
        assert_eq!(flags, vec![FLAG_KEY, 0]);
        rfi::reset_display(8);
    }

    /// Live: a full queue drops the frame, reports it fetched at once and leaves its id unused.
    #[test]
    fn latency_live_full_queue_drops_and_reports_fetched() {
        let tap = VideoTap::default();
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared::new(LaneClock::new()));
        let (hooks, fetched) = counting_hooks();
        tap.activate_live(tx, shared.clone(), hooks, 1, false);
        for pts in 0..3 {
            assert!(!tap.route(&video(2, pts, false), Instant::now()).tcp);
        }
        assert_eq!(shared.qdrop.load(Ordering::Relaxed), 2);
        assert_eq!(*fetched.lock().unwrap(), vec![2, 2]);
        let job = rx.try_recv().unwrap();
        assert_eq!(job.frame_id, Some(0));
        drop(job);
        assert_eq!(fetched.lock().unwrap().len(), 3);
    }

    /// The send window: past WINDOW_FRAMES unsettled frames the newest is held (not fetched)
    /// until an ACK settles enough, or until WINDOW_WAIT_MAX.
    #[test]
    fn latency_live_window_holds_until_ack() {
        let shared = Shared::new(LaneClock::new());
        let (hooks, fetched) = counting_hooks();
        let guard = |d: usize| {
            Some(Fetched {
                hooks: hooks.clone(),
                display: d,
                queued: Instant::now(),
            })
        };
        for id in 0..WINDOW_FRAMES {
            on_live_sent(&shared, 0, id, guard(0));
        }
        assert_eq!(fetched.lock().unwrap().len(), 3);
        // The fourth unsettled frame waits.
        on_live_sent(&shared, 0, 3, guard(0));
        assert_eq!(fetched.lock().unwrap().len(), 3);
        let ack = |settled: u32, lost_total: u32| Ack {
            display: 0,
            settled,
            has_settled: true,
            echo_send_us: 0,
            hold_us: ACK_NO_FRAME,
            lost_total,
        };
        // Settling frame 0 leaves 3 in flight: released.
        shared.on_ack(&ack(0, 1), 0, Instant::now());
        assert_eq!(fetched.lock().unwrap().len(), 4);
        // An ACK can never settle past what was sent.
        shared.on_ack(&ack(100, 1), 0, Instant::now());
        assert_eq!(shared.live.lock().unwrap().displays[0].settled, Some(3));
        // Held again, then released by the timeout.
        for id in 4..8 {
            on_live_sent(&shared, 0, id, guard(0));
        }
        assert_eq!(fetched.lock().unwrap().len(), 7);
        std::thread::sleep(WINDOW_WAIT_MAX + Duration::from_millis(5));
        expire_window(&shared);
        assert_eq!(fetched.lock().unwrap().len(), 8);
        let l = shared.live.lock().unwrap();
        assert_eq!((l.win_timeouts, l.lost, l.inflight_max), (1, 1, 4));
    }

    /// The QoS signal: excess over the baseline round trip and a loss penalty, once per sample.
    /// A newest frame the viewer never settles is a tail loss once it is `TAIL_LOSS_AFTER` old,
    /// reported once per newest id; a settled one is not; an older id finishing late (the tap
    /// dropped a newer one meanwhile) never moves the newest back.
    #[test]
    fn latency_live_tail_loss_once_per_newest() {
        let shared = Shared::new(LaneClock::new());
        let t0 = Instant::now();
        {
            let mut live = shared.live.lock().unwrap();
            let d = live.disp(0);
            d.on_newest(4, t0);
            d.settled = Some(3);
            d.on_newest(3, t0);
            assert_eq!(d.last_sent, Some(4));
            let d = live.disp(1);
            d.on_newest(0, t0);
            d.settled = Some(0);
            // Display 2's first frame never arrived: no ACK settles anything.
            live.disp(2).on_newest(0, t0);
        }
        assert!(tail_losses(&shared, t0 + TAIL_LOSS_AFTER / 2).is_empty());
        assert_eq!(tail_losses(&shared, t0 + TAIL_LOSS_AFTER), vec![0, 2]);
        assert!(tail_losses(&shared, t0 + TAIL_LOSS_AFTER * 3).is_empty());
        // The repairing key frame is a newer id: its own tail is watched again.
        shared.live.lock().unwrap().disp(0).on_newest(5, t0);
        assert_eq!(tail_losses(&shared, t0 + TAIL_LOSS_AFTER), vec![0]);
    }

    #[test]
    fn latency_live_qos_excess() {
        let mut l = Live::default();
        let t0 = Instant::now();
        l.on_rtt(2_000, t0);
        l.on_rtt(30_000, t0);
        l.on_rtt(5_000, t0);
        assert_eq!(l.baseline(), Some(2_000));
        assert_eq!(l.excess_max, 28_000);
        // The baseline window rotates: the old minimum is kept one more window.
        l.on_rtt(9_000, t0 + BASE_WINDOW);
        assert_eq!(l.baseline(), Some(2_000));
        l.on_rtt(9_000, t0 + BASE_WINDOW * 2);
        assert_eq!(l.baseline(), Some(9_000));
    }

    /// A PONG sealed by the receive thread while the sender thread is in the middle of a frame
    /// goes out between two of the frame's datagrams, never ahead of datagrams sealed before
    /// it: the viewer sees no reorder, no loss and the whole frame, also for a frame longer than
    /// the 1024-seq replay window.
    #[test]
    fn pong_mid_frame_keeps_seq_order() {
        use crate::udp_video::{fragment::Event, lane::LaneReceiver};
        use std::sync::Barrier;
        let secret = [3u8; SECRET_LEN];
        let sender = Arc::new(Mutex::new(LaneSender::new(&secret, 11, Role::Host, PATH)));
        let wire = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let barrier = Arc::new(Barrier::new(2));
        let pong = {
            let (sender, wire, barrier) = (sender.clone(), wire.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                seal_send(&sender, &Inner::Pong { id: 1, echo_us: 2 }, |d| {
                    wire.lock().unwrap().push(d.to_vec());
                    Ok(d.len())
                })
                .unwrap();
            })
        };
        // About 1.6 MB: more fragments than the replay window holds.
        let msg = video(0, 1_600_000, true);
        let bytes = msg.write_to_bytes().unwrap();
        let job = Job {
            msg,
            display: 0,
            epoch: 0,
            flags: FLAG_SHADOW | FLAG_KEY,
            queued: Instant::now(),
            frame_id: None,
            fetched: None,
        };
        let (mut ids, mut w) = (Vec::new(), Window::default());
        let mut n = 0usize;
        send_job(&sender, &mut ids, &mut w, job, LaneClock::new(), 0, |d| {
            wire.lock().unwrap().push(d.to_vec());
            n += 1;
            if n == 10 {
                // The ping arrives now: the receive thread wants the sender mid-frame.
                barrier.wait();
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(d.len())
        });
        pong.join().unwrap();
        let wire = std::mem::take(&mut *wire.lock().unwrap());
        assert!(wire.len() > 1100);
        assert_eq!((w.errors, w.dgrams as usize), (0, wire.len() - 1));
        let mut rx = LaneReceiver::new(&secret, 11, Role::Viewer, Default::default());
        let now = Instant::now();
        let mut pongs = 0;
        for mut d in wire.iter().cloned() {
            if let Received::Pong { .. } = rx.on_datagram(&mut d, now).unwrap() {
                pongs += 1;
            }
        }
        assert_eq!(pongs, 1);
        let frames: Vec<_> = rx
            .poll(now)
            .into_iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f.data),
                _ => None,
            })
            .collect();
        assert_eq!(frames, vec![bytes]);
        let (loss, drops) = rx.take_stats();
        assert_eq!((loss.reorder_events, drops.replayed), (0, 0));
        assert_eq!(loss.datagrams_expected, wire.len() as u64);
        assert_eq!(loss.datagrams_received, wire.len() as u64);
    }

    /// Live sending in paced bursts with 20% FEC: every datagram of a frame goes out in order,
    /// parity after the data, the bursts are spaced by the pacer, and the frame is whole on
    /// the other end although three data datagrams are lost.
    #[test]
    fn latency_live_paced_bursts() {
        use crate::udp_video::{fragment::Event, lane::LaneReceiver};
        let secret = [4u8; SECRET_LEN];
        let sender = Mutex::new(LaneSender::new(&secret, 12, Role::Host, PATH));
        let msg = video(0, 60_000, true);
        let bytes = msg.write_to_bytes().unwrap();
        let job = Job {
            msg,
            display: 0,
            epoch: 0,
            flags: FLAG_KEY,
            queued: Instant::now(),
            frame_id: Some(0),
            fetched: None,
        };
        let mut p = Pacing::new(4);
        let mut w = Window::default();
        let mut wire: Vec<(Instant, Vec<u8>)> = Vec::new();
        let took = send_live(&sender, &mut p, &mut w, &job, 0, LaneClock::new(), 20, |d| {
            wire.push((Instant::now(), d.to_vec()));
            Ok(d.len())
        });
        assert!(took.is_some());
        let n = bytes.len().div_ceil(PATH.max_fragment_payload());
        let parity = fec::parity_for(n, 20);
        assert_eq!(wire.len(), n + parity);
        assert_eq!((w.fec_frames, w.fec_dgrams, w.fec_off), (1, parity as u64, 0));
        // 63 KB at the frame term (whole frame within ~8.3 ms): bursts of 4 datagrams are
        // spaced, so the frame took a few milliseconds, not microseconds.
        let span = wire.last().unwrap().0 - wire[0].0;
        assert!(span >= Duration::from_millis(3), "{span:?}");
        assert!(span <= Duration::from_millis(100), "{span:?}");
        let mut rx = LaneReceiver::new(&secret, 12, Role::Viewer, Default::default());
        let now = Instant::now();
        for (i, (_, mut d)) in wire.into_iter().enumerate() {
            if ![0, 5, n - 1].contains(&i) {
                rx.on_datagram(&mut d, now).unwrap();
            }
        }
        let frames: Vec<_> = rx
            .poll(now)
            .into_iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f.data),
                _ => None,
            })
            .collect();
        assert_eq!(frames, vec![bytes]);
        assert_eq!(w.errors, 0);
        let (loss, _) = rx.take_stats();
        assert_eq!((loss.fec_recovered, loss.fec_shards), (1, 3));
    }

    #[test]
    fn tap_full_queue_drops() {
        let tap = VideoTap::default();
        let (tx, _rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared::new(LaneClock::new()));
        tap.activate_shadow(tx, shared.clone(), 1);
        for pts in 0..3 {
            tap.route(&video(0, pts, false), Instant::now());
        }
        assert_eq!(shared.qdrop.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn latency_feedback_fec_percent_adapts() {
        let shared = Shared::new(LaneClock::new());
        assert_eq!(shared.fec_percent(20), 20);
        shared.fec_now.store(35, Ordering::Relaxed);
        assert_eq!(shared.fec_percent(20), 35);
    }
}
