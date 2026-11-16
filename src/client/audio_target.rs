//! Adaptive playback buffer target for received audio (local option `audio-target-ms`).
//!
//! The playback ring buffer only grows by itself: TCP delivers the audio that piled up during a
//! network stall as one burst, and producer and consumer then run at the same rate, so that
//! backlog stays as permanent lag. The legacy rule (`AudioBuffer::try_shrink`) trims it at most
//! every 12 s and never below 600 ms. This policy keeps a per-buffer target instead:
//!
//! - target = clamp(p95 of the arrival lateness over the last 20 s + 20 ms, floor, ceiling)
//!   plus an underrun boost. Lateness is how much later than its schedule a packet arrived, the
//!   schedule being the earliest arrival seen so far plus the audio received since, re-based
//!   every second so clock drift does not add up.
//! - Each output underrun during a stall (audio resumed within `PAUSE`) raises the boost by
//!   `UNDERRUN_BOOST`; it decays by `BOOST_DECAY` per `BOOST_DECAY_EVERY` without one.
//! - The buffer level is sampled just before each push. When the lowest level of a 1 s slot is
//!   above target + `TRIM_MARGIN` for `TRIM_AFTER_SLOTS` slots in a row, the excess is trimmed
//!   down to the target (not to half of it), with the playback crossfade.
//!
//! Only trims, never pads: a buffer below the target stays as it is.

use std::time::{Duration, Instant};

/// Default lower bound of the target.
pub(super) const DEFAULT_FLOOR: Duration = Duration::from_millis(60);
/// Upper bound of the target, unless the option sets a higher floor.
const CEILING: Duration = Duration::from_millis(300);
/// Largest floor the option can set.
const MAX_FLOOR: Duration = Duration::from_millis(2000);
/// Added to the lateness p95.
const LATENESS_MARGIN: Duration = Duration::from_millis(20);
/// Hysteresis above the target before anything is trimmed.
const TRIM_MARGIN: Duration = Duration::from_millis(20);
const TRIM_AFTER_SLOTS: u32 = 2;
const SLOT: Duration = Duration::from_secs(1);
/// Lateness window: `SLOTS` x `SLOT`.
const SLOTS: usize = 20;
/// Lateness histogram: `LATE_BIN_MS` wide bins, the last one open-ended.
const LATE_BIN_MS: u64 = 10;
const LATE_BINS: usize = 31;
/// A gap this long between two packets is the host pausing audio (it stops sending after
/// about 8 s of silence), not a network stall: the schedule restarts and the underrun that
/// ended the old audio does not raise the target.
pub(super) const PAUSE: Duration = Duration::from_secs(1);
const UNDERRUN_BOOST: Duration = Duration::from_millis(20);
const BOOST_DECAY: Duration = Duration::from_millis(10);
const BOOST_DECAY_EVERY: Duration = Duration::from_secs(15);

/// What the policy is doing, for the `LATENCY client audio` line.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct AudioTargetState {
    pub target: Duration,
    /// p95 of the arrival lateness over the window.
    pub late95: Duration,
    /// What underruns added to the target.
    pub boost: Duration,
}

pub(super) struct AudioTarget {
    floor: Duration,
    ceiling: Duration,
    state: AudioTargetState,
    // Earliest arrival time of the next packet: the lateness schedule.
    next_due: Option<Instant>,
    last_arrival: Option<Instant>,
    slot_start: Option<Instant>,
    slot: usize,
    late_hist: [[u32; LATE_BINS]; SLOTS],
    // Lowest lateness and level of the current slot.
    slot_late_min: Option<Duration>,
    slot_level_min: Option<Duration>,
    slots_over: u32,
    last_boost_change: Option<Instant>,
}

