//! Viewer-side latency measurement of the video path. Measurement and logging only;
//! nothing here changes how frames are queued, decoded or shown.
//! What each number means and its error bounds: docs/LATENCY-METRICS.md.

use super::VideoCountersTick;
use base::{
    config::keys,
    message_proto::{message, video_frame, Message, TestDelay, VideoFrame},
};
use hbb_common::{config::LocalConfig, log};
use scrap::CodecFormat;
use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub const REPORT_INTERVAL: Duration = Duration::from_secs(5);
// 5 s at 120 fps is 600 samples; the rest of a window is counted but not sampled.
const MAX_SAMPLES: usize = 1024;
// Frames received but not decoded yet. Only bounds memory when frames are discarded.
const MAX_PENDING: usize = 256;
// The clock anchor keeps the best frame of the last ANCHOR_BUCKETS * REPORT_INTERVAL.
const ANCHOR_BUCKETS: usize = 6;
// Buckets older than this are dropped even when no frame arrived (static screen), so
// clock drift cannot build up in a stale minimum.
const ANCHOR_WINDOW: Duration =
    Duration::from_secs(ANCHOR_BUCKETS as u64 * REPORT_INTERVAL.as_secs());
// The host sends one TestDelay per second.
const RTT_SAMPLES: usize = 10;
// Longer gaps between shown frames are an idle (static) screen, not a stutter.
const MAX_SHOWN_GAP: Duration = Duration::from_secs(1);

lazy_static::lazy_static! {
    // Keyed by `stream_key()` of a video thread's queue, so concurrent sessions do not mix.
    static ref STREAMS: Mutex<HashMap<usize, StreamLatency>> = Default::default();
    // Keyed by `stream_key()` of a session's `sender`.
    static ref INPUTS: Mutex<HashMap<usize, InputLatency>> = Default::default();
}

// Number of entries in STREAMS. The per-frame hooks return before locking while it is 0,
// so with measurement off they never touch the lock.
static ENABLED_STREAMS: AtomicUsize = AtomicUsize::new(0);
// Number of entries in INPUTS, for the same reason.
static ENABLED_INPUTS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Set by the soft renderer, on the video thread, when it drops a frame because the UI
    // has not taken the previous one yet.
    static UI_DROPPED: Cell<bool> = Cell::new(false);
    // Set by the soft renderer, on the video thread, when the frame replaced a buffer Flutter
    // had not fetched yet (present pacing).
    static UI_REPLACED: Cell<bool> = Cell::new(false);
    // The stream of the video thread this runs on, set by `set_thread_stream`.
    static THREAD_STREAM: Cell<Option<usize>> = Cell::new(None);
}

/// Whether any video stream of this process is measured. Gates the per-frame audio hook.
pub fn is_any_enabled() -> bool {
    ENABLED_STREAMS.load(Ordering::Relaxed) > 0
}

/// Whether a session measures and logs: quality monitor shown, or the local
/// `enable-latency-log` option set to "Y".
pub fn is_measuring(show_quality_monitor: bool) -> bool {
    show_quality_monitor || is_log_enabled()
}

/// Whether the local `enable-latency-log` option is "Y": per-tick lines (`FPSCTL`) are only
/// logged then, not for the quality monitor alone.
pub fn is_log_enabled() -> bool {
    LocalConfig::get_option(keys::OPTION_ENABLE_LATENCY_LOG) == "Y"
}

/// Durations of one report window, in microseconds, bounded to `MAX_SAMPLES`.
#[derive(Default)]
pub struct Samples(Vec<u32>);

#[derive(Clone, Copy)]
pub struct Summary {
    pub min: u32,
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
}

