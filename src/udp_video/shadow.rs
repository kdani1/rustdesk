//! Shadow-mode measurement on the viewer (M19): pairs the UDP copy of a frame with its TCP copy
//! by (display, pts) and reports how much earlier UDP had it.
//!
//! - `ahead_net` = t(TCP copy handled in the io_loop) - t(UDP copy complete on the receive
//!   thread). Credits UDP with the io_loop's own scheduling delay, so it is the upper bound.
//! - `ahead_app` = t(TCP copy handled) - t(UDP copy handled in the io_loop's select branch the
//!   live path (M20) will use). Both copies then pay the same loop delay: the fair number.
//!
//! Positive means UDP was first. A pair is formed whichever copy comes first; copies left
//! unpaired for `PAIR_TTL` are dropped (TCP copies of frames that were not shadowed are the
//! normal case, since only every Nth frame goes over UDP).

use std::{
    collections::VecDeque,
    fmt::Write as _,
    time::{Duration, Instant},
};

/// How long a copy waits for its twin.
pub const PAIR_TTL: Duration = Duration::from_secs(2);
/// Unpaired copies kept per side, at most.
const MAX_PENDING: usize = 512;

/// A window of samples, in microseconds.
#[derive(Clone, Debug, Default)]
pub struct Samples(Vec<i64>);

impl Samples {
    pub fn push(&mut self, v: i64) {
        // Bounded: a 5 s window at 240 fps is 1200 frames.
        if self.0.len() < 8192 {
            self.0.push(v);
        }
    }

    pub fn push_duration(&mut self, d: Duration) {
        self.push(d.as_micros().min(i64::MAX as u128) as i64);
    }

    /// (p50, p95, max, min), nearest rank, or None when empty.
    pub fn summary(&mut self) -> Option<(i64, i64, i64, i64)> {
        if self.0.is_empty() {
            return None;
        }
        self.0.sort_unstable();
        let n = self.0.len();
        // Nearest rank.
        let at = |q: usize| self.0[((n * q).div_ceil(100)).saturating_sub(1).min(n - 1)];
        Some((at(50), at(95), self.0[n - 1], self.0[0]))
    }

    /// `p50/p95/max` in ms with one decimal, or `-`.
    pub fn ms3(&mut self) -> String {
        match self.summary() {
            Some((p50, p95, max, _)) => format!("{}/{}/{}", ms(p50), ms(p95), ms(max)),
            None => "-".to_owned(),
        }
    }

    /// `p50/p95/max/min` in ms with one decimal, or `-`.
    pub fn ms4(&mut self) -> String {
        match self.summary() {
            Some((p50, p95, max, min)) => {
                format!("{}/{}/{}/{}", ms(p50), ms(p95), ms(max), ms(min))
            }
            None => "-".to_owned(),
        }
    }

    /// Each sample minus the window's minimum (variation over the window's best case).
    pub fn above_min(&self) -> Samples {
        let min = self.0.iter().copied().min().unwrap_or(0);
        Samples(self.0.iter().map(|v| v - min).collect())
    }
}

fn ms(us: i64) -> String {
    let mut s = String::new();
    let _ = write!(s, "{:.1}", us as f64 / 1000.0);
    s
}

/// `a - b` in microseconds, signed.
pub fn signed_us(a: Instant, b: Instant) -> i64 {
    if a >= b {
        (a - b).as_micros() as i64
    } else {
        -((b - a).as_micros() as i64)
    }
}

struct UdpCopy {
    display: i32,
    pts: i64,
    net: Instant,
    app: Instant,
}

struct TcpCopy {
    display: i32,
    pts: i64,
    at: Instant,
}

/// What one window of pairing measured.
#[derive(Debug, Default)]
pub struct MatchWindow {
    pub matched: u64,
    /// Pairs where the TCP copy was handled before the UDP copy completed.
    pub tcp_first: u64,
    /// UDP copies that never met their TCP copy.
    pub udp_unpaired: u64,
    pub ahead_net: Samples,
    pub ahead_app: Samples,
}

