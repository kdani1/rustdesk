//! Host side of the per-frame feedback (M23).
//!
//! - `SendLog`: each video frame's (display, pts), size and the time it was taken off the
//!   connection's video channel (TCP: right before the write; UDP: before the lane's queue and
//!   pacer). Shared with the split writer; one relaxed load per video item while off.
//! - `Estimator`: one per connection; turns each viewer batch into a `FeedbackInput`.
//!   - `q_owd`: one-way delay of each frame (viewer arrival minus host send, an unknown clock
//!     offset included) minus its lowest value over the last 10 s (1 s buckets, dropped by
//!     age): the queueing on the whole path (host write, network, relay, viewer io_loop). The
//!     batch value is the smallest over its frames, so a large frame's own transmission time
//!     does not count as queue (the fast ABR's `min(Q) over 100 ms` idea); `q_owd_max_ms`
//!     keeps the largest. TCP and the UDP lane have different fixed delays: when a frame's
//!     transport differs from the previous one the baseline and the trendline's previous frame
//!     start over, and `q_owd` stays None until the new baseline is `BASE_WARMUP` old.
//!   - GCC trendline (draft-ietf-rmcat-gcc-02, WebRTC's `TrendlineEstimator`): per frame the
//!     delay gradient (arrival delta minus send delta), accumulated and smoothed (0.9), slope
//!     over the last 20 frames, times min(frames, 60) times 4; the overuse detector with the
//!     adaptive threshold (start 12.5, k_up 0.0087, k_down 0.039, 6..600, overuse after more
//!     than 10 ms and two samples above it with a rising trend).
//!   - Jitter (RFC 3550 style, gain 1/16) of the same per-frame delay variation.
//!   - Delivered rate of the batch's frames, the viewer's arrival-to-present delay, its queue.
//!   - UDP: datagram and frame loss over the last 1 s and 3 s from the lane counters.
//! - `FeedbackSink`: the seam to a bitrate controller. This build feeds `LegacyQos` (a delay
//!   sample for video_qos); the fast ABR of M15/M16 implements it at the merge.
//! - `FecAdapt`: the FEC percentage of a live lane from the reported loss.
//!
//! The viewer's clock is never compared with the host's directly: only differences of the
//! per-frame delay, and its distance from a running minimum, are used.

use super::{us_since, VERSION};
use crate::udp_video::shadow::Samples;
use base::{
    config::keys,
    message_proto::{message, Message, VideoFeedback, VideoFrame},
};
use hbb_common::config::Config;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Frames remembered for matching (about 8 s at 60 fps on one display).
const SEND_LOG_CAP: usize = 512;
/// Batches taken per second at most; more (a broken or hostile viewer) are dropped and counted.
pub const MAX_BATCHES_PER_SEC: u32 = 40;
/// Frames of one batch looked at, at most.
const MAX_BATCH_FRAMES: usize = 128;
/// The lowest one-way delay is kept over this long, in 1 s buckets.
const BASE_BUCKET: Duration = Duration::from_secs(1);
const BASE_BUCKETS: usize = 10;
/// `q_owd` is None until the baseline has this much behind it (at the start and after a
/// transport switch).
const BASE_WARMUP: Duration = Duration::from_secs(1);
/// Each lane counter of one batch is taken up to this (a broken or hostile viewer).
const MAX_BATCH_COUNT: u64 = 1_000_000;
/// UDP counters kept for the loss rates.
const UDP_HISTORY: Duration = Duration::from_secs(3);
/// GCC trendline (WebRTC defaults).
const TREND_WINDOW: usize = 20;
const TREND_SMOOTHING: f64 = 0.9;
const TREND_GAIN: f64 = 4.0;
/// GCC overuse detector (WebRTC defaults).
const THRESHOLD_INIT: f64 = 12.5;
const THRESHOLD_RANGE: (f64, f64) = (6.0, 600.0);
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const OVERUSE_TIME_MS: f64 = 10.0;
/// GCC's loss controller: above this datagram loss the rate must come down.
pub const LOSS_HIGH: f64 = 0.10;
/// What `LegacyQos` adds to the delay sample on high loss (as the UDP lane's own penalty).
pub const LOSS_PENALTY_MS: u32 = crate::udp_video::host::LOSS_PENALTY_MS;
/// Lane fallback: frames lost after FEC over `FALLBACK_WINDOW`, with at least
/// `FALLBACK_MIN_FRAMES` frames in it.
pub const FALLBACK_FRAME_LOSS: f64 = 0.10;
pub const FALLBACK_WINDOW: Duration = Duration::from_secs(3);
pub const FALLBACK_MIN_FRAMES: u64 = 30;
/// Adaptive FEC: clamp(FEC_MIN, FEC_MAX, 2 x loss% + 5), lowered only after `FEC_LOWER_AFTER`.
pub const FEC_MIN: u32 = 10;
pub const FEC_MAX: u32 = 50;
pub const FEC_LOWER_AFTER: Duration = Duration::from_secs(2);

/// `enable-video-feedback=Y` on a Windows host.
pub fn enabled() -> bool {
    cfg!(windows) && Config::get_option(keys::OPTION_ENABLE_VIDEO_FEEDBACK) == "Y"
}

/// `video-feedback-qos=Y`.
pub fn qos_option() -> bool {
    Config::get_option(keys::OPTION_VIDEO_FEEDBACK_QOS) == "Y"
}

/// `udp-video-fec-adaptive=Y`.
pub fn fec_adaptive_option() -> bool {
    Config::get_option(keys::OPTION_UDP_VIDEO_FEC_ADAPTIVE) == "Y"
}

/// The version to advertise in `Features.video_feedback`.
pub fn advertised_version() -> u32 {
    if enabled() {
        VERSION
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug)]
struct Sent {
    display: i32,
    pts: i64,
    at: Instant,
    bytes: u32,
    udp: bool,
}

/// Send time and size of the connection's recent video frames.
#[derive(Default)]
pub struct SendLog {
    on: AtomicBool,
    ring: Mutex<VecDeque<Sent>>,
}

