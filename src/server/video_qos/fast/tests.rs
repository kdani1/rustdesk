//! Fast estimator tests (roadmap M15). Two kinds:
//!
//! - Replay of recorded `LATENCY server` windows (`../tests/fast_traces.txt`): each 5 s window's
//!   frame count, send and wait percentiles become per-frame writes, deterministically (the
//!   quantiles are spread over the window by a fixed permutation, the largest value goes to one
//!   frame). The recording has no TCP meter, so these runs are the unmetered estimator.
//! - A small fluid model of one sender buffer draining into a link of given capacity, for what
//!   the recording does not contain: a capacity step, link pauses, the meter's signals. It is
//!   not a TCP model (no congestion window, no loss); it only produces the signals the
//!   estimator reads, in the shape the TCP meter reports them.
use super::*;

const TRACES: &str = include_str!("../tests/fast_traces.txt");

#[derive(Clone, Copy, Debug)]
struct Window {
    spf_ms: f64,
    wait: [f64; 3],
    send: [f64; 3],
    sent: usize,
    kbps: Option<f64>,
}

fn triple(v: &str) -> Option<[f64; 3]> {
    let mut it = v.split('/').map(|x| x.parse::<f64>().ok());
    Some([it.next()??, it.next()??, it.next()??])
}

fn section(name: &str) -> Vec<Window> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in TRACES.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            inside = line == format!("[{name}]");
            continue;
        }
        if !inside {
            continue;
        }
        let mut w = Window {
            spf_ms: 33.0,
            wait: [0.0; 3],
            send: [0.0; 3],
            sent: 0,
            kbps: None,
        };
        for f in line.split_whitespace() {
            let Some((k, v)) = f.split_once('=') else {
                continue;
            };
            match k {
                "spf" => w.spf_ms = v.trim_end_matches("ms").parse().unwrap_or(33.0),
                "wait" => w.wait = triple(v).unwrap_or([0.0; 3]),
                "send" => w.send = triple(v).unwrap_or([0.0; 3]),
                "sent" => w.sent = v.parse().unwrap_or(0),
                "set_kbps" => w.kbps = v.parse().ok(),
                _ => {}
            }
        }
        out.push(w);
    }
    assert!(!out.is_empty(), "no windows in section {name}");
    out
}

// The value at quantile `q` of a window summarized as p50/p95/max.
fn quantile(s: [f64; 3], q: f64, top: bool) -> f64 {
    if top {
        return s[2];
    }
    if q < 0.5 {
        s[0]
    } else if q < 0.95 {
        s[0] + (s[1] - s[0]) * (q - 0.5) / 0.45
    } else {
        s[1] + (s[2] - s[1]) * ((q - 0.95) / 0.05).powi(3)
    }
}

enum Ev {
    Begin { enq: u64, bytes: u32 },
    End,
    Stall { waited: u64 },
    Second,
    Tick,
}

#[derive(Default)]
struct Outcome {
    decisions: Vec<Decision>,
    totals: Counters,
}

impl Outcome {
    fn count(&self, kind: Kind) -> usize {
        self.decisions.iter().filter(|d| d.kind == kind).count()
    }

    fn cuts(&self) -> Vec<&Decision> {
        self.decisions
            .iter()
            .filter(|d| d.kind != Kind::Increase)
            .collect()
    }
}

fn add(t: &mut Counters, c: &Counters) {
    t.evals += c.evals;
    for i in 0..5 {
        t.states[i] += c.states[i];
    }
    t.cuts += c.cuts;
    t.severe += c.severe;
    t.rto_cuts += c.rto_cuts;
    t.rto_ignored += c.rto_ignored;
    t.incs += c.incs;
    t.held += c.held;
    t.floor += c.floor;
    t.false_cuts += c.false_cuts;
    t.stalls += c.stalls;
    t.react_max_ms = t.react_max_ms.max(c.react_max_ms);
}

