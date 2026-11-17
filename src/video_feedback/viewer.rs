//! Viewer side of the per-frame feedback (M23): what the io_loop and the video threads record,
//! and the batches that go to the host.
//!
//! - Arrivals are recorded by the io_loop as each video frame enters `handle_video_frame`
//!   (TCP: right after the parse; UDP: as the lane releases it), with the frame's size and the
//!   display's decoder queue length.
//! - Decode and present ends are recorded by each display's video thread into its `DoneLog`
//!   (one relaxed load per frame while feedback is off) and turned into arrival-relative delays
//!   when a batch is built.
//! - UDP loss comes from the lane (`ViewerLane::take_feedback`).
//!
//! A batch goes out at most every `INTERVAL`, only when there is something in it, and carries
//! at most `MAX_FRAMES` arrivals, `MAX_DONE` done entries and `MAX_LOST` loss ranges; the rest
//! is counted in `dropped`.

use super::{frame_key, us_since, VERSION};
use base::{
    config::keys,
    message_proto::{
        Message, Misc, VideoFeedback, VideoFeedbackDone, VideoFeedbackFrame, VideoFeedbackLost,
        VideoFeedbackUdp, VideoFrame,
    },
};
use hbb_common::config::LocalConfig;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Shortest time between two batches.
pub const INTERVAL: Duration = Duration::from_millis(50);
pub const MAX_FRAMES: usize = 64;
pub const MAX_DONE: usize = 64;
pub const MAX_LOST: usize = 16;
/// Arrivals remembered for the decode/present delays (about 4 s at 60 fps).
const ARRIVALS_KEPT: usize = 256;
/// Done entries a video thread keeps between two batches.
const DONE_CAP: usize = 256;

/// Whether this viewer sends feedback to a host that advertised `host_version`: Windows only,
/// and not with `enable-video-feedback=N`.
pub fn enabled(host_version: u32) -> bool {
    cfg!(windows)
        && host_version >= VERSION
        && LocalConfig::get_option(keys::OPTION_ENABLE_VIDEO_FEEDBACK) != "N"
}

/// A frame the video thread finished.
#[derive(Clone, Copy, Debug)]
pub struct Done {
    pub pts: i64,
    pub decoded: Instant,
    /// None: present pacing did not hand it to the UI.
    pub presented: Option<Instant>,
}

/// One display's decode/present ends, written by its video thread, taken by the io_loop.
#[derive(Default)]
pub struct DoneLog {
    on: AtomicBool,
    q: Mutex<Vec<Done>>,
}

impl DoneLog {
    pub fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
        if !on {
            if let Ok(mut q) = self.q.lock() {
                q.clear();
            }
        }
    }

    /// The video thread, once per decoded frame.
    #[inline]
    pub fn on_frame(&self, pts: Option<i64>, decoded: Instant, presented: Option<Instant>) {
        if !self.on.load(Ordering::Relaxed) {
            return;
        }
        let Some(pts) = pts else {
            return;
        };
        if let Ok(mut q) = self.q.lock() {
            if q.len() < DONE_CAP {
                q.push(Done {
                    pts,
                    decoded,
                    presented,
                });
            }
        }
    }

    pub fn take(&self) -> Vec<Done> {
        if !self.on.load(Ordering::Relaxed) {
            return Vec::new();
        }
        self.q
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
}

/// UDP lane receive counters since the previous batch (see `fragment::LossStats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdpCounts {
    pub datagrams_expected: u64,
    pub datagrams_received: u64,
    pub frames_completed: u64,
    pub frames_lost: u64,
    pub fec_recovered: u64,
    pub fec_unrecoverable: u64,
}

impl UdpCounts {
    pub fn add(&mut self, o: &UdpCounts) {
        self.datagrams_expected += o.datagrams_expected;
        self.datagrams_received += o.datagrams_received;
        self.frames_completed += o.frames_completed;
        self.frames_lost += o.frames_lost;
        self.fec_recovered += o.fec_recovered;
        self.fec_unrecoverable += o.fec_unrecoverable;
    }

