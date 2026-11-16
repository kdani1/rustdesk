//! Viewer end of the UDP video lane: shadow probe (M19) and live video (M20).
//!
//! - The viewer advertises `LoginRequest.udp_video_version` ([`advertised_version`], 5: live
//!   mode with EPOCH markers, FEC parity shards and RECOVERY_POINT frames too) on Windows, for
//!   remote control, unless `enable-udp-video` is `N`.
//! - RFI (M22): a display broken by a loss resumes at a key frame or at a frame the host flagged
//!   `RECOVERY_POINT` (the receive thread's loss gate and the io_loop both take either); the host
//!   flags one only for this viewer's own loss, so it never references a frame the viewer lacks.
//! - FEC (M21): the receive thread takes parity shards whenever they come and rebuilds lost
//!   fragments at once (`fragment::Reassembler::push_parity`); the window line splits losses
//!   into recovered (`fec_rec`) and unrecoverable (`fec_unrec`).
//! - On the host's offer, [`ViewerLane::on_offer`] checks it and starts a [`UdpVideoClient`]:
//!   a socket bound to the session's own local TCP address (never the wildcard), connected to
//!   the host's offered address, and one receive thread. The thread sends HELLO every 20 ms for
//!   up to 1 s; on HELLO_ACK the io_loop sends READY over TCP. Then it pings every 200 ms and
//!   reassembles frames; it parses each complete one on the thread (so the io_loop does no
//!   full-frame parse) and hands it, stamped, through a tokio mpsc to a `select!` branch of the
//!   io_loop.
//! - Shadow: the io_loop pairs UDP and TCP copies by pts ([`super::shadow::ShadowMatcher`]) and
//!   logs a `UDPVIDEO viewer` line every 5 s. UDP copies are never decoded or shown.
//! - Live: the receive thread reassembles with the loss gate on (after a loss no frame reaches
//!   the io_loop until a key frame), reports losses, ACKs every display's progress over UDP
//!   (the host's send window and QoS signal), and gives up when the host is silent for the
//!   offer's `timeout_ms`. The io_loop ([`ViewerLane`]) holds UDP frames until it read the
//!   ACTIVE marker and the SwitchDisplay of their epoch (the one right after an EPOCH marker)
//!   on TCP, feeds them to the same video frame handling as TCP frames (decoder queue, M4
//!   pacer, M5 fps control, latency stats), asks the host for a key frame while a display is
//!   broken (`UdpVideoFeedback`: at once, then again every `KF_RETRY` until one arrives), and
//!   asks it to stop the lane (`KindRequestStop`) when a display stays broken for 1 s, frames
//!   are held (for ACTIVE or a SwitchDisplay) for 1 s, frames arrive for 1 s but only of an
//!   ended epoch, or the thread gave up. The host's in-band STOP says which frames went over UDP last, so
//!   the TCP stream after it is decoded only from a key frame if any of them did not arrive.
//!
//! Dropping the client (STOP from the host, a refused handshake, the end of the io_loop round)
//! stops the thread within 100 ms and closes the socket.