impl Samples {
    pub fn push(&mut self, d: Duration) {
        if self.0.len() < MAX_SAMPLES {
            self.0.push(d.as_micros().min(u32::MAX as u128) as u32);
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn summary(&self) -> Option<Summary> {
        if self.0.is_empty() {
            return None;
        }
        let mut v = self.0.clone();
        v.sort_unstable();
        let n = v.len();
        let rank = |p: f64| ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
        Some(Summary {
            min: v[0],
            p50: v[rank(0.5)],
            p95: v[rank(0.95)],
            max: v[n - 1],
        })
    }
}

fn us_to_ms(us: u32) -> f64 {
    us as f64 / 1000.0
}

fn round_ms(us: u32) -> f64 {
    (us as f64 / 100.0).round() / 10.0
}

/// "p50/p95/max" in milliseconds, or "n/a".
pub fn format_summary(s: Option<Summary>) -> String {
    match s {
        Some(s) => format!(
            "{:.1}/{:.1}/{:.1}",
            us_to_ms(s.p50),
            us_to_ms(s.p95),
            us_to_ms(s.max)
        ),
        None => "n/a".to_owned(),
    }
}

/// "min/p50/max" in milliseconds, or "n/a".
fn format_min_p50_max(s: Option<Summary>) -> String {
    match s {
        Some(s) => format!(
            "{:.1}/{:.1}/{:.1}",
            us_to_ms(s.min),
            us_to_ms(s.p50),
            us_to_ms(s.max)
        ),
        None => "n/a".to_owned(),
    }
}

/// Identifies one video thread. io_loop and the decoder thread share the queue `Arc`.
pub fn stream_key<T>(queue: &Arc<T>) -> usize {
    Arc::as_ptr(queue) as *const () as usize
}

/// The host's capture timestamp: milliseconds since the host's capture loop started.
pub fn frame_pts(vf: &VideoFrame) -> Option<i64> {
    use video_frame::Union::*;
    match vf.union.as_ref()? {
        Vp8s(f) | Vp9s(f) | Av1s(f) | H264s(f) | H265s(f) => {
            f.frames.iter().map(|e| e.pts).max()
        }
        _ => None,
    }
}

struct Pending {
    pts: i64,
    recv: Instant,
    // Local receive time minus pts. Contains an unknown constant (the two clocks' offset),
    // which cancels out against the anchor.
    transit_us: i64,
}

pub struct DecodedFrame {
    pub pts: Option<i64>,
    pub dequeued: Instant,
    pub decode_start: Instant,
    pub decode_end: Instant,
    pub format: CodecFormat,
    pub gpu_texture: bool,
    // Present pacing on for this display.
    pub paced: bool,
    // Decoded without output (`DecodeOutput::DecodedNoOutput`): no conversion was done.
    pub no_output: bool,
}

struct StreamLatency {
    display: usize,
    base: Instant,
    pending: VecDeque<Pending>,
    last_pts: Option<i64>,
    // (bucket start, minimum transit_us in the bucket), newest first.
    anchor_buckets: VecDeque<(Instant, i64)>,
    rtts: VecDeque<u32>,
    window_start: Instant,
    frames: usize,
    ui_dropped: usize,
    ui_replaced: usize,
    queue_len_max: usize,
    codec: Option<CodecFormat>,
    gpu_texture: bool,
    paced: bool,
    // When the previous shown frame was handed to the UI, and the one before it; kept across
    // windows.
    last_shown: Option<Instant>,
    prev_shown: Option<Instant>,
    // The `shown_gap` of the last shown frame, pushed once the next frame did not replace it
    // (a replaced frame never reached the screen).
    pending_gap: Option<Duration>,
    // Transport of the session (`TRANSPORT viewer` type).
    via: &'static str,
    // `VideoCounters` ticks summed over the window, and the time they cover.
    counts: VideoCountersTick,
    counts_secs: f64,
    last_net_us: Option<u64>,
    net_srv: Samples,
    queue: Samples,
    decode: Samples,
    // `decode` of the frames present pacing did not hand to the UI and that were decoded
    // without output.
    decode_skip: Samples,
    // `decode` of the frames present pacing did not hand to the UI although they were
    // converted (turned stale during the decode, pacing decided after the decode, or a decoder
    // that always outputs).
    decode_skip_conv: Samples,
    to_ui: Samples,
    ui_take: Samples,
    total: Samples,
    shown_gap: Samples,
    seq: u64,
    latest: Option<serde_json::Value>,
}

impl StreamLatency {
    fn new(display: usize) -> Self {
        let now = Instant::now();
        Self {
            display,
            base: now,
            pending: VecDeque::new(),
            last_pts: None,
            anchor_buckets: VecDeque::new(),
            rtts: VecDeque::new(),
            window_start: now,
            frames: 0,
            ui_dropped: 0,
            ui_replaced: 0,
            queue_len_max: 0,
            codec: None,
            gpu_texture: false,
            paced: false,
            last_shown: None,
            prev_shown: None,
            pending_gap: None,
            via: "",
            counts: Default::default(),
            counts_secs: 0.0,
            last_net_us: None,
            net_srv: Default::default(),
            queue: Default::default(),
            decode: Default::default(),
            decode_skip: Default::default(),
            decode_skip_conv: Default::default(),
            to_ui: Default::default(),
            ui_take: Default::default(),
            total: Default::default(),
            shown_gap: Default::default(),
            seq: 0,
            latest: None,
        }
    }

    fn on_received(&mut self, pts: i64, now: Instant) {
        if self.last_pts.map_or(false, |last| pts < last) {
            // The host restarted its capture loop, so pts counts from zero again.
            self.anchor_buckets.clear();
        }
        self.last_pts = Some(pts);
        let transit_us = now.saturating_duration_since(self.base).as_micros() as i64
            - pts.saturating_mul(1000);
        self.anchor_buckets
            .retain(|(start, _)| now.saturating_duration_since(*start) < ANCHOR_WINDOW);
        let rotate = self.anchor_buckets.front().map_or(true, |(start, _)| {
            now.saturating_duration_since(*start) >= REPORT_INTERVAL
        });
        if rotate {
            self.anchor_buckets.push_front((now, transit_us));
            self.anchor_buckets.truncate(ANCHOR_BUCKETS);
        } else if let Some((_, min)) = self.anchor_buckets.front_mut() {
            if transit_us < *min {
                *min = transit_us;
            }
        }
        if self.pending.len() >= MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back(Pending {
            pts,
            recv: now,
            transit_us,
        });
    }

    fn add_counts(&mut self, t: &VideoCountersTick, elapsed: Duration) {
        let c = &mut self.counts;
        c.received += t.received;
        c.discarded += t.discarded;
        c.decoded += t.decoded;
        c.no_output += t.no_output;
        c.failed += t.failed;
        c.shown += t.shown;
        c.ui_lost += t.ui_lost;
        self.counts_secs += elapsed.as_secs_f64();
    }

    fn on_rtt(&mut self, rtt_ms: u32) {
        self.rtts.push_back(rtt_ms);
        while self.rtts.len() > RTT_SAMPLES {
            self.rtts.pop_front();
        }
    }

    // The host's first TestDelay carries 0 because nothing is measured yet, so zeros count
    // only when nothing else is known (a sub-millisecond LAN also reports 0).
    fn rtt_estimate_ms(&self) -> Option<u32> {
        if self.rtts.is_empty() {
            return None;
        }
        Some(
            self.rtts
                .iter()
                .copied()
                .filter(|r| *r > 0)
                .min()
                .unwrap_or(0),
        )
    }

    // Frames are decoded in arrival order, so everything before the match was discarded.
    fn take_pending(&mut self, pts: i64) -> Option<Pending> {
        let pos = self.pending.iter().position(|p| p.pts == pts)?;
        self.pending.drain(..pos);
        self.pending.pop_front()
    }

    fn net_srv_estimate_us(&self, transit_us: i64) -> Option<u64> {
        let anchor = self.anchor_buckets.iter().map(|(_, min)| *min).min()?;
        let extra = (transit_us - anchor).max(0) as u64;
        let floor = self.rtt_estimate_ms().unwrap_or(0) as u64 * 500;
        Some(extra + floor)
    }

    // What every decoded frame records: its pending entry and its `net+server(est)` sample.
    fn on_decoded(
        &mut self,
        frame: &DecodedFrame,
        queue_len: usize,
    ) -> (Option<Pending>, Option<Duration>) {
        self.frames += 1;
        self.codec = Some(frame.format);
        self.gpu_texture = frame.gpu_texture;
        self.paced = frame.paced;
        self.queue_len_max = self.queue_len_max.max(queue_len);
        let pending = frame.pts.and_then(|pts| self.take_pending(pts));
        let net = pending
            .as_ref()
            .and_then(|p| self.net_srv_estimate_us(p.transit_us))
            .map(Duration::from_micros);
        if let Some(net) = net {
            self.net_srv.push(net);
            self.last_net_us = Some(net.as_micros() as u64);
        }
        (pending, net)
    }

    fn on_frame_done(
        &mut self,
        frame: DecodedFrame,
        ui_done: Instant,
        ui_dropped: bool,
        ui_replaced: bool,
        queue_len: usize,
    ) {
        let (pending, net) = self.on_decoded(&frame, queue_len);
        self.decode
            .push(frame.decode_end.saturating_duration_since(frame.decode_start));
        if let Some(p) = &pending {
            self.queue.push(frame.dequeued.saturating_duration_since(p.recv));
        }
        if ui_dropped {
            self.ui_dropped += 1;
            return;
        }
        if ui_replaced {
            self.ui_replaced += 1;
            // The frame before this one was never shown: its gap is dropped, and this one's
            // counts from the frame shown before it.
            self.pending_gap = self
                .prev_shown
                .map(|prev| ui_done.saturating_duration_since(prev));
        } else {
            if let Some(gap) = self.pending_gap.take() {
                if gap <= MAX_SHOWN_GAP {
                    self.shown_gap.push(gap);
                }
            }
            self.pending_gap = self
                .last_shown
                .map(|last| ui_done.saturating_duration_since(last));
            self.prev_shown = self.last_shown;
        }
        self.last_shown = Some(ui_done);
        self.to_ui
            .push(ui_done.saturating_duration_since(frame.decode_end));
        if let (Some(p), Some(net)) = (pending, net) {
            self.total.push(net + ui_done.saturating_duration_since(p.recv));
        }
    }

    // A frame present pacing decoded but did not hand to the UI: no `queue`/`to_ui`/`total`.
    fn on_frame_skipped(&mut self, frame: DecodedFrame, queue_len: usize) {
        self.on_decoded(&frame, queue_len);
        let took = frame.decode_end.saturating_duration_since(frame.decode_start);
        if frame.no_output {
            self.decode_skip.push(took);
        } else {
            self.decode_skip_conv.push(took);
        }
    }

    /// Returns the report line when a window ended; the caller logs it after
    /// releasing the `STREAMS` lock.
    fn maybe_report(&mut self, now: Instant) -> Option<String> {
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < REPORT_INTERVAL {
            return None;
        }
        let mut report = None;
        if self.frames > 0 {
            let net = self.net_srv.summary();
            let queue = self.queue.summary();
            let decode = self.decode.summary();
            let decode_skip = self.decode_skip.summary();
            let decode_skip_conv = self.decode_skip_conv.summary();
            let shown_gap = self.shown_gap.summary();
            let to_ui = self.to_ui.summary();
            let ui_take = self.ui_take.summary();
            let total = self.total.summary();
            let rtt = self.rtt_estimate_ms();
            let c = self.counts;
            let secs = self.counts_secs;
            let rate = |n: usize| (secs > 0.0).then(|| n as f64 / secs);
            let fmt_rate = |n: usize| rate(n).map_or("n/a".to_owned(), |r| format!("{:.1}", r));
            let skipped = c.decoded.saturating_sub(c.shown);
            let line = format!(
                "LATENCY client display={} codec={} hw={} pacing={} fps={:.1} net+server(est)={} queue={} decode={} to_ui={} ui_take={} total_to_ui(est)={} offset_err<={} queue_len={} ui_dropped={} ui_replaced={} rtt_min={} rx_fps={} dec_fps={} shown_fps={} skip={} decode_skip={} decode_skip_conv={} noout={} shown_gap={} discard={} dec_fail={} via={}",
                self.display,
                self.codec.map_or("-".to_owned(), |c| c.to_string()),
                self.gpu_texture,
                if self.paced { "on" } else { "off" },
                self.frames as f64 / elapsed.as_secs_f64(),
                format_summary(net),
                format_summary(queue),
                format_summary(decode),
                format_summary(to_ui),
                format_summary(ui_take),
                format_summary(total),
                rtt.map_or("n/a".to_owned(), |r| format!("{:.1}ms", r as f64 / 2.0)),
                self.queue_len_max,
                self.ui_dropped,
                self.ui_replaced,
                rtt.map_or("n/a".to_owned(), |r| format!("{}ms", r)),
                fmt_rate(c.received),
                fmt_rate(c.decoded),
                fmt_rate(c.displayed()),
                skipped,
                format_summary(decode_skip),
                format_summary(decode_skip_conv),
                c.no_output,
                format_summary(shown_gap),
                c.discarded,
                c.failed,
                if self.via.is_empty() { "?" } else { self.via },
            );
            let round1 = |v: f64| (v * 10.0).round() / 10.0;
            self.seq += 1;
            self.latest = Some(serde_json::json!({
                "seq": self.seq,
                "line": line,
                "net": net.map(|s| round_ms(s.p50)),
                "queue": queue.map(|s| round_ms(s.p50)),
                "dec": decode.map(|s| round_ms(s.p50)),
                "ui": to_ui.map(|s| round_ms(s.p50)),
                "take": ui_take.map(|s| round_ms(s.p50)),
                "total": total.map(|s| round_ms(s.p50)),
                "err": rtt.map(|r| r as f64 / 2.0),
                "rfps": rate(c.received).map(round1),
                "dfps": rate(c.decoded).map(round1),
                "sfps": rate(c.displayed()).map(round1),
                "skip": rate(skipped).map(round1),
            }));
            report = Some(line);
        }
        self.window_start = now;
        self.frames = 0;
        self.ui_dropped = 0;
        self.ui_replaced = 0;
        self.queue_len_max = 0;
        self.counts = Default::default();
        self.counts_secs = 0.0;
        self.net_srv.clear();
        self.queue.clear();
        self.decode.clear();
        self.decode_skip.clear();
        self.decode_skip_conv.clear();
        self.to_ui.clear();
        self.ui_take.clear();
        self.total.clear();
        self.shown_gap.clear();
        report
    }
}

/// Called once per second from io_loop with this session's video threads as
/// `(display, stream_key, counters taken this tick)`, the tick's length and the session's
/// transport. Starts or stops measuring them (`enabled`: `is_measuring`) and returns the
/// latest per-display summaries as JSON for the quality monitor.
pub fn refresh_streams(
    streams: impl Iterator<Item = (usize, usize, Option<VideoCountersTick>)>,
    tick: Duration,
    via: &'static str,
    enabled: bool,
) -> Option<String> {
    let mut map = STREAMS.lock().unwrap();
    let mut out = serde_json::Map::new();
    for (display, key, counts) in streams {
        if !enabled {
            map.remove(&key);
            continue;
        }
        let s = map.entry(key).or_insert_with(|| StreamLatency::new(display));
        s.display = display;
        s.via = via;
        if let Some(counts) = counts {
            s.add_counts(&counts, tick);
        }
        if let Some(latest) = &s.latest {
            out.insert(display.to_string(), latest.clone());
        }
    }
    ENABLED_STREAMS.store(map.len(), Ordering::Relaxed);
    drop(map);
    if out.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(out).to_string())
    }
}