    /// `self - o`, each field at least 0.
    pub fn minus(&self, o: &UdpCounts) -> UdpCounts {
        UdpCounts {
            datagrams_expected: self.datagrams_expected.saturating_sub(o.datagrams_expected),
            datagrams_received: self.datagrams_received.saturating_sub(o.datagrams_received),
            frames_completed: self.frames_completed.saturating_sub(o.frames_completed),
            frames_lost: self.frames_lost.saturating_sub(o.frames_lost),
            fec_recovered: self.fec_recovered.saturating_sub(o.fec_recovered),
            fec_unrecoverable: self.fec_unrecoverable.saturating_sub(o.fec_unrecoverable),
        }
    }

    pub fn is_zero(&self) -> bool {
        *self == UdpCounts::default()
    }
}

/// A UDP lane loss range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LostRange {
    pub display: u8,
    pub from: u32,
    pub to: u32,
    pub repaired: bool,
}

#[derive(Default)]
struct Win {
    batches: u64,
    frames: u64,
    done: u64,
    lost: u64,
    dropped: u64,
    bytes: u64,
}

/// One session's feedback state in the io_loop (created when the host's PeerInfo asks for it).
pub struct Collector {
    base: Instant,
    seq: u32,
    last_sent: Option<Instant>,
    frames: Vec<VideoFeedbackFrame>,
    arrivals: VecDeque<(i32, i64, Instant)>,
    lost: Vec<VideoFeedbackLost>,
    udp: UdpCounts,
    dropped: u32,
    win: Win,
    win_start: Instant,
}

impl Collector {
    pub fn new(now: Instant) -> Self {
        Self {
            base: now,
            seq: 0,
            last_sent: None,
            frames: Vec::new(),
            arrivals: VecDeque::new(),
            lost: Vec::new(),
            udp: UdpCounts::default(),
            dropped: 0,
            win: Win::default(),
            win_start: now,
        }
    }

    /// A video frame entered the decoder path.
    pub fn on_arrival(&mut self, vf: &VideoFrame, at: Instant, queue_len: usize, udp: bool) {
        let Some((pts, bytes)) = frame_key(vf) else {
            return;
        };
        if self.arrivals.len() >= ARRIVALS_KEPT {
            self.arrivals.pop_front();
        }
        self.arrivals.push_back((vf.display, pts, at));
        if self.frames.len() >= MAX_FRAMES {
            self.dropped += 1;
            return;
        }
        self.frames.push(VideoFeedbackFrame {
            display: vf.display,
            pts,
            arrival_us: us_since(self.base, at),
            bytes,
            queue_len: queue_len.min(u32::MAX as usize) as u32,
            udp,
            ..Default::default()
        });
    }

    /// UDP lane loss since the last call.
    pub fn on_udp(&mut self, counts: &UdpCounts, lost: &[LostRange]) {
        self.udp.add(counts);
        for l in lost {
            if self.lost.len() >= MAX_LOST {
                self.dropped += 1;
                continue;
            }
            self.lost.push(VideoFeedbackLost {
                display: l.display as i32,
                from: l.from,
                to: l.to,
                repaired: l.repaired,
                ..Default::default()
            });
        }
    }

    /// A batch may go now.
    pub fn due(&self, now: Instant) -> bool {
        self.last_sent
            .map_or(true, |t| now.saturating_duration_since(t) >= INTERVAL)
    }

