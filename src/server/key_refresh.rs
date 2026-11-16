//! Roadmap M13/M14 bookkeeping of the capture loop (video_service.rs), Windows hosts only:
//! - `KeyRefresh`: a viewer's RefreshVideo is served by a forced key frame on the running encoder
//!   instead of a capture restart (`SWITCH`), option `enable-refresh-keyframe`.
//! - `StaticRefine`: after a key frame or a large frame, a screen that stays unchanged gets its
//!   last image encoded again a few times, option `enable-static-refine` / `nvenc-vbv-frames`.
//! Pure state, no I/O: the capture loop does the encoding and sending. See
//! docs/LATENCY-METRICS.md.

use super::video_service::Encoded;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

// At most one forced key frame per this interval: a refresh that comes later than a key frame
// was sent but sooner than this waits for it (never dropped: the viewer that sent it discards
// frames until it sees a key frame).
pub(super) const MIN_KEY_INTERVAL: Duration = Duration::from_millis(200);
// This many refreshes served by key frames within `ESCALATE_WINDOW`, and the next refresh
// restarts capture and encoder: the key frames did not help that viewer.
pub(super) const ESCALATE_COUNT: usize = 3;
pub(super) const ESCALATE_WINDOW: Duration = Duration::from_secs(3);
// A requested key frame not encoded from a captured image within min(2 frame periods, this) is
// encoded from the kept last image (a static screen gives DXGI no new image).
const KEPT_KEY_AFTER_MAX: Duration = Duration::from_millis(100);
// A requested key frame that has not come out after this restarts capture and encoder.
pub(super) const KEY_TIMEOUT: Duration = Duration::from_secs(1);

// Static refine: the screen has to stay unchanged this long after the frame worth refining.
pub(super) const REFINE_AFTER: Duration = Duration::from_millis(150);
pub(super) const REFINE_MAX: u32 = 3;
// A frame larger than this many times the running average is worth refining.
const REFINE_LARGE_FACTOR: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshAction {
    /// Ask the encoder for a key frame now.
    Request,
    /// Merged into the pending key frame, or deferred to the minimum key interval.
    Merged,
    /// Restart capture and encoder (`SWITCH`), with this reason.
    Switch(&'static str),
}

#[derive(Default)]
pub(super) struct KeyRefresh {
    // A key frame was requested from the encoder and has not come out yet.
    requested: Option<Instant>,
    // A refresh waits for `MIN_KEY_INTERVAL` after the last key frame.
    deferred: bool,
    last_key: Option<Instant>,
    // When the key frames that served refreshes were encoded, within `ESCALATE_WINDOW`.
    served: VecDeque<Instant>,
}

impl KeyRefresh {
    /// A refresh request reached the capture loop.
    pub(super) fn on_refresh(&mut self, now: Instant) -> RefreshAction {
        while let Some(t) = self.served.front() {
            if now.saturating_duration_since(*t) > ESCALATE_WINDOW {
                self.served.pop_front();
            } else {
                break;
            }
        }
        if self.served.len() >= ESCALATE_COUNT {
            return RefreshAction::Switch("refresh_escalated");
        }
        if self.requested.is_some() || self.deferred {
            return RefreshAction::Merged;
        }
        if let Some(last) = self.last_key {
            if now.saturating_duration_since(last) < MIN_KEY_INTERVAL {
                self.deferred = true;
                return RefreshAction::Merged;
            }
        }
        RefreshAction::Request
    }

    /// A deferred refresh whose minimum key interval has passed: request its key frame now.
    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        if !self.deferred || self.requested.is_some() {
            return false;
        }
        let due = self
            .last_key
            .map(|last| now.saturating_duration_since(last) >= MIN_KEY_INTERVAL)
            .unwrap_or(true);
        if due {
            self.deferred = false;
        }
        due
    }

    /// The encoder accepted a key frame request.
    pub(super) fn on_requested(&mut self, now: Instant) {
        self.requested = Some(now);
    }

    pub(super) fn pending(&self) -> bool {
        self.requested.is_some()
    }

    /// An encoded message. While a key frame is requested it has to be one: Err with the
    /// `SWITCH` reason otherwise. Ok(true): this message is the requested key frame.
    pub(super) fn on_encoded(&mut self, encoded: Encoded, now: Instant) -> Result<bool, &'static str> {
        if encoded.key {
            self.last_key = Some(now);
        }
        if self.requested.is_none() {
            return Ok(false);
        }
        if !encoded.key {
            return Err("refresh_no_key");
        }
        self.requested = None;
        self.served.push_back(now);
        Ok(true)
    }

    /// The requested key frame should now be encoded from the kept last image.
    pub(super) fn kept_due(&self, now: Instant, spf: Duration) -> bool {
        self.requested
            .map(|t| now.saturating_duration_since(t) >= (spf * 2).min(KEPT_KEY_AFTER_MAX))
            .unwrap_or(false)
    }

    /// The requested key frame did not come in time.
    pub(super) fn timed_out(&self, now: Instant) -> bool {
        self.requested
            .map(|t| now.saturating_duration_since(t) >= KEY_TIMEOUT)
            .unwrap_or(false)
    }
}

