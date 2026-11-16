//! Fast delay-based congestion estimator, shadow mode (roadmap M15).
//!
//! One estimator per authorized remote-control or camera connection, fed per video frame write
//! (the connection's video branch, or the split session's writer task), by the TCP meter of the
//! session socket when there is one (M11, `enable-tcp-meter`), and by the capture loop when it
//! waits too long for this connection to take a frame (`on_send_stall`). Every 50 ms (driven by
//! those events, plus the connection's 1 s timer) it decides what a fast controller would do to
//! the shared encoder's bitrate: cut, cut hard, cut after an RTO, raise, or hold. In shadow mode
//! the decision is only logged; nothing the host sends changes.
//!
//! Units: the estimator works in measured kbps only (bytes the session wrote or TCP delivered).
//! `k`, an EWMA of measured / configured bitrate over full-rate seconds that are not
//! content-limited, turns a measured target into the configured bitrate it would set
//! (`would_set_kbps`), so a 30-fps nvenc at 60 fps or a virtual framerate (M7) do not skew the
//! thresholds.
//!
//! What each log field means: docs/LATENCY-METRICS.md, "Host: fast bitrate estimator (shadow)".

use base::{config::keys, message_proto::Message};
use hbb_common::{config::Config, log};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------------------------
// Constants. Times in microseconds.

/// Decision grid: at most one evaluation per tick, except a capture-loop stall report.
pub const TICK_US: u64 = 50_000;
/// Send and delivery rates are measured over this window.
const RATE_WINDOW_US: u64 = 500_000;
/// OVERUSE needs the queue above `T_hi` all through this window (minimum of the step function).
const OVERUSE_WINDOW_US: u64 = 100_000;
/// NORMAL needs the p90 of the queue below `T_lo` over this window.
const NORMAL_WINDOW_US: u64 = 500_000;
/// Window of the writer busy share (unmetered growth test).
const BUSY_WINDOW_US: u64 = 250_000;
const BUSY_SHORTFALL: f64 = 0.8;
const BUSY_MIN_WRITES: usize = 3;
/// The adaptive threshold looks at the queue seen in NORMAL over this window.
const THRESHOLD_WINDOW_US: u64 = 10_000_000;
const THRESHOLD_EVERY_US: u64 = 1_000_000;
const THRESHOLD_MIN_SAMPLES: usize = 20;
const T_HI_INIT_MS: f64 = 60.0;
const T_HI_MIN_MS: f64 = 30.0;
const T_HI_MAX_MS: f64 = 150.0;
const T_HI_GAIN: f64 = 2.5;
/// Windowed minimum of the frame delay: two buckets of this length.
const BASE_BUCKET_US: u64 = 5_000_000;
/// A frame's queue value stands for this long after its write ended.
const FRAME_Q_HOLD_US: u64 = 200_000;
/// A meter reading older than this is not used for the queue.
const TCP_FRESH_US: u64 = 300_000;
/// SEVERE: the capture loop waited this long for this connection to take a frame. At most once
/// per stall episode: the reports until this connection's next write ends are one episode.
const SEVERE_STALL_US: u64 = 200_000;
/// After a cut no other cut for max(this, 2 x srtt): one queue is punished once.
const HOLD_MIN_US: u64 = 250_000;
/// Increases need this long in NORMAL, at most one per spacing.
const INCREASE_AFTER_US: u64 = 1_000_000;
const INCREASE_SPACING_US: u64 = 500_000;
/// Below 0.9 x the learned ceiling: x1.08 per second (GCC); near it: +max(150 kbps, 2 %) per s.
const MULT_INCREASE_PER_S: f64 = 1.08;
const ADD_INCREASE_KBPS_PER_S: f64 = 150.0;
const ADD_INCREASE_SHARE_PER_S: f64 = 0.02;
/// OVERUSE within this long of reaching the ceiling lowers the ceiling by 10 %.
const CEILING_RECUR_US: u64 = 10_000_000;
const CEILING_EWMA: f64 = 0.3;
const CEILING_FORGET: f64 = 1.15;
/// After an RTO: judged once on the meter's delivery and send from the reading that showed it
/// on, over at least `RTO_JUDGE_US` and outside a pause; cut only if delivery stayed below
/// 0.85 x send. Not judged within `RTO_WATCH_US` (a pause longer than that is SEVERE's): ignored.
const RTO_JUDGE_US: u64 = 300_000;
const RTO_WATCH_US: u64 = 1_000_000;
const RTO_SHORTFALL: f64 = 0.85;
/// An RTO seen this soon after a cut belongs to the episode that cut: not judged again.
const RTO_AFTER_CUT_US: u64 = 500_000;
/// A growing queue: delivery below 0.95 x send (metered, last 100 ms).
const DELIVERY_SHORTFALL: f64 = 0.95;
/// A queue above `T_hi` for this long is standing, not a transient: OVERUSE without growth.
const STANDING_WINDOW_US: u64 = 500_000;
/// No delivery for this long is a pause of the link (Wi-Fi airtime), not a capacity shortfall:
/// it never counts as OVERUSE (a long one still is SEVERE through the capture loop's stall).
const PAUSE_WINDOW_US: u64 = 100_000;
const PAUSE_BYTES: u64 = 1500;
/// GCC's decrease: 0.85 x the delivered rate, at most a 10 % step, never below half.
const BETA: f64 = 0.85;
const OVERUSE_STEP: f64 = 0.9;
/// A new target that differs by less than this is no decision.
const MIN_CHANGE: f64 = 0.03;
const K_MIN: f64 = 0.2;
const K_MAX: f64 = 5.0;
const K_EWMA: f64 = 0.2;
/// A sample above `k` comes in with this weight (the encoder reaching its budget again).
const K_EWMA_UP: f64 = 0.5;
/// A full-rate second under this share of `k` is content-limited (a static or simple screen
/// undershoots the budget): not learned from, `k` only drifts down by `K_DECAY` per such second
/// (so a real change of the coupling is still followed, slowly).
const K_CONTENT_LIMITED: f64 = 0.8;
const K_DECAY: f64 = 0.99;
/// Without a full-rate second to learn `k` from, the target starts after this long anyway.
const INIT_AFTER_US: u64 = 3_000_000;
const MAX_FRAMES: usize = 512;
const MAX_TCP: usize = 256;
const MAX_QS: usize = 2048;
const FRAME_KEEP_US: u64 = 2_000_000;
const TCP_KEEP_US: u64 = 2_000_000;
/// Frames larger than this many times the average have their excess serialization discounted.
const LARGE_FRAME: f64 = 2.0;