impl AudioTarget {
    /// The policy for an `audio-target-ms` value, or None for the legacy rule. Empty (or not a
    /// number) is the adaptive default where `default_on`, the legacy rule elsewhere; "0" is
    /// the legacy rule; N > 0 is adaptive with N ms as the floor.
    pub(super) fn from_option(value: &str, default_on: bool) -> Option<Self> {
        match value.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(ms) => Some(Self::new(Duration::from_millis(ms).min(MAX_FLOOR))),
            Err(_) => default_on.then(|| Self::new(DEFAULT_FLOOR)),
        }
    }

    pub(super) fn new(floor: Duration) -> Self {
        let mut target = Self {
            floor,
            ceiling: CEILING.max(floor),
            state: Default::default(),
            next_due: None,
            last_arrival: None,
            slot_start: None,
            slot: 0,
            late_hist: [[0; LATE_BINS]; SLOTS],
            slot_late_min: None,
            slot_level_min: None,
            slots_over: 0,
            last_boost_change: None,
        };
        target.update_target();
        target
    }

    pub(super) fn state(&self) -> AudioTargetState {
        self.state
    }

    pub(super) fn floor(&self) -> Duration {
        self.floor
    }

    /// One received packet of `packet` audio, `level` the buffer level just before it is
    /// pushed, `underruns` the output underruns since the previous packet (starved with the
    /// buffer readable, not lock contention). Returns how much buffered audio to trim.
    pub(super) fn on_packet(
        &mut self,
        now: Instant,
        level: Duration,
        packet: Duration,
        underruns: usize,
    ) -> Option<Duration> {
        let paused = self
            .last_arrival
            .map_or(true, |last| now.saturating_duration_since(last) >= PAUSE);
        self.last_arrival = Some(now);
        if paused {
            self.next_due = None;
        } else if underruns > 0 {
            self.state.boost =
                (self.state.boost + UNDERRUN_BOOST * underruns.min(15) as u32).min(self.ceiling);
            self.last_boost_change = Some(now);
            self.update_target();
        }
        if !self.state.boost.is_zero() {
            let since = *self.last_boost_change.get_or_insert(now);
            if now.saturating_duration_since(since) >= BOOST_DECAY_EVERY {
                self.state.boost = self.state.boost.saturating_sub(BOOST_DECAY);
                self.last_boost_change = Some(now);
                self.update_target();
            }
        }

        // Lateness against the schedule; an early packet moves the schedule back.
        let due = *self.next_due.get_or_insert(now);
        let due = due.min(now);
        let late = now.saturating_duration_since(due);
        self.next_due = Some(due + packet);

        let trim = self.end_slot_if_due(now, level);
        let bin = ((late.as_millis() as u64 / LATE_BIN_MS) as usize).min(LATE_BINS - 1);
        self.late_hist[self.slot][bin] += 1;
        self.slot_late_min = Some(self.slot_late_min.map_or(late, |m| m.min(late)));
        self.slot_level_min = Some(self.slot_level_min.map_or(level, |m| m.min(level)));
        trim
    }

    /// At most once per `SLOT`: re-base the schedule, recompute the target, decide on a trim.
    fn end_slot_if_due(&mut self, now: Instant, level: Duration) -> Option<Duration> {
        let start = *self.slot_start.get_or_insert(now);
        if now.saturating_duration_since(start) < SLOT {
            return None;
        }
        self.slot_start = Some(now);
        // Every packet of the slot was at least this late: that is drift (or a constant delay),
        // not jitter.
        if let (Some(min), Some(due)) = (self.slot_late_min.take(), self.next_due.as_mut()) {
            *due += min;
        }
        let floor = self.slot_level_min.take();
        self.slot = (self.slot + 1) % SLOTS;
        self.late_hist[self.slot] = [0; LATE_BINS];
        self.state.late95 = self.lateness_p95();
        self.update_target();
        let target = self.state.target;
        match floor {
            Some(floor) if floor > target + TRIM_MARGIN => self.slots_over += 1,
            _ => self.slots_over = 0,
        }
        if self.slots_over < TRIM_AFTER_SLOTS {
            return None;
        }
        self.slots_over = 0;
        let excess = floor.unwrap_or_default().min(level).saturating_sub(target);
        (!excess.is_zero()).then_some(excess)
    }

    fn lateness_p95(&self) -> Duration {
        let mut bins = [0u64; LATE_BINS];
        for slot in self.late_hist.iter() {
            for (sum, n) in bins.iter_mut().zip(slot.iter()) {
                *sum += *n as u64;
            }
        }
        let total: u64 = bins.iter().sum();
        if total == 0 {
            return Duration::ZERO;
        }
        let mut seen = 0;
        for (i, n) in bins.iter().enumerate() {
            seen += n;
            if seen * 100 >= total * 95 {
                // Upper edge of the bin; the open-ended last bin counts as its lower edge.
                let edge = if i + 1 < LATE_BINS { i + 1 } else { i };
                return Duration::from_millis(edge as u64 * LATE_BIN_MS);
            }
        }
        Duration::from_millis((LATE_BINS - 1) as u64 * LATE_BIN_MS)
    }

    fn update_target(&mut self) {
        let base = (self.state.late95 + LATENESS_MARGIN).max(self.floor);
        self.state.target = (base + self.state.boost).min(self.ceiling);
    }
}