impl SendLog {
    pub fn enable(&self) {
        self.on.store(true, Ordering::Relaxed);
    }

    /// A video channel item is about to be written (`udp`: it went to the live lane instead).
    #[inline]
    pub fn record(&self, msg: &Message, at: Instant, udp: bool) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        if let Some(message::Union::VideoFrame(vf)) = &msg.union {
            self.record_frame(vf, at, udp);
        }
    }

    fn record_frame(&self, vf: &VideoFrame, at: Instant, udp: bool) {
        let Some((pts, bytes)) = super::frame_key(vf) else {
            return;
        };
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() >= SEND_LOG_CAP {
                ring.pop_front();
            }
            ring.push_back(Sent {
                display: vf.display,
                pts,
                at,
                bytes,
                udp,
            });
        }
    }

    fn find(&self, display: i32, pts: i64) -> Option<Sent> {
        let ring = self.ring.lock().ok()?;
        ring.iter()
            .rev()
            .find(|s| s.display == display && s.pts == pts)
            .copied()
    }
}

/// GCC's verdict on the delay trend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Normal,
    Overuse,
    Underuse,
}

/// What one viewer batch says, the input of a bitrate controller (the seam to the fast ABR,
/// docs/UDP-VIDEO.md "Per-frame feedback (M23)"). Times are host `Instant`s; delays in ms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FeedbackInput {
    /// When the host took the batch.
    pub at: Instant,
    /// Frames of the batch matched to the send log.
    pub frames: u32,
    /// Queueing one-way delay: the batch's smallest per-frame delay above the 10 s minimum.
    pub q_owd_ms: Option<f64>,
    /// The batch's largest one (includes a large frame's own transmission time).
    pub q_owd_max_ms: Option<f64>,
    /// GCC modified trend and the detector's threshold and verdict after the batch.
    pub trend: f64,
    pub threshold: f64,
    pub usage: Usage,
    /// Inter-arrival jitter, ms.
    pub jitter_ms: f64,
    /// Bytes of the batch's frames over the time they took to arrive (from the previous
    /// batch's last arrival), kbit/s; None for a span under 10 ms.
    pub recv_kbps: Option<f64>,
    /// The viewer's arrival-to-shown and arrival-to-decoded delays, largest of the batch.
    pub viewer_ms: Option<f64>,
    pub decode_ms: Option<f64>,
    /// Largest decoder queue length at arrival.
    pub queue_len: u32,
    /// Any of the batch's frames came over the UDP lane.
    pub udp: bool,
    /// UDP lane, last 1 s: datagrams lost per expected, and frames lost (after FEC) per frame.
    pub loss: Option<f64>,
    pub frame_loss: Option<f64>,
    /// UDP lane, this batch: frames FEC saved, frames lost in ranges not repaired at once.
    pub fec_recovered: u64,
    pub lost_frames: u64,
}

/// The receiving end of `FeedbackInput`. `LegacyQos` in this build; the fast ABR (M15/M16) at
/// the merge, next to its `on_tcp(TcpInput)`.
pub trait FeedbackSink {
    fn on_feedback(&mut self, input: &FeedbackInput);
}

/// The feedback as a `video_qos` delay sample (`video-feedback-qos=Y`): the TestDelay sample
/// (or the UDP lane's) becomes `max(sample, q_owd)`, plus `LOSS_PENALTY_MS` when datagram loss
/// went above `LOSS_HIGH` since the previous sample, unless the lane's sample already carries
/// its own loss penalty (one penalty per sample at most). `q_owd` is the largest batch value
/// since then.
#[derive(Default)]
pub struct LegacyQos {
    q_owd_ms: f64,
    loss_high: bool,
}

impl FeedbackSink for LegacyQos {
    fn on_feedback(&mut self, input: &FeedbackInput) {
        if let Some(q) = input.q_owd_ms {
            self.q_owd_ms = self.q_owd_ms.max(q);
        }
        if input.loss.map_or(false, |l| l > LOSS_HIGH) {
            self.loss_high = true;
        }
    }
}

impl LegacyQos {
    /// The delay sample for `user_network_delay`; resets what it used. `penalised`: the sample
    /// already has the lane's `LOSS_PENALTY_MS` in it.
    pub fn delay(&mut self, sample_ms: u32, penalised: bool) -> u32 {
        let q = std::mem::take(&mut self.q_owd_ms).round().min(u32::MAX as f64) as u32;
        let penalty = if std::mem::take(&mut self.loss_high) && !penalised {
            LOSS_PENALTY_MS
        } else {
            0
        };
        sample_ms.max(q).saturating_add(penalty)
    }
}

/// The FEC percentage of a live lane from the reported datagram loss.
pub struct FecAdapt {
    cur: u32,
    lower_since: Option<Instant>,
}

impl FecAdapt {
    pub fn new(start: u32) -> Self {
        Self {
            cur: start,
            lower_since: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.cur
    }

    /// The target for a loss fraction: clamp(10, 50, 2 x loss% + 5).
    pub fn target(loss: f64) -> u32 {
        let pct = 2.0 * loss.clamp(0.0, 1.0) * 100.0 + 5.0;
        (pct.round() as u32).clamp(FEC_MIN, FEC_MAX)
    }

    /// Some(new percentage) when it changes: up at once, down after `FEC_LOWER_AFTER` below.
    pub fn on_loss(&mut self, loss: f64, now: Instant) -> Option<u32> {
        let target = Self::target(loss);
        if target > self.cur {
            self.cur = target;
            self.lower_since = None;
            return Some(target);
        }
        if target == self.cur {
            self.lower_since = None;
            return None;
        }
        let since = *self.lower_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= FEC_LOWER_AFTER {
            self.cur = target;
            self.lower_since = None;
            return Some(target);
        }
        None
    }
}

struct Trendline {
    acc_ms: f64,
    smoothed_ms: f64,
    first_arrival_ms: Option<f64>,
    history: VecDeque<(f64, f64)>,
    deltas: u32,
    trend: f64,
}

impl Trendline {
    fn new() -> Self {
        Self {
            acc_ms: 0.0,
            smoothed_ms: 0.0,
            first_arrival_ms: None,
            history: VecDeque::with_capacity(TREND_WINDOW + 1),
            deltas: 0,
            trend: 0.0,
        }
    }