/// Replays recorded windows as one connection's frame writes, lock-stepped like the capture loop.
fn replay(windows: &[Window], default_kbps: f64) -> Outcome {
    let mut events: Vec<(u64, u8, Ev)> = Vec::new();
    let mut start = 0u64;
    let mut prev_begin = 0u64;
    let mut prev_end = 0u64;
    for w in windows {
        let n = w.sent;
        let spf_us = (w.spf_ms * 1000.0) as u64;
        let fps = (1000.0 / w.spf_ms).round().max(1.0);
        let kbps = w.kbps.unwrap_or(default_kbps);
        let bytes = (kbps * 1000.0 / 8.0 / fps) as u32;
        // Waits not explained by the writes: the connection loop was busy elsewhere.
        let loop_busy = w.wait[2] > w.send[2] + w.spf_ms;
        let thr = stall_threshold(Duration::from_micros(spf_us)).as_micros() as u64;
        let slice = slice_for(Duration::from_micros(spf_us)).as_micros() as u64;
        if n > 0 {
            let interval = 5_000_000 / n as u64;
            for i in 0..n {
                let qs = ((i * 7919 + 13) % n) as f64 / n as f64;
                let qw = ((i * 104_729 + 7) % n) as f64 / n as f64;
                let send = quantile(w.send, qs, (i * 7919 + 13) % n == n - 1);
                let busy = if loop_busy {
                    quantile(w.wait, qw, (i * 104_729 + 7) % n == n - 1)
                } else {
                    0.0
                };
                let enq = (start + i as u64 * interval).max(prev_begin);
                let begin = (enq + (busy * 1000.0) as u64).max(prev_end);
                let end = begin + (send * 1000.0) as u64;
                let mut t = enq + thr;
                while t < begin {
                    events.push((t, 1, Ev::Stall { waited: t - enq }));
                    t += slice;
                }
                events.push((begin, 2, Ev::Begin { enq, bytes }));
                events.push((end, 0, Ev::End));
                prev_begin = begin;
                prev_end = end;
            }
        }
        let mut t = start;
        while t < start + 5_000_000 {
            if (t - start) % 1_000_000 == 0 {
                events.push((t, 3, Ev::Second));
            }
            events.push((t, 4, Ev::Tick));
            t += TICK_US;
        }
        start += 5_000_000;
    }
    events.sort_by_key(|e| (e.0, e.1));
    let mut est = FastEstimator::new(false);
    let mut out = Outcome::default();
    let mut ctx_kbps = default_kbps;
    let mut wi = 0usize;
    for (t, _, ev) in events {
        let d = match ev {
            Ev::Begin { enq, bytes } => {
                est.on_write_begin(enq, t, bytes);
                None
            }
            Ev::End => est.on_write_end(t, 0),
            Ev::Stall { waited } => est.on_stall(t, waited),
            Ev::Second => {
                let w = &windows[((t / 5_000_000) as usize).min(windows.len() - 1)];
                if (t / 5_000_000) as usize != wi {
                    wi = (t / 5_000_000) as usize;
                }
                ctx_kbps = w.kbps.unwrap_or(ctx_kbps);
                let fps = (1000.0 / w.spf_ms).round() as u32;
                let d = est.on_second(t, ctx(ctx_kbps, fps));
                add(&mut out.totals, &est.counters);
                est.counters = Counters::default();
                d
            }
            Ev::Tick => est.tick(t),
        };
        if let Some(d) = d {
            out.decisions.push(d);
        }
    }
    add(&mut out.totals, &est.counters);
    out
}

fn ctx(kbps: f64, fps: u32) -> Context {
    Context {
        configured_kbps: kbps as u32,
        fps,
        floor_kbps: (kbps * 0.3) as u32,
        ceil_kbps: (kbps * 1.5) as u32,
        quality: 0.67,
        vbr: true,
    }
}

#[test]
fn latency_fast_replay_lan_makes_no_cut() {
    let out = replay(&section("lan-60fps"), 4804.0);
    assert_eq!(out.count(Kind::Cut), 0);
    assert_eq!(out.count(Kind::Severe), 0);
    assert_eq!(out.count(Kind::Rto), 0);
    // A clean link earns increases up to the ceiling (the encoder undershoots, k < 1).
    assert!(out.totals.states[0] > out.totals.evals * 9 / 10, "{:?}", out.totals);
}