    /// The batch to send now, with the video threads' done entries `(display, entries)`; None
    /// when not due or empty (the done entries are then kept for the next one).
    pub fn build(&mut self, now: Instant, done: Vec<(i32, Vec<Done>)>) -> Option<Message> {
        let mut out_done = Vec::new();
        for (display, entries) in done {
            for d in entries {
                if out_done.len() >= MAX_DONE {
                    self.dropped += 1;
                    continue;
                }
                // Arrival of this frame; one that arrived before feedback started is skipped.
                let Some(arrived) = self
                    .arrivals
                    .iter()
                    .rev()
                    .find(|(disp, pts, _)| *disp == display && *pts == d.pts)
                    .map(|(_, _, at)| *at)
                else {
                    continue;
                };
                let delay = |t: Instant| {
                    t.saturating_duration_since(arrived)
                        .as_micros()
                        .clamp(1, u32::MAX as u128) as u32
                };
                out_done.push(VideoFeedbackDone {
                    display,
                    pts: d.pts,
                    decode_us: delay(d.decoded),
                    present_us: d.presented.map_or(0, delay),
                    ..Default::default()
                });
            }
        }
        if self.frames.is_empty()
            && out_done.is_empty()
            && self.lost.is_empty()
            && self.udp.is_zero()
            && self.dropped == 0
        {
            return None;
        }
        self.last_sent = Some(now);
        let udp = std::mem::take(&mut self.udp);
        let fb = VideoFeedback {
            seq: self.seq,
            sent_us: us_since(self.base, now),
            frames: std::mem::take(&mut self.frames),
            done: out_done,
            lost: std::mem::take(&mut self.lost),
            udp: (!udp.is_zero())
                .then(|| VideoFeedbackUdp {
                    datagrams_expected: udp.datagrams_expected,
                    datagrams_received: udp.datagrams_received,
                    frames_completed: udp.frames_completed,
                    frames_lost: udp.frames_lost,
                    fec_recovered: udp.fec_recovered,
                    fec_unrecoverable: udp.fec_unrecoverable,
                    ..Default::default()
                })
                .into(),
            dropped: std::mem::take(&mut self.dropped),
            ..Default::default()
        };
        self.seq = self.seq.wrapping_add(1);
        self.win.batches += 1;
        self.win.frames += fb.frames.len() as u64;
        self.win.done += fb.done.len() as u64;
        self.win.lost += fb.lost.len() as u64;
        self.win.dropped += fb.dropped as u64;
        let mut misc = Misc::new();
        misc.set_video_feedback(fb);
        let mut msg = Message::new();
        msg.set_misc(misc);
        self.win.bytes += hbb_common::protobuf::Message::compute_size(&msg);
        Some(msg)
    }

    /// The `VFB viewer` log fields of the window since the last call, once it is `every` long.
    pub fn take_window(&mut self, now: Instant, every: Duration) -> Option<String> {
        let span = now.saturating_duration_since(self.win_start);
        if span < every {
            return None;
        }
        let w = std::mem::take(&mut self.win);
        self.win_start = now;
        Some(format!(
            "secs={:.1} batches={} frames={} done={} lost={} dropped={} bytes={}",
            span.as_secs_f64(),
            w.batches,
            w.frames,
            w.done,
            w.lost,
            w.dropped,
            w.bytes
        ))
    }
}

#[cfg(test)]
mod latency_tests {
    use super::*;
    use base::message_proto::{message, misc, EncodedVideoFrame, EncodedVideoFrames};

    fn frame(display: i32, pts: i64, bytes: usize) -> VideoFrame {
        let mut vf = VideoFrame::new();
        vf.display = display;
        vf.set_h265s(EncodedVideoFrames {
            frames: vec![EncodedVideoFrame {
                data: vec![0u8; bytes].into(),
                pts,
                ..Default::default()
            }],
            ..Default::default()
        });
        vf
    }

    fn feedback(m: Message) -> VideoFeedback {
        match m.union {
            Some(message::Union::Misc(misc)) => match misc.union {
                Some(misc::Union::VideoFeedback(f)) => f,
                _ => panic!("not feedback"),
            },
            _ => panic!("not misc"),
        }
    }

