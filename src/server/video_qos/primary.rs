//! Several viewers, one encoder: the "primary viewer" rule (roadmap M18), host option
//! `enable-primary-viewer-rule` (Windows; off unless "Y").
//!
//! - PRIMARY: connections with control (keyboard permission on a remote-control session) and
//!   input within `PRIMARY_INPUT_WINDOW`; when none qualifies, every connection. Only primary
//!   connections steer the shared stream's fps, bitrate and quality (video_qos.rs).
//! - LAGGING: a non-primary connection that keeps falling behind the stream
//!   (`LAG_ENTER_SECS` seconds in a row) until it keeps up for `LAG_LEAVE_SECS`. The capture
//!   loop waits for it at most one frame period, and its video queue is bounded: with
//!   `LAG_QUEUE_FRAMES` frames still unwritten its next frames are dropped up to the next key
//!   frame, which the capture loop requests at most once per `LAG_KEY_INTERVAL` per display.
//! - Fps floor for viewer requests: a viewer's custom_fps below the one it logged in with
//!   (stock viewers halve it under load) counts as at least min(login value, `REQUEST_FLOOR_FPS`).
//!
//! `VideoGate` is the per-connection state the capture thread (enqueue), the connection or its
//! writer task (written) and the connection loop (input, control) share; atomics only. See
//! docs/LATENCY-METRICS.md ("PRIMARY").

use base::{config::keys, message_proto::Message};
use hbb_common::{config::Config, log};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

/// Input this recent makes a connection with control primary.
pub const PRIMARY_INPUT_WINDOW: Duration = Duration::from_secs(30);
/// Unwritten frames (per display) at which a lagging connection starts dropping to a key frame,
/// and the queue peak that counts as falling behind.
pub const LAG_QUEUE_FRAMES: u32 = 3;
/// Seconds in a row behind before a non-primary connection is LAGGING; clean seconds in a row
/// before it is not.
pub const LAG_ENTER_SECS: u8 = 2;
pub const LAG_LEAVE_SECS: u8 = 5;
/// A non-primary connection whose own fast estimator target is below this share of the stream's
/// is behind.
pub const LAG_TARGET_SHARE: f32 = 0.6;
/// At most one key frame per display per this interval for lagging connections.
pub const LAG_KEY_INTERVAL: Duration = Duration::from_secs(3);
/// A viewer's lowered custom_fps counts as at least min(its login custom_fps, this).
pub const REQUEST_FLOOR_FPS: u32 = 24;

// Displays a gate tracks; frames of higher display indexes are never dropped.
const MAX_DISPLAYS: usize = 8;

lazy_static::lazy_static! {
    static ref GATES: Mutex<HashMap<i32, Weak<VideoGate>>> = Default::default();
    // The LAGGING connection ids, for the capture loops (`LagCache`).
    static ref LAGGING: Mutex<Arc<HashSet<i32>>> = Default::default();
    static ref EPOCH: Instant = Instant::now();
}
static LAGGING_GEN: AtomicU64 = AtomicU64::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const ZERO_U32: AtomicU32 = AtomicU32::new(0);
#[allow(clippy::declare_interior_mutable_const)]
const FALSE: AtomicBool = AtomicBool::new(false);
// Connections dropping frames of each display until a key frame.
static DROPPING: [AtomicU32; MAX_DISPLAYS] = [ZERO_U32; MAX_DISPLAYS];
// The encoder of this display refused a forced key frame: no drop-to-key there (sticky until a
// forced key frame is accepted again).
static KEY_UNSUPPORTED: [AtomicBool; MAX_DISPLAYS] = [FALSE; MAX_DISPLAYS];

/// enable-primary-viewer-rule: Windows hosts, off unless "Y". Read once per second.
pub fn rule_option() -> bool {
    cfg!(windows) && Config::get_option(keys::OPTION_ENABLE_PRIMARY_VIEWER_RULE) == "Y"
}

fn now_ms() -> u64 {
    // 0 means "never"; the epoch is taken before the first gate exists.
    EPOCH.elapsed().as_millis() as u64 + 1
}