#[test]
fn latency_fast_replay_wifi_60fps_false_cut_bound() {
    // 10 minutes of a Wi-Fi viewer: isolated 30-70 ms waits (the loop busy elsewhere) and one
    // 165 ms write. None is a capacity shortfall; the roadmap's gate allows one cut per 10 min.
    let w = section("wifi-60fps");
    assert!(w.len() >= 115);
    let out = replay(&w, 6000.0);
    assert_eq!(out.count(Kind::Cut), 0, "{:?}", out.cuts());
    assert!(
        out.count(Kind::Severe) + out.count(Kind::Cut) <= 1,
        "{:?}",
        out.cuts()
    );
}

#[test]
fn latency_fast_replay_wifi_30fps_no_cut_on_isolated_writes() {
    // 12 minutes at 15-30 fps with single writes of 97 ms, 128 ms and 264 ms, each alone in
    // its window as replayed: one blocked write is a pause of the link, not a capacity
    // shortfall. The 264 ms one keeps the capture loop waiting 231 ms for the next frame, but
    // the 66 ms report slices see at most 165 ms of it, under the 200 ms SEVERE bound.
    let out = replay(&section("wifi-30fps"), 6000.0);
    assert_eq!(out.count(Kind::Cut), 0, "{:?}", out.cuts());
    assert!(out.count(Kind::Severe) <= 1, "{:?}", out.cuts());
    assert!(out.totals.stalls > 0);
}

// ---------------------------------------------------------------------------------------------
// Fluid model

struct Model {
    metered: bool,
    ack: bool,
    fps: f64,
    enc_kbps: f64,
    sndbuf: f64,
    rtt_us: u64,
    // Capacity in kbps at time t (ms); 0 is a pause.
    cap: Box<dyn Fn(u64) -> f64>,
    // Receive-window-limited share of time at t (ms), for the v1 counter.
    rwin_share: Box<dyn Fn(u64) -> f64>,
    // RTO episodes counted up to time t (ms).
    timeouts: Box<dyn Fn(u64) -> u32>,
    displays: i32,
}

impl Model {
    fn new(cap: impl Fn(u64) -> f64 + 'static) -> Self {
        Self {
            metered: true,
            ack: false,
            fps: 60.0,
            enc_kbps: 8000.0,
            sndbuf: 256.0 * 1024.0,
            rtt_us: 2_000,
            cap: Box::new(cap),
            rwin_share: Box::new(|_| 0.0),
            timeouts: Box::new(|_| 0),
            displays: 1,
        }
    }