#[cfg(test)]
mod audio_target_latency_tests {
    use super::*;

    const PACKET: Duration = Duration::from_millis(10);

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn option_values() {
        assert!(AudioTarget::from_option("0", true).is_none());
        assert!(AudioTarget::from_option("", false).is_none());
        assert_eq!(
            AudioTarget::from_option("", true).unwrap().state().target,
            DEFAULT_FLOOR
        );
        assert_eq!(
            AudioTarget::from_option("abc", true)
                .unwrap()
                .state()
                .target,
            DEFAULT_FLOOR
        );
        let t = AudioTarget::from_option("120", false).unwrap();
        assert_eq!(t.state().target, ms(120));
        let t = AudioTarget::from_option("5000", false).unwrap();
        assert_eq!(t.state().target, MAX_FLOOR);
    }

    /// Steady 10 ms packets: no lateness, the floor is the target, and a level that stays far
    /// above it (the backlog of an old stall) is trimmed down to the target after 2 slots.
    #[test]
    fn steady_backlog_is_trimmed_to_target() {
        let mut t = AudioTarget::new(DEFAULT_FLOOR);
        let start = Instant::now();
        let mut trims = vec![];
        for i in 0..400u64 {
            let now = start + PACKET * i as u32;
            if let Some(trim) = t.on_packet(now, ms(500), PACKET, 0) {
                trims.push((i, trim));
            }
        }
        assert_eq!(t.state().late95, ms(10));
        assert_eq!(t.state().target, DEFAULT_FLOOR);
        // Slots end at packets 100, 200, 300: the second slot over the target trims.
        assert_eq!(trims.first(), Some(&(200, ms(500) - DEFAULT_FLOOR)));
    }

    #[test]
    fn level_near_target_is_not_trimmed() {
        let mut t = AudioTarget::new(DEFAULT_FLOOR);
        let start = Instant::now();
        for i in 0..1000u64 {
            let now = start + PACKET * i as u32;
            assert_eq!(t.on_packet(now, ms(75), PACKET, 0), None);
        }
    }

    /// A 200 ms stall with an underrun raises the target by the boost; a pause (host stopped
    /// sending) does not.
    #[test]
    fn stall_underrun_boosts_pause_does_not() {
        let mut t = AudioTarget::new(DEFAULT_FLOOR);
        let start = Instant::now();
        let mut now = start;
        for _ in 0..50 {
            t.on_packet(now, ms(40), PACKET, 0);
            now += PACKET;
        }
        now += ms(200);
        t.on_packet(now, Duration::ZERO, PACKET, 1);
        assert_eq!(t.state().boost, UNDERRUN_BOOST);
        assert_eq!(t.state().target, DEFAULT_FLOOR + UNDERRUN_BOOST);
        now += ms(3000);
        t.on_packet(now, Duration::ZERO, PACKET, 1);
        assert_eq!(t.state().boost, UNDERRUN_BOOST);
    }

    /// Bursty arrival: lateness p95 sets the target above the floor, within the ceiling.
    #[test]
    fn jitter_raises_target() {
        let mut t = AudioTarget::new(DEFAULT_FLOOR);
        let start = Instant::now();
        // 100 ms of audio arrives as one burst every 100 ms, so the packets of a burst are 90,
        // 80, ... 0 ms late: the p95 falls in the 90 ms bin.
        for burst in 0..300u64 {
            let arrive = start + ms(burst * 100 + 90);
            for _ in 0..10 {
                t.on_packet(arrive, ms(100), PACKET, 0);
            }
        }
        let state = t.state();
        assert!(state.late95 >= ms(80), "{state:?}");
        assert!(
            state.target >= ms(100) && state.target <= CEILING,
            "{state:?}"
        );
    }

    #[test]
    fn boost_decays() {
        let mut t = AudioTarget::new(DEFAULT_FLOOR);
        let start = Instant::now();
        let mut now = start;
        t.on_packet(now, ms(40), PACKET, 0);
        now += ms(100);
        t.on_packet(now, Duration::ZERO, PACKET, 2);
        assert_eq!(t.state().boost, UNDERRUN_BOOST * 2);
        for _ in 0..(16 * 100) {
            now += PACKET;
            t.on_packet(now, ms(40), PACKET, 0);
        }
        assert_eq!(t.state().boost, UNDERRUN_BOOST * 2 - BOOST_DECAY);
    }
}