/// The current `net+server(est)` of a stream as "last/p95" ms (the last frame, and the p95 of
/// the window so far), for the `Set fps` and `FPSCTL` lines. None while it is not measured.
pub fn net_srv_now(key: usize) -> Option<String> {
    if !is_any_enabled() {
        return None;
    }
    let map = STREAMS.lock().unwrap();
    let s = map.get(&key)?;
    let last = s.last_net_us?;
    Some(format!(
        "{:.1}/{}",
        last as f64 / 1000.0,
        s.net_srv
            .summary()
            .map_or("n/a".to_owned(), |x| format!("{:.1}", us_to_ms(x.p95)))
    ))
}

pub fn remove_stream(key: usize) {
    let mut map = STREAMS.lock().unwrap();
    map.remove(&key);
    ENABLED_STREAMS.store(map.len(), Ordering::Relaxed);
}

/// Called once at the start of a video thread, so the soft renderer can tell which
/// stream a frame it hands to Flutter belongs to.
pub fn set_thread_stream(key: usize) {
    THREAD_STREAM.with(|s| s.set(Some(key)));
}

pub fn on_received(key: usize, vf: &VideoFrame) {
    if !is_any_enabled() {
        return;
    }
    let now = Instant::now();
    let mut map = STREAMS.lock().unwrap();
    let Some(s) = map.get_mut(&key) else {
        return;
    };
    if let Some(pts) = frame_pts(vf) {
        s.on_received(pts, now);
    }
}