    fn run(&self, secs: u64) -> (Outcome, FastEstimator) {
        let mut est = FastEstimator::new(self.ack);
        let mut out = Outcome::default();
        let spf_us = (1e6 / self.fps) as u64;
        let frame = self.enc_kbps * 1000.0 / 8.0 / self.fps;
        let thr = stall_threshold(Duration::from_micros(spf_us)).as_micros() as u64;
        let slice = slice_for(Duration::from_micros(spf_us)).as_micros() as u64;
        let mut buffered = 0.0f64;
        let mut app = 0.0f64;
        let mut out_bytes = 0.0f64;
        let mut queued: Option<u64> = None; // enqueue time of the frame waiting for the writer
        let mut writing: Option<f64> = None; // bytes of the current write still to place
        let mut next_capture = 0u64;
        let mut next_stall = 0u64;
        let mut rwin_ms = 0.0f64;
        let mut seq = 0i32;
        for ms in 0..secs * 1000 {
            let now = ms * 1000;
            let cap = (self.cap)(ms);
            let drain = (cap * 1000.0 / 8.0 / 1000.0).min(buffered);
            buffered -= drain;
            out_bytes += drain;
            rwin_ms += (self.rwin_share)(ms);
            // Lock-step like the capture loop: the next frame once the writer took this one.
            if queued.is_none() && now >= next_capture {
                queued = Some(now);
                next_capture = (next_capture + spf_us).max(now);
                next_stall = now + thr;
            }
            let mut d = None;
            if writing.is_none() {
                if let Some(enq) = queued.take() {
                    est.on_write_begin(enq, now, frame as u32);
                    writing = Some(frame);
                }
            } else if let Some(enq) = queued {
                if now >= next_stall {
                    d = d.or(est.on_stall(now, now - enq));
                    next_stall += slice;
                }
            }
            if let Some(rest) = writing.as_mut() {
                let take = (self.sndbuf - buffered).max(0.0).min(*rest);
                buffered += take;
                app += take;
                *rest -= take;
                if *rest <= 0.0 {
                    writing = None;
                    seq += 1;
                    d = d.or(est.on_write_end(now, seq % self.displays));
                }
            }
            if self.metered && ms % 20 == 0 {
                let in_flight = (cap * self.rtt_us as f64 / 8.0 / 1000.0).min(out_bytes);
                est.on_tcp(TcpInput {
                    at_us: now,
                    app_bytes: app as u64,
                    unique_out: out_bytes as u64,
                    in_flight: in_flight as u32,
                    rtt_us: self.rtt_us as u32,
                    min_rtt_us: self.rtt_us as u32,
                    timeouts: (self.timeouts)(ms),
                    lim_rwin_ms: Some(rwin_ms as u32),
                });
            }
            if ms % 1000 == 0 {
                d = d.or(est.on_second(now, ctx(self.enc_kbps, self.fps as u32)));
                add(&mut out.totals, &est.counters);
                est.counters = Counters::default();
            }
            if ms % 50 == 0 {
                d = d.or(est.tick(now));
            }
            if let Some(d) = d {
                out.decisions.push(d);
            }
        }
        add(&mut out.totals, &est.counters);
        (out, est)
    }
}

fn first_cut_after(out: &Outcome, t_us: u64) -> Option<&Decision> {
    out.decisions
        .iter()
        .find(|d| d.at_us >= t_us && d.kind != Kind::Increase)
}

#[test]
fn latency_fast_capacity_step_reaction_under_200ms_metered() {
    // 50 Mbps, then 4 Mbps from t = 20 s under an 8 Mbps encoder.
    let m = Model::new(|ms| if ms < 20_000 { 50_000.0 } else { 4_000.0 });
    let (out, _) = m.run(25);
    assert!(
        out.decisions
            .iter()
            .all(|d| d.at_us >= 20_000_000 || d.kind == Kind::Increase),
        "{:?}",
        out.cuts()
    );
    let d = first_cut_after(&out, 20_000_000).expect("no cut after the step");
    assert_eq!(d.kind, Kind::Cut);
    // The roadmap's reaction time: from the queue passing T_hi to the decision, 100 ms of
    // persistence plus at most one 50 ms tick. Before that the backlog has to build up T_hi
    // (30 ms) beyond one frame: about 80 ms at 4 Mbps of excess.
    assert!(d.react_ms <= 200.0, "{:?}", d);
    assert!(d.at_us - 20_000_000 <= 300_000, "{:?}", d);
    assert_eq!(d.false_cut, Some(false));
    // Repeated cuts are spaced by the hold; the target approaches the capacity.
    let cuts: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.kind != Kind::Increase)
        .collect();
    for w in cuts.windows(2) {
        assert!(w[1].at_us - w[0].at_us >= HOLD_MIN_US);
    }
    assert!(cuts.last().unwrap().to_kbps <= 4_000.0 * 1.1, "{:?}", cuts);
}

#[test]
fn latency_fast_capacity_step_unmetered_cuts_after_buffer_fills() {
    // Without the meter the sender buffer hides the queue until it is full (256 KB at 4 Mbps
    // of excess: about 0.5 s); then the blocked writes show it.
    let mut m = Model::new(|ms| if ms < 20_000 { 50_000.0 } else { 4_000.0 });
    m.metered = false;
    let (out, _) = m.run(25);
    let d = first_cut_after(&out, 20_000_000).expect("no cut after the step");
    let fill_us = (m.sndbuf * 8.0 / (m.enc_kbps - 4_000.0) * 1000.0) as u64;
    assert!(
        d.at_us - 20_000_000 <= fill_us + 300_000,
        "{:?} fill {}",
        d,
        fill_us
    );
    assert!(
        out.decisions
            .iter()
            .all(|d| d.at_us >= 20_000_000 || d.kind == Kind::Increase),
        "{:?}",
        out.cuts()
    );
}