// ---------------------------------------------------------------------------------------------
// The estimator: pure, clock passed in, so recorded traces replay deterministically.

/// What the shared encoder looks like, read once per second (`VideoQoS::fast_context`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Context {
    /// The bitrate the encoder was last set to, kbps.
    pub configured_kbps: u32,
    pub fps: u32,
    /// The lowest and highest bitrate QoS may set (`min_ratio`, the latest quality), kbps.
    pub floor_kbps: u32,
    pub ceil_kbps: u32,
    /// The latest quality's ratio: a change is the user picking another quality.
    pub quality: f32,
    /// ABR on and every display's encoder can change its bitrate.
    pub vbr: bool,
}

/// One TCP meter reading, cumulative.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpInput {
    pub at_us: u64,
    pub app_bytes: u64,
    pub unique_out: u64,
    pub in_flight: u32,
    pub rtt_us: u32,
    pub min_rtt_us: u32,
    pub timeouts: u32,
    pub lim_rwin_ms: Option<u32>,
}

impl TcpInput {
    fn acked(&self) -> u64 {
        self.unique_out.saturating_sub(self.in_flight as u64)
    }

    fn unsent(&self) -> i64 {
        self.app_bytes as i64 - self.unique_out as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Normal,
    /// Between the thresholds: no change.
    Hold,
    Overuse,
    Severe,
    /// The viewer does not read (receive window), not the network: no cut.
    Receiver,
}

impl State {
    fn letter(self) -> char {
        match self {
            State::Normal => 'N',
            State::Hold => 'H',
            State::Overuse => 'O',
            State::Severe => 'S',
            State::Receiver => 'R',
        }
    }

    fn index(self) -> usize {
        match self {
            State::Normal => 0,
            State::Hold => 1,
            State::Overuse => 2,
            State::Severe => 3,
            State::Receiver => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Cut,
    Severe,
    Rto,
    Increase,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Cut => "cut",
            Kind::Severe => "severe",
            Kind::Rto => "rto",
            Kind::Increase => "inc",
        }
    }
}

/// A bitrate the estimator would set.
#[derive(Clone, Copy, Debug)]
pub struct Decision {
    pub kind: Kind,
    #[cfg_attr(not(test), allow(dead_code))]
    pub at_us: u64,
    /// Measured kbps.
    pub from_kbps: f64,
    pub to_kbps: f64,
    /// `to_kbps / k`: the configured bitrate it would set.
    pub set_kbps: f64,
    pub q_ms: f64,
    pub t_hi_ms: f64,
    pub r_send: f64,
    pub r_acked: f64,
    /// From the queue first exceeding `T_hi` (or the stalled write's start + `T_hi`) to here.
    pub react_ms: f64,
    /// Metered only: delivery was at least 0.95 x send at the cut (roadmap's false-cut test).
    pub false_cut: Option<bool>,
    pub metered: bool,
}

/// Counters of one report window.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub evals: u32,
    /// Evaluations per state: N H O S R.
    pub states: [u32; 5],
    pub cuts: u32,
    pub severe: u32,
    pub rto_cuts: u32,
    pub rto_ignored: u32,
    pub incs: u32,
    /// Cuts not made because a HOLD from the previous one was running.
    pub held: u32,
    /// Cuts not made because the target already was at the floor.
    pub floor: u32,
    pub false_cuts: u32,
    pub stalls: u32,
    /// Full-rate seconds whose `k` sample was content-limited (not learned from).
    pub k_rejected: u32,
    pub react_max_ms: f64,
}

#[derive(Clone, Copy)]
struct FrameRec {
    enq: u64,
    begin: u64,
    end: u64,
    bytes: u32,
    display: i32,
    q_ms: f32,
}

#[derive(Clone, Copy)]
struct Pending {
    enq: u64,
    begin: u64,
    bytes: u32,
}

#[derive(Clone, Copy, Default)]
struct Rates {
    r_send: f64,
    r_acked: f64,
    // The queue grows now. Metered: TCP delivered under 0.95 x what was written over the last
    // 100 ms. Unmetered: the writer was busy most of the last 250 ms, or the frame delays rose
    // across the last 100 ms.
    growing: bool,
    // Nothing was delivered in the last 100 ms (metered), or one write has been blocked all
    // through it (unmetered).
    paused: bool,
    metered: bool,
}

pub struct FastEstimator {
    ack: bool,
    first_us: Option<u64>,
    ctx: Option<Context>,
    k: f64,
    k_samples: u32,
    target: Option<f64>,
    ceiling_c: Option<f64>,
    c_reached_at: Option<u64>,
    frames: VecDeque<FrameRec>,
    tcp: VecDeque<TcpInput>,
    // Queue samples (time, ms, taken while NORMAL).
    qs: VecDeque<(u64, f32, bool)>,
    pending: Option<Pending>,
    base_bucket_at: u64,
    base_cur: Option<u64>,
    base_prev: Option<u64>,
    avg_bytes: f64,
    peak_acked: f64,
    peak_at: u64,
    state: State,
    normal_since: Option<u64>,
    hold_until: u64,
    last_inc: u64,
    last_eval: Option<u64>,
    onset: Option<u64>,
    t_hi: f64,
    t_hi_at: u64,
    timeouts_seen: Option<u32>,
    // The time and the meter reading at which a new RTO was seen.
    rto_watch: Option<(u64, TcpInput)>,
    last_cut_at: Option<u64>,
    // A stall report was SEVERE and this connection's write has not ended since.
    severe_episode: bool,
    app_limited: bool,
    displays_seen: usize,
    r_send_1s: f64,
    last_q: f64,
    last_rates: Rates,
    pub counters: Counters,
}