/// `last_delay` of a host TestDelay is the round trip the host measured for its previous probe.
pub fn on_test_delay(keys: impl Iterator<Item = usize>, t: &TestDelay) {
    if t.from_client || !is_any_enabled() {
        return;
    }
    let mut map = STREAMS.lock().unwrap();
    for key in keys {
        if let Some(s) = map.get_mut(&key) {
            s.on_rtt(t.last_delay);
        }
    }
}

/// Whether the soft renderer dropped the frame just handed to it (`ui_dropped`) or put it in
/// place of one Flutter had not fetched yet (`ui_replaced`): either way one frame fewer reaches
/// the screen. Peeked on the video thread right after the UI callback; `on_frame_done` clears
/// both.
pub fn ui_frame_lost() -> bool {
    UI_DROPPED.with(|d| d.get()) || UI_REPLACED.with(|d| d.get())
}

/// Called on the video thread right after the frame was handed to the UI callback.
pub fn on_frame_done(key: usize, frame: DecodedFrame, queue_len: usize) {
    let ui_done = Instant::now();
    let ui_dropped = UI_DROPPED.with(|d| d.replace(false));
    let ui_replaced = UI_REPLACED.with(|d| d.replace(false));
    if !is_any_enabled() {
        return;
    }
    let report = {
        let mut map = STREAMS.lock().unwrap();
        let Some(s) = map.get_mut(&key) else {
            return;
        };
        s.on_frame_done(frame, ui_done, ui_dropped, ui_replaced, queue_len);
        s.maybe_report(ui_done)
    };
    if let Some(line) = report {
        log::info!("{}", line);
    }
}

