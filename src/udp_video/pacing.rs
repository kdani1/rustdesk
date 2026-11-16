//! Send pacing for the UDP video lane.
//!
//! A frame of N datagrams sent back to back is a burst the Wi-Fi AP or a switch queue may drop
//! the tail of, and TCP's congestion control is no longer there to spread it. The pacer is a
//! token bucket: up to `burst_bytes` go out at once, the rest at `rate`. The target rate is
//! `max(1.5 x video bitrate, frame_bytes / (0.5 x frame interval))`, so a frame is always out
//! within half a frame interval, an IDR included (the udp-video design, step 4).
//!
//! The pacer only computes when to send; the live sender thread (M20) waits with a [`Waiter`]:
//! a high-resolution waitable timer on Windows (1803+) for all but the last half millisecond,
//! then a spin, so the wait between bursts costs little CPU and is not rounded up to the 1 ms
//! (or 15.6 ms) scheduler tick.

use std::time::{Duration, Instant};

/// Initial burst: 8 LAN datagrams, about 11 KB.
pub const DEFAULT_BURST_PACKETS: usize = 8;

/// The pacing rate for the current bitrate and frame, in bytes per second.
pub fn target_rate(bitrate_bps: u64, frame_bytes: usize, frame_interval: Duration) -> f64 {
    let by_bitrate = 1.5 * bitrate_bps as f64 / 8.0;
    let half = frame_interval.as_secs_f64() * 0.5;
    let by_frame = if half > 0.0 {
        frame_bytes as f64 / half
    } else {
        f64::INFINITY
    };
    by_bitrate.max(by_frame)
}

#[derive(Clone, Debug)]
pub struct Pacer {
    rate: f64,
    burst: f64,
    /// Bytes that may go out now; negative while paying off a send that has been scheduled
    /// ahead of the bucket.
    tokens: f64,
    last: Instant,
}

impl Pacer {
    /// `rate` in bytes per second (non-positive or NaN means unpaced), `burst_bytes` the
    /// bucket size. The bucket starts full.
    pub fn new(rate: f64, burst_bytes: usize, now: Instant) -> Self {
        let burst = burst_bytes.max(1) as f64;
        Self {
            rate: Self::sane(rate),
            burst,
            tokens: burst,
            last: now,
        }
    }

    fn sane(rate: f64) -> f64 {
        if rate.is_nan() || rate <= 0.0 {
            f64::INFINITY
        } else {
            rate
        }
    }

    pub fn set_rate(&mut self, rate: f64, now: Instant) {
        self.refill(now);
        self.rate = Self::sane(rate);
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        if self.rate.is_infinite() {
            self.tokens = self.burst;
        } else {
            self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        }
    }

    /// Accounts for a datagram of `bytes` and returns how long after `now` it may be sent.
    /// Calls must come in send order; the next call's `now` may be earlier than this one's
    /// send time (the delays then add up).
    pub fn schedule(&mut self, bytes: usize, now: Instant) -> Duration {
        self.refill(now);
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 || self.rate.is_infinite() {
            if self.rate.is_infinite() {
                self.tokens = self.burst;
            }
            return Duration::ZERO;
        }
        Duration::from_secs_f64(-self.tokens / self.rate)
    }
}