    /// One delay gradient; returns the modified trend.
    fn update(&mut self, delta_ms: f64, arrival_ms: f64) -> f64 {
        self.deltas = (self.deltas + 1).min(1000);
        self.acc_ms += delta_ms;
        self.smoothed_ms =
            TREND_SMOOTHING * self.smoothed_ms + (1.0 - TREND_SMOOTHING) * self.acc_ms;
        let first = *self.first_arrival_ms.get_or_insert(arrival_ms);
        self.history
            .push_back((arrival_ms - first, self.smoothed_ms));
        if self.history.len() > TREND_WINDOW {
            self.history.pop_front();
        }
        if self.history.len() == TREND_WINDOW {
            if let Some(slope) = linear_fit(&self.history) {
                self.trend = self.deltas.min(60) as f64 * slope * TREND_GAIN;
            }
        }
        self.trend
    }
}

fn linear_fit(points: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = points.len() as f64;
    if n < 2.0 {
        return None;
    }
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
    let (mx, my) = (sx / n, sy / n);
    let (num, den) = points.iter().fold((0.0, 0.0), |(num, den), (x, y)| {
        (num + (x - mx) * (y - my), den + (x - mx) * (x - mx))
    });
    (den > 0.0).then(|| num / den)
}

struct Detector {
    threshold: f64,
    time_over_ms: f64,
    over_count: u32,
    prev_trend: f64,
    last_update_ms: Option<f64>,
    state: Usage,
}

impl Detector {
    fn new() -> Self {
        Self {
            threshold: THRESHOLD_INIT,
            time_over_ms: -1.0,
            over_count: 0,
            prev_trend: 0.0,
            last_update_ms: None,
            state: Usage::Normal,
        }
    }

    fn detect(&mut self, trend: f64, send_delta_ms: f64, now_ms: f64) -> Usage {
        if trend > self.threshold {
            if self.time_over_ms < 0.0 {
                self.time_over_ms = send_delta_ms / 2.0;
            } else {
                self.time_over_ms += send_delta_ms;
            }
            self.over_count += 1;
            if self.time_over_ms > OVERUSE_TIME_MS
                && self.over_count > 1
                && trend >= self.prev_trend
            {
                self.time_over_ms = 0.0;
                self.over_count = 0;
                self.state = Usage::Overuse;
            }
        } else if trend < -self.threshold {
            self.time_over_ms = -1.0;
            self.over_count = 0;
            self.state = Usage::Underuse;
        } else {
            self.time_over_ms = -1.0;
            self.over_count = 0;
            self.state = Usage::Normal;
        }
        self.prev_trend = trend;
        self.adapt(trend, now_ms);
        self.state
    }

    fn adapt(&mut self, trend: f64, now_ms: f64) {
        let last = *self.last_update_ms.get_or_insert(now_ms);
        self.last_update_ms = Some(now_ms);
        // A spike far above the threshold does not drag it up (WebRTC's kMaxAdaptOffsetMs).
        if trend.abs() > self.threshold + 15.0 {
            return;
        }
        let k = if trend.abs() < self.threshold {
            K_DOWN
        } else {
            K_UP
        };
        let dt = (now_ms - last).clamp(0.0, 100.0);
        self.threshold += k * (trend.abs() - self.threshold) * dt;
        self.threshold = self.threshold.clamp(THRESHOLD_RANGE.0, THRESHOLD_RANGE.1);
    }
}

#[derive(Clone, Copy, Default)]
struct UdpCounts {
    expected: u64,
    received: u64,
    completed: u64,
    lost: u64,
}

impl UdpCounts {
    fn add(&mut self, c: &UdpCounts) {
        self.expected = self.expected.saturating_add(c.expected);
        self.received = self.received.saturating_add(c.received);
        self.completed = self.completed.saturating_add(c.completed);
        self.lost = self.lost.saturating_add(c.lost);
    }
}

#[derive(Default)]
struct Win {
    batches: u64,
    rate_limited: u64,
    frames: u64,
    unmatched: u64,
    udp_frames: u64,
    done: u64,
    dropped: u64,
    lost_ranges: u64,
    lost_frames: u64,
    fec_recovered: u64,
    fec_unrec: u64,
    overuse: u64,
    underuse: u64,
    trend_max: f64,
    q_owd: Samples,
    viewer: Samples,
    decode: Samples,
    queue_max: u32,
    udp: UdpCounts,
    /// Transport switches seen in the matched frames.
    switches: u64,
}

/// One connection's feedback state.
pub struct Estimator {
    base: Instant,
    second: Option<Instant>,
    in_second: u32,
    base_buckets: VecDeque<(Instant, i64)>,
    /// Send time of the first frame of the current baseline.
    base_from: Option<Instant>,
    /// Transport (UDP lane) of the previous matched frame.
    path_udp: Option<bool>,
    /// (send µs on the host clock, arrival µs on the viewer clock) of the previous frame.
    prev: Option<(i64, i64)>,
    trend: Trendline,
    detector: Detector,
    jitter_ms: f64,
    udp_hist: VecDeque<(Instant, UdpCounts)>,
    /// The live lane's own queue drops (running total) at each fallback check.
    qdrop_hist: VecDeque<(Instant, u64)>,
    last_input: Option<FeedbackInput>,
    win: Win,
    win_start: Instant,
}

impl Estimator {
    pub fn new(now: Instant) -> Self {
        Self {
            base: now,
            second: None,
            in_second: 0,
            base_buckets: VecDeque::new(),
            base_from: None,
            path_udp: None,
            prev: None,
            trend: Trendline::new(),
            detector: Detector::new(),
            jitter_ms: 0.0,
            udp_hist: VecDeque::new(),
            qdrop_hist: VecDeque::new(),
            last_input: None,
            win: Win::default(),
            win_start: now,
        }
    }

