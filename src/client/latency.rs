//! Viewer-side latency measurement of the video path. Measurement and logging only;
//! nothing here changes how frames are queued, decoded or shown.
//! What each number means and its error bounds: docs/LATENCY-METRICS.md.

use base::{
    config::keys,
    message_proto::{video_frame, TestDelay, VideoFrame},
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

lazy_static::lazy_static! {
    // Keyed by `stream_key()` of a video thread's queue, so concurrent sessions do not mix.
    static ref STREAMS: Mutex<HashMap<usize, StreamLatency>> = Default::default();
}

// Number of entries in STREAMS. The per-frame hooks return before locking while it is 0,
// so with measurement off they never touch the lock.
static ENABLED_STREAMS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Set by the soft renderer, on the video thread, when it drops a frame because the UI
    // has not taken the previous one yet.
    static UI_DROPPED: Cell<bool> = Cell::new(false);
    // The stream of the video thread this runs on, set by `set_thread_stream`.
    static THREAD_STREAM: Cell<Option<usize>> = Cell::new(None);
}

fn is_any_enabled() -> bool {
    ENABLED_STREAMS.load(Ordering::Relaxed) > 0
}

/// Durations of one report window, in microseconds, bounded to `MAX_SAMPLES`.
#[derive(Default)]
pub struct Samples(Vec<u32>);

#[derive(Clone, Copy)]
pub struct Summary {
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
    queue_len_max: usize,
    codec: Option<CodecFormat>,
    gpu_texture: bool,
    net_srv: Samples,
    queue: Samples,
    decode: Samples,
    to_ui: Samples,
    ui_take: Samples,
    total: Samples,
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
            queue_len_max: 0,
            codec: None,
            gpu_texture: false,
            net_srv: Default::default(),
            queue: Default::default(),
            decode: Default::default(),
            to_ui: Default::default(),
            ui_take: Default::default(),
            total: Default::default(),
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

    fn on_frame_done(
        &mut self,
        frame: DecodedFrame,
        ui_done: Instant,
        ui_dropped: bool,
        queue_len: usize,
    ) {
        self.frames += 1;
        self.codec = Some(frame.format);
        self.gpu_texture = frame.gpu_texture;
        self.queue_len_max = self.queue_len_max.max(queue_len);
        self.decode
            .push(frame.decode_end.saturating_duration_since(frame.decode_start));
        let pending = frame.pts.and_then(|pts| self.take_pending(pts));
        let mut net = None;
        if let Some(p) = &pending {
            self.queue.push(frame.dequeued.saturating_duration_since(p.recv));
            net = self.net_srv_estimate_us(p.transit_us).map(Duration::from_micros);
            if let Some(net) = net {
                self.net_srv.push(net);
            }
        }
        if ui_dropped {
            self.ui_dropped += 1;
            return;
        }
        self.to_ui
            .push(ui_done.saturating_duration_since(frame.decode_end));
        if let (Some(p), Some(net)) = (pending, net) {
            self.total.push(net + ui_done.saturating_duration_since(p.recv));
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
            let to_ui = self.to_ui.summary();
            let ui_take = self.ui_take.summary();
            let total = self.total.summary();
            let rtt = self.rtt_estimate_ms();
            let line = format!(
                "LATENCY client display={} codec={} hw={} fps={:.1} net+server(est)={} queue={} decode={} to_ui={} ui_take={} total_to_ui(est)={} offset_err<={} queue_len={} ui_dropped={} rtt_min={}",
                self.display,
                self.codec.map_or("-".to_owned(), |c| c.to_string()),
                self.gpu_texture,
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
                rtt.map_or("n/a".to_owned(), |r| format!("{}ms", r)),
            );
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
            }));
            report = Some(line);
        }
        self.window_start = now;
        self.frames = 0;
        self.ui_dropped = 0;
        self.queue_len_max = 0;
        self.net_srv.clear();
        self.queue.clear();
        self.decode.clear();
        self.to_ui.clear();
        self.ui_take.clear();
        self.total.clear();
        report
    }
}

/// Called once per second from io_loop with this session's video threads as
/// `(display, stream_key)`. Starts or stops measuring them and returns the latest
/// per-display summaries as JSON for the quality monitor.
pub fn refresh_streams(
    streams: impl Iterator<Item = (usize, usize)>,
    show_quality_monitor: bool,
) -> Option<String> {
    let enabled = show_quality_monitor
        || LocalConfig::get_option(keys::OPTION_ENABLE_LATENCY_LOG) == "Y";
    let mut map = STREAMS.lock().unwrap();
    let mut out = serde_json::Map::new();
    for (display, key) in streams {
        if !enabled {
            map.remove(&key);
            continue;
        }
        let s = map.entry(key).or_insert_with(|| StreamLatency::new(display));
        s.display = display;
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

/// Called on the video thread right after the frame was handed to the UI callback.
pub fn on_frame_done(key: usize, frame: DecodedFrame, queue_len: usize) {
    let ui_done = Instant::now();
    let ui_dropped = UI_DROPPED.with(|d| d.replace(false));
    if !is_any_enabled() {
        return;
    }
    let report = {
        let mut map = STREAMS.lock().unwrap();
        let Some(s) = map.get_mut(&key) else {
            return;
        };
        s.on_frame_done(frame, ui_done, ui_dropped, queue_len);
        s.maybe_report(ui_done)
    };
    if let Some(line) = report {
        log::info!("{}", line);
    }
}

#[cfg(any(target_os = "android", target_os = "ios", feature = "flutter"))]
pub fn mark_ui_dropped() {
    UI_DROPPED.with(|d| d.set(true));
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
}