use super::{
    crypto::{Role, SECRET_LEN},
    fragment::{Event, LossStats, ReassemblyConfig},
    fec,
    host::{DEFAULT_TIMEOUT, LANE_VERSION, LIVE_LANE_VERSION, TIMEOUT_RANGE_MS, VIEWER_RFI_VERSION},
    lane::{DropStats, LaneReceiver, LaneSender, Received},
    net::{self, LaneClock, ReadTimeout},
    packet::{Ack, Inner, PathClass, ACK_NO_FRAME, FLAG_KEY, FLAG_RECOVERY_POINT, VIDEO_OVERHEAD},
    shadow::{Samples, ShadowMatcher},
};
use base::{
    config::keys,
    message_proto::{
        message, udp_video_control, udp_video_offer, Message, Misc, UdpVideoControl,
        UdpVideoFeedback, UdpVideoOffer, VideoFrame,
    },
};
use crate::video_feedback::viewer::{LostRange, UdpCounts};
use hbb_common::{
    config::LocalConfig, log, protobuf::Message as _, rendezvous_proto::ConnType, tokio::sync::mpsc,
};
use std::{
    collections::VecDeque,
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
/// Live: every display's ACK is repeated this often even without progress.
const ACK_EVERY: Duration = Duration::from_millis(50);
const WINDOW: Duration = Duration::from_secs(5);
const RECV_BUF: usize = 2048;
/// Shadow frames are unpaced and share the link with the full TCP copy, so a big frame can
/// take well over the live lane's 50 ms to arrive whole. A slow but complete frame must still
/// be delivered and paired (a late one shows as a negative ahead sample), not counted as lost.
const SHADOW_FRAME_TIMEOUT: Duration = Duration::from_millis(500);
/// Live: receive errors in a row that end the lane.
const RECV_ERRORS_MAX: u32 = 5;
/// Live: UDP frames held for ACTIVE or a SwitchDisplay, at most; an overflow is a loss.
const HOLD_MAX: usize = 32;
/// Live: a display broken this long after a loss (no key frame), or frames waiting this long
/// for ACTIVE, and the viewer asks the host to stop the lane.
pub const WATCHDOG: Duration = Duration::from_secs(1);
/// Loss ranges kept for the per-frame feedback between two batches (a batch carries 16).
const FEEDBACK_LOST_MAX: usize = 32;
/// Live: while a display waits for a key frame, the request is repeated this long after the
/// last one (the key frame or its request may be lost too). Losses in between ask nothing: the
/// key frame already asked for repairs them. About 2.5x the usual request-to-IDR time on a LAN.
const KF_RETRY: Duration = Duration::from_millis(250);
/// Live: a frame not complete this long after its first fragment is lost. Above the pacer's
/// longest spread (`pacing::MAX_SPREAD`, 25 ms) with room for Wi-Fi jitter.
pub const LIVE_FRAME_TIMEOUT: Duration = Duration::from_millis(80);

/// The lane version to advertise in the login request, 0 for none.
pub fn advertised_version(conn_type: ConnType) -> u32 {
    if cfg!(windows) && conn_type == ConnType::DEFAULT_CONN && enabled() {
        VIEWER_RFI_VERSION
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
    /// The serialized message (compared by the tests; the io_loop pairs by `key` only).
    #[cfg_attr(not(test), allow(dead_code))]
    pub data: Vec<u8>,
    /// (display, pts) of the video frame inside, if it parsed and has a pts.
    pub key: Option<(i32, i64)>,
    pub completed_at: Instant,
}

/// A complete live frame, parsed by the receive thread.
pub struct LiveFrame {
    pub vf: VideoFrame,
    pub display: u8,
    pub frame_id: u32,
    pub epoch: u8,
    /// The host flagged it as a key frame.
    pub key: bool,
    /// The host flagged it as a recovery point for this viewer's loss (M22): it ends a broken
    /// display like a key frame.
    pub recovery: bool,
    pub bytes: usize,
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
    pub acks: u64,
    pub recv_errors: u64,
}

pub enum ViewerEvent {
    HelloAck {
        after: Duration,
        hellos: u32,
    },
    HandshakeFailed(String),
    Frame(ShadowFrame),
    Live(LiveFrame),
    /// Live: frames `from..=to` of `display` will never come; nothing of that display comes
    /// until a key frame. `repaired`: a key frame of the display completed in the same poll and
    /// is its next event, so nothing needs asking.
    Lost {
        display: u8,
        from: u32,
        to: u32,
        repaired: bool,
    },
    /// Live: every `ACK_EVERY` (50 ms) while the lane is up; the io_loop repeats key frame
    /// requests on it.
    Tick,
    /// Live: the receiver's loss counters since the previous one, right before a `Tick` when
    /// any changed; the per-frame feedback (M23) reports them.
    Counts(UdpCounts),
    /// Live: the thread gave up on the lane (silence, socket errors) and stopped.
    Failed(String),
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
    pub live: bool,
    /// Live: silence from the host this long ends the lane.
    pub timeout: Duration,
    /// FEC parity the host adds, in percent (0: none). Logged only: parity shards are taken
    /// whenever they come.
    pub fec: u32,
}

/// Checks an offer against what this build speaks and the session's own local address.
pub fn check_offer(offer: &UdpVideoOffer, local: SocketAddr) -> Result<Accepted, String> {
    let live = match offer.mode.enum_value() {
        Ok(udp_video_offer::Mode::ModeShadow) => false,
        Ok(udp_video_offer::Mode::ModeLive) => true,
        _ => return Err(format!("mode {:?} unsupported", offer.mode)),
    };
    // Live needs the EPOCH markers of lane v2: an older live host marks no SwitchDisplay, and
    // this viewer bumps its epoch only for marked ones.
    let want = if live { LIVE_LANE_VERSION } else { LANE_VERSION };
    if offer.version != want {
        return Err(format!(
            "{} lane version {} unsupported (want {want})",
            if live { "live" } else { "shadow" },
            offer.version
        ));
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
    let timeout = Some(offer.timeout_ms as u64)
        .filter(|ms| (TIMEOUT_RANGE_MS.0..=TIMEOUT_RANGE_MS.1).contains(ms))
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT);
    Ok(Accepted {
        local: local_ip,
        host: SocketAddr::new(host_ip, port),
        secret,
        every: offer.shadow_every.max(1),
        live,
        timeout,
        fec: offer.fec_percent.min(fec::MAX_PERCENT),
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
        let params = RecvParams {
            secret: a.secret,
            lane_id,
            live: a.live,
            timeout: a.timeout,
        };
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("udp-video-rx".to_owned())
            .spawn(move || recv_loop(socket, params, tx, &stop2))
            .map_err(|e| format!("spawn: {e}"))?;
        Ok((Self { lane_id, stop }, local))
    }
}

struct RecvParams {
    secret: [u8; SECRET_LEN],
    lane_id: u64,
    live: bool,
    timeout: Duration,
}

/// Live: what the next ACK of a display says besides `settled`.
struct AckState {
    display: u8,
    echo_send_us: u32,
    first_at: Option<Instant>,
    lost_total: u32,
}

fn ack_state(acks: &mut Vec<AckState>, display: u8) -> &mut AckState {
    let i = match acks.iter().position(|a| a.display == display) {
        Some(i) => i,
        None => {
            acks.push(AckState {
                display,
                echo_send_us: 0,
                first_at: None,
                lost_total: 0,
            });
            acks.len() - 1
        }
    };
    &mut acks[i]
}

fn recv_loop(
    socket: UdpSocket,
    p: RecvParams,
    tx: mpsc::UnboundedSender<ViewerEvent>,
    stop: &AtomicBool,
) {
    #[cfg(windows)]
    let _boost = crate::platform::windows::viewer_thread_boost("udp_video", None);
    let lane_id = p.lane_id;
    let mut lane_tx = LaneSender::new(&p.secret, lane_id, Role::Viewer, PathClass::Lan);
    let config = if p.live {
        // Live: never a frame whose reference is missing; a display starts at frame 0, the
        // host's first.
        ReassemblyConfig {
            first_frame_id: Some(0),
            frame_timeout: LIVE_FRAME_TIMEOUT,
            ..Default::default()
        }
    } else {
        // Shadow frames are measured, never decoded: no gate after a loss, every complete
        // frame counts, also a slow one.
        ReassemblyConfig {
            gate_after_loss: false,
            frame_timeout: SHADOW_FRAME_TIMEOUT,
            ..Default::default()
        }
    };
    let mut lane_rx = LaneReceiver::new(&p.secret, lane_id, Role::Viewer, config);
    let clock = LaneClock::new();
    let started = Instant::now();
    let mut acked = false;
    let mut hellos = 0u32;
    let mut next_hello = started;
    let mut next_ping = started;
    let mut next_ack = started;
    let mut ping_id = 0u32;
    let mut win = RecvWindow::default();
    let mut win_start = started;
    let mut timeout = ReadTimeout::new();
    let mut buf = vec![0u8; RECV_BUF];
    let mut error_logged = false;
    let mut errors_in_row = 0u32;
    let mut last_heard = started;
    let mut acks: Vec<AckState> = Vec::new();
    // Feedback (M23): the receiver's counters at the last `Counts`, and what a window reset
    // took since then.
    let mut fb_prev = UdpCounts::default();
    let mut fb_carry = UdpCounts::default();
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
        } else {
            if now >= next_ping {
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
            if p.live {
                let silent = now.saturating_duration_since(last_heard);
                if silent >= p.timeout {
                    let _ = tx.send(ViewerEvent::Failed(format!(
                        "no datagram from the host in {} ms",
                        silent.as_millis()
                    )));
                    return;
                }
            }
        }
        let busy = !acked || lane_rx.next_deadline().is_some() || (p.live && !acks.is_empty());
        timeout.set(
            &socket,
            if busy {
                net::RECV_TIMEOUT_BUSY
            } else {
                net::RECV_TIMEOUT_IDLE
            },
        );
        match socket.recv(&mut buf) {
            Ok(n) => {
                errors_in_row = 0;
                let res = lane_rx.on_datagram(&mut buf[..n], Instant::now());
                if res.is_ok() {
                    last_heard = Instant::now();
                }
                match res {
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
                }
            }
            Err(e) if net::is_timeout(&e) => {}
            Err(e) => {
                win.recv_errors += 1;
                errors_in_row += 1;
                if !error_logged {
                    error_logged = true;
                    log::warn!("UDPVIDEO viewer recv error: {e}");
                }
                if p.live && errors_in_row >= RECV_ERRORS_MAX {
                    let _ = tx.send(ViewerEvent::Failed(format!(
                        "{errors_in_row} receive errors in a row: {e}"
                    )));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let mut changed: Vec<u8> = Vec::new();
        let events = lane_rx.poll(Instant::now());
        // A loss followed in the same poll by a key frame of its display (that frame overtook
        // the gap, which is why it was declared lost) is already repaired.
        let repaired: Vec<bool> = (0..events.len())
            .map(|i| match &events[i] {
                Event::Lost { display, .. } => events[i + 1..].iter().any(|e| {
                    matches!(e, Event::Frame(f) if f.display == *display && f.flags & FLAG_KEY != 0)
                }),
                Event::Frame(_) => false,
            })
            .collect();
        for (ev, repaired) in events.into_iter().zip(repaired) {
            match ev {
                Event::Frame(f) => {
                    win.transit
                        .push(net::us_diff(clock.us(f.first_fragment_at), f.send_us));
                    win.assembly.push_duration(
                        f.completed_at
                            .saturating_duration_since(f.first_fragment_at),
                    );
                    let out = if p.live {
                        let a = ack_state(&mut acks, f.display);
                        a.echo_send_us = f.send_us;
                        a.first_at = Some(f.first_fragment_at);
                        if !changed.contains(&f.display) {
                            changed.push(f.display);
                        }
                        let bytes = f.data.len();
                        match parse_video(&f.data) {
                            Some(vf) => ViewerEvent::Live(LiveFrame {
                                vf,
                                display: f.display,
                                frame_id: f.frame_id,
                                epoch: f.epoch,
                                key: f.flags & FLAG_KEY != 0,
                                recovery: f.flags & FLAG_RECOVERY_POINT != 0,
                                bytes,
                            }),
                            None => {
                                // Authenticated but not a video frame: a loss like any other.
                                lane_rx.reassembler().force_recovery(f.display);
                                ViewerEvent::Lost {
                                    display: f.display,
                                    from: f.frame_id,
                                    to: f.frame_id,
                                    repaired: false,
                                }
                            }
                        }
                    } else {
                        let key = parse_video(&f.data).and_then(|vf| {
                            crate::client::latency::frame_pts(&vf).map(|pts| (vf.display, pts))
                        });
                        ViewerEvent::Frame(ShadowFrame {
                            data: f.data,
                            key,
                            completed_at: f.completed_at,
                        })
                    };
                    if tx.send(out).is_err() {
                        return;
                    }
                }
                Event::Lost { display, from, to } => {
                    if !p.live {
                        continue;
                    }
                    let a = ack_state(&mut acks, display);
                    a.lost_total = a
                        .lost_total
                        .wrapping_add(to.wrapping_sub(from).wrapping_add(1));
                    if !changed.contains(&display) {
                        changed.push(display);
                    }
                    let ev = ViewerEvent::Lost {
                        display,
                        from,
                        to,
                        repaired,
                    };
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
            }
        }
        if p.live && acked {
            let now = Instant::now();
            let periodic = now >= next_ack;
            if periodic {
                next_ack = now + ACK_EVERY;
            }
            for a in acks.iter() {
                if !periodic && !changed.contains(&a.display) {
                    continue;
                }
                let settled = lane_rx.reassembler().settled(a.display);
                let ack = Ack {
                    display: a.display,
                    settled: settled.unwrap_or(0),
                    has_settled: settled.is_some(),
                    echo_send_us: a.echo_send_us,
                    hold_us: a.first_at.map_or(ACK_NO_FRAME, |t| {
                        now.saturating_duration_since(t)
                            .as_micros()
                            .min(ACK_NO_FRAME as u128 - 1) as u32
                    }),
                    lost_total: a.lost_total,
                };
                send(&mut lane_tx, &Inner::Ack(ack));
                win.acks += 1;
            }
            if periodic {
                let cur = udp_counts(lane_rx.reassembler().stats());
                let mut delta = cur.minus(&fb_prev);
                delta.add(&fb_carry);
                fb_prev = cur;
                fb_carry = UdpCounts::default();
                if !delta.is_zero() && tx.send(ViewerEvent::Counts(delta)).is_err() {
                    return;
                }
                if tx.send(ViewerEvent::Tick).is_err() {
                    return;
                }
            }
        }
        let now = Instant::now();
        if now.duration_since(win_start) >= WINDOW {
            let (loss, drops) = lane_rx.take_stats();
            fb_carry.add(&udp_counts(&loss).minus(&fb_prev));
            fb_prev = UdpCounts::default();
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

/// The counters the per-frame feedback reports, from the receiver's window statistics.
fn udp_counts(s: &LossStats) -> UdpCounts {
    UdpCounts {
        datagrams_expected: s.datagrams_expected,
        datagrams_received: s.datagrams_received,
        frames_completed: s.frames_completed,
        frames_lost: s.frames_lost,
        fec_recovered: s.fec_recovered,
        fec_unrecoverable: s.fec_unrecoverable,
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

fn keyframe_request(lane_id: u64, display: u8, from: u32, to: u32) -> Message {
    let mut misc = Misc::new();
    misc.set_udp_video_feedback(UdpVideoFeedback {
        lane_id,
        display: display as i32,
        request_keyframe: true,
        lost_from: from,
        lost_to: to,
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

/// Live: one display's state in the io_loop.
struct LiveDisp {
    display: u8,
    /// The last frame id handed to the decoder path.
    delivered: Option<u32>,
    /// Since the loss that broke this display; frames are dropped until a key frame.
    awaiting: Option<Instant>,
    /// The last key frame request while awaiting.
    last_req: Option<Instant>,
    /// The first frames lost since `awaiting` (what the requests report).
    lost: (u32, u32),
}

/// Live: the io_loop's state of a live lane, kept after the client closed until the host's
/// in-band STOP said which frames went over UDP.
struct LiveLane {
    lane_id: u64,
    /// The ACTIVE marker was read on TCP.
    active: bool,
    /// The epoch UDP frames must carry to be shown now.
    epoch: u8,
    held: VecDeque<LiveFrame>,
    held_since: Option<Instant>,
    /// The EPOCH marker read on TCP: the next SwitchDisplay makes this epoch current.
    next_epoch: Option<u8>,
    /// Since frames arrive only of an epoch that has ended (none reached the decoder since).
    stale_since: Option<Instant>,
    /// The receive side is gone (REQUEST_STOP sent); waiting for the in-band STOP.
    closed: bool,
    displays: Vec<LiveDisp>,
}

impl LiveLane {
    fn new(lane_id: u64) -> Self {
        Self {
            lane_id,
            active: false,
            epoch: 0,
            held: VecDeque::new(),
            held_since: None,
            next_epoch: None,
            stale_since: None,
            closed: false,
            displays: Vec::new(),
        }
    }

    fn disp(&mut self, display: u8) -> &mut LiveDisp {
        let i = match self.displays.iter().position(|d| d.display == display) {
            Some(i) => i,
            None => {
                self.displays.push(LiveDisp {
                    display,
                    delivered: None,
                    awaiting: None,
                    last_req: None,
                    lost: (0, 0),
                });
                self.displays.len() - 1
            }
        };
        &mut self.displays[i]
    }
}

/// `a` is a later epoch than `b`.
fn epoch_ahead(a: u8, b: u8) -> bool {
    (a.wrapping_sub(b) as i8) > 0
}

/// Live counters for the window line.
#[derive(Default)]
struct LiveStats {
    delivered: u64,
    held: u64,
    held_max: usize,
    stale: u64,
    gated: u64,
    kf_req: u64,
    /// Losses a key frame right behind them repaired (no request).
    lost_fixed: u64,
    tcp_drop: u64,
    /// Broken displays resumed at a RECOVERY_POINT frame (M22), and from the loss to the frame
    /// that resumed a broken display (key or recovery point).
    rp: u64,
    fix: Samples,
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
    live: Option<LiveLane>,
    /// Displays whose TCP frames are dropped until a key frame: a live lane ended with frames
    /// of them that never reached the decoder.
    tcp_await_key: Vec<i32>,
    /// Live frames ready for the decoder path, in order.
    ready: VecDeque<VideoFrame>,
    udp_bytes: usize,
    stats: LiveStats,
    /// For the per-frame feedback (M23): receiver counters and loss ranges since
    /// `take_feedback`, the ranges bounded by `FEEDBACK_LOST_MAX`.
    fb_counts: UdpCounts,
    fb_lost: Vec<LostRange>,
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
        self.live = None;
        self.tcp_await_key.clear();
        self.ready.clear();
        self.stats = LiveStats::default();
        self.fb_counts = UdpCounts::default();
        self.fb_lost.clear();
        self.matcher = ShadowMatcher::default();
        self.peer = peer.to_owned();
        self.round = Some(Round { local, tx });
        rx
    }

    pub fn end_round(&mut self) {
        self.close("session round ended");
        self.live = None;
        self.ready.clear();
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
        self.replace_live();
        match UdpVideoClient::start(&a, offer.lane_id, tx) {
            Ok((client, bound)) => {
                log::info!(
                    "UDPVIDEO viewer peer={} accepted lane={:016x} mode={} host={} local={} every={} timeout_ms={} fec={}",
                    self.peer,
                    offer.lane_id,
                    if a.live { "live" } else { "shadow" },
                    a.host,
                    bound,
                    a.every,
                    a.timeout.as_millis(),
                    a.fec
                );
                self.client = Some(client);
                self.matcher = ShadowMatcher::default();
                if a.live {
                    self.live = Some(LiveLane::new(offer.lane_id));
                }
                None
            }
            Err(why) => refuse(why),
        }
    }

    /// A live lane is replaced before its in-band STOP came: whatever it carried is unknown, so
    /// its displays start again on TCP only from a key frame.
    fn replace_live(&mut self) {
        if let Some(l) = self.live.take() {
            if l.active {
                for d in &l.displays {
                    self.await_tcp_key(d.display as i32);
                }
            }
        }
    }

    fn await_tcp_key(&mut self, display: i32) {
        if !self.tcp_await_key.contains(&display) {
            self.tcp_await_key.push(display);
        }
    }

    /// Lane control from the host.
    pub fn on_control(&mut self, c: &UdpVideoControl) {
        let kind = c.kind.enum_value();
        if self.live.as_ref().map(|l| l.lane_id) == Some(c.lane_id) {
            match kind {
                Ok(udp_video_control::Kind::KindActive) => {
                    let peer = self.peer.clone();
                    if let Some(l) = self.live.as_mut() {
                        l.active = true;
                        l.epoch = c.epoch as u8;
                        log::info!(
                            "UDPVIDEO viewer peer={peer} live lane={:016x} epoch={} held={} closed={}",
                            l.lane_id,
                            l.epoch,
                            l.held.len(),
                            l.closed
                        );
                    }
                    self.release_held();
                }
                Ok(udp_video_control::Kind::KindEpoch) => {
                    if let Some(l) = self.live.as_mut() {
                        l.next_epoch = Some(c.epoch as u8);
                    }
                }
                Ok(udp_video_control::Kind::KindStop) => self.finish_live(c),
                _ => {}
            }
            return;
        }
        let ours = self.client.as_ref().map(|c| c.lane_id);
        if ours != Some(c.lane_id) {
            return;
        }
        if kind == Ok(udp_video_control::Kind::KindStop) {
            let why = format!("host stopped: {}", c.reason);
            self.close(&why);
        }
    }

    /// The host ended the live lane. In band, it lists the last frame it gave UDP per display:
    /// a display whose last frame did not reach the decoder takes TCP frames only from a key
    /// frame (the host asked for one).
    fn finish_live(&mut self, c: &UdpVideoControl) {
        let Some(mut l) = self.live.take() else {
            return;
        };
        self.close(&format!(
            "host stopped live lane: {} (in_band={})",
            c.reason, c.in_band
        ));
        if !l.active {
            return;
        }
        let mut broken = Vec::new();
        if c.in_band {
            for lf in &c.last_frames {
                let display = lf.display.clamp(0, u8::MAX as i32) as u8;
                let d = l.disp(display);
                if d.delivered != Some(lf.frame_id) || d.awaiting.is_some() {
                    broken.push(lf.display);
                }
            }
        } else {
            broken.extend(l.displays.iter().map(|d| d.display as i32));
        }
        log::info!(
            "UDPVIDEO viewer peer={} live lane={:016x} back on TCP: key frame awaited on displays {:?}",
            self.peer,
            l.lane_id,
            broken
        );
        for d in broken {
            self.await_tcp_key(d);
        }
    }

    /// A SwitchDisplay was read on TCP. After an EPOCH marker it starts the marker's epoch: UDP
    /// frames of it may be shown now. Without one it is a SwitchDisplay the host sent outside
    /// the video stream (display switch answer, message query), which its tap never counted.
    pub fn on_switch_display(&mut self) {
        if let Some(l) = self.live.as_mut() {
            if !l.active || l.closed {
                return;
            }
            if let Some(epoch) = l.next_epoch.take() {
                l.epoch = epoch;
                self.release_held();
            }
        }
    }

    /// Delivers held frames whose epoch is current, in order; later epochs stay held, earlier
    /// ones are dropped.
    fn release_held(&mut self) {
        let Some(l) = self.live.as_mut() else {
            return;
        };
        if !l.active || l.closed {
            return;
        }
        let held = std::mem::take(&mut l.held);
        l.held_since = None;
        for f in held {
            self.on_live_frame(f);
        }
    }

    fn on_live_frame(&mut self, f: LiveFrame) {
        let Some(l) = self.live.as_mut() else {
            return;
        };
        if l.closed {
            return;
        }
        if !l.active || epoch_ahead(f.epoch, l.epoch) {
            if l.held.len() >= HOLD_MAX {
                if let Some(old) = l.held.pop_front() {
                    // A dropped frame breaks its display's chain: a loss, asked for like one.
                    let d = l.disp(old.display);
                    if d.awaiting.is_none() {
                        d.awaiting = Some(Instant::now());
                        d.lost = (old.frame_id, old.frame_id);
                    }
                }
            }
            l.held.push_back(f);
            l.held_since.get_or_insert_with(Instant::now);
            self.stats.held += 1;
            self.stats.held_max = self.stats.held_max.max(l.held.len());
            return;
        }
        if f.epoch != l.epoch {
            // From before a SwitchDisplay already read: that stream has ended.
            self.stats.stale += 1;
            l.stale_since.get_or_insert_with(Instant::now);
            return;
        }
        let d = l.disp(f.display);
        if d.awaiting.is_some() && !f.key && !f.recovery {
            self.stats.gated += 1;
            return;
        }
        if let Some(since) = d.awaiting.take() {
            self.stats.fix.push_duration(since.elapsed());
            if !f.key {
                self.stats.rp += 1;
            }
        }
        if f.key || f.recovery {
            d.last_req = None;
        }
        l.stale_since = None;
        let d = l.disp(f.display);
        d.delivered = Some(f.frame_id);
        self.stats.delivered += 1;
        self.ready.push_back(f.vf);
    }

    /// Asks the host to stop the lane; the in-band STOP will still say what went over UDP.
    fn request_stop(&mut self, why: &str) -> Option<Message> {
        let lane_id = self
            .live
            .as_ref()
            .map(|l| l.lane_id)
            .or(self.client.as_ref().map(|c| c.lane_id))?;
        log::warn!(
            "UDPVIDEO viewer peer={} request stop lane={lane_id:016x}: {why}",
            self.peer
        );
        self.close(why);
        if let Some(l) = self.live.as_mut() {
            l.closed = true;
            // Held frames will never be shown: their displays are broken.
            for f in std::mem::take(&mut l.held) {
                l.disp(f.display).awaiting.get_or_insert_with(Instant::now);
            }
        }
        Some(control(
            udp_video_control::Kind::KindRequestStop,
            lane_id,
            why,
        ))
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
                // No READY went out, so the host never went live.
                self.live = None;
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
            ViewerEvent::Live(f) => {
                self.udp_bytes += f.bytes;
                // A hold overflow may break a display: asked for below.
                self.on_live_frame(f);
                self.next_request(lane_id, Instant::now())
            }
            ViewerEvent::Lost {
                display,
                from,
                to,
                repaired,
            } => {
                let now = Instant::now();
                let l = self.live.as_mut()?;
                if self.fb_lost.len() < FEEDBACK_LOST_MAX {
                    self.fb_lost.push(LostRange {
                        display,
                        from,
                        to,
                        repaired,
                    });
                }
                if repaired {
                    // The key frame behind it is the display's next event.
                    self.stats.lost_fixed += 1;
                    return None;
                }
                let d = l.disp(display);
                if d.awaiting.is_none() {
                    d.awaiting = Some(now);
                    d.lost = (from, to);
                }
                // A display already waiting asks nothing new: the key frame it asked for
                // repairs this loss too; `KF_RETRY` covers a lost key frame or request.
                self.next_request(lane_id, now)
            }
            ViewerEvent::Tick => self.next_request(lane_id, Instant::now()),
            ViewerEvent::Counts(c) => {
                self.fb_counts.add(&c);
                None
            }
            ViewerEvent::Failed(why) => self.request_stop(&why),
            ViewerEvent::Window(w) => {
                self.log_window(lane_id, *w);
                None
            }
        }
    }

    /// Live: the key frame request due now, if any: a broken display that has not asked yet,
    /// or whose last request is `KF_RETRY` old. One per call (the next event or tick, 50 ms at
    /// most, takes the next display).
    fn next_request(&mut self, lane_id: u64, now: Instant) -> Option<Message> {
        let l = self.live.as_mut()?;
        if l.closed {
            return None;
        }
        let d = l.displays.iter_mut().find(|d| {
            d.awaiting.is_some()
                && d.last_req
                    .map_or(true, |t| now.saturating_duration_since(t) >= KF_RETRY)
        })?;
        d.last_req = Some(now);
        self.stats.kf_req += 1;
        Some(keyframe_request(lane_id, d.display, d.lost.0, d.lost.1))
    }

    /// Every status tick (1 s): the live watchdog. Returns the stop request to send, if any.
    pub fn tick(&mut self) -> Option<Message> {
        let l = self.live.as_ref()?;
        if l.closed || self.client.is_none() {
            return None;
        }
        let now = Instant::now();
        if let Some(since) = l.held_since {
            let held = now.saturating_duration_since(since);
            if held >= WATCHDOG {
                let why = if l.active {
                    format!(
                        "UDP frames waited {} ms for the SwitchDisplay of their epoch (at {})",
                        held.as_millis(),
                        l.epoch
                    )
                } else {
                    format!("UDP frames waited {} ms for the ACTIVE marker", held.as_millis())
                };
                return self.request_stop(&why);
            }
        }
        if let Some(since) = l.stale_since {
            let stale = now.saturating_duration_since(since);
            if stale >= WATCHDOG {
                let why = format!(
                    "UDP frames arrive but only of an ended epoch, none shown for {} ms (at {})",
                    stale.as_millis(),
                    l.epoch
                );
                return self.request_stop(&why);
            }
        }
        let broken = l.displays.iter().find_map(|d| {
            d.awaiting
                .filter(|t| now.saturating_duration_since(*t) >= WATCHDOG)
                .map(|t| (d.display, now.saturating_duration_since(t)))
        });
        if let Some((display, for_)) = broken {
            return self.request_stop(&format!(
                "display {display} without a key frame {} ms after a loss",
                for_.as_millis()
            ));
        }
        None
    }

    /// The next live frame for the decoder path.
    #[inline]
    pub fn next_ready(&mut self) -> Option<VideoFrame> {
        self.ready.pop_front()
    }

    /// The per-frame feedback's share (M23): receiver counters and loss ranges since the last
    /// call.
    pub fn take_feedback(&mut self) -> (UdpCounts, Vec<LostRange>) {
        (
            std::mem::take(&mut self.fb_counts),
            std::mem::take(&mut self.fb_lost),
        )
    }

    /// Bytes of live frames received since the last call (the session's kB/s).
    #[inline]
    pub fn take_udp_bytes(&mut self) -> usize {
        std::mem::take(&mut self.udp_bytes)
    }

    /// The time `peer.next()` returned the TCP message about to be parsed and handled.
    #[inline]
    pub fn note_tcp_rx(&mut self, at: Instant) {
        self.tcp_rx_at = Some(at);
    }

    /// The TCP copy of a frame, as the io_loop handles it. Returns false when the frame must
    /// be dropped: after a live lane, a display whose UDP frames did not all arrive restarts
    /// on TCP from a key frame. Shadow: stamped with [`Self::note_tcp_rx`], taken before the
    /// message was parsed, like the UDP copy.
    #[inline]
    pub fn on_tcp_frame(&mut self, vf: &VideoFrame) -> bool {
        let at = self.tcp_rx_at.take();
        if self.client.is_some() && self.live.is_none() {
            if let Some(pts) = crate::client::latency::frame_pts(vf) {
                self.matcher
                    .on_tcp(vf.display, pts, at.unwrap_or_else(Instant::now));
            }
        }
        if self.live.is_none() && self.tcp_await_key.is_empty() {
            return true;
        }
        self.on_tcp_frame_live(vf)
    }

    fn on_tcp_frame_live(&mut self, vf: &VideoFrame) -> bool {
        if self.live.as_ref().map_or(false, |l| l.active) {
            // The host sends no TCP video while its live lane is active; one without the STOP
            // before it is out of order: take nothing that may reference a UDP frame.
            self.await_tcp_key(vf.display);
        }
        let Some(i) = self.tcp_await_key.iter().position(|d| *d == vf.display) else {
            return true;
        };
        if super::host::has_key_frame(vf) {
            self.tcp_await_key.swap_remove(i);
            return true;
        }
        self.stats.tcp_drop += 1;
        false
    }

    fn log_window(&mut self, lane_id: u64, mut w: RecvWindow) {
        let l = &w.loss;
        let d = &w.drops;
        if self.live.is_some() {
            let s = std::mem::take(&mut self.stats);
            log::info!(
                "UDPVIDEO viewer peer={} lane={lane_id:016x} mode=live win_s={:.1} frames={} lost={} gated={} frag_miss={} dgrams={}/{} loss_pct={:.2} reorder={}/{} dup={} late={} bad={} drop={}/{}/{}/{} shown={} held={}/{} stale={} gated_app={} kf_req={} lost_fixed={} tcp_drop={} rp={} fix_ms={} owd_var_ms={} asm_ms={} rtt_ms={} pings={}/{} acks={} rx_err={} fec_rec={} fec_unrec={} fec_shards={} parity={}",
                self.peer,
                w.span.as_secs_f64(),
                l.frames_completed,
                l.frames_lost,
                l.frames_gated,
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
                s.delivered,
                s.held,
                s.held_max,
                s.stale,
                s.gated,
                s.kf_req,
                s.lost_fixed,
                s.tcp_drop,
                s.rp,
                { s.fix }.ms3(),
                w.transit.above_min().ms3(),
                w.assembly.ms3(),
                w.rtt.ms3(),
                w.pings,
                w.pongs,
                w.acks,
                w.recv_errors,
                l.fec_recovered,
                l.fec_unrecoverable,
                l.fec_shards,
                l.parity_received,
            );
            return;
        }
        let mut m = self.matcher.take_window();
        log::info!(
            "UDPVIDEO viewer peer={} lane={lane_id:016x} mode=shadow win_s={:.1} frames={} lost={} frag_miss={} dgrams={}/{} loss_pct={:.2} reorder={}/{} dup={} late={} bad={} drop={}/{}/{}/{} matched={} tcp_first={} unpaired={} ahead_net_ms={} ahead_app_ms={} owd_var_ms={} asm_ms={} rtt_ms={} pings={}/{} rx_err={} fec_rec={} fec_unrec={} fec_shards={} parity={}",
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
            l.fec_recovered,
            l.fec_unrecoverable,
            l.fec_shards,
            l.parity_received,
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
    use crate::udp_video::host::{
        latency_tests::{counting_hooks, video},
        Mode, Params, UdpVideoHost, VideoTap,
    };
    use base::message_proto::UdpVideoLastFrame;

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
    fn latency_check_offer_rules() {
        let local: SocketAddr = "192.168.1.9:50123".parse().unwrap();
        let a = check_offer(&offer(), local).unwrap();
        assert_eq!(a.local, "192.168.1.9".parse::<IpAddr>().unwrap());
        assert_eq!(a.host, "192.168.1.5:40000".parse().unwrap());
        assert_eq!((a.every, a.live, a.timeout, a.fec), (2, false, DEFAULT_TIMEOUT, 0));
        let mut fec_offer = offer();
        fec_offer.fec_percent = 20;
        assert_eq!(check_offer(&fec_offer, local).unwrap().fec, 20);
        fec_offer.fec_percent = 900;
        assert_eq!(check_offer(&fec_offer, local).unwrap().fec, fec::MAX_PERCENT);
        let mut live = offer();
        live.mode = udp_video_offer::Mode::ModeLive.into();
        // A live offer of lane v1 (the first M20 host, no EPOCH markers) is refused.
        assert!(check_offer(&live, local).is_err());
        live.version = LIVE_LANE_VERSION;
        live.timeout_ms = 400;
        let a = check_offer(&live, local).unwrap();
        assert_eq!((a.live, a.timeout), (true, Duration::from_millis(400)));
        live.timeout_ms = 50;
        assert_eq!(check_offer(&live, local).unwrap().timeout, DEFAULT_TIMEOUT);
        let bad = |f: &dyn Fn(&mut UdpVideoOffer)| {
            let mut o = offer();
            f(&mut o);
            check_offer(&o, local).is_err()
        };
        assert!(bad(&|o| o.version = 2));
        assert!(bad(&|o| o.mode = udp_video_offer::Mode::ModeOff.into()));
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
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("no event: {e:?}"),
            }
        }
    }

    fn params(mode: Mode) -> Params {
        Params::from_options(mode, "2", "", "", "")
    }

    /// Host and viewer ends over loopback (the LAN gate is bypassed: it refuses loopback):
    /// HELLO, HELLO_ACK, READY, then every 2nd frame arrives whole as the same message.
    #[test]
    fn loopback_handshake_and_shadow_frames() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let tap = Arc::new(VideoTap::default());
        let (mut host, offer) =
            UdpVideoHost::start(1, tap.clone(), lo, lo, params(Mode::Shadow), None).unwrap();
        let a = Accepted {
            local: lo,
            host: SocketAddr::new(lo, offer.port as u16),
            secret: offer.secret.as_ref().try_into().unwrap(),
            every: offer.shadow_every,
            live: false,
            timeout: DEFAULT_TIMEOUT,
            fec: offer.fec_percent,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_client, _) = UdpVideoClient::start(&a, offer.lane_id, tx).unwrap();
        match next_event(&mut rx) {
            ViewerEvent::HelloAck { hellos, .. } => assert!(hellos >= 1),
            _ => panic!("expected HELLO_ACK"),
        }
        assert!(host.on_ready(offer.lane_id.wrapping_add(1)).is_err());
        host.on_ready(offer.lane_id).unwrap();
        let sent: Vec<_> = (0..6).map(|pts| video(0, pts * 1000, pts == 0)).collect();
        for m in &sent {
            assert!(tap.route(m, Instant::now()).tcp);
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

    /// Live over loopback: frames go to UDP only (the first route carries the ACTIVE marker),
    /// arrive whole and in order with ids from 0, the viewer's ACKs settle the window so every
    /// frame is reported fetched, and shutting down leaves the in-band STOP with the last id.
    #[test]
    fn latency_loopback_live_frames_acks_and_stop() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let tap = Arc::new(VideoTap::default());
        let (hooks, fetched) = counting_hooks();
        let (mut host, offer) =
            UdpVideoHost::start(2, tap.clone(), lo, lo, params(Mode::Live), Some(hooks)).unwrap();
        assert_eq!(offer.mode.enum_value(), Ok(udp_video_offer::Mode::ModeLive));
        let a = Accepted {
            local: lo,
            host: SocketAddr::new(lo, offer.port as u16),
            secret: offer.secret.as_ref().try_into().unwrap(),
            every: 1,
            live: true,
            timeout: DEFAULT_TIMEOUT,
            fec: 0,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_client, _) = UdpVideoClient::start(&a, offer.lane_id, tx).unwrap();
        match next_event(&mut rx) {
            ViewerEvent::HelloAck { .. } => {}
            _ => panic!("expected HELLO_ACK"),
        }
        host.on_ready(offer.lane_id).unwrap();
        assert!(host.is_live_active());
        let sent: Vec<_> = (0..8).map(|pts| video(0, pts * 500, pts == 0)).collect();
        for (i, m) in sent.iter().enumerate() {
            let r = tap.route(m, Instant::now());
            assert!(!r.tcp);
            assert_eq!(r.before.len(), (i == 0) as usize);
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut got = Vec::new();
        while got.len() < sent.len() {
            match next_event(&mut rx) {
                ViewerEvent::Live(f) => {
                    assert_eq!(f.frame_id, got.len() as u32);
                    assert_eq!(f.key, f.frame_id == 0);
                    got.push(f.vf);
                }
                ViewerEvent::Lost { .. } => panic!("loss on loopback"),
                _ => {}
            }
        }
        for (m, vf) in sent.iter().zip(got) {
            let mut msg = Message::new();
            msg.set_video_frame(vf);
            assert_eq!(msg, **m);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while fetched.lock().unwrap().len() < sent.len() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(fetched.lock().unwrap().len(), sent.len());
        assert!(host.acks_seen() > 0);
        assert!(host.check().is_ok());
        assert!(host.qos_delay(3).is_some());
        let info = host.shutdown("test");
        assert!(info.in_band && info.was_live);
        let r = tap.route(&video(0, 9_000, true), Instant::now());
        assert!(r.tcp);
        let Some(message::Union::Misc(m)) = &r.before[0].union else {
            panic!("no marker");
        };
        let Some(base::message_proto::misc::Union::UdpVideoControl(c)) = &m.union else {
            panic!("no control");
        };
        assert_eq!(c.last_frames[0].frame_id, 7);
    }

    fn live_frame(display: u8, frame_id: u32, epoch: u8, key: bool) -> LiveFrame {
        let m = video(display as i32, frame_id as i64, key);
        let Some(message::Union::VideoFrame(vf)) = m.union.clone() else {
            unreachable!()
        };
        LiveFrame {
            vf,
            display,
            frame_id,
            epoch,
            key,
            recovery: false,
            bytes: 100,
        }
    }

    fn ready_ids(lane: &mut ViewerLane) -> Vec<i64> {
        std::iter::from_fn(|| lane.next_ready())
            .map(|vf| crate::client::latency::frame_pts(&vf).unwrap())
            .collect()
    }

    fn ctl(kind: udp_video_control::Kind, epoch: u32) -> UdpVideoControl {
        UdpVideoControl {
            kind: kind.into(),
            lane_id: 5,
            epoch,
            ..Default::default()
        }
    }

    /// The io_loop side of a live lane: hold until ACTIVE, epochs and SwitchDisplay, the gate
    /// after a loss, and the in-band STOP deciding which TCP frames may be decoded.
    #[test]
    fn latency_viewer_live_hold_epoch_gate_and_stop() {
        let mut lane = ViewerLane::default();
        lane.live = Some(LiveLane::new(5));
        // Before ACTIVE: held.
        lane.on_live_frame(live_frame(0, 0, 3, true));
        lane.on_live_frame(live_frame(0, 1, 3, false));
        assert!(ready_ids(&mut lane).is_empty());
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 3));
        assert_eq!(ready_ids(&mut lane), vec![0, 1]);
        // A frame of the next epoch waits for the SwitchDisplay; a stale one is dropped.
        lane.on_live_frame(live_frame(0, 2, 4, true));
        lane.on_live_frame(live_frame(0, 3, 2, false));
        assert!(ready_ids(&mut lane).is_empty());
        assert!(lane.live.as_ref().unwrap().stale_since.is_some());
        // A SwitchDisplay without an EPOCH marker (sent outside the video stream, which the
        // host's tap never counts) leaves the epoch alone.
        lane.on_switch_display();
        assert!(ready_ids(&mut lane).is_empty());
        assert_eq!(lane.live.as_ref().unwrap().epoch, 3);
        lane.on_control(&ctl(udp_video_control::Kind::KindEpoch, 4));
        assert!(ready_ids(&mut lane).is_empty());
        lane.on_switch_display();
        assert_eq!(ready_ids(&mut lane), vec![2]);
        assert!(lane.live.as_ref().unwrap().stale_since.is_none());
        // The marker is used once.
        lane.on_switch_display();
        assert_eq!(lane.live.as_ref().unwrap().epoch, 4);
        assert_eq!((lane.stats.stale, lane.stats.held), (1, 3));
        // A loss on display 0: P-frames are dropped until a key frame.
        lane.live.as_mut().unwrap().disp(0).awaiting = Some(Instant::now());
        lane.on_live_frame(live_frame(0, 5, 4, false));
        lane.on_live_frame(live_frame(0, 6, 4, true));
        lane.on_live_frame(live_frame(1, 0, 4, true));
        assert_eq!(ready_ids(&mut lane), vec![6, 0]);
        assert_eq!(lane.stats.gated, 1);
        // In-band STOP: display 0's last UDP frame (7) never came, display 1's did.
        let mut stop = ctl(udp_video_control::Kind::KindStop, 0);
        stop.in_band = true;
        stop.last_frames = vec![
            UdpVideoLastFrame {
                display: 0,
                frame_id: 7,
                ..Default::default()
            },
            UdpVideoLastFrame {
                display: 1,
                frame_id: 0,
                ..Default::default()
            },
        ];
        lane.on_control(&stop);
        assert!(lane.live.is_none());
        assert_eq!(lane.tcp_await_key, vec![0]);
        let tcp = |d: i32, key: bool| {
            let m = video(d, 1, key);
            let Some(message::Union::VideoFrame(vf)) = m.union.clone() else {
                unreachable!()
            };
            vf
        };
        assert!(lane.on_tcp_frame(&tcp(1, false)));
        assert!(!lane.on_tcp_frame(&tcp(0, false)));
        assert!(lane.on_tcp_frame(&tcp(0, true)));
        assert!(lane.on_tcp_frame(&tcp(0, false)));
        assert_eq!(lane.stats.tcp_drop, 1);
    }

    /// M22: a broken display resumes at a RECOVERY_POINT frame as at a key frame, and the
    /// window counts it and the time to repair.
    #[test]
    fn latency_viewer_live_recovery_point_resumes() {
        let mut lane = ViewerLane::default();
        lane.live = Some(LiveLane::new(5));
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 0));
        lane.on_live_frame(live_frame(0, 0, 0, true));
        lane.live.as_mut().unwrap().disp(0).awaiting = Some(Instant::now());
        lane.live.as_mut().unwrap().disp(0).last_req = Some(Instant::now());
        lane.on_live_frame(live_frame(0, 1, 0, false));
        let mut rp = live_frame(0, 2, 0, false);
        rp.recovery = true;
        lane.on_live_frame(rp);
        lane.on_live_frame(live_frame(0, 3, 0, false));
        assert_eq!(ready_ids(&mut lane), vec![0, 2, 3]);
        let d = lane.live.as_mut().unwrap().disp(0);
        assert!(d.awaiting.is_none() && d.last_req.is_none());
        assert_eq!((lane.stats.gated, lane.stats.rp), (1, 1));
        assert!(lane.stats.fix.summary().is_some());
        // A recovery point on a display that is not broken is just the next frame.
        let mut rp = live_frame(0, 4, 0, false);
        rp.recovery = true;
        lane.on_live_frame(rp);
        assert_eq!(ready_ids(&mut lane), vec![4]);
        assert_eq!(lane.stats.rp, 1);
    }

    /// The watchdog asks to stop a live lane whose display stays broken; frames that come
    /// after the request are not shown, and the in-band STOP then marks the display.
    #[test]
    fn latency_viewer_live_watchdog() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let a = Accepted {
            local: lo,
            host: SocketAddr::new(lo, 9),
            secret: [1u8; SECRET_LEN],
            every: 1,
            live: true,
            timeout: DEFAULT_TIMEOUT,
            fec: 0,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut lane = ViewerLane::default();
        let (client, _) = UdpVideoClient::start(&a, 5, tx).unwrap();
        lane.client = Some(client);
        lane.live = Some(LiveLane::new(5));
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 0));
        assert!(lane.tick().is_none());
        // A loss a key frame right behind it repaired asks nothing and breaks nothing.
        let lost = |from: u32, to: u32, repaired: bool| ViewerEvent::Lost {
            display: 0,
            from,
            to,
            repaired,
        };
        assert!(lane.on_event(lost(1, 1, true)).is_none());
        assert!(lane.live.as_mut().unwrap().disp(0).awaiting.is_none());
        assert_eq!(lane.stats.lost_fixed, 1);
        // A loss: one key frame request at once; a later loss while waiting asks nothing.
        let req = lane.on_event(lost(3, 4, false)).expect("request");
        let Some(message::Union::Misc(m)) = &req.union else {
            panic!("no misc")
        };
        let Some(base::message_proto::misc::Union::UdpVideoFeedback(f)) = &m.union else {
            panic!("no feedback")
        };
        assert_eq!((f.display, f.lost_from, f.lost_to), (0, 3, 4));
        assert!(lane.on_event(lost(6, 6, false)).is_none());
        assert!(lane.on_event(ViewerEvent::Tick).is_none());
        // No key frame within KF_RETRY: the next tick asks again, still for the first loss.
        let d = lane.live.as_mut().unwrap().disp(0);
        d.last_req = Some(Instant::now() - KF_RETRY);
        assert!(lane.on_event(ViewerEvent::Tick).is_some());
        assert!(lane.on_event(ViewerEvent::Tick).is_none());
        assert_eq!(lane.stats.kf_req, 2);
        // The per-frame feedback (M23) gets every range, repaired or not, and the counters.
        let counts = UdpCounts {
            datagrams_expected: 10,
            datagrams_received: 9,
            ..Default::default()
        };
        assert!(lane.on_event(ViewerEvent::Counts(counts)).is_none());
        let (c, ranges) = lane.take_feedback();
        assert_eq!(c, counts);
        assert_eq!(
            ranges
                .iter()
                .map(|r| (r.from, r.to, r.repaired))
                .collect::<Vec<_>>(),
            vec![(1, 1, true), (3, 4, false), (6, 6, false)]
        );
        assert!(lane.take_feedback().1.is_empty());
        // The key frame ends the wait; a new loss then asks at once.
        lane.on_live_frame(live_frame(0, 7, 0, true));
        assert_eq!(ready_ids(&mut lane), vec![7]);
        assert!(lane.on_event(lost(8, 8, false)).is_some());
        lane.live.as_mut().unwrap().disp(0).awaiting = Some(Instant::now() - WATCHDOG);
        let stop = lane.tick().expect("watchdog");
        let Some(message::Union::Misc(m)) = &stop.union else {
            panic!("no misc")
        };
        let Some(base::message_proto::misc::Union::UdpVideoControl(c)) = &m.union else {
            panic!("no control")
        };
        assert_eq!(
            c.kind.enum_value(),
            Ok(udp_video_control::Kind::KindRequestStop)
        );
        assert!(lane.client.is_none() && lane.live.as_ref().unwrap().closed);
        lane.on_live_frame(live_frame(0, 9, 0, true));
        assert!(ready_ids(&mut lane).is_empty());
        let mut stop = ctl(udp_video_control::Kind::KindStop, 0);
        stop.in_band = true;
        stop.last_frames = vec![UdpVideoLastFrame {
            display: 0,
            frame_id: 9,
            ..Default::default()
        }];
        lane.on_control(&stop);
        assert_eq!(lane.tcp_await_key, vec![0]);
    }

    fn live_lane_with_client() -> ViewerLane {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let a = Accepted {
            local: lo,
            host: SocketAddr::new(lo, 9),
            secret: [1u8; SECRET_LEN],
            every: 1,
            live: true,
            timeout: DEFAULT_TIMEOUT,
            fec: 0,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut lane = ViewerLane::default();
        let (client, _) = UdpVideoClient::start(&a, 5, tx).unwrap();
        lane.client = Some(client);
        lane.live = Some(LiveLane::new(5));
        lane
    }

    /// A full hold buffer drops its oldest frame as a loss and asks for a key frame for it.
    #[test]
    fn latency_viewer_live_hold_overflow_requests() {
        let mut lane = live_lane_with_client();
        for id in 0..HOLD_MAX as u32 {
            assert!(lane
                .on_event(ViewerEvent::Live(live_frame(0, id, 0, id == 0)))
                .is_none());
        }
        let req = lane
            .on_event(ViewerEvent::Live(live_frame(0, HOLD_MAX as u32, 0, false)))
            .expect("request for the dropped frame");
        let Some(message::Union::Misc(m)) = &req.union else {
            panic!("no misc")
        };
        let Some(base::message_proto::misc::Union::UdpVideoFeedback(f)) = &m.union else {
            panic!("no feedback")
        };
        assert_eq!((f.display, f.lost_from, f.lost_to), (0, 0, 0));
        // After ACTIVE the rest is gated until the key frame asked for.
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 0));
        assert!(ready_ids(&mut lane).is_empty());
        assert_eq!(lane.stats.gated, HOLD_MAX as u64);
    }

    /// Frames that arrive only of an ended epoch, or wait for a SwitchDisplay, for a second
    /// stop the lane.
    #[test]
    fn latency_viewer_live_stale_and_held_watchdog() {
        let mut lane = live_lane_with_client();
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 3));
        lane.on_live_frame(live_frame(0, 0, 2, true));
        assert!(lane.tick().is_none());
        lane.live.as_mut().unwrap().stale_since = Some(Instant::now() - WATCHDOG);
        assert!(lane.tick().is_some());
        assert!(lane.live.as_ref().unwrap().closed);

        let mut lane = live_lane_with_client();
        lane.on_control(&ctl(udp_video_control::Kind::KindActive, 3));
        lane.on_live_frame(live_frame(0, 0, 4, true));
        assert!(lane.tick().is_none());
        lane.live.as_mut().unwrap().held_since = Some(Instant::now() - WATCHDOG);
        assert!(lane.tick().is_some());
        assert!(lane.live.as_ref().unwrap().closed);
    }
}