#[derive(Default)]
pub struct ShadowMatcher {
    udp: VecDeque<UdpCopy>,
    tcp: VecDeque<TcpCopy>,
    window: MatchWindow,
}

impl ShadowMatcher {
    pub fn on_tcp(&mut self, display: i32, pts: i64, at: Instant) {
        self.expire(at);
        if let Some(i) = self
            .udp
            .iter()
            .position(|u| u.display == display && u.pts == pts)
        {
            let u = self.udp.remove(i).unwrap_or_else(|| unreachable!());
            self.record(at, u.net, u.app);
            return;
        }
        if self.tcp.len() >= MAX_PENDING {
            self.tcp.pop_front();
        }
        self.tcp.push_back(TcpCopy { display, pts, at });
    }

    pub fn on_udp(&mut self, display: i32, pts: i64, net: Instant, app: Instant) {
        self.expire(app);
        if let Some(i) = self
            .tcp
            .iter()
            .position(|t| t.display == display && t.pts == pts)
        {
            let t = self.tcp.remove(i).unwrap_or_else(|| unreachable!());
            self.record(t.at, net, app);
            return;
        }
        if self.udp.len() >= MAX_PENDING {
            self.udp.pop_front();
            self.window.udp_unpaired += 1;
        }
        self.udp.push_back(UdpCopy {
            display,
            pts,
            net,
            app,
        });
    }

    fn record(&mut self, tcp: Instant, net: Instant, app: Instant) {
        let w = &mut self.window;
        w.matched += 1;
        let net_ahead = signed_us(tcp, net);
        if net_ahead < 0 {
            w.tcp_first += 1;
        }
        w.ahead_net.push(net_ahead);
        w.ahead_app.push(signed_us(tcp, app));
    }

    fn expire(&mut self, now: Instant) {
        while self
            .udp
            .front()
            .map_or(false, |u| now.saturating_duration_since(u.app) > PAIR_TTL)
        {
            self.udp.pop_front();
            self.window.udp_unpaired += 1;
        }
        while self
            .tcp
            .front()
            .map_or(false, |t| now.saturating_duration_since(t.at) > PAIR_TTL)
        {
            self.tcp.pop_front();
        }
    }

    pub fn take_window(&mut self) -> MatchWindow {
        std::mem::take(&mut self.window)
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn pairs_both_orders_and_expires() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut m = ShadowMatcher::default();
        // UDP first, TCP 3 ms later; the loop took 1 ms to run the UDP branch.
        m.on_udp(0, 100, ms(10), ms(11));
        m.on_tcp(0, 100, ms(13));
        // TCP first (UDP 2 ms late).
        m.on_tcp(0, 101, ms(20));
        m.on_udp(0, 101, ms(22), ms(22));
        // Another display with the same pts does not pair.
        m.on_udp(1, 102, ms(30), ms(30));
        m.on_tcp(0, 102, ms(31));
        // Not shadowed: a TCP copy alone is not an error.
        m.on_tcp(0, 103, ms(40));
        // Long after: the display-1 UDP copy expires unpaired.
        m.on_tcp(0, 104, ms(40) + PAIR_TTL + Duration::from_millis(1));
        let mut w = m.take_window();
        assert_eq!(w.matched, 2);
        assert_eq!(w.tcp_first, 1);
        assert_eq!(w.udp_unpaired, 1);
        assert_eq!(w.ahead_net.summary(), Some((-2000, 3000, 3000, -2000)));
        assert_eq!(w.ahead_app.summary().map(|s| s.2), Some(2000));
    }

    #[test]
    fn samples_summary() {
        let mut s = Samples::default();
        assert_eq!(s.ms3(), "-");
        for v in [5, 1, 3, 2, 4] {
            s.push(v * 1000);
        }
        assert_eq!(s.summary(), Some((3000, 5000, 5000, 1000)));
        assert_eq!(s.ms4(), "3.0/5.0/5.0/1.0");
        assert_eq!(s.above_min().summary(), Some((2000, 4000, 4000, 0)));
    }
}