    #[test]
    fn latency_feedback_batches_arrivals_and_done() {
        let t0 = Instant::now();
        let mut c = Collector::new(t0);
        assert!(c.build(t0, Vec::new()).is_none(), "nothing to report");
        c.on_arrival(&frame(0, 100, 1000), t0 + Duration::from_millis(5), 2, false);
        c.on_arrival(&frame(1, 100, 500), t0 + Duration::from_millis(6), 0, true);
        let done = vec![(
            0,
            vec![Done {
                pts: 100,
                decoded: t0 + Duration::from_millis(9),
                presented: Some(t0 + Duration::from_millis(12)),
            }],
        )];
        let fb = feedback(c.build(t0 + Duration::from_millis(20), done).unwrap());
        assert_eq!(fb.seq, 0);
        assert_eq!(fb.frames.len(), 2);
        assert_eq!(
            (fb.frames[0].pts, fb.frames[0].bytes, fb.frames[0].queue_len),
            (100, 1000, 2)
        );
        assert_eq!(fb.frames[0].arrival_us, 5000);
        assert!(fb.frames[1].udp);
        assert_eq!(fb.done.len(), 1);
        assert_eq!((fb.done[0].decode_us, fb.done[0].present_us), (4000, 7000));
        // Rate limit: not due 10 ms later, due 50 ms later.
        assert!(!c.due(t0 + Duration::from_millis(30)));
        assert!(c.due(t0 + Duration::from_millis(70)));
        // A skipped frame reports present 0; one never seen arriving is left out.
        let done = vec![(
            0,
            vec![
                Done {
                    pts: 100,
                    decoded: t0 + Duration::from_millis(30),
                    presented: None,
                },
                Done {
                    pts: 7,
                    decoded: t0,
                    presented: None,
                },
            ],
        )];
        let fb = feedback(c.build(t0 + Duration::from_millis(80), done).unwrap());
        assert_eq!(fb.seq, 1);
        assert_eq!(fb.done.len(), 1);
        assert_eq!(fb.done[0].present_us, 0);
    }

    #[test]
    fn latency_feedback_caps_and_udp() {
        let t0 = Instant::now();
        let mut c = Collector::new(t0);
        for i in 0..(MAX_FRAMES + 3) {
            c.on_arrival(&frame(0, i as i64, 10), t0, 0, true);
        }
        let lost: Vec<LostRange> = (0..(MAX_LOST + 2) as u32)
            .map(|i| LostRange {
                display: 0,
                from: i,
                to: i,
                repaired: false,
            })
            .collect();
        let counts = UdpCounts {
            datagrams_expected: 100,
            datagrams_received: 90,
            frames_completed: 10,
            frames_lost: 1,
            fec_recovered: 2,
            fec_unrecoverable: 1,
        };
        c.on_udp(&counts, &lost);
        let fb = feedback(c.build(t0, Vec::new()).unwrap());
        assert_eq!(fb.frames.len(), MAX_FRAMES);
        assert_eq!(fb.lost.len(), MAX_LOST);
        assert_eq!(fb.dropped, 5);
        let udp = fb.udp.as_ref().unwrap();
        assert_eq!((udp.datagrams_expected, udp.datagrams_received), (100, 90));
        assert_eq!(udp.fec_recovered, 2);
        // Counters are per batch.
        c.on_arrival(&frame(0, 1000, 10), t0, 0, false);
        let fb = feedback(c.build(t0 + INTERVAL, Vec::new()).unwrap());
        assert!(fb.udp.is_none());
        assert_eq!(fb.dropped, 0);
    }

    #[test]
    fn latency_feedback_done_log_off_costs_nothing() {
        let log = DoneLog::default();
        log.on_frame(Some(1), Instant::now(), None);
        assert!(log.take().is_empty());
        log.set_on(true);
        log.on_frame(Some(1), Instant::now(), None);
        log.on_frame(None, Instant::now(), None);
        assert_eq!(log.take().len(), 1);
        assert!(log.take().is_empty());
    }
}
