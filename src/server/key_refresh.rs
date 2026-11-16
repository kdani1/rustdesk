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
    // The encoder took a key frame request in this run; until then a refresh is not deferred,
    // so an encoder without forced key frames restarts at once instead of after the interval.
    supported: bool,
    // An encoded message came in this run, so a kept image exists for `kept_due`.
    has_encoded: bool,
    // When the key frames that served refreshes were encoded, within `ESCALATE_WINDOW`.
    served: VecDeque<Instant>,
    // The last refresh was served from the kept image, at this time, and the capturer has given
    // no new image since. A further refresh then restarts capture: the viewer's Refresh is also
    // how a user recovers a stalled capturer, which the kept image cannot show.
    kept_served: Option<Instant>,
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
        // Not within the minimum key interval: a refresh sent before the viewer got that key
        // frame is merged or deferred as usual.
        if let Some(t) = self.kept_served {
            let since = now.saturating_duration_since(t);
            if since >= MIN_KEY_INTERVAL && since <= ESCALATE_WINDOW {
                return RefreshAction::Switch("refresh_kept_again");
            }
        }
        if self.requested.is_some() || self.deferred {
            return RefreshAction::Merged;
        }
        if let Some(last) = self.last_key.filter(|_| self.supported) {
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
        self.supported = true;
    }

    pub(super) fn pending(&self) -> bool {
        self.requested.is_some()
    }

    /// An encoded message (`kept`: encoded from the kept image, not from a captured one). While
    /// a key frame is requested it has to be one: Err with the `SWITCH` reason otherwise.
    /// Ok(true): this message is the requested key frame.
    pub(super) fn on_encoded(
        &mut self,
        encoded: Encoded,
        now: Instant,
        kept: bool,
    ) -> Result<bool, &'static str> {
        self.has_encoded = true;
        if !kept {
            self.kept_served = None;
        }
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
        if kept {
            self.kept_served = Some(now);
        }
        Ok(true)
    }

    /// The requested key frame should now be encoded from the kept last image. Not before this
    /// run encoded a frame: there is no kept image yet, and the first frame of a new encoder is a
    /// key frame anyway (`KEY_TIMEOUT` still applies).
    pub(super) fn kept_due(&self, now: Instant, spf: Duration) -> bool {
        if !self.has_encoded {
            return false;
        }
        self.requested
            .map(|t| now.saturating_duration_since(t) >= (spf * 2).min(KEPT_KEY_AFTER_MAX))
            .unwrap_or(false)
    }

    /// When the capture loop has to check this refresh again: the kept-image deadline of a
    /// requested key frame (`KEY_TIMEOUT` before the run encoded a frame), or the end of the
    /// minimum key interval of a deferred one. The capturer's wait and the loop's sleep end there
    /// at the latest, so at a low frame rate they do not hold the key frame back.
    pub(super) fn next_due(&self, spf: Duration) -> Option<Instant> {
        if let Some(t) = self.requested {
            return Some(if self.has_encoded {
                t + (spf * 2).min(KEPT_KEY_AFTER_MAX)
            } else {
                t + KEY_TIMEOUT
            });
        }
        if self.deferred {
            return Some(match self.last_key {
                Some(last) => last + MIN_KEY_INTERVAL,
                None => Instant::now(),
            });
        }
        None
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
        assert_eq!(r.on_encoded(KEY, ms(t, 16), false), Ok(true));
        assert!(!r.pending());
        // Right after the key was sent: deferred, never dropped.
        assert_eq!(r.on_refresh(ms(t, 50)), RefreshAction::Merged);
        assert_eq!(r.on_refresh(ms(t, 60)), RefreshAction::Merged);
        assert!(!r.take_due(ms(t, 100)));
        assert!(r.take_due(ms(t, 216)));
        assert!(!r.take_due(ms(t, 230)));
    }

    #[test]
    fn latency_refresh_not_deferred_before_support_is_known() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        // The first frame of a new encoder is a key frame; a refresh right after it asks the
        // encoder at once, so one without forced key frames restarts without waiting.
        assert_eq!(r.on_encoded(KEY, t, false), Ok(false));
        assert_eq!(r.on_refresh(ms(t, 50)), RefreshAction::Request);
        r.on_requested(ms(t, 50));
        assert_eq!(r.on_encoded(KEY, ms(t, 66), false), Ok(true));
        // Supported now: the next one waits for the minimum key interval.
        assert_eq!(r.on_refresh(ms(t, 100)), RefreshAction::Merged);
        assert!(r.take_due(ms(t, 266)));
    }

    #[test]
    fn latency_refresh_escalates_after_served_keys() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        for i in 0..ESCALATE_COUNT as u64 {
            let at = ms(t, i * 500);
            assert_eq!(r.on_refresh(at), RefreshAction::Request);
            r.on_requested(at);
            assert_eq!(r.on_encoded(KEY, at, false), Ok(true));
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
        // Requested before the run encoded anything: no kept image, wait for the first frame.
        r.on_requested(t);
        assert!(!r.kept_due(ms(t, 100), spf));
        assert!(r.timed_out(ms(t, 1_000)));
        let mut r = KeyRefresh::default();
        // Without a request any frame is fine.
        assert_eq!(r.on_encoded(DELTA, t, false), Ok(false));
        r.on_requested(t);
        assert!(!r.kept_due(ms(t, 20), spf));
        assert!(r.kept_due(ms(t, 32), spf));
        // At 1 fps the kept image is used after 100 ms, not 2 s.
        assert!(r.kept_due(ms(t, 100), Duration::from_secs(1)));
        assert!(r.timed_out(ms(t, 1_000)));
        assert_eq!(r.on_encoded(DELTA, ms(t, 20), false), Err("refresh_no_key"));
    }

    #[test]
    fn latency_refresh_next_due_bounds_the_wait_at_low_fps() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        let spf = Duration::from_secs(1);
        assert_eq!(r.next_due(spf), None);
        // Requested before the run encoded anything: only the key timeout.
        r.on_requested(t);
        assert_eq!(r.next_due(spf), Some(ms(t, 1_000)));
        assert_eq!(r.on_encoded(KEY, ms(t, 10), false), Ok(true));
        assert_eq!(r.next_due(spf), None);
        // At 1 fps the capturer's 1 s wait ends at the kept-image deadline, 100 ms.
        assert_eq!(r.on_refresh(ms(t, 500)), RefreshAction::Request);
        r.on_requested(ms(t, 500));
        assert_eq!(r.next_due(spf), Some(ms(t, 600)));
        assert!(r.kept_due(ms(t, 600), spf));
        assert_eq!(r.on_encoded(KEY, ms(t, 600), true), Ok(true));
        // Deferred: due at the end of the minimum key interval.
        let mut r = KeyRefresh::default();
        r.on_requested(t);
        assert_eq!(r.on_encoded(KEY, t, false), Ok(true));
        assert_eq!(r.on_refresh(ms(t, 50)), RefreshAction::Merged);
        assert_eq!(r.next_due(spf), Some(ms(t, 200)));
        assert!(r.take_due(ms(t, 200)));
    }

    #[test]
    fn latency_refresh_after_kept_key_without_new_image_switches() {
        let t = Instant::now();
        let mut r = KeyRefresh::default();
        assert_eq!(r.on_encoded(DELTA, t, false), Ok(false));
        assert_eq!(r.on_refresh(ms(t, 1_000)), RefreshAction::Request);
        r.on_requested(ms(t, 1_000));
        assert_eq!(r.on_encoded(KEY, ms(t, 1_100), true), Ok(true));
        // One sent before the viewer had that key frame: deferred as usual.
        assert_eq!(r.on_refresh(ms(t, 1_150)), RefreshAction::Merged);
        assert!(r.take_due(ms(t, 1_300)));
        r.on_requested(ms(t, 1_300));
        // A refine of the kept image is no new image either.
        assert_eq!(r.on_encoded(KEY, ms(t, 1_400), true), Ok(true));
        assert_eq!(r.on_encoded(DELTA, ms(t, 1_550), true), Ok(false));
        assert_eq!(
            r.on_refresh(ms(t, 1_700)),
            RefreshAction::Switch("refresh_kept_again")
        );
        // A captured image in between: served by a key frame again.
        let mut r = KeyRefresh::default();
        assert_eq!(r.on_encoded(DELTA, t, false), Ok(false));
        r.on_requested(ms(t, 1_000));
        assert_eq!(r.on_encoded(KEY, ms(t, 1_100), true), Ok(true));
        assert_eq!(r.on_encoded(DELTA, ms(t, 1_200), false), Ok(false));
        assert_eq!(r.on_refresh(ms(t, 1_500)), RefreshAction::Request);
        // Long after the kept key frame: served again.
        let mut r = KeyRefresh::default();
        assert_eq!(r.on_encoded(DELTA, t, false), Ok(false));
        r.on_requested(ms(t, 1_000));
        assert_eq!(r.on_encoded(KEY, ms(t, 1_100), true), Ok(true));
        assert_eq!(r.on_refresh(ms(t, 5_000)), RefreshAction::Request);
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
