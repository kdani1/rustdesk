//! Per-frame video feedback (roadmap M23, docs/UDP-VIDEO.md "Per-frame feedback (M23)").
//!
//! A fork viewer tells a fork host, over the session stream, what happened to each video frame:
//! when it arrived, how big it was, how long the viewer took to decode and show it, and, on a
//! live UDP lane, which frames were lost and how many datagrams. The host turns that into a
//! one-way queueing delay (`q_owd`), a GCC trendline, jitter, the viewer's own delay and loss
//! rates (`host::FeedbackInput`), which feed its bitrate control, the FEC level of a UDP lane and
//! the lane's fallback decision.
//!
//! - `viewer`: `Collector` (arrivals, decode/present ends, UDP loss; batches of at most one per
//!   50 ms) and `DoneLog` (the video thread's per-frame hook).
//! - `host`: `SendLog` (send time and size of each frame), `Estimator`, the `FeedbackSink` seam
//!   for a bitrate controller, `LegacyQos` (the delay sample for video_qos), `FecAdapt`.
//!
//! Negotiated: the host advertises `Features.video_feedback` (1001) only with
//! `enable-video-feedback=Y`, the viewer sends `Misc.video_feedback` (1005) only to such a host.
//! Stock peers never see either. Platform-neutral code; hosts advertise and viewers send only on
//! Windows.

pub mod host;
pub mod viewer;

use base::message_proto::{video_frame, VideoFrame};

/// The feedback version this build speaks (`Features.video_feedback`).
pub const VERSION: u32 = 1;

/// (pts, encoded bytes) of a video frame: the key both ends match frames by (the largest pts of
/// its encoded frames, as `client::latency::frame_pts`), and its size. None for raw frames.
pub fn frame_key(vf: &VideoFrame) -> Option<(i64, u32)> {
    use video_frame::Union::*;
    match vf.union.as_ref()? {
        Vp8s(f) | Vp9s(f) | Av1s(f) | H264s(f) | H265s(f) => {
            let pts = f.frames.iter().map(|e| e.pts).max()?;
            let bytes: usize = f.frames.iter().map(|e| e.data.len()).sum();
            Some((pts, bytes.min(u32::MAX as usize) as u32))
        }
        _ => None,
    }
}

/// Microseconds from `base` to `t` (0 before it).
fn us_since(base: std::time::Instant, t: std::time::Instant) -> u64 {
    t.saturating_duration_since(base)
        .as_micros()
        .min(u64::MAX as u128) as u64
}