/// Called on the video thread for a decoded frame that present pacing did not hand to the UI
/// callback because a newer frame was waiting.
pub fn on_frame_skipped(key: usize, frame: DecodedFrame, queue_len: usize) {
    if !is_any_enabled() {
        return;
    }
    let now = Instant::now();
    let report = {
        let mut map = STREAMS.lock().unwrap();
        let Some(s) = map.get_mut(&key) else {
            return;
        };
        s.on_frame_skipped(frame, queue_len);
        s.maybe_report(now)
    };
    if let Some(line) = report {
        log::info!("{}", line);
    }
}

// Input messages queued but not yet written, per session. Bounds memory if io_loop stops
// popping (it pops one per written input message).
const MAX_QUEUED_INPUTS: usize = 256;

/// Viewer input send time (`in=`): `Session::send` of an input message until io_loop finished
/// writing it to the peer stream. Messages go through one FIFO channel, so the n-th queued
/// input message is the n-th written one.
struct InputLatency {
    queued: VecDeque<Instant>,
    window_start: Instant,
    written: Samples,
    count: usize,
    backlog_max: usize,
}

impl InputLatency {
    fn new() -> Self {
        Self {
            queued: VecDeque::new(),
            window_start: Instant::now(),
            written: Default::default(),
            count: 0,
            backlog_max: 0,
        }
    }
}

/// Mouse, key and pointer-device events: the messages `in=` measures.
pub fn is_input(msg: &Message) -> bool {
    matches!(
        msg.union,
        Some(message::Union::MouseEvent(_))
            | Some(message::Union::KeyEvent(_))
            | Some(message::Union::PointerDeviceEvent(_))
    )
}

/// Called by `Session::send` before it hands a message to io_loop's channel.
pub fn on_input_queued(key: usize, msg: &Message) {
    if ENABLED_INPUTS.load(Ordering::Relaxed) == 0 || !is_input(msg) {
        return;
    }
    let now = Instant::now();
    let mut map = INPUTS.lock().unwrap();
    if let Some(s) = map.get_mut(&key) {
        if s.queued.len() >= MAX_QUEUED_INPUTS {
            s.queued.pop_front();
        }
        s.queued.push_back(now);
    }
}

/// Called by io_loop after it wrote a message from the UI to the peer stream.
pub fn on_input_written(key: usize, msg: &Message) {
    if ENABLED_INPUTS.load(Ordering::Relaxed) == 0 || !is_input(msg) {
        return;
    }
    let now = Instant::now();
    let mut map = INPUTS.lock().unwrap();
    let Some(s) = map.get_mut(&key) else {
        return;
    };
    s.backlog_max = s.backlog_max.max(s.queued.len());
    // None when measuring started while the message was already queued.
    if let Some(queued) = s.queued.pop_front() {
        s.written.push(now.saturating_duration_since(queued));
        s.count += 1;
    }
}