pub(super) struct StaticRefine {
    enabled: bool,
    // Running average of the encoded size of captured frames, bytes.
    avg: f64,
    // The last captured frame was a key frame or a large one: refine it once the screen stays.
    armed: bool,
    idle_since: Option<Instant>,
    done: u32,
}

impl StaticRefine {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            avg: 0.0,
            armed: false,
            idle_since: None,
            done: 0,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// A refine encode failed: no more refining in this capture loop.
    pub(super) fn disable(&mut self) {
        self.enabled = false;
    }

    /// A newly captured image was encoded.
    pub(super) fn on_frame(&mut self, encoded: Encoded) {
        self.idle_since = None;
        self.done = 0;
        let bytes = encoded.bytes as f64;
        self.armed = encoded.key || (self.avg > 0.0 && bytes > self.avg * REFINE_LARGE_FACTOR);
        self.avg = if self.avg > 0.0 {
            self.avg * 0.9 + bytes * 0.1
        } else {
            bytes
        };
    }

    /// A key frame was encoded from the kept image (refresh on a static screen).
    pub(super) fn on_kept_key(&mut self) {
        self.idle_since = None;
        self.done = 0;
        self.armed = true;
    }

    /// No new image in this loop iteration. True: re-encode the kept image now.
    pub(super) fn on_idle(&mut self, now: Instant) -> bool {
        if !self.enabled || !self.armed || self.done >= REFINE_MAX {
            return false;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= REFINE_AFTER {
            self.done += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Encoded = Encoded {
        key: true,
        bytes: 100_000,
    };
    const DELTA: Encoded = Encoded {
        key: false,
        bytes: 5_000,
    };

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn latency_refresh_requests_merges_and_defers() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        assert_eq!(r.on_refresh(t), RefreshAction::Request);
        r.on_requested(t);
        // A burst while the key is pending merges into it.
        assert_eq!(r.on_refresh(ms(t, 5)), RefreshAction::Merged);
        assert_eq!(r.on_encoded(KEY, ms(t, 16)), Ok(true));
        assert!(!r.pending());
        // Right after the key was sent: deferred, never dropped.
        assert_eq!(r.on_refresh(ms(t, 50)), RefreshAction::Merged);
        assert_eq!(r.on_refresh(ms(t, 60)), RefreshAction::Merged);
        assert!(!r.take_due(ms(t, 100)));
        assert!(r.take_due(ms(t, 216)));
        assert!(!r.take_due(ms(t, 230)));
    }

    #[test]
    fn latency_refresh_escalates_after_served_keys() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        for i in 0..ESCALATE_COUNT as u64 {
            let at = ms(t, i * 500);
            assert_eq!(r.on_refresh(at), RefreshAction::Request);
            r.on_requested(at);
            assert_eq!(r.on_encoded(KEY, at), Ok(true));
        }
        assert_eq!(
            r.on_refresh(ms(t, 1_500)),
            RefreshAction::Switch("refresh_escalated")
        );
        // Served keys older than the window no longer count.
        assert_eq!(r.on_refresh(ms(t, 3_600)), RefreshAction::Request);
    }

    #[test]
    fn latency_refresh_needs_key_and_deadlines() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        let spf = Duration::from_millis(16);
        assert!(!r.kept_due(t, spf) && !r.timed_out(t));
        // Without a request any frame is fine.
        assert_eq!(r.on_encoded(DELTA, t), Ok(false));
        r.on_requested(t);
        assert!(!r.kept_due(ms(t, 20), spf));
        assert!(r.kept_due(ms(t, 32), spf));
        // At 1 fps the kept image is used after 100 ms, not 2 s.
        assert!(r.kept_due(ms(t, 100), Duration::from_secs(1)));
        assert!(r.timed_out(ms(t, 1_000)));
        assert_eq!(r.on_encoded(DELTA, ms(t, 20)), Err("refresh_no_key"));
    }

    #[test]
    fn latency_static_refine_after_key_or_large_frame() {
        let t = Instant::now();
        let mut s = StaticRefine::new(true);
        s.on_frame(DELTA);
        // A normal frame does not arm it.
        assert!(!s.on_idle(t) && !s.on_idle(ms(t, 200)));
        s.on_frame(KEY);
        assert!(!s.on_idle(ms(t, 300)));
        assert!(!s.on_idle(ms(t, 400)));
        let mut n = 0;
        for i in 0..10 {
            if s.on_idle(ms(t, 450 + i * 16)) {
                n += 1;
            }
        }
        assert_eq!(n, REFINE_MAX);
        // A large frame arms it again; a new image resets the idle time.
        s.on_frame(Encoded {
            key: false,
            bytes: 60_000,
        });
        assert!(!s.on_idle(ms(t, 1_000)));
        assert!(s.on_idle(ms(t, 1_150)));
        s.on_frame(DELTA);
        assert!(!s.on_idle(ms(t, 2_000)) && !s.on_idle(ms(t, 2_200)));
        s.on_kept_key();
        assert!(!s.on_idle(ms(t, 3_000)) && s.on_idle(ms(t, 3_150)));
        s.disable();
        s.on_kept_key();
        assert!(!s.on_idle(ms(t, 4_000)) && !s.on_idle(ms(t, 4_200)));
    }
}