impl FastEstimator {
    /// `ack`: the viewer acknowledges frames (`video_ack_required`), so the capture loop's wait
    /// for this connection includes the network round trip; its stalls never cut.
    pub fn new(ack: bool) -> Self {
        Self {
            ack,
            first_us: None,
            ctx: None,
            k: 1.0,
            k_samples: 0,
            target: None,
            ceiling_c: None,
            c_reached_at: None,
            frames: VecDeque::new(),
            tcp: VecDeque::new(),
            qs: VecDeque::new(),
            pending: None,
            base_bucket_at: 0,
            base_cur: None,
            base_prev: None,
            avg_bytes: 0.0,
            peak_acked: 0.0,
            peak_at: 0,
            state: State::Normal,
            normal_since: None,
            hold_until: 0,
            last_inc: 0,
            last_eval: None,
            onset: None,
            t_hi: T_HI_INIT_MS,
            t_hi_at: 0,
            timeouts_seen: None,
            rto_watch: None,
            last_cut_at: None,
            severe_episode: false,
            app_limited: true,
            displays_seen: 0,
            r_send_1s: 0.0,
            last_q: 0.0,
            last_rates: Rates::default(),
            counters: Counters::default(),
        }
    }

    pub fn target(&self) -> Option<f64> {
        self.target
    }

    pub fn k(&self) -> f64 {
        self.k
    }

    pub fn t_hi(&self) -> f64 {
        self.t_hi
    }

    pub fn ceiling(&self) -> Option<f64> {
        self.ceiling_c
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn last_q(&self) -> f64 {
        self.last_q
    }

    pub fn last_rates(&self) -> (f64, f64) {
        (self.last_rates.r_send, self.last_rates.r_acked)
    }

    pub fn r_send_1s(&self) -> f64 {
        self.r_send_1s
    }

    pub fn app_limited(&self) -> bool {
        self.app_limited
    }

    pub fn is_metered(&self) -> bool {
        !self.tcp.is_empty()
    }

    pub fn is_ack(&self) -> bool {
        self.ack
    }

    /// Why no decision is made, or None when the estimator decides.
    pub fn ineligible(&self) -> Option<&'static str> {
        let Some(ctx) = self.ctx else {
            return Some("ctx");
        };
        if !ctx.vbr {
            Some("vbr")
        } else if self.ack {
            Some("ack")
        } else if self.displays_seen > 1 {
            Some("multi")
        } else if ctx.configured_kbps == 0 {
            Some("br")
        } else if self.target.is_none() {
            Some("init")
        } else {
            None
        }
    }

    /// A video frame was taken off the queue (`enq`: when the capture loop queued it) and its
    /// write starts at `begin`.
    pub fn on_write_begin(&mut self, enq: u64, begin: u64, bytes: u32) {
        self.first_us.get_or_insert(enq.min(begin));
        self.pending = Some(Pending { enq, begin, bytes });
    }

    /// The write that `on_write_begin` started ended at `end`.
    pub fn on_write_end(&mut self, end: u64, display: i32) -> Option<Decision> {
        let p = self.pending.take()?;
        // The connection takes frames again: a later stall is a new episode. The queue the
        // episode built up (this write's delay, the stall samples) was its SEVERE's: a HOLD from
        // here lets fresh frames show whether the link really falls short after it.
        if std::mem::take(&mut self.severe_episode) {
            self.hold_until = self.hold_until.max(end + HOLD_MIN_US);
        }
        let delay = end.saturating_sub(p.enq);
        self.update_base(end, delay);
        let base = self.base();
        // A frame much larger than average (key frame, scene change) is allowed its excess
        // serialization at the best delivery rate seen: one big frame is not a queue.
        let rate = self.peak_acked.max(self.target.unwrap_or(0.0)).max(1.0);
        let excess = (p.bytes as f64 - LARGE_FRAME * self.avg_bytes).max(0.0);
        let ser_us = if self.avg_bytes > 0.0 {
            excess * 8.0 / rate * 1000.0
        } else {
            0.0
        };
        let q_ms = ((delay.saturating_sub(base)) as f64 - ser_us).max(0.0) / 1000.0;
        self.avg_bytes = if self.avg_bytes == 0.0 {
            p.bytes as f64
        } else {
            0.95 * self.avg_bytes + 0.05 * p.bytes as f64
        };
        self.frames.push_back(FrameRec {
            enq: p.enq,
            begin: p.begin,
            end,
            bytes: p.bytes,
            display,
            q_ms: q_ms as f32,
        });
        while self.frames.len() > MAX_FRAMES
            || self
                .frames
                .front()
                .is_some_and(|f| f.end + FRAME_KEEP_US < end)
        {
            self.frames.pop_front();
        }
        // The sample carries the meter's queue too, so a backlog in the socket shows in every
        // frame's value, not only at the 50 ms evaluations.
        let q = q_ms.max(self.tcp_q(end, self.last_rates.r_acked));
        self.push_q(end, q);
        self.maybe_eval(end)
    }

    /// One TCP meter reading.
    pub fn on_tcp(&mut self, s: TcpInput) {
        if self.tcp.back().is_some_and(|b| b.at_us >= s.at_us) {
            return;
        }
        self.tcp.push_back(s);
        while self.tcp.len() > MAX_TCP
            || self
                .tcp
                .front()
                .is_some_and(|t| t.at_us + TCP_KEEP_US < s.at_us)
        {
            self.tcp.pop_front();
        }
    }

    /// The capture loop has waited `waited` for this connection to take its frame.
    pub fn on_stall(&mut self, now: u64, waited: u64) -> Option<Decision> {
        self.counters.stalls += 1;
        self.evaluate(now, Some(waited))
    }