/// Called once per second from io_loop: starts or stops measuring the session's input
/// (`enabled`: `is_measuring`), and logs the `LATENCY client input` line every 5 s.
pub fn refresh_input(key: usize, enabled: bool, via: &str) {
    let line = {
        let mut map = INPUTS.lock().unwrap();
        if !enabled {
            map.remove(&key);
            ENABLED_INPUTS.store(map.len(), Ordering::Relaxed);
            return;
        }
        map.entry(key).or_insert_with(InputLatency::new);
        ENABLED_INPUTS.store(map.len(), Ordering::Relaxed);
        let Some(s) = map.get_mut(&key) else {
            return;
        };
        let now = Instant::now();
        if now.saturating_duration_since(s.window_start) < REPORT_INTERVAL {
            return;
        }
        let line = (s.count > 0).then(|| {
            format!(
                "LATENCY client input via={} in={} n={} backlog_max={}",
                if via.is_empty() { "?" } else { via },
                format_summary(s.written.summary()),
                s.count,
                s.backlog_max,
            )
        });
        s.window_start = now;
        s.written.clear();
        s.count = 0;
        s.backlog_max = 0;
        line
    };
    if let Some(line) = line {
        log::info!("{}", line);
    }
}

pub fn remove_input(key: usize) {
    let mut map = INPUTS.lock().unwrap();
    map.remove(&key);
    ENABLED_INPUTS.store(map.len(), Ordering::Relaxed);
}

/// One played audio frame: the playback buffer level before it was appended, what the
/// buffer shrink dropped, and the output underruns since the previous frame.
#[derive(Default, Clone, Copy)]
pub struct AudioLevelSample {
    pub level: Duration,
    pub trimmed: Duration,
    pub underruns: usize,
    /// The adaptive target (`audio-target-ms`) after this frame; None under the legacy rule.
    #[cfg(not(target_os = "linux"))]
    pub target: Option<super::audio_target::AudioTargetState>,
}

/// The `LATENCY client audio` line, kept by a session's audio thread.
#[derive(Default)]
pub struct AudioLevel {
    window_start: Option<Instant>,
    level: Samples,
    frames: usize,
    trims: usize,
    trimmed: Duration,
    underruns: usize,
    #[cfg(not(target_os = "linux"))]
    target: Option<super::audio_target::AudioTargetState>,
}