    /// Takes one viewer batch. None when it was rate-limited.
    pub fn on_batch(
        &mut self,
        fb: &VideoFeedback,
        log: &SendLog,
        now: Instant,
    ) -> Option<FeedbackInput> {
        match self.second {
            Some(s) if now.saturating_duration_since(s) < Duration::from_secs(1) => {
                if self.in_second >= MAX_BATCHES_PER_SEC {
                    self.win.rate_limited += 1;
                    return None;
                }
                self.in_second += 1;
            }
            _ => {
                self.second = Some(now);
                self.in_second = 1;
            }
        }
        self.win.batches += 1;
        self.win.dropped = self.win.dropped.saturating_add(fb.dropped as u64);
        let mut input = FeedbackInput {
            at: now,
            frames: 0,
            q_owd_ms: None,
            q_owd_max_ms: None,
            trend: self.trend.trend,
            threshold: self.detector.threshold,
            usage: self.detector.state,
            jitter_ms: self.jitter_ms,
            recv_kbps: None,
            viewer_ms: None,
            decode_ms: None,
            queue_len: 0,
            udp: false,
            loss: None,
            frame_loss: None,
            fec_recovered: 0,
            lost_frames: 0,
        };
        let span_from = self.prev.map(|(_, a)| a);
        let mut bytes = 0u64;
        let mut last_arrival = None;
        for f in fb.frames.iter().take(MAX_BATCH_FRAMES) {
            self.win.frames += 1;
            input.queue_len = input.queue_len.max(f.queue_len);
            self.win.queue_max = self.win.queue_max.max(f.queue_len);
            let Some(sent) = log.find(f.display, f.pts) else {
                self.win.unmatched += 1;
                continue;
            };
            let udp = sent.udp || f.udp;
            if udp {
                input.udp = true;
                self.win.udp_frames += 1;
            }
            if self.path_udp.map_or(false, |p| p != udp) {
                // Another transport, another fixed delay: a new baseline, and no gradient
                // across the switch.
                self.base_buckets.clear();
                self.base_from = None;
                self.prev = None;
                self.win.switches += 1;
            }
            self.path_udp = Some(udp);
            input.frames += 1;
            bytes += sent.bytes as u64;
            let send_us = us_since(self.base, sent.at) as i64;
            let arrival_us = f.arrival_us.min(i64::MAX as u64) as i64;
            last_arrival = Some(arrival_us);
            let owd = arrival_us.saturating_sub(send_us);
            let min = self.base_min(owd, sent.at);
            let from = *self.base_from.get_or_insert(sent.at);
            if sent.at.saturating_duration_since(from) >= BASE_WARMUP {
                let q_us = owd.saturating_sub(min);
                let q_ms = q_us as f64 / 1000.0;
                input.q_owd_ms = Some(input.q_owd_ms.map_or(q_ms, |q: f64| q.min(q_ms)));
                input.q_owd_max_ms =
                    Some(input.q_owd_max_ms.map_or(q_ms, |q: f64| q.max(q_ms)));
                self.win.q_owd.push(q_us);
            }
            if let Some((ps, pa)) = self.prev {
                if send_us > ps && arrival_us >= pa {
                    let send_delta_ms = (send_us - ps) as f64 / 1000.0;
                    let delta_ms = (arrival_us - pa) as f64 / 1000.0 - send_delta_ms;
                    self.jitter_ms += (delta_ms.abs() - self.jitter_ms) / 16.0;
                    let trend = self.trend.update(delta_ms, arrival_us as f64 / 1000.0);
                    self.win.trend_max = self.win.trend_max.max(trend);
                    let before = self.detector.state;
                    let usage = self.detector.detect(trend, send_delta_ms, send_us as f64 / 1000.0);
                    if usage != before {
                        match usage {
                            Usage::Overuse => self.win.overuse += 1,
                            Usage::Underuse => self.win.underuse += 1,
                            Usage::Normal => {}
                        }
                    }
                }
            }
            if self.prev.map_or(true, |(ps, _)| send_us > ps) {
                self.prev = Some((send_us, arrival_us));
            }
        }
        if let (Some(from), Some(to)) = (span_from, last_arrival) {
            let span_us = to.saturating_sub(from);
            if span_us >= 10_000 {
                input.recv_kbps = Some(bytes as f64 * 8.0 * 1000.0 / span_us as f64);
            }
        }
        for d in fb.done.iter().take(MAX_BATCH_FRAMES) {
            self.win.done += 1;
            let decode_ms = d.decode_us as f64 / 1000.0;
            input.decode_ms = Some(input.decode_ms.map_or(decode_ms, |v: f64| v.max(decode_ms)));
            self.win.decode.push(d.decode_us as i64);
            if d.present_us > 0 {
                let ms = d.present_us as f64 / 1000.0;
                input.viewer_ms = Some(input.viewer_ms.map_or(ms, |v: f64| v.max(ms)));
                self.win.viewer.push(d.present_us as i64);
            }
        }
        for l in fb.lost.iter().take(MAX_BATCH_FRAMES) {
            self.win.lost_ranges += 1;
            if !l.repaired {
                let n = l.to.wrapping_sub(l.from).wrapping_add(1).min(1 << 16) as u64;
                input.lost_frames += n;
                self.win.lost_frames += n;
            }
        }
        if let Some(u) = fb.udp.as_ref() {
            let clamp = |v: u64| v.min(MAX_BATCH_COUNT);
            let expected = clamp(u.datagrams_expected);
            let c = UdpCounts {
                expected,
                received: clamp(u.datagrams_received).min(expected),
                completed: clamp(u.frames_completed),
                lost: clamp(u.frames_lost),
            };
            input.fec_recovered = clamp(u.fec_recovered);
            let w = &mut self.win;
            w.fec_recovered = w.fec_recovered.saturating_add(input.fec_recovered);
            w.fec_unrec = w.fec_unrec.saturating_add(clamp(u.fec_unrecoverable));
            w.udp.add(&c);
            self.udp_hist.push_back((now, c));
        }
        while self
            .udp_hist
            .front()
            .map_or(false, |(t, _)| now.saturating_duration_since(*t) > UDP_HISTORY)
        {
            self.udp_hist.pop_front();
        }
        let last_1s = self.udp_sum(now, Duration::from_secs(1));
        if last_1s.expected > 0 {
            input.loss =
                Some(last_1s.expected.saturating_sub(last_1s.received) as f64 / last_1s.expected as f64);
        }
        let frames_1s = last_1s.completed.saturating_add(last_1s.lost);
        if frames_1s > 0 {
            input.frame_loss = Some(last_1s.lost as f64 / frames_1s as f64);
        }
        input.trend = self.trend.trend;
        input.threshold = self.detector.threshold;
        input.usage = self.detector.state;
        input.jitter_ms = self.jitter_ms;
        self.last_input = Some(input);
        Some(input)
    }