    /// Once per second: the encoder context, `k`, app-limited, displays.
    pub fn on_second(&mut self, now: u64, ctx: Context) -> Option<Decision> {
        self.first_us.get_or_insert(now);
        if let Some(old) = self.ctx {
            if old.quality != ctx.quality && ctx.ceil_kbps > 0 && self.target.is_some() {
                // The user changed the quality: QoS jumps to the new ceiling, so does the target.
                self.target = Some(ctx.ceil_kbps as f64 * self.k);
                self.ceiling_c = None;
                self.c_reached_at = None;
            }
        }
        self.ctx = Some(ctx);
        let from = now.saturating_sub(1_000_000);
        let mut frames_1s = 0u32;
        let mut bytes_1s = 0u64;
        let mut displays: Vec<i32> = Vec::new();
        for f in self.frames.iter() {
            if f.end > from {
                frames_1s += 1;
                bytes_1s += f.bytes as u64;
            }
            if !displays.contains(&f.display) {
                displays.push(f.display);
            }
        }
        self.displays_seen = displays.len();
        let r1 = match self.tcp_rate(now, 1_000_000, 500_000) {
            Some((r_send, _)) => r_send,
            None => bytes_1s as f64 * 8.0 / 1000.0,
        };
        self.r_send_1s = r1;
        let full = ctx.fps > 0 && frames_1s as f64 >= 0.8 * ctx.fps as f64;
        let mut k_decayed = false;
        if full && ctx.configured_kbps > 0 && r1 > 0.0 {
            let sample = (r1 / ctx.configured_kbps as f64).clamp(K_MIN, K_MAX);
            if self.k_samples == 0 {
                self.k = sample;
                self.k_samples = 1;
            } else if sample < K_CONTENT_LIMITED * self.k {
                // The content, not the encoder's coupling, limits this second.
                self.k = (self.k * K_DECAY).max(sample);
                self.counters.k_rejected += 1;
                k_decayed = true;
            } else {
                let w = if sample > self.k { K_EWMA_UP } else { K_EWMA };
                self.k = (1.0 - w) * self.k + w * sample;
                self.k_samples += 1;
            }
        }
        if self.target.is_none()
            && ctx.configured_kbps > 0
            && (self.k_samples > 0
                || self
                    .first_us
                    .is_some_and(|t| now.saturating_sub(t) >= INIT_AFTER_US))
        {
            self.target = Some(ctx.configured_kbps as f64 * self.k);
            self.last_inc = now;
        }
        // Not while `k` drifts down on content-limited seconds: that is not a lower ceiling.
        if let Some(t) = self.target.as_mut().filter(|_| !k_decayed) {
            let ceil = ctx.ceil_kbps as f64 * self.k;
            if ceil > 0.0 && *t > ceil {
                *t = ceil;
            }
        }
        self.app_limited = (ctx.fps > 0 && (frames_1s as f64) < 0.7 * ctx.fps as f64)
            || self.target.is_some_and(|t| r1 < 0.7 * t);
        self.maybe_eval(now)
    }

    /// Evaluates if a tick is due (replay tests; the connection's 1 s timer uses `on_second`).
    #[cfg(test)]
    pub fn tick(&mut self, now: u64) -> Option<Decision> {
        self.maybe_eval(now)
    }

    fn maybe_eval(&mut self, now: u64) -> Option<Decision> {
        if self.last_eval.is_some_and(|t| now < t + TICK_US) {
            return None;
        }
        self.evaluate(now, None)
    }

    fn update_base(&mut self, now: u64, delay: u64) {
        if now >= self.base_bucket_at + BASE_BUCKET_US {
            self.base_prev = self.base_cur;
            self.base_cur = None;
            self.base_bucket_at = now;
        }
        self.base_cur = Some(self.base_cur.map_or(delay, |b| b.min(delay)));
    }