impl AudioLevel {
    /// `sample` is None while measurement is off, which drops the window.
    pub fn on_frame(&mut self, sample: Option<AudioLevelSample>) {
        let Some(sample) = sample else {
            if self.window_start.is_some() {
                *self = Self::default();
            }
            return;
        };
        let now = Instant::now();
        let start = *self.window_start.get_or_insert(now);
        self.level.push(sample.level);
        self.frames += 1;
        if !sample.trimmed.is_zero() {
            self.trims += 1;
            self.trimmed += sample.trimmed;
        }
        self.underruns += sample.underruns;
        #[cfg(not(target_os = "linux"))]
        {
            self.target = sample.target;
        }
        if now.saturating_duration_since(start) < REPORT_INTERVAL {
            return;
        }
        #[cfg(not(target_os = "linux"))]
        let target = match self.target {
            Some(t) => format!(
                " target={} late95={} boost={}",
                t.target.as_millis(),
                t.late95.as_millis(),
                t.boost.as_millis()
            ),
            None => " target=legacy".to_owned(),
        };
        #[cfg(target_os = "linux")]
        let target = "";
        log::info!(
            "LATENCY client audio level={} trim={} trimmed={}ms underrun={} frames={}{}",
            format_min_p50_max(self.level.summary()),
            self.trims,
            self.trimmed.as_millis(),
            self.underruns,
            self.frames,
            target,
        );
        *self = Self {
            window_start: Some(now),
            ..Default::default()
        };
    }
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub fn mark_ui_dropped() {
    UI_DROPPED.with(|d| d.set(true));
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub fn mark_ui_replaced() {
    UI_REPLACED.with(|d| d.set(true));
}

/// A soft-rendered frame handed to Flutter: which stream, and when.
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
#[derive(Clone, Copy)]
pub struct UiHandoff {
    key: usize,
    at: Instant,
}

/// Called by the soft renderer on the video thread when it marks a new buffer ready for
/// Flutter, just before posting `EventToUI::Rgba`. None while measurement is off or when
/// not called on a video thread.
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub fn ui_handoff() -> Option<UiHandoff> {
    if !is_any_enabled() {
        return None;
    }
    let key = THREAD_STREAM.with(|s| s.get())?;
    Some(UiHandoff {
        key,
        at: Instant::now(),
    })
}

/// Called when Flutter released the buffer (`next_rgba`, after it created the `ui.Image`).
/// Must not be called while holding the renderer's `display_rgbas` lock.
#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub fn on_ui_taken(handoff: UiHandoff) {
    let take = handoff.at.elapsed();
    let mut map = STREAMS.lock().unwrap();
    if let Some(s) = map.get_mut(&handoff.key) {
        s.ui_take.push(take);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_nearest_rank() {
        let mut s = Samples::default();
        for ms in 1..=100u64 {
            s.push(Duration::from_millis(ms));
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.p50, 50_000);
        assert_eq!(sum.p95, 95_000);
        assert_eq!(sum.max, 100_000);
        assert!(Samples::default().summary().is_none());
    }

    #[test]
    fn excess_over_best_frame_plus_half_rtt() {
        let mut s = StreamLatency::new(0);
        let t0 = s.base + Duration::from_millis(1000);
        // Frame 1 arrives 10 ms after pts 0, frame 2 arrives 25 ms after pts 33.
        s.on_received(0, t0 + Duration::from_millis(10));
        s.on_received(33, t0 + Duration::from_millis(33 + 25));
        s.on_rtt(0);
        s.on_rtt(4);
        let p1 = s.take_pending(0).unwrap();
        let p2 = s.take_pending(33).unwrap();
        assert_eq!(s.net_srv_estimate_us(p1.transit_us), Some(2_000));
        assert_eq!(s.net_srv_estimate_us(p2.transit_us), Some(17_000));
    }

    #[test]
    fn anchor_expires_after_static_screen() {
        let mut s = StreamLatency::new(0);
        let t0 = s.base + Duration::from_millis(1000);
        // A fast frame, then nothing for a minute, then a frame that arrives 20 ms later
        // relative to its pts (as host clock drift would show it).
        s.on_received(0, t0 + Duration::from_millis(10));
        s.on_received(60_000, t0 + Duration::from_millis(60_000 + 30));
        let p2 = s.take_pending(60_000).unwrap();
        // The old bucket is gone, so the new frame is its own anchor.
        assert_eq!(s.net_srv_estimate_us(p2.transit_us), Some(0));
    }

    #[test]
    fn input_send_time_counts_input_messages_only() {
        let key = usize::MAX - 7;
        let mut input = Message::new();
        input.set_mouse_event(Default::default());
        let other = Message::new();
        // Not registered yet: nothing is recorded.
        on_input_queued(key, &input);
        refresh_input(key, true, "TCP");
        on_input_queued(key, &input);
        on_input_queued(key, &other);
        on_input_queued(key, &input);
        on_input_written(key, &other);
        on_input_written(key, &input);
        {
            let map = INPUTS.lock().unwrap();
            let s = map.get(&key).unwrap();
            assert_eq!(s.count, 1);
            assert_eq!(s.queued.len(), 1);
            assert_eq!(s.backlog_max, 2);
        }
        remove_input(key);
        assert!(INPUTS.lock().unwrap().get(&key).is_none());
    }

    #[test]
    fn stream_counts_become_rates_in_the_report() {
        let mut s = StreamLatency::new(0);
        let tick = VideoCountersTick {
            received: 60,
            decoded: 58,
            shown: 57,
            ..Default::default()
        };
        s.add_counts(&tick, Duration::from_secs(1));
        s.add_counts(&tick, Duration::from_secs(1));
        s.frames = 1;
        let line = s.maybe_report(s.window_start + REPORT_INTERVAL).unwrap();
        assert!(line.contains("rx_fps=60.0 dec_fps=58.0 shown_fps=57.0 skip=2 "));
        let json = s.latest.as_ref().unwrap();
        assert_eq!(json["dfps"], 58.0);
        assert_eq!(json["skip"], 1.0);
        assert_eq!(s.counts_secs, 0.0);
    }

    fn pacer() -> super::super::PresentPacer {
        super::super::PresentPacer::new(Arc::new(super::super::PresentPacing {
            enabled: true,
            key_frames_pending: Default::default(),
            newest_pts: std::sync::atomic::AtomicI64::new(i64::MIN),
            decode_no_output: true,
        }))
    }

    fn receive(p: &super::super::PresentPacer, pts: i64) {
        p.shared.newest_pts.store(pts, Ordering::Relaxed);
    }

    #[test]
    fn present_pacing_latency_shows_pairs_and_skips_bursts() {
        let mut p = pacer();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // Steady 60 fps, nothing waiting: every frame is shown (the first one unconditionally).
        for i in 0..10i64 {
            receive(&p, i * 17);
            assert!(p.should_present(false, Some(i * 17), 0, ms(i as u64 * 17)));
        }
        // A pair (Wi-Fi aggregation): the next frame waits, one interval newer. Both shown.
        receive(&p, 187);
        assert!(p.should_present(false, Some(170), 1, ms(190)));
        assert!(p.should_present(false, Some(187), 0, ms(193)));
        // A burst after a 200 ms stall: older frames are skipped, the newest is shown.
        receive(&p, 391);
        for i in 0..11i64 {
            let pts = 204 + i * 17;
            let at = ms(400 + i as u64 * 3);
            assert!(!p.should_present(false, Some(pts), (11 - i) as usize, at));
        }
        assert!(p.should_present(false, Some(391), 0, ms(436)));
        // A key frame is always shown.
        receive(&p, 500);
        assert!(p.should_present(true, Some(408), 5, ms(440)));
    }

    #[test]
    fn present_pacing_latency_rechecks_after_decode() {
        let mut p = pacer();
        assert!(p.decode_without_output());
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        // First frame: shown unconditionally, no recheck.
        receive(&p, 0);
        assert!(p.should_present(false, Some(0), 0, ms(0)));
        receive(&p, 100);
        assert!(p.still_present(Some(0), 3, ms(5)));
        // Nothing waits before the decode: decoded with output. Two newer frames arrived during
        // the decode: only the UI callback is skipped.
        receive(&p, 17);
        assert!(p.should_present(false, Some(17), 0, ms(20)));
        receive(&p, 51);
        assert!(!p.still_present(Some(17), 2, ms(30)));
        // Nothing arrived during the decode: shown.
        receive(&p, 68);
        assert!(p.should_present(false, Some(68), 0, ms(70)));
        assert!(p.still_present(Some(68), 0, ms(75)));
        // A key frame is never rechecked.
        receive(&p, 85);
        assert!(p.should_present(true, Some(85), 0, ms(90)));
        receive(&p, 200);
        assert!(p.still_present(Some(85), 5, ms(95)));
    }

    #[test]
    fn present_pacing_latency_forces_a_frame_while_behind() {
        let mut p = pacer();
        let t0 = Instant::now();
        assert!(p.should_present(false, Some(0), 0, t0));
        // The decoder stays behind: one frame always waits, far newer than this one.
        receive(&p, 10_000);
        let mut shown = Vec::new();
        for i in 1..=40u64 {
            let at = t0 + Duration::from_millis(i * 5);
            if p.should_present(false, Some(i as i64 * 17), 3, at) {
                shown.push(i * 5);
            }
        }
        // One shown frame per 50 ms of backlog, counted from when the backlog began.
        assert_eq!(shown, vec![55, 105, 155]);
        // Reset: the next frame is shown whatever waits.
        p.reset();
        assert!(p.should_present(false, Some(800), 3, t0 + Duration::from_millis(210)));
    }

    #[test]
    fn skipped_frames_count_apart_from_shown_ones() {
        let mut s = StreamLatency::new(0);
        let t0 = s.base + Duration::from_millis(1000);
        s.on_received(0, t0);
        s.on_received(17, t0 + Duration::from_millis(1));
        let frame = |pts: i64, at: Instant| DecodedFrame {
            pts: Some(pts),
            dequeued: at,
            decode_start: at,
            decode_end: at + Duration::from_millis(4),
            format: CodecFormat::H265,
            gpu_texture: true,
            paced: true,
            no_output: pts == 0,
        };
        s.on_frame_skipped(frame(0, t0 + Duration::from_millis(2)), 1);
        s.on_frame_done(
            frame(17, t0 + Duration::from_millis(6)),
            t0 + Duration::from_millis(12),
            false,
            true,
            0,
        );
        assert!(s.pending.is_empty());
        assert_eq!(s.frames, 2);
        assert_eq!(s.decode.0.len(), 1);
        assert_eq!(s.decode_skip.0.len(), 1);
        assert_eq!(s.queue.0.len(), 1);
        assert_eq!(s.net_srv.0.len(), 2);
        let line = s.maybe_report(s.window_start + REPORT_INTERVAL).unwrap();
        assert!(line.contains(" pacing=on "));
        assert!(line.contains(" ui_replaced=1 "));
        assert!(line.contains(" decode_skip=4.0/4.0/4.0 "));
        assert!(line.contains(" decode_skip_conv=n/a "));
    }

    #[test]
    fn latency_converted_skips_stay_out_of_decode_skip() {
        let mut s = StreamLatency::new(0);
        let t0 = s.base + Duration::from_millis(1000);
        let frame = |no_output: bool, at: Instant, ms: u64| DecodedFrame {
            pts: None,
            dequeued: at,
            decode_start: at,
            decode_end: at + Duration::from_millis(ms),
            format: CodecFormat::H264,
            gpu_texture: true,
            paced: true,
            no_output,
        };
        s.on_frame_skipped(frame(true, t0, 1), 1);
        s.on_frame_skipped(frame(false, t0 + Duration::from_millis(5), 6), 1);
        assert_eq!(s.decode_skip.0.len(), 1);
        assert_eq!(s.decode_skip_conv.0.len(), 1);
        let line = s.maybe_report(s.window_start + REPORT_INTERVAL).unwrap();
        assert!(line.contains(" decode_skip=1.0/1.0/1.0 "));
        assert!(line.contains(" decode_skip_conv=6.0/6.0/6.0 "));
    }

    #[test]
    fn shown_gap_skips_replaced_frames() {
        let mut s = StreamLatency::new(0);
        let t0 = s.base + Duration::from_millis(1000);
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let frame = |at: Instant| DecodedFrame {
            pts: None,
            dequeued: at,
            decode_start: at,
            decode_end: at,
            format: CodecFormat::H265,
            gpu_texture: false,
            paced: true,
            no_output: false,
        };
        s.on_frame_done(frame(ms(0)), ms(0), false, false, 0);
        s.on_frame_done(frame(ms(17)), ms(17), false, false, 0);
        // Replaces the frame of 17 ms before Flutter fetched it: the gap is 0 -> 20.
        s.on_frame_done(frame(ms(20)), ms(20), false, true, 0);
        s.on_frame_done(frame(ms(34)), ms(34), false, false, 0);
        assert_eq!(s.shown_gap.0, vec![20_000]);
        assert_eq!(s.pending_gap, Some(Duration::from_millis(14)));
    }
}