#[test]
fn latency_fast_wifi_pauses_do_not_cut() {
    // 50 Mbps with a 150 ms airtime pause every 2 s, for 10 minutes, metered and not.
    for metered in [true, false] {
        let mut m = Model::new(|ms| if ms % 2000 >= 1850 { 0.0 } else { 50_000.0 });
        m.metered = metered;
        let (out, _) = m.run(600);
        let cuts = out
            .decisions
            .iter()
            .filter(|d| d.kind != Kind::Increase)
            .count();
        assert!(cuts <= 1, "metered={} {:?}", metered, out.cuts());
    }
}

#[test]
fn latency_fast_long_outage_is_severe_once_per_hold() {
    // A 1 s outage with a small sender buffer: the writes block, the capture loop reports the
    // stall, SEVERE halves once per hold, and nothing else cuts in between.
    let mut m = Model::new(|ms| if (30_000..31_000).contains(&ms) { 0.0 } else { 50_000.0 });
    m.sndbuf = 32.0 * 1024.0;
    let (out, _) = m.run(35);
    let cuts: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.kind != Kind::Increase)
        .collect();
    assert!(!cuts.is_empty());
    assert!(cuts.iter().all(|d| d.kind == Kind::Severe), "{:?}", cuts);
    assert!(cuts[0].at_us <= 30_000_000 + 300_000, "{:?}", cuts[0]);
    for w in cuts.windows(2) {
        assert!(w[1].at_us - w[0].at_us >= HOLD_MIN_US);
    }
    assert!(out.totals.held > 0);
}

#[test]
fn latency_fast_recovery_is_gradual() {
    // 4 Mbps for 10 s from t = 20 s, then 50 Mbps again. In shadow mode the encoder kept
    // sending 8 Mbps, so the target went down to the floor; afterwards it climbs at most 8 %/s,
    // no cut follows, and it is back at the encoder's rate within 30 s.
    let m = Model::new(|ms| if (20_000..30_000).contains(&ms) { 4_000.0 } else { 50_000.0 });
    let (out, _) = m.run(70);
    let after: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.at_us > 30_500_000)
        .collect();
    assert!(after.iter().all(|d| d.kind == Kind::Increase), "{:?}", after);
    assert!(!after.is_empty());
    for d in &after {
        assert!(d.to_kbps / d.from_kbps <= 1.081, "{:?}", d);
    }
    let low = after[0].from_kbps;
    let trace = || {
        after
            .iter()
            .map(|d| format!("{}:{:.0}", d.at_us / 100_000, d.to_kbps))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // From the floor, doubling at 8 %/s takes 9 s plus the 1 s in NORMAL first.
    let doubled = after
        .iter()
        .find(|d| d.to_kbps >= 2.0 * low)
        .expect("never doubled");
    // Near the learned ceiling the climb is additive, so a little slower than 9 s.
    assert!(
        doubled.at_us - 30_000_000 <= 15_000_000,
        "{:?} from {}: {}",
        doubled,
        low,
        trace()
    );
    let back = after
        .iter()
        .find(|d| d.to_kbps >= 0.9 * 8000.0)
        .unwrap_or_else(|| panic!("never recovered: {}", trace()));
    assert!(back.at_us - 30_000_000 <= 30_000_000, "{:?}", back);
}

#[test]
fn latency_fast_receiver_limited_does_not_cut() {
    // The viewer stops reading (receive window) while the link itself is fine: the backlog
    // grows, but the meter says rwin-limited, so no cut.
    let mut m = Model::new(|ms| if (20_000..25_000).contains(&ms) { 4_000.0 } else { 50_000.0 });
    m.rwin_share = Box::new(|ms| if (20_000..25_000).contains(&ms) { 0.9 } else { 0.0 });
    let (out, _) = m.run(25);
    assert!(
        out.decisions.iter().all(|d| d.kind == Kind::Increase),
        "{:?}",
        out.cuts()
    );
    assert!(out.totals.states[State::Receiver.index()] > 0);
}

