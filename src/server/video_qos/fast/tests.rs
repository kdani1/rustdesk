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
    t.k_rejected += c.k_rejected;
    t.resync += c.resync;
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
        ..Default::default()
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
    // Live (roadmap M16): every decision sets the encoder's bitrate, as `VideoQoS::on_fast` does
    // for a single viewer (cuts down to the target, increases by their factor).
    live: bool,
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
            live: false,
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
        // The encoder's bitrate: fixed in shadow mode, the estimator's live.
        let mut enc = self.enc_kbps;
        let (floor, ceil) = (self.enc_kbps * 0.3, self.enc_kbps * 1.5);
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
                    let frame = enc * 1000.0 / 8.0 / self.fps;
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
                let c = if self.live {
                    Context {
                        configured_kbps: enc as u32,
                        fps: self.fps as u32,
                        floor_kbps: floor as u32,
                        ceil_kbps: ceil as u32,
                        quality: 0.67,
                        vbr: true,
                        stream_kbps: enc as u32,
                        displays: 1,
                        live: true,
                        ..Default::default()
                    }
                } else {
                    ctx(self.enc_kbps, self.fps as u32)
                };
                d = d.or(est.on_second(now, c));
                add(&mut out.totals, &est.counters);
                est.counters = Counters::default();
            }
            if ms % 50 == 0 {
                d = d.or(est.tick(now));
            }
            if let Some(d) = d {
                if self.live {
                    let r = est.report(Some(&d));
                    match (r.step, r.set_kbps) {
                        (Some(Step::Cut { .. }), Some(set)) => enc = enc.min(set),
                        (Some(Step::Increase { factor }), Some(set)) => {
                            enc = (enc * factor).min(set)
                        }
                        _ => {}
                    }
                    enc = enc.clamp(floor, ceil);
                }
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
fn latency_fast_long_outage_is_severe_once() {
    // A 1 s outage with a small sender buffer: the writes block and the capture loop reports
    // the same unfetched frame after every slice. SEVERE halves once for the whole outage
    // (metered and not), and nothing else cuts.
    for metered in [true, false] {
        let mut m =
            Model::new(|ms| if (30_000..31_000).contains(&ms) { 0.0 } else { 50_000.0 });
        m.sndbuf = 32.0 * 1024.0;
        m.metered = metered;
        let (out, _) = m.run(35);
        let cuts: Vec<_> = out
            .decisions
            .iter()
            .filter(|d| d.kind != Kind::Increase)
            .collect();
        assert_eq!(cuts.len(), 1, "metered={} {:?}", metered, cuts);
        assert_eq!(cuts[0].kind, Kind::Severe);
        assert!(cuts[0].at_us <= 30_000_000 + 300_000, "{:?}", cuts[0]);
        assert!(out.totals.stalls > 5, "{:?}", out.totals);
    }
}

#[test]
fn latency_fast_severe_once_per_stall_episode() {
    let mut est = FastEstimator::new(false);
    let _ = est.on_second(0, ctx(8000.0, 30));
    let _ = est.on_second(3_000_000, ctx(8000.0, 30));
    assert_eq!(est.target(), Some(8000.0));
    // One frame stays unfetched for 1.2 s: reported every 66 ms from 99 ms on.
    est.on_write_begin(3_100_000, 3_100_000, 30_000);
    let mut severe = 0;
    let mut t = 3_200_000;
    while t < 4_300_000 {
        if let Some(d) = est.on_stall(t, t - 3_100_000) {
            assert_eq!(d.kind, Kind::Severe);
            severe += 1;
        }
        t += 66_000;
    }
    assert_eq!(severe, 1);
    assert_eq!(est.target(), Some(4000.0));
    // The write ended: a later stall of another frame is a new episode.
    let _ = est.on_write_end(4_300_000, 0);
    est.on_write_begin(4_400_000, 4_400_000, 30_000);
    let d = est.on_stall(4_700_000, 300_000).expect("a new stall");
    assert_eq!(d.kind, Kind::Severe);
}

#[test]
fn latency_fast_rto_in_pause_does_not_cut() {
    // A 400 ms airtime pause on a metered 50 Mbps link; Windows' minimum RTO (300 ms) fires in
    // it, and delivery is full afterwards: judged on what followed the RTO, no cut of any kind.
    let mut m = Model::new(|ms| if (20_000..20_400).contains(&ms) { 0.0 } else { 50_000.0 });
    m.timeouts = Box::new(|ms| (ms >= 20_300) as u32);
    let (out, _) = m.run(25);
    let cuts: Vec<_> = out
        .decisions
        .iter()
        .filter(|d| d.kind != Kind::Increase)
        .collect();
    assert!(cuts.is_empty(), "{:?}", cuts);
    assert_eq!(out.totals.rto_ignored, 1, "{:?}", out.totals);
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
    // With delivery down to a third right after it, a cut follows (OVERUSE or the RTO's).
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
fn latency_fast_rto_judged_on_delivery_after_it() {
    // Unit level, no queue: an RTO followed by 300 ms of delivery at a third of the send rate
    // is an RTO cut (0.85 x that delivery, at most a halving); the same RTO followed by full
    // delivery is not.
    for (acked_share, cut) in [(0.33, true), (1.0, false)] {
        let mut est = FastEstimator::new(false);
        let _ = est.on_second(0, ctx(8000.0, 30));
        let _ = est.on_second(3_000_000, ctx(8000.0, 30));
        let mut app = 0u64;
        let mut acked = 0u64;
        let mut decisions = Vec::new();
        let mut t = 3_000_000u64;
        while t <= 4_000_000 {
            // 8 Mbps written; delivered at full rate before the RTO at 3.5 s.
            app += 8000 * 1000 / 8 / 50;
            let share = if t >= 3_500_000 { acked_share } else { 1.0 };
            acked += (8000.0 * 1000.0 / 8.0 / 50.0 * share) as u64;
            // Everything written is out (no unsent backlog, no RTT growth: no queue signal);
            // what is not acked is in flight.
            est.on_tcp(TcpInput {
                at_us: t,
                app_bytes: app,
                unique_out: app,
                in_flight: (app - acked) as u32,
                rtt_us: 2_000,
                min_rtt_us: 2_000,
                timeouts: (t >= 3_500_000) as u32,
                lim_rwin_ms: None,
            });
            if let Some(d) = est.tick(t) {
                decisions.push(d);
            }
            t += 20_000;
        }
        let rto: Vec<_> = decisions.iter().filter(|d| d.kind == Kind::Rto).collect();
        if cut {
            assert_eq!(rto.len(), 1, "{:?}", decisions);
            assert!(rto[0].at_us >= 3_800_000, "{:?}", rto[0]);
            assert!((rto[0].to_kbps - 4000.0).abs() < 1.0, "{:?}", rto[0]);
        } else {
            assert!(rto.is_empty(), "{:?}", decisions);
            assert_eq!(est.counters.rto_ignored, 1);
        }
    }
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
fn latency_fast_k_ignores_content_limited_seconds() {
    // k = 2 learned on full-budget seconds; then 10 s of typing at full frame rate at a tenth
    // of the budget. k barely moves (only the slow drift), the target keeps its ceiling, and
    // the first full-budget second brings k back at once.
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    let second = |est: &mut FastEstimator, t: &mut u64, kbps: u64| {
        for _ in 0..60 {
            est.on_write_begin(*t, *t, (kbps * 1000 / 8 / 60) as u32);
            *t += 100;
            let _ = est.on_write_end(*t, 0);
            *t += 16_567;
        }
        let _ = est.on_second(*t, ctx(8000.0, 60));
    };
    for _ in 0..5 {
        second(&mut est, &mut t, 16_000);
    }
    assert!((est.k() - 2.0).abs() < 0.1, "k {}", est.k());
    let target = est.target().unwrap();
    est.counters = Counters::default();
    for _ in 0..10 {
        second(&mut est, &mut t, 1_600);
    }
    assert!(est.k() > 1.75, "k {}", est.k());
    assert_eq!(est.counters.k_rejected, 10);
    assert!(est.target().unwrap() >= target, "{:?} {}", est.target(), target);
    second(&mut est, &mut t, 16_000);
    assert!((est.k() - 2.0).abs() < 0.1, "k {}", est.k());
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

// ---------------------------------------------------------------------------------------------
// Live (roadmap M16)

fn live_ctx(configured: f64, stream: f64) -> Context {
    Context {
        configured_kbps: configured as u32,
        fps: 30,
        floor_kbps: 1000,
        ceil_kbps: 12_000,
        quality: 0.67,
        vbr: true,
        stream_kbps: stream as u32,
        displays: 1,
        live: true,
        ..Default::default()
    }
}

#[test]
fn latency_fast_live_report_only_when_live_and_eligible() {
    let mut est = FastEstimator::new(false);
    let _ = est.on_second(0, ctx(8000.0, 30));
    let _ = est.on_second(3_000_000, ctx(8000.0, 30));
    // Shadow: no target for the stream.
    assert!(est.report(None).set_kbps.is_none());
    let _ = est.on_second(4_000_000, live_ctx(8000.0, 8000.0));
    let r = est.report(None);
    assert_eq!(r.set_kbps, Some(8000.0));
    assert!(r.step.is_none());
    // Several video services share the ratio: not eligible live.
    let mut multi = live_ctx(8000.0, 8000.0);
    multi.displays = 2;
    let _ = est.on_second(5_000_000, multi);
    assert_eq!(est.ineligible(), Some("displays"));
    assert!(est.report(None).set_kbps.is_none());
}

#[test]
fn latency_fast_live_target_follows_a_lower_stream() {
    let mut est = FastEstimator::new(false);
    let _ = est.on_second(0, live_ctx(8000.0, 8000.0));
    let _ = est.on_second(3_000_000, live_ctx(8000.0, 8000.0));
    assert_eq!(est.target(), Some(8000.0));
    // Another viewer's target or the TestDelay backstop holds the stream at 5 Mbps.
    let _ = est.on_second(4_000_000, live_ctx(8000.0, 5000.0));
    assert_eq!(est.target(), Some(5000.0));
    assert_eq!(est.counters.resync, 1);
    // Within the quantisation step it is left alone.
    let _ = est.on_second(5_000_000, live_ctx(5000.0, 4900.0));
    assert_eq!(est.target(), Some(5000.0));
    // A quality choice (new epoch) resets the target to the new ceiling.
    let mut q = live_ctx(5000.0, 12_000.0);
    q.epoch = 1;
    let _ = est.on_second(6_000_000, q);
    assert_eq!(est.target(), Some(12_000.0));
}

#[test]
fn latency_fast_live_closed_loop_capacity_step() {
    // 50 Mbps, 4 Mbps from t = 20 s to 30 s, then 50 Mbps again, under an 8 Mbps encoder that
    // follows the estimator. The first cut comes within 300 ms of the step and the encoder
    // settles under the capacity in a few cuts, without falling to the floor; afterwards it
    // climbs back to 8 Mbps.
    let mut m = Model::new(|ms| {
        if (20_000..30_000).contains(&ms) {
            4_000.0
        } else {
            50_000.0
        }
    });
    m.live = true;
    let (out, est) = m.run(60);
    let before: Vec<_> = out
        .cuts()
        .into_iter()
        .filter(|d| d.at_us < 20_000_000)
        .collect();
    assert!(before.is_empty(), "{:?}", before);
    let first = first_cut_after(&out, 20_000_000).expect("no cut after the step");
    assert!(first.at_us - 20_000_000 <= 300_000, "{:?}", first);
    assert!(first.react_ms <= 200.0, "{:?}", first);
    let episode: Vec<_> = out
        .cuts()
        .into_iter()
        .filter(|d| (20_000_000..30_000_000).contains(&d.at_us))
        .collect();
    // Settled within 3 s; later at most a probe or two at the capacity (x1.08/s climbs back
    // to it within the 10 s).
    let settle = episode.iter().filter(|d| d.at_us < 23_000_000).count();
    assert!(settle <= 4, "{:?}", episode);
    assert!(episode.len() - settle <= 2, "{:?}", episode);
    let lowest = episode
        .iter()
        .map(|d| d.set_kbps)
        .fold(f64::INFINITY, f64::min);
    assert!(lowest <= 4_000.0, "{:?}", episode);
    assert!(lowest >= 2_000.0, "fell too far: {:?}", episode);
    let back = out
        .decisions
        .iter()
        .find(|d| d.at_us > 30_000_000 && d.set_kbps >= 0.9 * 8000.0)
        .expect("never recovered");
    assert!(back.at_us - 30_000_000 <= 25_000_000, "{:?}", back);
    assert!(est.target().is_some_and(|t| t >= 0.9 * 8000.0));
}

/// One second of 30 frames carrying `kbps` measured, each written at once (no queue), then the
/// once-a-second context.
fn light_second(est: &mut FastEstimator, t: &mut u64, kbps: f64, c: Context) {
    for _ in 0..30 {
        est.on_write_begin(*t, *t, (kbps * 1000.0 / 8.0 / 30.0) as u32);
        *t += 100;
        let _ = est.on_write_end(*t, 0);
        let _ = est.tick(*t);
        *t += 33_233;
    }
    let _ = est.on_second(*t, c);
}

#[test]
fn latency_fast_live_k_skips_a_second_with_a_bitrate_change() {
    // k = 1 learned at 8 Mbps. Another viewer's cut halves the configured bitrate mid-second:
    // that second carried 6 Mbps against 4 configured, which would read as k = 1.5.
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    for _ in 0..4 {
        light_second(&mut est, &mut t, 8000.0, live_ctx(8000.0, 8000.0));
    }
    assert!((est.k() - 1.0).abs() < 0.05, "k {}", est.k());
    light_second(&mut est, &mut t, 6000.0, live_ctx(4000.0, 4000.0));
    assert!((est.k() - 1.0).abs() < 0.05, "k {}", est.k());
    // The next whole second at the new setting is learned from again.
    light_second(&mut est, &mut t, 4000.0, live_ctx(4000.0, 4000.0));
    assert!((est.k() - 1.0).abs() < 0.05, "k {}", est.k());
    // So its target is the stream's, not 20 % under it.
    assert!(est.target().is_some_and(|x| (x - 4000.0).abs() < 200.0), "{:?}", est.target());
}

#[test]
fn latency_fast_live_app_limited_increase_bounded_by_ceiling() {
    // After a cut to 2 Mbps the content turns light (500 kbps sent at full frame rate): the
    // target still climbs, with the stream following it, but not past the learned ceiling C.
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    for _ in 0..4 {
        light_second(&mut est, &mut t, 8000.0, live_ctx(8000.0, 8000.0));
    }
    light_second(&mut est, &mut t, 2000.0, live_ctx(2000.0, 2000.0));
    light_second(&mut est, &mut t, 2000.0, live_ctx(2000.0, 2000.0));
    let climb = |est: &mut FastEstimator, t: &mut u64, secs: usize| {
        est.counters = Counters::default();
        for _ in 0..secs {
            let stream = est.target().unwrap() / est.k();
            light_second(est, t, 500.0, live_ctx(stream, stream));
            assert!(est.app_limited());
        }
        est.target().unwrap()
    };
    // The content turns light (the second that tells it may still increase as before).
    let start = climb(&mut est, &mut t, 1);
    assert!(start <= 2200.0, "{start}");
    // Not a cut (the stream was lowered elsewhere): nothing to undo, no app-limited increase.
    assert_eq!(climb(&mut est, &mut t, 10), start);
    assert_eq!(est.counters.incs, 0);
    // A cut from 8 Mbps: undone up to C.
    est.restore = Some(8000.0);
    est.ceiling_c = Some(4000.0);
    let end = climb(&mut est, &mut t, 30);
    assert!(est.counters.app_incs > 0, "{:?}", est.counters.app_incs);
    assert!(end > 1.5 * start, "{start} -> {end}");
    assert!(end <= 4000.0 + 1.0, "{end}");
    // An app-limited climb neither reaches nor forgets C.
    assert_eq!(est.ceiling_c, Some(4000.0));
    assert!(est.c_reached_at.is_none());
    // Without C: back to the target before the cut, not beyond.
    est.ceiling_c = None;
    let end = climb(&mut est, &mut t, 30);
    assert!((end - 8000.0).abs() < 1.0, "{end}");
    assert!(est.restore.is_none());
    assert_eq!(climb(&mut est, &mut t, 5), end);
    // No frames at all (a static screen): no increase.
    est.restore = Some(12_000.0);
    est.counters = Counters::default();
    for _ in 0..10 {
        t += 1_000_000;
        let _ = est.on_second(t, live_ctx(end, end));
    }
    assert_eq!(est.counters.app_incs, 0);
}

// Per-frame feedback (M23) on a transport the estimator has no other signal for: a live UDP
// lane takes the frames off TCP, so only the viewer's batches (every 50 ms) and the 1 s
// context reach the estimator.
fn fb_run(
    est: &mut FastEstimator,
    t: &mut u64,
    secs: f64,
    c: Context,
    mut sample: impl FnMut(u64) -> FeedbackSample,
) -> Vec<Decision> {
    let mut out = Vec::new();
    let end = *t + (secs * 1e6) as u64;
    while *t < end {
        *t += 50_000;
        let mut s = sample(*t);
        s.at_us = *t;
        out.extend(est.on_feedback(s));
        if *t % 1_000_000 == 0 {
            out.extend(est.on_second(*t, c));
        }
    }
    out
}

fn calm(_: u64) -> FeedbackSample {
    FeedbackSample {
        q_owd_ms: Some(2.0),
        recv_kbps: Some(8000.0),
        viewer_ms: Some(20.0),
        loss: Some(0.0),
        ..Default::default()
    }
}

#[test]
fn latency_fast_feedback_queue_cuts_without_tcp_writes() {
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    let c = live_ctx(8000.0, 8000.0);
    assert!(fb_run(&mut est, &mut t, 4.0, c, calm).is_empty());
    assert_eq!(est.target(), Some(8000.0));
    // The viewer's link congests: queueing delay 120 ms and growing, GCC overuse, 5 Mbps
    // arriving.
    let step = t;
    let cuts = fb_run(&mut est, &mut t, 0.5, c, |at| FeedbackSample {
        q_owd_ms: Some(120.0 + (at - step) as f64 / 10_000.0),
        overuse: true,
        recv_kbps: Some(5000.0),
        viewer_ms: Some(150.0),
        loss: Some(0.0),
        ..Default::default()
    });
    let first = cuts.iter().find(|d| d.kind == Kind::Cut).expect("no cut");
    assert!(first.at_us - step <= 200_000, "react {}", first.at_us - step);
    // min(0.9 x 8000, 0.85 x the delivered rate), which the viewer's batches give: the 500 ms
    // window still holds some 8 Mbps batches at the first cut, so between 0.85 x 5000 and 7200.
    assert!(
        first.to_kbps <= 7200.0 + 1.0 && first.to_kbps >= 4250.0 - 1.0,
        "{}",
        first.to_kbps
    );
    assert!(first.r_acked < 8000.0, "{}", first.r_acked);
    assert!(est.counters.feedback > 0);
}

#[test]
fn latency_fast_feedback_loss_cuts_once_per_second_and_blocks_increase() {
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    let c = live_ctx(8000.0, 8000.0);
    let _ = fb_run(&mut est, &mut t, 4.0, c, calm);
    let lossy = |_| FeedbackSample {
        loss: Some(0.2),
        ..calm(0)
    };
    let d = fb_run(&mut est, &mut t, 1.5, c, lossy);
    let loss: Vec<_> = d.iter().filter(|d| d.kind == Kind::Loss).collect();
    assert_eq!(loss.len(), 2, "{:?}", loss);
    assert!(loss[1].at_us - loss[0].at_us >= 1_000_000);
    // (1 - 0.5 x 0.2) x 8000.
    assert!((loss[0].to_kbps - 7200.0).abs() < 1.0, "{}", loss[0].to_kbps);
    assert_eq!(est.counters.loss_cuts, 2);
    // Some loss left (2 %..10 %): no cut, no increase.
    let some = |_| FeedbackSample {
        loss: Some(0.05),
        ..calm(0)
    };
    let before = est.target();
    let d = fb_run(&mut est, &mut t, 5.0, c, some);
    assert!(d.is_empty(), "{:?}", d);
    assert_eq!(est.target(), before);
}

#[test]
fn latency_fast_feedback_slow_viewer_is_receiver_limited() {
    let mut est = FastEstimator::new(false);
    let mut t = 0u64;
    let c = live_ctx(8000.0, 8000.0);
    let _ = fb_run(&mut est, &mut t, 4.0, c, calm);
    let slow = |_| FeedbackSample {
        viewer_ms: Some(400.0),
        ..calm(0)
    };
    let d = fb_run(&mut est, &mut t, 2.0, c, slow);
    assert!(d.iter().all(|d| d.kind == Kind::Increase), "{:?}", d);
    assert_eq!(est.state(), State::Receiver);
}