    /// The lowest one-way delay over the last `BASE_BUCKETS` seconds, with `owd` in it.
    /// Buckets go by age as well as by count: a static screen opens few of them.
    fn base_min(&mut self, owd: i64, at: Instant) -> i64 {
        match self.base_buckets.back_mut() {
            Some((start, min)) if at.saturating_duration_since(*start) < BASE_BUCKET => {
                *min = (*min).min(owd);
            }
            _ => {
                let keep = BASE_BUCKET * BASE_BUCKETS as u32;
                while self.base_buckets.len() >= BASE_BUCKETS
                    || self
                        .base_buckets
                        .front()
                        .map_or(false, |(start, _)| at.saturating_duration_since(*start) >= keep)
                {
                    self.base_buckets.pop_front();
                }
                self.base_buckets.push_back((at, owd));
            }
        }
        self.base_buckets
            .iter()
            .map(|(_, m)| *m)
            .min()
            .unwrap_or(owd)
    }

    fn udp_sum(&self, now: Instant, over: Duration) -> UdpCounts {
        let mut s = UdpCounts::default();
        for (_, c) in self
            .udp_hist
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= over)
        {
            s.add(c);
        }
        s
    }

    /// The live lane's fallback rule: Some(reason) when more than `FALLBACK_FRAME_LOSS` of the
    /// frames of the last `FALLBACK_WINDOW` (at least `FALLBACK_MIN_FRAMES`) were lost after
    /// FEC. `qdrop_total` is the lane's own queue drops so far (called about once a second):
    /// the viewer counts those frames as lost too, but they never reached the network, so the
    /// ones of the window are taken off.
    pub fn lane_verdict(&mut self, now: Instant, qdrop_total: u64) -> Option<String> {
        while self
            .qdrop_hist
            .front()
            .map_or(false, |(t, _)| now.saturating_duration_since(*t) > FALLBACK_WINDOW)
        {
            self.qdrop_hist.pop_front();
        }
        self.qdrop_hist.push_back((now, qdrop_total));
        let qdrop = self
            .qdrop_hist
            .front()
            .map_or(0, |(_, first)| qdrop_total.saturating_sub(*first));
        let s = self.udp_sum(now, FALLBACK_WINDOW);
        let frames = s.completed.saturating_add(s.lost);
        if frames < FALLBACK_MIN_FRAMES {
            return None;
        }
        let lost = s.lost.saturating_sub(qdrop);
        let loss = lost as f64 / frames as f64;
        (loss > FALLBACK_FRAME_LOSS).then(|| {
            format!(
                "feedback: {} of {} frames lost after FEC in {} s ({:.1}% > {:.0}%; {} host queue drops left out), datagram loss {:.1}%",
                lost,
                frames,
                FALLBACK_WINDOW.as_secs(),
                loss * 100.0,
                FALLBACK_FRAME_LOSS * 100.0,
                qdrop,
                if s.expected > 0 {
                    s.expected.saturating_sub(s.received) as f64 * 100.0 / s.expected as f64
                } else {
                    0.0
                }
            )
        })
    }

    /// Forget the UDP history (a lane ended or started): its losses say nothing about the next.
    pub fn reset_udp(&mut self) {
        self.udp_hist.clear();
        self.qdrop_hist.clear();
    }

    /// The `VFB host` log fields of the window since the last call, once it is `every` long
    /// and had a batch.
    pub fn take_window(&mut self, now: Instant, every: Duration) -> Option<String> {
        let span = now.saturating_duration_since(self.win_start);
        if span < every {
            return None;
        }
        self.win_start = now;
        let mut w = std::mem::take(&mut self.win);
        if w.batches == 0 && w.rate_limited == 0 {
            return None;
        }
        let last = self
            .last_input
            .as_ref()
            .map_or("-".to_owned(), |i| i.fields(now));
        let loss = if w.udp.expected > 0 {
            format!(
                "{:.2}",
                w.udp.expected.saturating_sub(w.udp.received) as f64 * 100.0
                    / w.udp.expected as f64
            )
        } else {
            "-".to_owned()
        };
        Some(format!(
            "secs={:.1} batches={} rl={} frames={} unmatched={} udp_frames={} done={} dropped={} q_owd={} trend_max={:.1} thr={:.1} usage={:?} over={} under={} jitter={:.1} viewer={} decode={} queue_max={} dgram_loss={} frames_lost={} lost_ranges={} fec_rec={} fec_unrec={} switches={} last=[{}]",
            span.as_secs_f64(),
            w.batches,
            w.rate_limited,
            w.frames,
            w.unmatched,
            w.udp_frames,
            w.done,
            w.dropped,
            w.q_owd.ms3(),
            w.trend_max,
            self.detector.threshold,
            self.detector.state,
            w.overuse,
            w.underuse,
            self.jitter_ms,
            w.viewer.ms3(),
            w.decode.ms3(),
            w.queue_max,
            loss,
            w.udp.lost,
            w.lost_ranges,
            w.fec_recovered,
            w.fec_unrec,
            w.switches,
            last,
        ))
    }
}