#[test]
fn latency_fast_rto_cuts_only_on_low_delivery() {
    // An RTO with full delivery afterwards is ignored.
    let mut m = Model::new(|_| 50_000.0);
    m.timeouts = Box::new(|ms| (ms >= 20_000) as u32);
    let (out, _) = m.run(23);
    assert_eq!(out.totals.rto_cuts, 0, "{:?}", out.cuts());
    assert_eq!(out.totals.rto_ignored, 1);
    // With delivery down to a third right after it, the cut goes to 0.85 x delivered.
    let mut m = Model::new(|ms| if ms < 20_000 { 50_000.0 } else { 2_500.0 });
    m.timeouts = Box::new(|ms| (ms >= 20_000) as u32);
    let (out, _) = m.run(21);
    assert!(
        out.decisions
            .iter()
            .any(|d| d.at_us >= 20_000_000 && d.kind != Kind::Increase),
        "{:?}",
        out.cuts()
    );
}

#[test]
fn latency_fast_ack_and_multi_display_make_no_decision() {
    let step = |ms: u64| if ms < 10_000 { 50_000.0 } else { 2_000.0 };
    let mut m = Model::new(step);
    m.ack = true;
    let (out, est) = m.run(15);
    assert!(out.decisions.is_empty(), "{:?}", out.decisions);
    assert_eq!(est.ineligible(), Some("ack"));
    let mut m = Model::new(step);
    m.displays = 2;
    let (out, est) = m.run(15);
    assert!(out.decisions.is_empty(), "{:?}", out.decisions);
    assert_eq!(est.ineligible(), Some("multi"));
}

#[test]
fn latency_fast_k_follows_measured_rate() {
    // The encoder sends twice its configured bitrate (nvenc opened at 30 fps, running at 60):
    // the estimator learns k = 2 and its target is in measured units.
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    for s in 0..5u64 {
        for _ in 0..60 {
            est.on_write_begin(t, t, 2 * 8000 * 1000 / 8 / 60);
            t += 100;
            let _ = est.on_write_end(t, 0);
            t += 16_567;
        }
        let _ = est.on_second(t, ctx(8000.0, 60));
        let _ = s;
    }
    assert!((est.k() - 2.0).abs() < 0.1, "k {}", est.k());
    // It started at the measured 16 Mbps and may have climbed toward the ceiling (1.5 x the
    // configured 8 Mbps, in measured units 24 Mbps); what it would set stays configured kbps.
    let target = est.target().unwrap();
    assert!((14_400.0..=24_000.0 * 1.01).contains(&target), "target {}", target);
    assert!(target / est.k() <= 12_000.0 * 1.01);
    assert_eq!(est.ineligible(), None);
}

#[test]
fn latency_fast_stall_timing() {
    assert_eq!(
        stall_threshold(Duration::from_millis(16)),
        Duration::from_millis(50)
    );
    assert_eq!(
        stall_threshold(Duration::from_millis(33)),
        Duration::from_millis(99)
    );
    assert_eq!(slice_for(Duration::from_millis(16)), Duration::from_millis(32));
    assert_eq!(slice_for(Duration::from_millis(1000)), Duration::from_millis(300));
    assert_eq!(slice_for(Duration::from_millis(5)), Duration::from_millis(20));
}

#[test]
fn latency_fast_quality_change_resets_target() {
    let mut est = FastEstimator::new(false);
    let _ = est.on_second(0, ctx(8000.0, 30));
    let _ = est.on_second(3_000_000, ctx(8000.0, 30));
    assert_eq!(est.target(), Some(8000.0));
    let mut best = ctx(8000.0, 30);
    best.quality = 1.5;
    best.ceil_kbps = 18_000;
    let _ = est.on_second(4_000_000, best);
    assert_eq!(est.target(), Some(18_000.0));
    assert!(est.ceiling().is_none());
}