/// Sleeps until `deadline`: a normal sleep for all but the last millisecond (the Windows
/// default timer granularity is 1 ms or worse), then a spin.
pub fn wait_until(deadline: Instant) {
    const SPIN: Duration = Duration::from_millis(1);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if left > SPIN {
            std::thread::sleep(left - SPIN);
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Waits for a deadline with sub-millisecond accuracy. Windows: a high-resolution waitable
/// timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`) for all but the last `SPIN`, then a spin;
/// without that timer (before Windows 10 1803) and on other platforms, `wait_until`.
pub struct Waiter {
    #[cfg(windows)]
    timer: Option<win::Timer>,
}

/// The tail of a wait that is spun rather than slept.
const SPIN_TAIL: Duration = Duration::from_micros(500);

impl Waiter {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            timer: win::Timer::new(),
        }
    }

    /// Whether the high-resolution timer is in use (logged once by the sender).
    pub fn high_resolution(&self) -> bool {
        #[cfg(windows)]
        {
            self.timer.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn wait_until(&mut self, deadline: Instant) {
        #[cfg(windows)]
        if let Some(t) = &self.timer {
            let now = Instant::now();
            if deadline > now + SPIN_TAIL {
                t.sleep(deadline - now - SPIN_TAIL);
            }
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            return;
        }
        wait_until(deadline);
    }
}

#[cfg(windows)]
mod win {
    use std::{ffi::c_void, time::Duration};

    const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x0000_0002;
    const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
    const INFINITE: u32 = 0xFFFF_FFFF;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateWaitableTimerExW(
            attrs: *mut c_void,
            name: *const u16,
            flags: u32,
            access: u32,
        ) -> *mut c_void;
        fn SetWaitableTimer(
            timer: *mut c_void,
            due: *const i64,
            period: i32,
            routine: *mut c_void,
            arg: *mut c_void,
            resume: i32,
        ) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    pub struct Timer(*mut c_void);

    // The handle is used by the one thread that owns the Waiter; moving it is fine.
    unsafe impl Send for Timer {}

    impl Timer {
        pub fn new() -> Option<Self> {
            let h = unsafe {
                CreateWaitableTimerExW(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
            };
            (!h.is_null()).then_some(Self(h))
        }

        pub fn sleep(&self, d: Duration) {
            // Relative due time in 100 ns units, negative.
            let due = -((d.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
            unsafe {
                if SetWaitableTimer(
                    self.0,
                    &due,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                ) != 0
                {
                    WaitForSingleObject(self.0, INFINITE);
                } else {
                    std::thread::sleep(d);
                }
            }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn target_rate_covers_bitrate_and_frame() {
        let iv = Duration::from_micros(16_667);
        // 20 Mbit/s, small P-frame: the bitrate term wins (3.75 MB/s).
        let r = target_rate(20_000_000, 10_000, iv);
        assert!((r - 3_750_000.0).abs() < 1.0);
        // 300 KB IDR: must be out within ~8.3 ms.
        let r = target_rate(20_000_000, 300_000, iv);
        assert!((r - 300_000.0 / 0.0083335).abs() / r < 1e-3);
        assert!(target_rate(0, 1, Duration::ZERO).is_infinite());
    }

    #[test]
    fn burst_then_rate() {
        let t0 = Instant::now();
        // 1 MB/s, burst of 3 x 1000 B.
        let mut p = Pacer::new(1_000_000.0, 3_000, t0);
        let delays: Vec<_> = (0..6).map(|_| p.schedule(1_000, t0)).collect();
        assert_eq!(&delays[..3], &[Duration::ZERO; 3]);
        assert_eq!(delays[3], Duration::from_millis(1));
        assert_eq!(delays[4], Duration::from_millis(2));
        assert_eq!(delays[5], Duration::from_millis(3));
        // After the backlog drains and the bucket refills, a burst goes out again.
        let later = t0 + Duration::from_millis(10);
        assert_eq!(p.schedule(1_000, later), Duration::ZERO);
    }

    #[test]
    fn whole_frame_within_half_an_interval() {
        let t0 = Instant::now();
        let iv = Duration::from_micros(16_667);
        let frame = 200_000usize;
        let rate = target_rate(10_000_000, frame, iv);
        let mut p = Pacer::new(rate, DEFAULT_BURST_PACKETS * 1400, t0);
        let mut last = Duration::ZERO;
        for _ in 0..frame.div_ceil(1400) {
            last = p.schedule(1400, t0);
        }
        assert!(last <= iv / 2, "{last:?}");
        assert!(last >= iv / 3, "{last:?}");
    }

    #[test]
    fn unpaced_and_rate_change() {
        let t0 = Instant::now();
        let mut p = Pacer::new(0.0, 1_000, t0);
        for _ in 0..100 {
            assert_eq!(p.schedule(1_400, t0), Duration::ZERO);
        }
        p.set_rate(1_000.0, t0);
        assert_eq!(p.schedule(1_000, t0), Duration::ZERO);
        assert_eq!(p.schedule(500, t0), Duration::from_millis(500));
    }

    #[test]
    fn wait_until_returns_at_or_after_deadline() {
        let deadline = Instant::now() + Duration::from_millis(3);
        wait_until(deadline);
        assert!(Instant::now() >= deadline);
        wait_until(Instant::now() - Duration::from_millis(1));
        let mut w = Waiter::new();
        for us in [200u64, 1_500, 4_000] {
            let deadline = Instant::now() + Duration::from_micros(us);
            w.wait_until(deadline);
            assert!(Instant::now() >= deadline);
        }
        #[cfg(windows)]
        assert!(w.high_resolution());
    }
}
