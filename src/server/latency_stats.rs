//! Host-side latency measurement of the capture loop and of each connection's select loop.
//! Measurement and logging only. What each number means: docs/LATENCY-METRICS.md.

use super::video_service::VideoSource;
use crate::client::latency::{format_summary, Samples, Summary, REPORT_INTERVAL};
use base::{
    config::keys,
    message_proto::{message, video_frame, Message},
};
use hbb_common::{config::Config, log};
use std::{
    cell::Cell,
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

// A video frame write longer than this is counted in `send_slow` (fixed, not the frame period).
const SLOW_SEND: Duration = Duration::from_millis(16);
// A select-loop branch body longer than this is counted in `busy_slow`.
const SLOW_BUSY: Duration = Duration::from_millis(16);
// A MouseEvent arrival gap longer than this is counted in `in_slow`.
const SLOW_INPUT_GAP: Duration = Duration::from_millis(16);
// Longer gaps between two MouseEvents are a pause of the user, not a stall: left out of `in_gap`.
const MAX_INPUT_GAP: Duration = Duration::from_secs(1);
// A capture restart whose first frame comes later than this is not reported as a `gap`: the
// session that asked for it is most likely gone, and the next one started the service anew.
const MAX_GAP: Duration = Duration::from_secs(60);
// At most one `LATENCY switch` line per display and source in this period.
const SWITCH_LOG_INTERVAL: Duration = Duration::from_secs(1);
const MAX_REASON_LEN: usize = 60;

// Video frame writes of the connections (connection.rs `rx_video`), per display, until that
// display's capture loop reports. Mirrors `LatencyStats::enabled`, so with logging off the
// per-frame cost is two clock reads (at the call site) plus one atomic load. Also gates the
// capture-restart (`gap`) bookkeeping.
static SEND_ENABLED: AtomicBool = AtomicBool::new(false);
lazy_static::lazy_static! {
    static ref SENDS: Mutex<HashMap<usize, Sends>> = Default::default();
    // A capture restart (or refresh request) per source and display, until the first frame
    // of the restarted capture loop is sent.
    static ref GAPS: Mutex<HashMap<(VideoSource, usize), Gap>> = Default::default();
    // When the last `LATENCY switch` line of a source and display was logged, and how many
    // were not logged since.
    static ref SWITCH_LOGS: Mutex<HashMap<(VideoSource, usize), (Option<Instant>, usize)>> = Default::default();
}

thread_local! {
    // The reason of the `bail!("SWITCH")` about to happen on this video service thread, set by
    // `note_switch` and taken by `on_restart`.
    static SWITCH_REASON: Cell<&'static str> = Cell::new("");
}

#[derive(Default)]
struct Sends {
    samples: Samples,
    slow: usize,
}

/// `stream.send` of one video frame on one connection. On TCP: protobuf serialization,
/// encryption and the write into the socket, including the wait for send-buffer space.
pub(super) fn on_video_send(msg: &Message, send: Duration) {
    if !SEND_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(message::Union::VideoFrame(vf)) = &msg.union {
        let mut sends = SENDS.lock().unwrap();
        let s = sends.entry(vf.display as usize).or_default();
        s.samples.push(send);
        if send > SLOW_SEND {
            s.slow += 1;
        }
    }
}

/// A capture restart, from the first request or `SWITCH` until the restarted loop sent a frame.
pub(super) struct Gap {
    since: Instant,
    reason: String,
    restarts: usize,
}

/// Names the reason of the `bail!("SWITCH")` that follows on this video service thread.
pub(super) fn note_switch(reason: &'static str) {
    SWITCH_REASON.with(|r| r.set(reason));
}

/// A viewer asked to refresh this display (`RefreshVideo`): the gap starts here, not when the
/// capture loop sees the request.
pub(super) fn on_refresh_request(source: VideoSource, display: usize) {
    if !SEND_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    GAPS.lock()
        .unwrap()
        .entry((source, display))
        .or_insert_with(|| Gap {
            since: Instant::now(),
            reason: String::new(),
            restarts: 0,
        });
}

/// The capture loop of this source and display returned an error and will be restarted.
pub(super) fn on_restart(source: VideoSource, display: usize, err: &hbb_common::anyhow::Error) {
    let noted = SWITCH_REASON.with(|r| r.replace(""));
    if !SEND_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let reason = if noted.is_empty() {
        // Not a SWITCH: the error text, as one token.
        err.to_string()
            .chars()
            .take(MAX_REASON_LEN)
            .map(|c| if c.is_whitespace() || c == '=' { '_' } else { c })
            .collect()
    } else {
        noted.to_owned()
    };
    let mut gaps = GAPS.lock().unwrap();
    let gap = gaps.entry((source, display)).or_insert_with(|| Gap {
        since: Instant::now(),
        reason: String::new(),
        restarts: 0,
    });
    if gap.reason.is_empty() {
        gap.reason = reason;
    }
    gap.restarts += 1;
}

fn take_gap(source: VideoSource, display: usize) -> Option<Gap> {
    GAPS.lock()
        .unwrap()
        .remove(&(source, display))
        .filter(|g| g.since.elapsed() <= MAX_GAP)
}

fn log_switch(source: VideoSource, display: usize, gap: &Gap, ms: u128) {
    let now = Instant::now();
    let mut logs = SWITCH_LOGS.lock().unwrap();
    let entry = logs.entry((source, display)).or_insert((None, 0));
    if let Some(last) = entry.0 {
        if now.saturating_duration_since(last) < SWITCH_LOG_INTERVAL {
            entry.1 += 1;
            return;
        }
    }
    log::info!(
        "LATENCY switch source={} display={} reason={} restarts={} gap_ms={} not_logged={}",
        source.service_name_prefix(),
        display,
        if gap.reason.is_empty() {
            "refresh"
        } else {
            &gap.reason
        },
        gap.restarts,
        ms,
        entry.1,
    );
    *entry = (Some(now), 0);
}

pub(super) fn encoded_frames(msg: &Message) -> Option<&[base::message_proto::EncodedVideoFrame]> {
    use video_frame::Union::*;
    match &msg.union {
        Some(message::Union::VideoFrame(vf)) => match vf.union.as_ref()? {
            Vp8s(f) | Vp9s(f) | Av1s(f) | H264s(f) | H265s(f) => Some(&f.frames),
            _ => None,
        },
        _ => None,
    }
}

pub(super) struct LatencyStats {
    display: usize,
    source: VideoSource,
    enabled: bool,
    window_start: Instant,
    sent: usize,
    skipped: usize,
    idle: usize,
    grab: Samples,
    encode: Samples,
    wait: Samples,
    sleep: Samples,
    // Deadline pacing: times the loop was more than one period late and started a new schedule.
    reanchor: usize,
    // DXGI frame age at acquire; no sample from other capturers.
    age: Samples,
    // Encoder output of the window: bytes of all encoded frames, the largest frame, key frames
    // and the largest key frame.
    bytes: u64,
    max_frame: usize,
    keys: usize,
    key_max: usize,
    // The bitrate the encoder was last set to, kbps.
    set_kbps: u32,
    // `set_quality` calls in the window, and how many of them were followed by a key frame.
    qchg: usize,
    qchg_key: usize,
    qchg_pending: bool,
    gap: Samples,
    // The restart this capture loop was started for, until its first frame is sent.
    gap_pending: Option<Gap>,
    // enable-refresh-keyframe: refreshes served by a forced key frame, how many of those key
    // frames were encoded from the kept last image (static screen), and refreshes merged into a
    // pending key frame or deferred to the minimum key interval.
    rkey: usize,
    rkept: usize,
    rmerge: usize,
    // Static refine: re-encodes of the kept last image on a static screen.
    refine: usize,
}

impl LatencyStats {
    pub(super) fn new(source: VideoSource, display: usize, kbps: u32) -> Self {
        let enabled = is_enabled();
        SEND_ENABLED.store(enabled, Ordering::Relaxed);
        SENDS.lock().unwrap().remove(&display);
        Self {
            display,
            source,
            enabled,
            window_start: Instant::now(),
            sent: 0,
            skipped: 0,
            idle: 0,
            grab: Default::default(),
            encode: Default::default(),
            wait: Default::default(),
            sleep: Default::default(),
            reanchor: 0,
            age: Default::default(),
            bytes: 0,
            max_frame: 0,
            keys: 0,
            key_max: 0,
            set_kbps: kbps,
            qchg: 0,
            qchg_key: 0,
            qchg_pending: false,
            gap: Default::default(),
            gap_pending: take_gap(source, display),
            rkey: 0,
            rkept: 0,
            rmerge: 0,
            refine: 0,
        }
    }

    /// `grab`: capturer call until it returned the frame.
    /// `encode`: from there until the encoded frame was queued to the connections.
    /// `age`: how old the frame was when the capturer got it (DXGI only).
    pub(super) fn on_frame(
        &mut self,
        grab: Duration,
        encode: Duration,
        sent: bool,
        age: Option<Duration>,
    ) {
        if !self.enabled {
            return;
        }
        self.grab.push(grab);
        self.encode.push(encode);
        if let Some(age) = age {
            self.age.push(age);
        }
        if sent {
            self.sent += 1;
        } else {
            self.skipped += 1;
        }
    }

    /// One encoded message, before it goes to the connections; repeat encodes included.
    pub(super) fn on_encoded(&mut self, msg: &Message) {
        if !self.enabled {
            return;
        }
        let Some(frames) = encoded_frames(msg) else {
            return;
        };
        let mut key = false;
        for f in frames {
            let n = f.data.len();
            self.bytes += n as u64;
            self.max_frame = self.max_frame.max(n);
            if f.key {
                key = true;
                self.keys += 1;
                self.key_max = self.key_max.max(n);
            }
        }
        if self.qchg_pending && !frames.is_empty() {
            self.qchg_pending = false;
            if key {
                self.qchg_key += 1;
            }
        }
    }

    /// A frame reached at least one connection: ends the restart gap this loop started with.
    #[inline]
    pub(super) fn on_sent(&mut self) {
        if let Some(gap) = self.gap_pending.take() {
            let d = gap.since.elapsed();
            self.gap.push(d);
            log_switch(self.source, self.display, &gap, d.as_millis());
        }
    }

    /// A forced key frame for a refresh reached at least one connection (`kept`: encoded from the
    /// kept last image). Ends the refresh's gap, logged like a restart with reason
    /// `refresh_keyframe` and `restarts=0`.
    pub(super) fn on_refresh_key_sent(&mut self, kept: bool) {
        if !self.enabled {
            return;
        }
        self.rkey += 1;
        if kept {
            self.rkept += 1;
        }
        if let Some(mut gap) = take_gap(self.source, self.display) {
            if gap.reason.is_empty() {
                gap.reason = "refresh_keyframe".to_owned();
            }
            let d = gap.since.elapsed();
            self.gap.push(d);
            log_switch(self.source, self.display, &gap, d.as_millis());
        }
    }

    /// A refresh merged into a pending key frame or deferred to the minimum key interval.
    pub(super) fn on_refresh_merged(&mut self) {
        if self.enabled {
            self.rmerge += 1;
        }
    }

    /// One static-refine re-encode of the kept last image.
    pub(super) fn on_refine(&mut self) {
        if self.enabled {
            self.refine += 1;
        }
    }

    /// The encoder's bitrate was changed by `set_quality` (QoS ratio change).
    pub(super) fn on_quality(&mut self, kbps: u32) {
        self.set_kbps = kbps;
        if self.enabled {
            self.qchg += 1;
            self.qchg_pending = true;
        }
    }

    /// The capturer had no new image within one frame period.
    pub(super) fn on_idle(&mut self) {
        if self.enabled {
            self.idle += 1;
        }
    }

    /// Time until every connection took the frame off its queue to start sending it.
    pub(super) fn on_wait(&mut self, sent: bool, wait: Duration) {
        if self.enabled && sent {
            self.wait.push(wait);
        }
    }

    pub(super) fn on_sleep(&mut self, sleep: Duration) {
        if self.enabled {
            self.sleep.push(sleep);
        }
    }

    /// Deadline pacing started a new schedule because the loop was more than one period late.
    pub(super) fn on_reanchor(&mut self) {
        if self.enabled {
            self.reanchor += 1;
        }
    }

    /// `deadline`: the loop runs on absolute deadlines (`enable-capture-deadline-pacing`).
    pub(super) fn tick(&mut self, spf: Duration, deadline: bool) {
        let elapsed = self.window_start.elapsed();
        if elapsed < REPORT_INTERVAL {
            return;
        }
        // Drained every window, whatever is logged, so the store stays bounded.
        let sends = SENDS.lock().unwrap().remove(&self.display).unwrap_or_default();
        if self.enabled && self.sent + self.skipped + self.idle > 0 {
            let secs = elapsed.as_secs_f64();
            log::info!(
                "LATENCY server display={} fps={:.1} spf={}ms grab={} encode={} wait={} send={} sleep={} sent={} skipped={} idle={} send_slow={} age={} kbps={:.0} set_kbps={} maxf={} key={} keymax={} qchg={} qchg_key={} gap={} pacing={} reanchor={} rkey={} rkept={} rmerge={} refine={}",
                self.display,
                self.sent as f64 / secs,
                spf.as_millis(),
                format_summary(self.grab.summary()),
                format_summary(self.encode.summary()),
                format_summary(self.wait.summary()),
                format_summary(sends.samples.summary()),
                format_summary(self.sleep.summary()),
                self.sent,
                self.skipped,
                self.idle,
                sends.slow,
                format_summary(self.age.summary()),
                self.bytes as f64 * 8.0 / 1000.0 / secs,
                self.set_kbps,
                self.max_frame,
                self.keys,
                self.key_max,
                self.qchg,
                self.qchg_key,
                format_summary(self.gap.summary()),
                if deadline { "deadline" } else { "relative" },
                self.reanchor,
                self.rkey,
                self.rkept,
                self.rmerge,
                self.refine,
            );
        }
        self.window_start = Instant::now();
        self.sent = 0;
        self.skipped = 0;
        self.idle = 0;
        self.grab.clear();
        self.encode.clear();
        self.wait.clear();
        self.sleep.clear();
        self.reanchor = 0;
        self.age.clear();
        self.bytes = 0;
        self.max_frame = 0;
        self.keys = 0;
        self.key_max = 0;
        self.qchg = 0;
        self.qchg_key = 0;
        self.gap.clear();
        self.rkey = 0;
        self.rkept = 0;
        self.rmerge = 0;
        self.refine = 0;
        self.enabled = is_enabled();
        SEND_ENABLED.store(self.enabled, Ordering::Relaxed);
    }
}

/// One connection's select loop (`Connection::start`): how long each branch body kept the loop
/// from polling the others, the arrival gaps of the viewer's MouseEvents at the reader, and this
/// connection's own video frame writes. One `LATENCY conn` line per 5 s.
pub(super) struct ConnStats {
    id: i32,
    via: &'static str,
    enabled: bool,
    window_start: Instant,
    // The branch running now and when it started; only set while measuring.
    branch: Option<(&'static str, Instant)>,
    busy: Samples,
    busy_max: Duration,
    busy_max_branch: &'static str,
    busy_slow: usize,
    last_input: Option<Instant>,
    in_gap: Samples,
    in_gap_max: Duration,
    in_slow: usize,
    inputs: usize,
    send: Samples,
    send_max: Duration,
    send_slow: usize,
    frames: usize,
}

impl ConnStats {
    pub(super) fn new(id: i32, via: &'static str) -> Self {
        Self {
            id,
            via,
            enabled: is_enabled(),
            window_start: Instant::now(),
            branch: None,
            busy: Default::default(),
            busy_max: Duration::ZERO,
            busy_max_branch: "",
            busy_slow: 0,
            last_input: None,
            in_gap: Default::default(),
            in_gap_max: Duration::ZERO,
            in_slow: 0,
            inputs: 0,
            send: Default::default(),
            send_max: Duration::ZERO,
            send_slow: 0,
            frames: 0,
        }
    }

    /// A branch of the select loop starts its body. With logging off: one `bool` check.
    #[inline]
    pub(super) fn begin(&mut self, branch: &'static str) {
        if self.enabled {
            self.branch = Some((branch, Instant::now()));
        }
    }

    /// The previous branch body ended (called at the top of the loop, so `continue` is covered).
    #[inline]
    pub(super) fn end(&mut self) {
        if let Some((branch, start)) = self.branch.take() {
            let d = start.elapsed();
            self.busy.push(d);
            if d > self.busy_max {
                self.busy_max = d;
                self.busy_max_branch = branch;
            }
            if d > SLOW_BUSY {
                self.busy_slow += 1;
            }
        }
    }

    /// A message parsed by the reader.
    #[inline]
    pub(super) fn on_message(&mut self, msg: &Message) {
        if !self.enabled {
            return;
        }
        if let Some(message::Union::MouseEvent(_)) = &msg.union {
            let now = Instant::now();
            self.inputs += 1;
            if let Some(last) = self.last_input.replace(now) {
                let gap = now.saturating_duration_since(last);
                if gap <= MAX_INPUT_GAP {
                    self.in_gap.push(gap);
                    self.in_gap_max = self.in_gap_max.max(gap);
                    if gap > SLOW_INPUT_GAP {
                        self.in_slow += 1;
                    }
                }
            }
        }
    }

    /// One video frame write of this connection (`stream.send`).
    #[inline]
    pub(super) fn on_video_send(&mut self, send: Duration) {
        if !self.enabled {
            return;
        }
        self.frames += 1;
        self.send.push(send);
        self.send_max = self.send_max.max(send);
        if send > SLOW_SEND {
            self.send_slow += 1;
        }
    }

    /// From the loop's 1 s timer: logs and resets every `REPORT_INTERVAL`.
    pub(super) fn tick(&mut self, log_it: bool) {
        let elapsed = self.window_start.elapsed();
        if elapsed < REPORT_INTERVAL {
            return;
        }
        if self.enabled && log_it && (self.frames > 0 || self.inputs > 0) {
            log::info!(
                "LATENCY conn id={} via={} busy={} busy_max_in={} busy_slow={} in_gap={} in_slow={} in_n={} send={} send_slow={} frames={}",
                self.id,
                self.via,
                format_summary(with_max(self.busy.summary(), self.busy_max)),
                if self.busy_max_branch.is_empty() {
                    "-"
                } else {
                    self.busy_max_branch
                },
                self.busy_slow,
                format_summary(with_max(self.in_gap.summary(), self.in_gap_max)),
                self.in_slow,
                self.inputs,
                format_summary(with_max(self.send.summary(), self.send_max)),
                self.send_slow,
                self.frames,
            );
        }
        self.window_start = Instant::now();
        self.busy.clear();
        self.busy_max = Duration::ZERO;
        self.busy_max_branch = "";
        self.busy_slow = 0;
        self.in_gap.clear();
        self.in_gap_max = Duration::ZERO;
        self.in_slow = 0;
        self.inputs = 0;
        self.send.clear();
        self.send_max = Duration::ZERO;
        self.send_slow = 0;
        self.frames = 0;
        self.enabled = is_enabled();
        if !self.enabled {
            self.branch = None;
            self.last_input = None;
        }
    }
}

// The samples keep the first `MAX_SAMPLES` of a window; the max is tracked over all of them.
fn with_max(s: Option<Summary>, max: Duration) -> Option<Summary> {
    s.map(|mut s| {
        s.max = s.max.max(max.as_micros().min(u32::MAX as u128) as u32);
        s
    })
}

fn is_enabled() -> bool {
    Config::get_option(keys::OPTION_ENABLE_LATENCY_LOG) == "Y"
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::{EncodedVideoFrame, EncodedVideoFrames, VideoFrame};

    fn h265(frames: &[(usize, bool)]) -> Message {
        let mut vf = VideoFrame::new();
        vf.set_h265s(EncodedVideoFrames {
            frames: frames
                .iter()
                .map(|&(n, key)| EncodedVideoFrame {
                    data: vec![0u8; n].into(),
                    key,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        let mut msg = Message::new();
        msg.set_video_frame(vf);
        msg
    }

    #[test]
    fn latency_encoder_output_counters() {
        let mut s = LatencyStats::new(VideoSource::Camera, 97, 3000);
        s.enabled = true;
        s.on_encoded(&h265(&[(100, false)]));
        s.on_quality(2000);
        s.on_encoded(&h265(&[(5000, true), (200, false)]));
        s.on_quality(1000);
        s.on_encoded(&h265(&[(300, false)]));
        assert_eq!(s.bytes, 5600);
        assert_eq!(s.max_frame, 5000);
        assert_eq!(s.keys, 1);
        assert_eq!(s.key_max, 5000);
        assert_eq!(s.set_kbps, 1000);
        assert_eq!((s.qchg, s.qchg_key), (2, 1));
    }

    #[test]
    fn latency_restart_gap_keeps_first_start_and_reason() {
        SEND_ENABLED.store(true, Ordering::Relaxed);
        let key = (VideoSource::Camera, 98);
        on_refresh_request(key.0, key.1);
        let since = GAPS.lock().unwrap()[&key].since;
        note_switch("refresh");
        on_restart(key.0, key.1, &hbb_common::anyhow::anyhow!("SWITCH"));
        on_restart(key.0, key.1, &hbb_common::anyhow::anyhow!("Desktop changed"));
        let gap = take_gap(key.0, key.1).unwrap();
        assert_eq!(gap.since, since);
        assert_eq!(gap.reason, "refresh");
        assert_eq!(gap.restarts, 2);
        assert!(take_gap(key.0, key.1).is_none());
        // Without a noted reason the error text is the reason, as one token.
        on_restart(key.0, key.1, &hbb_common::anyhow::anyhow!("Desktop changed"));
        assert_eq!(take_gap(key.0, key.1).unwrap().reason, "Desktop_changed");
        SEND_ENABLED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn latency_refresh_key_ends_gap() {
        SEND_ENABLED.store(true, Ordering::Relaxed);
        let key = (VideoSource::Camera, 96);
        let mut s = LatencyStats::new(key.0, key.1, 3000);
        s.enabled = true;
        on_refresh_request(key.0, key.1);
        s.on_refresh_merged();
        s.on_refresh_key_sent(true);
        assert!(take_gap(key.0, key.1).is_none());
        assert_eq!((s.rkey, s.rkept, s.rmerge), (1, 1, 1));
        assert_eq!(s.gap.summary().map(|_| ()), Some(()));
        // No refresh pending: counted, no gap sample.
        s.gap.clear();
        s.on_refresh_key_sent(false);
        assert_eq!((s.rkey, s.rkept), (2, 1));
        assert!(s.gap.summary().is_none());
        SEND_ENABLED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn latency_conn_busy_tracks_slowest_branch() {
        let mut c = ConnStats::new(1, "TCP-punch");
        c.enabled = true;
        c.begin("read");
        c.end();
        c.begin("video");
        std::thread::sleep(Duration::from_millis(20));
        c.end();
        c.end(); // no branch running: no sample
        assert_eq!(c.busy.summary().map(|_| ()), Some(()));
        assert_eq!(c.busy_max_branch, "video");
        assert_eq!(c.busy_slow, 1);
        c.enabled = false;
        c.begin("rx");
        assert!(c.branch.is_none());
    }
}