impl FeedbackInput {
    /// Every field, for the `last=[...]` part of the `VFB host` line.
    fn fields(&self, now: Instant) -> String {
        let opt = |v: Option<f64>, digits: usize| {
            v.map_or("-".to_owned(), |v| format!("{v:.digits$}"))
        };
        format!(
            "age_ms={} frames={} q_owd={}/{} trend={:.1} thr={:.1} usage={:?} jitter={:.1} kbps={} viewer={} decode={} queue={} udp={} loss={} frame_loss={} fec_rec={} lost={}",
            now.saturating_duration_since(self.at).as_millis(),
            self.frames,
            opt(self.q_owd_ms, 1),
            opt(self.q_owd_max_ms, 1),
            self.trend,
            self.threshold,
            self.usage,
            self.jitter_ms,
            opt(self.recv_kbps, 0),
            opt(self.viewer_ms, 1),
            opt(self.decode_ms, 1),
            self.queue_len,
            self.udp,
            opt(self.loss.map(|l| l * 100.0), 2),
            opt(self.frame_loss.map(|l| l * 100.0), 2),
            self.fec_recovered,
            self.lost_frames,
        )
    }
}

/// A connection's feedback: the estimator, what this build does with its output, and the
/// sinks a bitrate controller adds (the fast ABR at the merge).
pub struct ConnFeedback {
    estimator: Estimator,
    legacy: LegacyQos,
    fec: Option<FecAdapt>,
    /// Further consumers of every `FeedbackInput`, in order. Empty in this build.
    pub sinks: Vec<Box<dyn FeedbackSink + Send>>,
    qos: bool,
    fec_adaptive: bool,
}

impl ConnFeedback {
    pub fn new(now: Instant) -> Self {
        Self {
            estimator: Estimator::new(now),
            legacy: LegacyQos::default(),
            fec: None,
            sinks: Vec::new(),
            qos: qos_option(),
            fec_adaptive: fec_adaptive_option(),
        }
    }

    /// Every second: `video-feedback-qos` and `udp-video-fec-adaptive`.
    pub fn refresh_options(&mut self) {
        self.qos = qos_option();
        self.fec_adaptive = fec_adaptive_option();
    }

    /// One viewer batch: the estimate, given to `LegacyQos` (with `video-feedback-qos=Y`) and
    /// to every sink. None when rate-limited.
    pub fn on_batch(
        &mut self,
        fb: &VideoFeedback,
        log: &SendLog,
        now: Instant,
    ) -> Option<FeedbackInput> {
        let input = self.estimator.on_batch(fb, log, now)?;
        if self.qos {
            self.legacy.on_feedback(&input);
        }
        for s in self.sinks.iter_mut() {
            s.on_feedback(&input);
        }
        Some(input)
    }

    /// The `video_qos` delay sample: `sample_ms` itself unless `video-feedback-qos=Y`.
    /// `penalised`: the UDP lane already added its loss penalty to it.
    pub fn qos_delay(&mut self, sample_ms: u32, penalised: bool) -> u32 {
        if self.qos {
            self.legacy.delay(sample_ms, penalised)
        } else {
            sample_ms
        }
    }

    /// Adaptive FEC of a live lane that started with `configured` percent parity: Some(new
    /// percentage) when it should change.
    pub fn fec_for(&mut self, input: &FeedbackInput, configured: u32, now: Instant) -> Option<u32> {
        if !self.fec_adaptive || configured == 0 {
            return None;
        }
        let loss = input.loss?;
        self.fec
            .get_or_insert_with(|| FecAdapt::new(configured))
            .on_loss(loss, now)
    }

    /// A new UDP lane: its FEC starts from the configured value, its losses from zero.
    pub fn lane_started(&mut self) {
        self.fec = None;
        self.estimator.reset_udp();
    }

    /// With `video-feedback-qos=Y`: Some(reason) when the live lane should go back to TCP.
    /// `qdrop_total`: the lane's own queue drops so far.
    pub fn lane_verdict(&mut self, now: Instant, qdrop_total: u64) -> Option<String> {
        if !self.qos {
            return None;
        }
        self.estimator.lane_verdict(now, qdrop_total)
    }