    fn base(&self) -> u64 {
        match (self.base_cur, self.base_prev) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => 0,
        }
    }

    fn push_q(&mut self, at: u64, q_ms: f64) {
        let normal = self.state == State::Normal;
        self.qs.push_back((at, q_ms as f32, normal));
        while self.qs.len() > MAX_QS
            || self
                .qs
                .front()
                .is_some_and(|q| q.0 + THRESHOLD_WINDOW_US < at)
        {
            self.qs.pop_front();
        }
    }

    fn fresh_tcp(&self, now: u64) -> Option<&TcpInput> {
        self.tcp
            .back()
            .filter(|t| now.saturating_sub(t.at_us) <= TCP_FRESH_US)
    }

    // Metered send and delivery rates over about `window` (kbps): readings at least
    // `min_span` apart.
    fn tcp_rate(&self, now: u64, window: u64, min_span: u64) -> Option<(f64, f64)> {
        let new = self.fresh_tcp(now)?;
        let old = self
            .tcp
            .iter()
            .rev()
            .find(|t| t.at_us + window <= new.at_us)
            .or_else(|| self.tcp.front())?;
        let span = new.at_us.saturating_sub(old.at_us);
        if span < min_span || span == 0 {
            return None;
        }
        let dt_ms = span as f64 / 1000.0;
        let r_send = new.app_bytes.saturating_sub(old.app_bytes) as f64 * 8.0 / dt_ms;
        let r_acked = new.acked().saturating_sub(old.acked()) as f64 * 8.0 / dt_ms;
        Some((r_send, r_acked))
    }

    // Metered send and delivery rates (kbps) from `base` to the newest reading, if at least
    // `min_span` apart.
    fn rates_since(&self, now: u64, base: &TcpInput, min_span: u64) -> Option<(f64, f64)> {
        let new = self.fresh_tcp(now)?;
        let span = new.at_us.saturating_sub(base.at_us);
        if span < min_span || span == 0 {
            return None;
        }
        let dt_ms = span as f64 / 1000.0;
        let r_send = new.app_bytes.saturating_sub(base.app_bytes) as f64 * 8.0 / dt_ms;
        let r_acked = new.acked().saturating_sub(base.acked()) as f64 * 8.0 / dt_ms;
        Some((r_send, r_acked))
    }

    fn rates(&self, now: u64) -> Rates {
        if let Some((r_send, r_acked)) = self.tcp_rate(now, RATE_WINDOW_US, RATE_WINDOW_US / 2) {
            let new = self.fresh_tcp(now);
            let paused = new.is_some_and(|new| {
                self.tcp
                    .iter()
                    .rev()
                    .find(|t| t.at_us + PAUSE_WINDOW_US <= new.at_us)
                    .is_some_and(|old| new.acked().saturating_sub(old.acked()) < PAUSE_BYTES)
            });
            // The backlog grows now: over the last 100 ms TCP delivered less than was written.
            // Right after a pause it delivers faster than that, so a pause never counts.
            let growing = self
                .tcp_rate(now, OVERUSE_WINDOW_US, OVERUSE_WINDOW_US * 4 / 5)
                .is_some_and(|(s, a)| s > 0.0 && a < DELIVERY_SHORTFALL * s);
            return Rates {
                r_send,
                r_acked,
                growing,
                paused,
                metered: true,
            };
        }
        let from = now.saturating_sub(RATE_WINDOW_US);
        let w_ms = RATE_WINDOW_US as f64 / 1000.0;
        let mut sent = 0u64;
        let mut done = 0u64;
        for f in self.frames.iter() {
            if f.enq > from && f.enq <= now {
                sent += f.bytes as u64;
            }
            if f.end > from && f.end <= now {
                done += f.bytes as u64;
            }
        }
        if let Some(p) = self.pending.filter(|p| p.enq > from) {
            sent += p.bytes as u64;
        }
        let paused = self
            .pending
            .is_some_and(|p| p.begin + PAUSE_WINDOW_US <= now)
            && self
                .frames
                .back()
                .map_or(true, |f| f.end + PAUSE_WINDOW_US <= now);
        Rates {
            r_send: sent as f64 * 8.0 / w_ms,
            r_acked: done as f64 * 8.0 / w_ms,
            growing: self.writer_saturated(now) || self.delays_rising(now),
            paused,
            metered: false,
        }
    }

    // A video write was in progress for most of the last 250 ms, over several writes: the
    // socket takes data no faster than the link drains it. One long write is a pause instead.
    fn writer_saturated(&self, now: u64) -> bool {
        let from = now.saturating_sub(BUSY_WINDOW_US);
        let mut busy = 0u64;
        let mut writes = 0;
        for f in self.frames.iter().rev() {
            if f.end <= from {
                break;
            }
            busy += f.end.min(now).saturating_sub(f.begin.max(from));
            writes += 1;
        }
        if let Some(p) = self.pending {
            busy += now.saturating_sub(p.begin.max(from));
        }
        writes >= BUSY_MIN_WRITES
            && busy.min(BUSY_WINDOW_US) as f64 >= BUSY_SHORTFALL * BUSY_WINDOW_US as f64
    }

    // The frames written in the last 100 ms saw a rising queue: a backlog building up.
    fn delays_rising(&self, now: u64) -> bool {
        let from = now.saturating_sub(OVERUSE_WINDOW_US);
        let mut it = self.frames.iter().filter(|f| f.end > from && f.end <= now);
        let Some(first) = it.next() else {
            return false;
        };
        let mut n = 1;
        let mut last = first;
        for f in it {
            n += 1;
            last = f;
        }
        n >= 3 && last.q_ms > first.q_ms + 5.0 && (last.q_ms as f64) > self.t_hi / 3.0
    }

    // The queue now, ms: the newest frame's value (for 200 ms), a write in progress, the meter.
    fn q_now(&self, now: u64, rates: &Rates) -> f64 {
        let mut q = 0.0f64;
        if let Some(f) = self.frames.back() {
            if now.saturating_sub(f.end) <= FRAME_Q_HOLD_US {
                q = f.q_ms as f64;
            }
        }
        if let Some(p) = self.pending {
            q = q.max(now.saturating_sub(p.enq).saturating_sub(self.base()) as f64 / 1000.0);
        }
        q.max(self.tcp_q(now, rates.r_acked))
    }

    // The meter's queue, ms: RTT above its minimum, and the unsent backlog beyond the frame just
    // written (that one is not a queue) at the delivery rate (a quarter of the target at least).
    fn tcp_q(&self, now: u64, r_acked: f64) -> f64 {
        let Some(t) = self.fresh_tcp(now) else {
            return 0.0;
        };
        let q_rtt = t.rtt_us.saturating_sub(t.min_rtt_us) as f64 / 1000.0;
        let rate = r_acked.max(0.25 * self.target.unwrap_or(0.0)).max(1.0);
        let backlog = (t.unsent() as f64 - self.avg_bytes).max(0.0);
        let q_backlog = backlog * 8.0 / rate;
        q_rtt.max(q_backlog)
    }

    // Minimum of the queue's step function over the last `window`.
    fn q_min(&self, now: u64, window: u64) -> f64 {
        let from = now.saturating_sub(window);
        let mut m = f64::INFINITY;
        for &(at, q, _) in self.qs.iter().rev() {
            if at > from {
                m = m.min(q as f64);
            } else {
                // The value at the window's start, unless it is stale.
                if at + 3 * window >= from {
                    m = m.min(q as f64);
                }
                break;
            }
        }
        if !m.is_finite() {
            m = 0.0;
        }
        // A write pending since before the window kept the queue above this all along.
        if let Some(p) = self.pending {
            let since = p.enq + self.base();
            if since < from {
                m = m.max((from - since) as f64 / 1000.0);
            }
        }
        m
    }

    fn is_normal(&self, now: u64, t_lo: f64) -> bool {
        if let Some(p) = self.pending {
            if now.saturating_sub(p.enq) as f64 / 1000.0 >= t_lo {
                return false;
            }
        }
        let from = now.saturating_sub(NORMAL_WINDOW_US);
        let mut v: Vec<f32> = self
            .qs
            .iter()
            .rev()
            .take_while(|q| q.0 > from)
            .map(|q| q.1)
            .collect();
        if v.is_empty() {
            return true;
        }
        v.sort_unstable_by(|a, b| a.total_cmp(b));
        let i = ((v.len() as f64 * 0.9).ceil() as usize).clamp(1, v.len()) - 1;
        (v[i] as f64) < t_lo
    }

    fn receiver_limited(&self, now: u64, t_lo: f64) -> bool {
        let Some(new) = self.fresh_tcp(now) else {
            return false;
        };
        let Some(new_lim) = new.lim_rwin_ms else {
            return false;
        };
        let Some(old) = self
            .tcp
            .iter()
            .rev()
            .find(|t| t.at_us + OVERUSE_WINDOW_US <= new.at_us)
        else {
            return false;
        };
        let Some(old_lim) = old.lim_rwin_ms else {
            return false;
        };
        let dt_ms = new.at_us.saturating_sub(old.at_us) as f64 / 1000.0;
        if dt_ms <= 0.0 {
            return false;
        }
        let share = new_lim.wrapping_sub(old_lim) as f64 / dt_ms;
        let q_rtt = new.rtt_us.saturating_sub(new.min_rtt_us) as f64 / 1000.0;
        share > 0.5 && q_rtt < t_lo
    }

    fn update_threshold(&mut self, now: u64) {
        if now < self.t_hi_at + THRESHOLD_EVERY_US && self.t_hi_at != 0 {
            return;
        }
        self.t_hi_at = now.max(1);
        let mut v: Vec<f32> = self.qs.iter().filter(|q| q.2).map(|q| q.1).collect();
        if v.len() < THRESHOLD_MIN_SAMPLES {
            return;
        }
        v.sort_unstable_by(|a, b| a.total_cmp(b));
        let i = ((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1;
        self.t_hi = (T_HI_GAIN * v[i] as f64).clamp(T_HI_MIN_MS, T_HI_MAX_MS);
    }

    fn evaluate(&mut self, now: u64, stall: Option<u64>) -> Option<Decision> {
        self.last_eval = Some(now);
        self.counters.evals += 1;
        self.update_threshold(now);
        let rates = self.rates(now);
        if now.saturating_sub(self.peak_at) > THRESHOLD_WINDOW_US || rates.r_acked > self.peak_acked
        {
            self.peak_acked = rates.r_acked;
            self.peak_at = now;
        }
        let q = self.q_now(now, &rates);
        self.push_q(now, q);
        let t_hi = self.t_hi;
        let t_lo = t_hi / 3.0;
        if q > t_hi {
            if self.onset.is_none() {
                let onset = match self.pending {
                    Some(p) => (p.enq + self.base() + (t_hi * 1000.0) as u64).min(now),
                    None => now,
                };
                self.onset = Some(onset);
            }
        } else if q < t_lo {
            self.onset = None;
        }
        if let Some(t) = self.tcp.back().copied() {
            if self.timeouts_seen.is_some_and(|seen| t.timeouts > seen) {
                if self
                    .last_cut_at
                    .is_some_and(|c| now.saturating_sub(c) < RTO_AFTER_CUT_US)
                {
                    // The pause or queue that just cut also made the RTO.
                    self.rto_watch = None;
                    self.counters.rto_ignored += 1;
                } else {
                    self.rto_watch = Some((now, t));
                }
            }
            self.timeouts_seen = Some(t.timeouts);
        }
        // One SEVERE per stall episode: the capture loop reports the same unfetched frame after
        // every slice, and an outage is not shortened by halving again.
        let severe_stall = !self.ack && stall.is_some_and(|w| w >= SEVERE_STALL_US);
        let severe = severe_stall && !self.severe_episode;
        if severe {
            self.severe_episode = true;
        }
        let receiver = self.receiver_limited(now, t_lo);
        // OVERUSE: the queue stayed above T_hi for 100 ms, the link is not paused, and the queue
        // grows now or has stood above T_hi for 500 ms (a pause drains faster than that).
        let overuse = !rates.paused
            && self.q_min(now, OVERUSE_WINDOW_US) > t_hi
            && (rates.growing || self.q_min(now, STANDING_WINDOW_US) > t_hi);
        let normal = self.is_normal(now, t_lo);
        let state = if severe {
            State::Severe
        } else if receiver {
            State::Receiver
        } else if overuse {
            State::Overuse
        } else if normal {
            State::Normal
        } else {
            State::Hold
        };
        self.state = state;
        self.counters.states[state.index()] += 1;
        if state == State::Normal {
            self.normal_since.get_or_insert(now);
        } else {
            self.normal_since = None;
        }
        self.last_q = q;
        self.last_rates = rates;
        if self.ineligible().is_some() {
            return None;
        }
        let (Some(target), Some(ctx)) = (self.target, self.ctx) else {
            return None;
        };
        let floor = ctx.floor_kbps as f64 * self.k;
        let ceil = (ctx.ceil_kbps as f64 * self.k).max(floor);
        // The RTO is judged on what happened after it, never on the outage that caused it.
        let mut rto_acked = 0.0;
        let rto_cut = match self.rto_watch {
            None => false,
            Some((at, base)) => match self.rates_since(now, &base, RTO_JUDGE_US) {
                Some((s, a)) if !rates.paused => {
                    self.rto_watch = None;
                    if s > 0.0 && a < RTO_SHORTFALL * s {
                        rto_acked = a;
                        true
                    } else {
                        self.counters.rto_ignored += 1;
                        false
                    }
                }
                _ if now > at + RTO_WATCH_US => {
                    self.rto_watch = None;
                    self.counters.rto_ignored += 1;
                    false
                }
                _ => false,
            },
        };
        let kind = if severe {
            Some(Kind::Severe)
        } else if overuse && !receiver {
            Some(Kind::Cut)
        } else if rto_cut && !receiver {
            Some(Kind::Rto)
        } else {
            None
        };
        let srtt_us = self.fresh_tcp(now).map(|t| t.rtt_us as u64).unwrap_or(0);
        if let Some(kind) = kind {
            if now < self.hold_until {
                self.counters.held += 1;
                return None;
            }
            let to = match kind {
                Kind::Severe => 0.5 * target,
                Kind::Rto => (BETA * rto_acked).max(0.5 * target),
                _ => (OVERUSE_STEP * target)
                    .min(BETA * rates.r_acked)
                    .max(0.5 * target),
            }
            .max(floor);
            if to > target * (1.0 - MIN_CHANGE) {
                self.counters.floor += 1;
                return None;
            }
            if kind != Kind::Severe && rates.r_acked > 0.0 {
                let mut c = self.ceiling_c.map_or(rates.r_acked, |c| {
                    (1.0 - CEILING_EWMA) * c + CEILING_EWMA * rates.r_acked
                });
                if self
                    .c_reached_at
                    .is_some_and(|t| now.saturating_sub(t) < CEILING_RECUR_US)
                {
                    c *= 0.9;
                }
                self.ceiling_c = Some(c);
                self.c_reached_at = None;
            }
            self.target = Some(to);
            self.hold_until = now + HOLD_MIN_US.max(2 * srtt_us);
            self.last_inc = now;
            self.normal_since = None;
            self.last_cut_at = Some(now);
            // An RTO being watched belongs to the episode this cut covers.
            self.rto_watch = None;
            let react_ms = self
                .onset
                .take()
                .map(|o| now.saturating_sub(o) as f64 / 1000.0)
                .unwrap_or(0.0);
            let false_cut = rates
                .metered
                .then(|| rates.r_send > 0.0 && rates.r_acked >= DELIVERY_SHORTFALL * rates.r_send);
            match kind {
                Kind::Severe => self.counters.severe += 1,
                Kind::Rto => self.counters.rto_cuts += 1,
                _ => self.counters.cuts += 1,
            }
            if false_cut == Some(true) {
                self.counters.false_cuts += 1;
            }
            self.counters.react_max_ms = self.counters.react_max_ms.max(react_ms);
            return Some(self.decision(kind, now, target, to, q, rates, react_ms, false_cut));
        }
        if state == State::Normal
            && !self.app_limited
            && now >= self.hold_until
            && self
                .normal_since
                .is_some_and(|t| now.saturating_sub(t) >= INCREASE_AFTER_US)
            && now >= self.last_inc + INCREASE_SPACING_US
            && target < ceil
        {
            let dt = now.saturating_sub(self.last_inc).min(1_000_000) as f64 / 1e6;
            let near = self.ceiling_c.is_some_and(|c| target >= 0.9 * c);
            let to = if near {
                target + ADD_INCREASE_KBPS_PER_S.max(ADD_INCREASE_SHARE_PER_S * target) * dt
            } else {
                target * MULT_INCREASE_PER_S.powf(dt)
            }
            .min(ceil);
            if !near && self.ceiling_c.is_some_and(|c| to >= 0.9 * c) {
                self.c_reached_at = Some(now);
            }
            if self.ceiling_c.is_some_and(|c| to > CEILING_FORGET * c) {
                // Well past the learned ceiling without an overuse: the link got better.
                self.ceiling_c = None;
                self.c_reached_at = None;
            }
            self.target = Some(to);
            self.last_inc = now;
            self.counters.incs += 1;
            return Some(self.decision(Kind::Increase, now, target, to, q, rates, 0.0, None));
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn decision(
        &self,
        kind: Kind,
        now: u64,
        from: f64,
        to: f64,
        q: f64,
        rates: Rates,
        react_ms: f64,
        false_cut: Option<bool>,
    ) -> Decision {
        Decision {
            kind,
            at_us: now,
            from_kbps: from,
            to_kbps: to,
            set_kbps: to / self.k.max(K_MIN),
            q_ms: q,
            t_hi_ms: self.t_hi,
            r_send: rates.r_send,
            r_acked: rates.r_acked,
            react_ms,
            false_cut,
            metered: rates.metered,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Per-connection wrapper: the estimator shared by the connection task, its writer task and the
// capture threads, and its log lines.

// Connections whose estimator is on (created on, and not turned off by the option since); the
// capture loop's wait keeps its 300 ms slices and makes no stall reports while 0.
static LIVE: AtomicUsize = AtomicUsize::new(0);
// At most one `FASTABR` decision line per connection in this period.
const DECISION_LOG_INTERVAL: Duration = Duration::from_secs(2);

lazy_static::lazy_static! {
    static ref REGISTRY: Mutex<HashMap<i32, Weak<ConnFastAbr>>> = Default::default();
}

/// enable-fast-abr-shadow: on unless "N" on Windows (where the TCP meter pairs with it); off
/// unless "Y" elsewhere, so other platforms behave as before by default.
pub fn shadow_option() -> bool {
    let v = Config::get_option(keys::OPTION_ENABLE_FAST_ABR_SHADOW);
    if cfg!(windows) {
        v != "N"
    } else {
        v == "Y"
    }
}

/// The capture loop's wait slice while any connection runs an estimator: 2 frame periods,
/// 20..300 ms, so a stall is reported at frame granularity. None: keep the 300 ms slice.
pub fn stall_slice(spf: Duration) -> Option<Duration> {
    if LIVE.load(Ordering::Relaxed) == 0 {
        return None;
    }
    Some(slice_for(spf))
}

fn slice_for(spf: Duration) -> Duration {
    (spf * 2).clamp(Duration::from_millis(20), Duration::from_millis(300))
}

/// A wait longer than this is reported to the estimators of the connections that did not take
/// the frame: max(3 frame periods, 50 ms).
pub fn stall_threshold(spf: Duration) -> Duration {
    (spf * 3).max(Duration::from_millis(50))
}

/// From the capture loop: these connections have not taken the current frame for `waited`.
pub fn on_send_stall(ids: impl Iterator<Item = i32>, waited: Duration) {
    let conns: Vec<Arc<ConnFastAbr>> = {
        let Ok(reg) = REGISTRY.lock() else {
            return;
        };
        ids.filter_map(|id| reg.get(&id).and_then(|w| w.upgrade()))
            .collect()
    };
    for c in conns {
        c.on_stall(waited);
    }
}

struct LogState {
    latency_log: bool,
    window_start: Instant,
    q: crate::client::latency::Samples,
    last_line: Option<Instant>,
    skipped: u32,
}

struct Inner {
    est: FastEstimator,
    log: LogState,
    #[cfg(windows)]
    tcp: Option<Arc<crate::platform::windows_qos::HostSocket>>,
}

/// One connection's estimator in shadow mode.
pub struct ConnFastAbr {
    id: i32,
    epoch: Instant,
    on: AtomicBool,
    inner: Mutex<Inner>,
}

impl ConnFastAbr {
    pub fn new(id: i32, ack: bool) -> Arc<Self> {
        let c = Arc::new(Self {
            id,
            epoch: Instant::now(),
            on: AtomicBool::new(true),
            inner: Mutex::new(Inner {
                est: FastEstimator::new(ack),
                log: LogState {
                    latency_log: latency_log_enabled(),
                    window_start: Instant::now(),
                    q: Default::default(),
                    last_line: None,
                    skipped: 0,
                },
                #[cfg(windows)]
                tcp: None,
            }),
        });
        if let Ok(mut reg) = REGISTRY.lock() {
            reg.insert(id, Arc::downgrade(&c));
        }
        LIVE.fetch_add(1, Ordering::Relaxed);
        log::info!("FASTABR shadow conn={} ack={}", id, ack as u8);
        c
    }

    /// The TCP meter of the session socket (metered sockets only).
    #[cfg(windows)]
    pub fn set_tcp(&self, socket: Arc<crate::platform::windows_qos::HostSocket>) {
        socket.set_fast_active(self.on.load(Ordering::Relaxed));
        if let Ok(mut inner) = self.inner.lock() {
            inner.tcp = Some(socket);
        }
    }

    fn us(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.epoch).as_micros() as u64
    }

    /// A video frame was taken off the queue; its write starts now.
    pub fn on_dequeue(&self, msg: &Message, queued_at: Instant) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let Some(frames) = super::super::latency_stats::encoded_frames(msg) else {
            return;
        };
        let bytes: usize = frames.iter().map(|f| f.data.len()).sum();
        let now = Instant::now();
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .est
                .on_write_begin(self.us(queued_at), self.us(now), bytes as u32);
        }
    }

    /// The write of a video frame ended now.
    pub fn on_sent(&self, msg: &Message) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let Some(base::message_proto::message::Union::VideoFrame(vf)) = &msg.union else {
            return;
        };
        let now = Instant::now();
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        #[cfg(windows)]
        self.feed_tcp(&mut inner);
        let d = inner.est.on_write_end(self.us(now), vf.display);
        self.after_eval(&mut inner, d);
    }

    fn on_stall(&self, waited: Duration) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        #[cfg(windows)]
        self.feed_tcp(&mut inner);
        let d = inner
            .est
            .on_stall(self.us(now), waited.as_micros() as u64);
        self.after_eval(&mut inner, d);
    }

    /// From the connection's 1 s timer: the encoder context, the option, the report window.
    pub fn on_second(&self, ctx: Context) {
        let on = shadow_option();
        if self.on.swap(on, Ordering::Relaxed) != on {
            // Only this timer flips `on`; `Drop` reads it to undo this connection's share.
            if on {
                LIVE.fetch_add(1, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(1, Ordering::Relaxed);
            }
            log::info!("FASTABR shadow conn={} {}", self.id, if on { "on" } else { "off" });
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        #[cfg(windows)]
        if let Some(t) = inner.tcp.as_ref() {
            t.set_fast_active(on);
        }
        if !on {
            return;
        }
        let now = Instant::now();
        #[cfg(windows)]
        self.feed_tcp(&mut inner);
        let d = inner.est.on_second(self.us(now), ctx);
        self.after_eval(&mut inner, d);
        if inner.log.window_start.elapsed() >= crate::client::latency::REPORT_INTERVAL {
            if inner.log.latency_log {
                self.log_window(&inner, ctx);
            }
            inner.est.counters = Counters::default();
            inner.log.q.clear();
            inner.log.window_start = Instant::now();
            inner.log.latency_log = latency_log_enabled();
        }
    }

    #[cfg(windows)]
    fn feed_tcp(&self, inner: &mut Inner) {
        let Some(s) = inner.tcp.as_ref().and_then(|t| t.snapshot()) else {
            return;
        };
        let input = TcpInput {
            at_us: self.us(s.at),
            app_bytes: s.app_bytes,
            unique_out: s.unique_out,
            in_flight: s.in_flight,
            rtt_us: s.rtt_us,
            min_rtt_us: s.min_rtt_us,
            timeouts: s.timeouts,
            lim_rwin_ms: s.lim_rwin_ms,
        };
        inner.est.on_tcp(input);
    }

    fn after_eval(&self, inner: &mut Inner, d: Option<Decision>) {
        if inner.log.latency_log {
            let q = inner.est.last_q();
            inner.log.q.push(Duration::from_micros((q * 1000.0) as u64));
        }
        let Some(d) = d else {
            return;
        };
        if d.kind == Kind::Increase {
            return;
        }
        let now = Instant::now();
        if inner
            .log
            .last_line
            .is_some_and(|t| now.saturating_duration_since(t) < DECISION_LOG_INTERVAL)
        {
            inner.log.skipped += 1;
            return;
        }
        let skipped = std::mem::take(&mut inner.log.skipped);
        inner.log.last_line = Some(now);
        log::info!(
            "FASTABR shadow conn={} kind={} from_kbps={:.0} to_kbps={:.0} would_set_kbps={:.0} k={:.2} q_ms={:.1} t_hi_ms={:.0} r_send={:.0} r_acked={:.0} react_ms={:.0} fc={} meter={} skipped={}",
            self.id,
            d.kind.name(),
            d.from_kbps,
            d.to_kbps,
            d.set_kbps,
            inner.est.k(),
            d.q_ms,
            d.t_hi_ms,
            d.r_send,
            d.r_acked,
            d.react_ms,
            match d.false_cut {
                Some(true) => "1",
                Some(false) => "0",
                None => "-",
            },
            d.metered as u8,
            skipped,
        );
    }

    fn log_window(&self, inner: &Inner, ctx: Context) {
        let e = &inner.est;
        let c = &e.counters;
        let (r_send, r_acked) = e.last_rates();
        let target = e.target();
        log::info!(
            "LATENCY fast conn={} mode=shadow elig={} state={} meter={} ack={} k={:.2} set_kbps={} target_kbps={} would_set_kbps={} floor_kbps={} ceil_kbps={} c_kbps={} r_send_1s={:.0} r_send={:.0} r_acked={:.0} app_lim={} q={} t_hi_ms={:.0} st={}/{}/{}/{}/{} cuts={} severe={} rto={} rto_ign={} incs={} held={} floor={} fc={} stalls={} react_max_ms={:.0} k_rej={}",
            self.id,
            e.ineligible().unwrap_or("yes"),
            e.state().letter(),
            e.is_metered() as u8,
            e.is_ack() as u8,
            e.k(),
            ctx.configured_kbps,
            fmt_kbps(target),
            fmt_kbps(target.map(|t| t / e.k().max(K_MIN))),
            ctx.floor_kbps,
            ctx.ceil_kbps,
            fmt_kbps(e.ceiling()),
            e.r_send_1s(),
            r_send,
            r_acked,
            e.app_limited() as u8,
            crate::client::latency::format_summary(inner.log.q.summary()),
            e.t_hi(),
            c.states[0],
            c.states[1],
            c.states[2],
            c.states[3],
            c.states[4],
            c.cuts,
            c.severe,
            c.rto_cuts,
            c.rto_ignored,
            c.incs,
            c.held,
            c.floor,
            c.false_cuts,
            c.stalls,
            c.react_max_ms,
            c.k_rejected,
        );
    }
}

impl Drop for ConnFastAbr {
    fn drop(&mut self) {
        if let Ok(mut reg) = REGISTRY.lock() {
            if reg
                .get(&self.id)
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), self as *const _))
            {
                reg.remove(&self.id);
            }
        }
        if self.on.load(Ordering::Relaxed) {
            LIVE.fetch_sub(1, Ordering::Relaxed);
        }
        #[cfg(windows)]
        if let Ok(inner) = self.inner.lock() {
            if let Some(t) = inner.tcp.as_ref() {
                t.set_fast_active(false);
            }
        }
    }
}

fn fmt_kbps(v: Option<f64>) -> String {
    v.map(|v| format!("{:.0}", v))
        .unwrap_or_else(|| "-".to_owned())
}

fn latency_log_enabled() -> bool {
    Config::get_option(keys::OPTION_ENABLE_LATENCY_LOG) == "Y"
}

#[cfg(test)]
mod tests;