/// Per-connection video gate (see the module doc).
#[derive(Default)]
pub struct VideoGate {
    // Set by the policy tick (VideoQoS) only while the rule is on.
    lagging: AtomicBool,
    // Bit per display: dropping frames until a key frame.
    dropping: AtomicU8,
    // Video frames enqueued and not yet written, per display.
    queued: [AtomicU32; MAX_DISPLAYS],
    // Since the last `take`: the highest number of frames already unwritten when a frame was
    // enqueued, frames dropped, drop runs started.
    peak: AtomicU32,
    dropped: AtomicU32,
    runs: AtomicU32,
    last_input_ms: AtomicU64,
    control: AtomicBool,
}

/// What the policy tick reads from a gate once per second (counters since the last tick).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct GateInfo {
    pub control: bool,
    // Since the last input; None: never.
    pub input_age: Option<Duration>,
    pub peak: u32,
    pub dropped: u32,
    pub runs: u32,
}

impl VideoGate {
    /// A frame of `display` is about to be queued for this connection: false drops it. `key`
    /// is asked for only while the connection is lagging or dropping.
    pub fn on_enqueue(&self, display: usize, key: impl FnOnce() -> bool) -> bool {
        if display >= MAX_DISPLAYS {
            return true;
        }
        let bit = 1u8 << display;
        if self.dropping.load(Ordering::Relaxed) & bit != 0 {
            if !key() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            // The key frame ends the run; the decoder can start over from it.
            self.dropping.fetch_and(!bit, Ordering::Relaxed);
            DROPPING[display].fetch_sub(1, Ordering::Relaxed);
        } else if self.lagging.load(Ordering::Relaxed)
            && key_on_demand(display)
            && self.queued[display].load(Ordering::Relaxed) >= LAG_QUEUE_FRAMES
            && !key()
        {
            self.dropping.fetch_or(bit, Ordering::Relaxed);
            DROPPING[display].fetch_add(1, Ordering::Relaxed);
            self.runs.fetch_add(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let before = self.queued[display].fetch_add(1, Ordering::Relaxed);
        self.peak.fetch_max(before, Ordering::Relaxed);
        true
    }

    /// A video frame of `display` left this connection's queue (written, or the write failed).
    pub fn on_written(&self, display: usize) {
        if display < MAX_DISPLAYS {
            let queued = &self.queued[display];
            let mut q = queued.load(Ordering::Relaxed);
            while q > 0 {
                match queued.compare_exchange_weak(q, q - 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(now) => q = now,
                }
            }
        }
    }

    /// Input from this connection's viewer (mouse, key, touch) that the host takes.
    pub fn on_input(&self) {
        self.last_input_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub fn set_control(&self, control: bool) {
        self.control.store(control, Ordering::Relaxed);
    }

    fn take(&self) -> GateInfo {
        let last = self.last_input_ms.load(Ordering::Relaxed);
        GateInfo {
            control: self.control.load(Ordering::Relaxed),
            input_age: (last > 0).then(|| Duration::from_millis(now_ms().saturating_sub(last))),
            peak: self.peak.swap(0, Ordering::Relaxed),
            dropped: self.dropped.swap(0, Ordering::Relaxed),
            runs: self.runs.swap(0, Ordering::Relaxed),
        }
    }
}

impl Drop for VideoGate {
    fn drop(&mut self) {
        let mask = *self.dropping.get_mut();
        for d in 0..MAX_DISPLAYS {
            if mask & (1 << d) != 0 {
                DROPPING[d].fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

/// A video frame message: its display and whether it carries a key frame (lazily).
pub fn video_display(msg: &Message) -> Option<usize> {
    match &msg.union {
        Some(base::message_proto::message::Union::VideoFrame(vf)) => Some(vf.display as usize),
        _ => None,
    }
}

pub fn is_key(msg: &Message) -> bool {
    super::super::latency_stats::encoded_frames(msg)
        .is_some_and(|frames| frames.iter().any(|f| f.key))
}

/// The gate of a new connection.
pub fn register(id: i32) -> Arc<VideoGate> {
    let gate = Arc::new(VideoGate::default());
    let mut gates = GATES.lock().unwrap();
    gates.retain(|_, g| g.strong_count() > 0);
    gates.insert(id, Arc::downgrade(&gate));
    gate
}

/// Once per second, for the policy tick: every live gate's state, counters reset.
pub fn take_infos() -> HashMap<i32, GateInfo> {
    let mut gates = GATES.lock().unwrap();
    gates.retain(|_, g| g.strong_count() > 0);
    gates
        .iter()
        .filter_map(|(id, g)| g.upgrade().map(|g| (*id, g.take())))
        .collect()
}

/// The policy's LAGGING set, to the gates and the capture loops.
pub fn publish_lagging(ids: &HashSet<i32>) {
    {
        let gates = GATES.lock().unwrap();
        for (id, g) in gates.iter() {
            if let Some(g) = g.upgrade() {
                g.lagging.store(ids.contains(id), Ordering::Relaxed);
            }
        }
    }
    let mut lagging = LAGGING.lock().unwrap();
    if **lagging != *ids {
        *lagging = Arc::new(ids.clone());
        LAGGING_GEN.fetch_add(1, Ordering::Relaxed);
    }
}

/// The capture loop's copy of the LAGGING set, refreshed when the policy changes it.
#[derive(Default)]
pub struct LagCache {
    gen: u64,
    ids: Arc<HashSet<i32>>,
}

impl LagCache {
    pub fn get(&mut self) -> &HashSet<i32> {
        let gen = LAGGING_GEN.load(Ordering::Relaxed);
        if gen != self.gen {
            self.ids = LAGGING.lock().unwrap().clone();
            self.gen = gen;
        }
        &self.ids
    }
}

/// Lagging connections of this display may be left behind: their frames can be dropped to a
/// key frame the capture loop can ask for (Windows, forced key frames: roadmap M13).
pub fn key_on_demand(display: usize) -> bool {
    cfg!(windows) && display < MAX_DISPLAYS && !KEY_UNSUPPORTED[display].load(Ordering::Relaxed)
}

/// Some connection waits for a key frame of this display.
pub fn key_wanted(display: usize) -> bool {
    display < MAX_DISPLAYS && DROPPING[display].load(Ordering::Relaxed) > 0
}

/// Connections dropping frames of this display until a key frame.
pub fn dropping(display: usize) -> u32 {
    if display < MAX_DISPLAYS {
        DROPPING[display].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// The encoder of this display refused a forced key frame: lagging connections are no longer
/// dropped to key frames there until the host process restarts (the capture loop restarts the
/// encoder once, which gives the dropping connections their key frame).
pub fn set_key_unsupported(display: usize) {
    if display < MAX_DISPLAYS && !KEY_UNSUPPORTED[display].swap(true, Ordering::Relaxed) {
        log::warn!(
            "PRIMARY display={display} encoder has no forced key frame: lagging viewers are no longer dropped to key frames"
        );
    }
}

/// The capture loop's wait: whether it may stop waiting for this frame. Every connection that
/// is not lagging has fetched it, and the lagging ones have had their grace of one frame period
/// (or fetched it too). `slice`: how long the next wait may take.
pub fn wait_done(
    send: &HashSet<i32>,
    fetched: &HashSet<i32>,
    lagging: &HashSet<i32>,
    waited: Duration,
    grace: Duration,
) -> (bool, Option<Duration>) {
    if fetched.len() >= send.len() {
        return (true, None);
    }
    if lagging.is_empty() {
        return (false, None);
    }
    let only_lagging = send
        .iter()
        .all(|id| fetched.contains(id) || lagging.contains(id));
    if !only_lagging {
        return (false, None);
    }
    if waited >= grace {
        return (true, None);
    }
    (false, Some(grace - waited))
}

/// Per-connection policy state, kept by VideoQoS.
#[derive(Debug, Default, Clone)]
pub struct ConnPolicy {
    // Counts for the stream (primary, or the rule is not in force).
    pub primary: bool,
    pub lagging: bool,
    behind: u8,
    clean: u8,
    // Drops since the last drop log line.
    dropped: u32,
    runs: u32,
    peak: u32,
}

impl ConnPolicy {
    /// One second of this non-primary connection: `behind` whether it fell behind the stream.
    /// Returns whether its LAGGING state changed.
    pub fn second(&mut self, behind: bool, info: &GateInfo) -> bool {
        self.dropped += info.dropped;
        self.runs += info.runs;
        self.peak = self.peak.max(info.peak);
        if behind {
            self.behind = self.behind.saturating_add(1);
            self.clean = 0;
        } else {
            self.clean = self.clean.saturating_add(1);
            self.behind = 0;
        }
        let was = self.lagging;
        if !self.lagging && self.behind >= LAG_ENTER_SECS {
            self.lagging = true;
        } else if self.lagging && self.clean >= LAG_LEAVE_SECS {
            self.lagging = false;
        }
        was != self.lagging
    }

    /// The connection is primary (or the rule is off): no lagging state.
    pub fn reset(&mut self, primary: bool) {
        *self = ConnPolicy {
            primary,
            dropped: self.dropped,
            runs: self.runs,
            peak: self.peak,
            ..Default::default()
        };
    }

    /// Drop counters for the periodic log line, reset.
    pub fn take_drops(&mut self) -> Option<(u32, u32, u32)> {
        if self.dropped == 0 && self.runs == 0 {
            self.peak = 0;
            return None;
        }
        let r = (self.dropped, self.runs, self.peak);
        self.dropped = 0;
        self.runs = 0;
        self.peak = 0;
        Some(r)
    }
}

/// The primary set from the connections' gates: control and recent input; all when none.
pub fn primary_set(ids: impl Iterator<Item = i32>, infos: &HashMap<i32, GateInfo>) -> HashSet<i32> {
    let ids: Vec<i32> = ids.collect();
    let primary: HashSet<i32> = ids
        .iter()
        .copied()
        .filter(|id| {
            infos.get(id).is_some_and(|i| {
                i.control && i.input_age.is_some_and(|age| age <= PRIMARY_INPUT_WINDOW)
            })
        })
        .collect();
    if primary.is_empty() {
        ids.into_iter().collect()
    } else {
        primary
    }
}

/// The fps a viewer's request stands for under the rule: a custom_fps lowered below the one it
/// logged in with counts as at least min(that, REQUEST_FLOOR_FPS).
pub fn floored_custom_fps(custom: u32, login: Option<u32>) -> u32 {
    let login = login.unwrap_or(custom);
    if custom >= login {
        return custom;
    }
    custom.max(login.min(REQUEST_FLOOR_FPS))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_lagging() -> VideoGate {
        let g = VideoGate::default();
        g.lagging.store(true, Ordering::Relaxed);
        g
    }

    // Uses display 7 so tests on other displays do not see its DROPPING count.
    #[test]
    fn latency_primary_lagging_gate_drops_to_key_frame() {
        let d = 7;
        let g = gate_lagging();
        for _ in 0..LAG_QUEUE_FRAMES {
            assert!(g.on_enqueue(d, || false));
        }
        let before = DROPPING[d].load(Ordering::Relaxed);
        // A full queue: the next delta frame starts a drop run.
        assert!(!g.on_enqueue(d, || false));
        assert_eq!(DROPPING[d].load(Ordering::Relaxed), before + 1);
        assert!(key_wanted(d) || !cfg!(windows));
        // The queue drains, but delta frames keep being dropped until a key frame.
        for _ in 0..LAG_QUEUE_FRAMES {
            g.on_written(d);
        }
        assert!(!g.on_enqueue(d, || false));
        assert!(g.on_enqueue(d, || true));
        assert_eq!(DROPPING[d].load(Ordering::Relaxed), before);
        let info = g.take();
        assert_eq!(info.dropped, 2);
        assert_eq!(info.runs, 1);
        assert_eq!(info.peak, LAG_QUEUE_FRAMES - 1);
    }

    #[test]
    fn latency_primary_gate_not_lagging_never_drops() {
        let g = VideoGate::default();
        for _ in 0..10 {
            assert!(g.on_enqueue(6, || panic!("key asked")));
        }
        assert_eq!(g.take().peak, 9);
        // Over-written does not underflow.
        for _ in 0..20 {
            g.on_written(6);
        }
        assert!(g.on_enqueue(6, || false));
        assert_eq!(g.take().peak, 0);
    }

    #[test]
    fn latency_primary_lagging_gate_passes_key_frame_with_full_queue() {
        let g = gate_lagging();
        for _ in 0..5 {
            assert!(g.on_enqueue(5, || true));
        }
        assert_eq!(g.take().dropped, 0);
    }

    #[test]
    fn latency_primary_gate_drop_releases_count() {
        let d = 4;
        let before = DROPPING[d].load(Ordering::Relaxed);
        {
            let g = gate_lagging();
            for _ in 0..LAG_QUEUE_FRAMES {
                g.on_enqueue(d, || false);
            }
            assert!(!g.on_enqueue(d, || false));
            assert_eq!(DROPPING[d].load(Ordering::Relaxed), before + 1);
        }
        assert_eq!(DROPPING[d].load(Ordering::Relaxed), before);
    }

    #[test]
    fn latency_primary_set_needs_control_and_recent_input() {
        let mut infos = HashMap::new();
        let recent = Some(Duration::from_secs(1));
        infos.insert(
            1,
            GateInfo {
                control: true,
                input_age: recent,
                ..Default::default()
            },
        );
        infos.insert(
            2,
            GateInfo {
                control: false,
                input_age: recent,
                ..Default::default()
            },
        );
        infos.insert(
            3,
            GateInfo {
                control: true,
                input_age: Some(Duration::from_secs(31)),
                ..Default::default()
            },
        );
        let set = primary_set([1, 2, 3].into_iter(), &infos);
        assert_eq!(set, HashSet::from([1]));
        infos.remove(&1);
        let set = primary_set([2, 3].into_iter(), &infos);
        assert_eq!(set, HashSet::from([2, 3]));
    }

    #[test]
    fn latency_primary_wait_grace_for_lagging_only() {
        let send = HashSet::from([1, 2]);
        let lag = HashSet::from([2]);
        let spf = Duration::from_millis(16);
        let none = HashSet::new();
        assert_eq!(
            wait_done(
                &send,
                &HashSet::from([1]),
                &none,
                Duration::from_secs(1),
                spf
            ),
            (false, None)
        );
        assert_eq!(
            wait_done(
                &send,
                &HashSet::from([2]),
                &lag,
                Duration::from_secs(1),
                spf
            ),
            (false, None)
        );
        assert_eq!(
            wait_done(
                &send,
                &HashSet::from([1]),
                &lag,
                Duration::from_millis(6),
                spf
            ),
            (false, Some(Duration::from_millis(10)))
        );
        assert_eq!(
            wait_done(
                &send,
                &HashSet::from([1]),
                &lag,
                Duration::from_millis(16),
                spf
            ),
            (true, None)
        );
        assert_eq!(
            wait_done(&send, &send, &lag, Duration::ZERO, spf),
            (true, None)
        );
    }

    #[test]
    fn latency_primary_lagging_hysteresis() {
        let mut p = ConnPolicy::default();
        let info = GateInfo::default();
        assert!(!p.second(true, &info));
        assert!(p.second(true, &info));
        assert!(p.lagging);
        for _ in 0..LAG_LEAVE_SECS - 1 {
            assert!(!p.second(false, &info));
        }
        // A behind second restarts the clean count.
        assert!(!p.second(true, &info));
        for _ in 0..LAG_LEAVE_SECS - 1 {
            assert!(!p.second(false, &info));
        }
        assert!(p.second(false, &info));
        assert!(!p.lagging);
    }

    #[test]
    fn latency_primary_custom_fps_floor() {
        assert_eq!(floored_custom_fps(15, Some(30)), 24);
        assert_eq!(floored_custom_fps(30, Some(30)), 30);
        assert_eq!(floored_custom_fps(60, Some(30)), 60);
        assert_eq!(floored_custom_fps(10, Some(20)), 20);
        assert_eq!(floored_custom_fps(8, None), 8);
    }
}