    pub fn take_window(&mut self, now: Instant, every: Duration) -> Option<String> {
        self.estimator.take_window(now, every).map(|mut line| {
            if let Some(f) = &self.fec {
                line.push_str(&format!(" fec_pct={}", f.current()));
            }
            line.push_str(&format!(" qos={}", self.qos));
            line
        })
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use base::message_proto::{
        EncodedVideoFrame, EncodedVideoFrames, VideoFeedbackDone, VideoFeedbackFrame,
        VideoFeedbackUdp,
    };

    fn msg(display: i32, pts: i64, bytes: usize) -> Message {
        let mut vf = VideoFrame::new();
        vf.display = display;
        vf.set_vp9s(EncodedVideoFrames {
            frames: vec![EncodedVideoFrame {
                data: vec![0u8; bytes].into(),
                pts,
                ..Default::default()
            }],
            ..Default::default()
        });
        let mut m = Message::new();
        m.set_video_frame(vf);
        m
    }

    /// Frames every 16 ms; each arrives `owd(i)` ms after it was sent (viewer clock offset
    /// 1000 s). Returns the input of each batch of 3 frames.
    fn run(n: usize, owd: impl Fn(usize) -> f64) -> Vec<FeedbackInput> {
        let t0 = Instant::now();
        let log = SendLog::default();
        log.enable();
        let mut est = Estimator::new(t0);
        let mut out = Vec::new();
        let mut batch = VideoFeedback::new();
        for i in 0..n {
            let sent = t0 + Duration::from_millis(16 * i as u64);
            log.record(&msg(0, i as i64, 20_000), sent, false);
            let arrival_us =
                1_000_000_000 + (16_000.0 * i as f64 + owd(i) * 1000.0).round() as u64;
            batch.frames.push(VideoFeedbackFrame {
                display: 0,
                pts: i as i64,
                arrival_us,
                bytes: 20_000,
                ..Default::default()
            });
            if batch.frames.len() == 3 {
                let now = sent + Duration::from_millis(owd(i) as u64 + 1);
                out.extend(est.on_batch(&std::mem::take(&mut batch), &log, now));
            }
        }
        out
    }

    #[test]
    fn latency_feedback_q_owd_and_trend_see_a_growing_queue() {
        // 2 s steady at 5 ms, then the queue grows 2 ms per frame.
        let steady = 125;
        let inputs = run(steady + 60, |i| {
            if i < steady {
                5.0
            } else {
                5.0 + 2.0 * (i - steady) as f64
            }
        });
        let calm = &inputs[steady / 3 - 2];
        assert!(calm.q_owd_ms.unwrap() < 0.5, "{calm:?}");
        assert_eq!(calm.usage, Usage::Normal);
        let last = inputs.last().unwrap();
        assert!(last.q_owd_ms.unwrap() > 100.0, "{last:?}");
        assert!(last.trend > last.threshold, "{last:?}");
        assert!(
            inputs.iter().any(|i| i.usage == Usage::Overuse),
            "overuse never detected"
        );
        let kbps = calm.recv_kbps.unwrap();
        assert!((kbps - 10_000.0).abs() < 500.0, "{kbps}");
    }

    #[test]
    fn latency_feedback_jitter_without_queue_is_not_overuse() {
        // +-4 ms alternating jitter, no trend.
        let inputs = run(300, |i| if i % 2 == 0 { 5.0 } else { 9.0 });
        assert!(inputs.iter().all(|i| i.usage != Usage::Overuse));
        let last = inputs.last().unwrap();
        assert!(last.q_owd_ms.unwrap() < 0.5);
        assert!(last.q_owd_max_ms.unwrap() > 3.5);
        assert!(last.jitter_ms > 3.5, "{last:?}");
    }

    #[test]
    fn latency_feedback_unmatched_done_and_rate_limit() {
        let t0 = Instant::now();
        let log = SendLog::default();
        // Off: nothing recorded.
        log.record(&msg(0, 1, 10), t0, false);
        assert!(log.find(0, 1).is_none());
        log.enable();
        log.record(&msg(0, 1, 10), t0, true);
        let mut est = Estimator::new(t0);
        let mut fb = VideoFeedback::new();
        fb.frames.push(VideoFeedbackFrame {
            display: 0,
            pts: 1,
            arrival_us: 5,
            ..Default::default()
        });
        fb.frames.push(VideoFeedbackFrame {
            display: 1,
            pts: 1,
            arrival_us: 5,
            ..Default::default()
        });
        fb.done.push(VideoFeedbackDone {
            display: 0,
            pts: 1,
            decode_us: 3000,
            present_us: 8000,
            ..Default::default()
        });
        let i = est.on_batch(&fb, &log, t0).unwrap();
        assert_eq!(i.frames, 1);
        assert!(i.udp);
        assert_eq!((i.decode_ms, i.viewer_ms), (Some(3.0), Some(8.0)));
        for _ in 1..MAX_BATCHES_PER_SEC {
            assert!(est.on_batch(&fb, &log, t0).is_some());
        }
        assert!(est.on_batch(&fb, &log, t0).is_none());
        assert!(est
            .on_batch(&fb, &log, t0 + Duration::from_millis(1001))
            .is_some());
        let line = est.take_window(t0 + Duration::from_secs(5), Duration::from_secs(5));
        let line = line.unwrap();
        assert!(line.contains("rl=1"), "{line}");
        assert!(line.contains("unmatched="), "{line}");
    }

    #[test]
    fn latency_feedback_udp_loss_and_fallback() {
        let t0 = Instant::now();
        let log = SendLog::default();
        let mut est = Estimator::new(t0);
        let batch = |expected, received, completed, lost| {
            let mut fb = VideoFeedback::new();
            fb.udp = Some(VideoFeedbackUdp {
                datagrams_expected: expected,
                datagrams_received: received,
                frames_completed: completed,
                frames_lost: lost,
                ..Default::default()
            })
            .into();
            fb
        };
        let mut t = t0;
        let mut last = None;
        for _ in 0..20 {
            t += Duration::from_millis(50);
            last = est.on_batch(&batch(100, 85, 3, 0), &log, t);
        }
        let last = last.unwrap();
        assert!((last.loss.unwrap() - 0.15).abs() < 1e-9);
        assert_eq!(last.frame_loss, Some(0.0));
        assert!(est.lane_verdict(t, 0).is_none());
        let mut legacy = LegacyQos::default();
        legacy.on_feedback(&last);
        assert_eq!(legacy.delay(20, false), 20 + LOSS_PENALTY_MS);
        assert_eq!(legacy.delay(20, false), 20);
        // The lane's sample already carries its penalty: not a second one.
        legacy.on_feedback(&last);
        assert_eq!(legacy.delay(20 + LOSS_PENALTY_MS, true), 20 + LOSS_PENALTY_MS);
        // Frames lost after FEC: 1 in 3 for 2 s.
        for _ in 0..40 {
            t += Duration::from_millis(50);
            est.on_batch(&batch(100, 60, 2, 1), &log, t);
        }
        // Host queue drops are not network loss: 30 of the 40 lost frames were the tap's.
        let _ = est.lane_verdict(t - Duration::from_secs(2), 0);
        assert!(est.lane_verdict(t, 30).is_none());
        // 1.5 s later those drops left the window; the loss is the network's.
        let later = t + Duration::from_millis(1500);
        let why = est.lane_verdict(later, 30).unwrap();
        assert!(why.contains("lost after FEC"), "{why}");
        assert!(why.contains("0 host queue drops"), "{why}");
        est.reset_udp();
        assert!(est.lane_verdict(later, 30).is_none());
    }

    #[test]
    fn latency_feedback_legacy_delay_takes_q_owd() {
        let inputs = run(200, |i| if i < 150 { 5.0 } else { 5.0 + 3.0 * (i - 150) as f64 });
        let mut legacy = LegacyQos::default();
        for i in &inputs {
            legacy.on_feedback(i);
        }
        let d = legacy.delay(30, false);
        assert!(d > 100, "{d}");
        assert_eq!(legacy.delay(30, false), 30);
    }

    #[test]
    fn latency_feedback_fec_adapt() {
        let t0 = Instant::now();
        assert_eq!(FecAdapt::target(0.0), FEC_MIN);
        assert_eq!(FecAdapt::target(0.05), 15);
        assert_eq!(FecAdapt::target(0.5), FEC_MAX);
        let mut f = FecAdapt::new(20);
        assert_eq!(f.on_loss(0.10, t0), Some(25));
        assert_eq!(f.on_loss(0.10, t0), None);
        // Lower only after 2 s below.
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(100)), None);
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(1500)), None);
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(2200)), Some(FEC_MIN));
        assert_eq!(f.current(), FEC_MIN);
        // A brief dip back to the current level restarts the wait.
        assert_eq!(f.on_loss(0.10, t0 + Duration::from_secs(3)), Some(25));
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_secs(4)), None);
        assert_eq!(f.on_loss(0.10, t0 + Duration::from_millis(4500)), None);
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(5000)), None);
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(6500)), None);
        assert_eq!(f.on_loss(0.0, t0 + Duration::from_millis(7100)), Some(FEC_MIN));
    }
    /// Frames every 16 ms over TCP, then (from frame `switch`) over the UDP lane with a
    /// one-way delay `udp_ms` lower; batches of 3. Returns (frame index, input) per batch.
    fn run_switch(n: usize, switch: usize, udp_ms: f64) -> Vec<(usize, FeedbackInput)> {
        let t0 = Instant::now();
        let log = SendLog::default();
        log.enable();
        let mut est = Estimator::new(t0);
        let mut out = Vec::new();
        let mut batch = VideoFeedback::new();
        for i in 0..n {
            let udp = i >= switch;
            let sent = t0 + Duration::from_millis(16 * i as u64);
            log.record(&msg(0, i as i64, 20_000), sent, udp);
            let owd = if udp { 25.0 - udp_ms } else { 25.0 };
            let arrival_us = 1_000_000_000 + (16_000.0 * i as f64 + owd * 1000.0) as u64;
            batch.frames.push(VideoFeedbackFrame {
                display: 0,
                pts: i as i64,
                arrival_us,
                udp,
                ..Default::default()
            });
            if batch.frames.len() == 3 {
                let now = sent + Duration::from_millis(30);
                if let Some(input) = est.on_batch(&std::mem::take(&mut batch), &log, now) {
                    out.push((i, input));
                }
            }
        }
        out
    }

    #[test]
    fn latency_feedback_transport_switch_restarts_the_baseline() {
        // UDP 20 ms faster than TCP: no phantom queue after the switch back either way.
        let inputs = run_switch(400, 150, 20.0);
        for (i, input) in &inputs {
            if let Some(q) = input.q_owd_ms {
                assert!(q < 0.5, "frame {i}: {input:?}");
            }
            assert_ne!(input.usage, Usage::Overuse, "frame {i}: {input:?}");
        }
        // q_owd is None for about 1 s after the switch, then back.
        let after: Vec<_> = inputs.iter().filter(|(i, _)| *i >= 150).collect();
        assert!(after[0].1.q_owd_ms.is_none());
        assert!(after.last().unwrap().1.q_owd_ms.is_some());
        // UDP 20 ms slower (pacer, hold): the same.
        for (i, input) in run_switch(400, 150, -20.0) {
            assert!(input.q_owd_ms.map_or(true, |q| q < 0.5), "frame {i}: {input:?}");
        }
    }

    #[test]
    fn latency_feedback_base_buckets_expire_by_age() {
        let t0 = Instant::now();
        let mut est = Estimator::new(t0);
        // One frame every 5 s at 10 ms, then the path gets 10 ms longer.
        for k in 0..4u64 {
            assert_eq!(est.base_min(10_000, t0 + Duration::from_secs(5 * k)), 10_000);
        }
        // 15 s after the last one only buckets of the last 10 s count.
        let at = t0 + Duration::from_secs(30);
        assert_eq!(est.base_min(20_000, at), 20_000);
        assert!(est.base_buckets.len() <= 1, "{}", est.base_buckets.len());
        // The count cap still holds with frames every second.
        for k in 0..30u64 {
            est.base_min(5_000, at + Duration::from_secs(k));
        }
        assert_eq!(est.base_buckets.len(), BASE_BUCKETS);
    }

    #[test]
    fn latency_feedback_hostile_counters_do_not_overflow() {
        let t0 = Instant::now();
        let log = SendLog::default();
        log.enable();
        let mut est = Estimator::new(t0);
        let mut fb = VideoFeedback::new();
        fb.udp = Some(VideoFeedbackUdp {
            datagrams_expected: u64::MAX,
            datagrams_received: u64::MAX,
            frames_completed: u64::MAX,
            frames_lost: u64::MAX,
            fec_recovered: u64::MAX,
            fec_unrecoverable: u64::MAX,
            ..Default::default()
        })
        .into();
        fb.dropped = u32::MAX;
        for k in 0..3u64 {
            let at = t0 + Duration::from_secs(k);
            log.record(&msg(0, k as i64, 10), at, true);
            fb.frames = vec![VideoFeedbackFrame {
                display: 0,
                pts: k as i64,
                arrival_us: if k == 1 { 0 } else { u64::MAX },
                ..Default::default()
            }];
            est.on_batch(&fb, &log, t0 + Duration::from_secs(2 * k)).unwrap();
        }
        let _ = est.lane_verdict(t0 + Duration::from_secs(4), u64::MAX);
        assert!(est
            .take_window(t0 + Duration::from_secs(10), Duration::from_secs(5))
            .is_some());
    }
}
