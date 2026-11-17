//! Roadmap M7, Windows hosts only: the frame rate the capture loop hands to an nvenc encoder
//! that runs with a virtual framerate (`nvenc-nominal-fps` > 0, `EncoderApi::set_fps`).
//!
//! The encoder was opened at a fixed nominal frame rate and NVENC's CBR budgets bitrate /
//! frame rate per frame, so the real bitrate is the QoS bitrate only when the encoder's bitrate
//! is rescaled to the frame rate actually encoded. This picks that frame rate:
//! - default: the QoS fps cap (exact under full motion; a sparse desktop gets fewer bits per
//!   frame than today),
//! - `nvenc-fps-from-send-rate=Y`: the frames encoded in the last second, kept between half the
//!   cap and the cap. Rises at once, falls only after two windows in a row, and ignores changes
//!   under 10 %, so a typing burst does not reconfigure the encoder every second.
//!
//! Pure state, no I/O: `check_qos` applies the result after it released the `VIDEO_QOS` lock.
//! See docs/LATENCY-METRICS.md.

// A lower measured frame rate is applied only after this many windows in a row.
const FALL_WINDOWS: u8 = 2;

pub(super) struct VirtualFps {
    from_send_rate: bool,
    // The frame rate the encoder's bitrate is scaled for now (0: not applied yet).
    applied: u32,
    // Consecutive windows whose target was below `applied`.
    falling: u8,
}

impl VirtualFps {
    pub(super) fn new(from_send_rate: bool) -> Self {
        Self {
            from_send_rate,
            applied: 0,
            falling: 0,
        }
    }

    /// The frame rate the encoder runs at now.
    pub(super) fn applied(&self) -> u32 {
        self.applied
    }

    /// Called on every `check_qos`: `cap` is the QoS fps cap, `measured` the frames encoded per
    /// second over the window that just ended (`Some` about once per second). Returns the frame
    /// rate to apply now, when it differs from the applied one; the caller reports back with
    /// `on_applied`.
    pub(super) fn next(&mut self, cap: u32, measured: Option<f64>) -> Option<u32> {
        let cap = cap.max(1);
        if !self.from_send_rate {
            return (self.applied != cap).then_some(cap);
        }
        let floor = ((cap + 1) / 2).max(1);
        if self.applied == 0 {
            // Until the first window: the cap, as without the send rate.
            return Some(cap);
        }
        if self.applied > cap || self.applied < floor {
            // The cap moved: back inside the range at once.
            self.falling = 0;
            return Some(self.applied.clamp(floor, cap));
        }
        let Some(measured) = measured else {
            return None;
        };
        let target = (measured.round().max(0.0) as u32).clamp(floor, cap);
        if target >= self.applied {
            self.falling = 0;
            if target > self.applied && (target == cap || significant(self.applied, target)) {
                return Some(target);
            }
            return None;
        }
        if !significant(self.applied, target) {
            self.falling = 0;
            return None;
        }
        self.falling += 1;
        if self.falling >= FALL_WINDOWS {
            self.falling = 0;
            return Some(target);
        }
        None
    }

    /// The encoder took `fps`.
    pub(super) fn on_applied(&mut self, fps: u32) {
        self.applied = fps;
    }
}

// A change of at least 10 % of the applied frame rate (and at least 2 fps).
fn significant(applied: u32, target: u32) -> bool {
    let d = applied.abs_diff(target);
    d >= 2 && d * 10 >= applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(v: &mut VirtualFps, cap: u32, measured: Option<f64>) -> Option<u32> {
        let r = v.next(cap, measured);
        if let Some(fps) = r {
            v.on_applied(fps);
        }
        r
    }

    #[test]
    fn latency_virtual_fps_cap_mode_follows_cap() {
        let mut v = VirtualFps::new(false);
        assert_eq!(apply(&mut v, 60, None), Some(60));
        assert_eq!(apply(&mut v, 60, Some(12.0)), None);
        assert_eq!(apply(&mut v, 27, None), Some(27));
        assert_eq!(apply(&mut v, 27, Some(27.0)), None);
        assert_eq!(v.applied(), 27);
    }

    #[test]
    fn latency_virtual_fps_send_rate_rises_at_once_falls_after_two_windows() {
        let mut v = VirtualFps::new(true);
        assert_eq!(apply(&mut v, 60, None), Some(60));
        // Sparse desktop: floor is half the cap.
        assert_eq!(apply(&mut v, 60, Some(5.0)), None);
        assert_eq!(apply(&mut v, 60, Some(4.0)), Some(30));
        assert_eq!(apply(&mut v, 60, None), None);
        // Motion: up in one window.
        assert_eq!(apply(&mut v, 60, Some(58.6)), Some(59));
        // Small wobble is ignored.
        assert_eq!(apply(&mut v, 60, Some(55.0)), None);
        assert_eq!(apply(&mut v, 60, Some(56.0)), None);
        // Up to the cap always applies.
        assert_eq!(apply(&mut v, 60, Some(61.0)), Some(60));
        // A single low window does not fall.
        assert_eq!(apply(&mut v, 60, Some(40.0)), None);
        assert_eq!(apply(&mut v, 60, Some(60.0)), None);
        assert_eq!(apply(&mut v, 60, Some(40.0)), None);
        assert_eq!(v.applied(), 60);
    }

    #[test]
    fn latency_virtual_fps_send_rate_follows_cap_changes() {
        let mut v = VirtualFps::new(true);
        assert_eq!(apply(&mut v, 60, None), Some(60));
        // The viewer's auto-fps lowers the cap: at once, without a window.
        assert_eq!(apply(&mut v, 20, None), Some(20));
        assert_eq!(apply(&mut v, 20, Some(20.0)), None);
        // The cap rises: the applied rate is now below the floor.
        assert_eq!(apply(&mut v, 60, None), Some(30));
        assert_eq!(apply(&mut v, 60, Some(60.0)), Some(60));
        assert_eq!(apply(&mut v, 1, None), Some(1));
    }
}
