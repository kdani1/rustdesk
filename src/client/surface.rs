//! Android surface decoding, the video thread's side: when a display's H.264 / H.265 stream is
//! decoded by MediaCodec into the Flutter texture of the one view that shows it
//! (`scrap::android::surface_decoder`), and when by the software decoder, which stays the
//! fallback for everything. See docs/LATENCY-METRICS.md, "Android surface decoding".
//!
//! Rules:
//! - A decoder starts only on a key frame. Until one arrives the running decoder keeps
//!   decoding; a key frame is requested (`refresh_video`) at most once a second, by this
//!   thread only (the view does not ask), with frames flowing or on a still screen.
//! - Once a surface decoder is gone nothing is decoded before the next key frame.
//! - A surface decoder that failed blocks its surface generation: that display stays on the
//!   software decoder until the view registers a new surface. A failure never reaches
//!   `fail_counter` or `mark_unsupported`: the codec itself is fine.

use super::{keys, CodecFormat, DecodeOutput, LocalConfig, VideoCounters, VideoFrame, VideoHandler};
use crate::ui_session_interface::{InvokeUiSession, Session};
use hbb_common::log;
use scrap::android::surface_decoder::{SurfaceDecoder, SurfaceInfo, SurfaceRef};
use std::{
    os::raw::c_void,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// How long a frame's decode waits for its own picture when no newer frame is waiting. The
/// decoder is a pipeline: the picture normally comes a few milliseconds after its access unit.
/// One that comes later is taken between frames (`idle`).
const OUTPUT_WAIT: Duration = Duration::from_millis(30);
/// While a picture is still owed, or a key frame request is held back, the video thread wakes
/// this often between frames.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(4);
/// At most one key frame request per display in this time: on the host a refresh restarts the
/// capturer and the encoder for every viewer.
const REFRESH_SPACING: Duration = Duration::from_secs(1);
/// While no frame comes the thread looks this often at the registered surface. A running
/// decoder whose surface is gone (a closed panel, a switched display, a memory trim) is
/// released, and not only with the display's next frame; and a surface that waits for its
/// decoder on a still screen gets its key frame.
const HOUSEKEEP_INTERVAL: Duration = Duration::from_millis(500);
/// A surface is left alone this long after its registration before a key frame is asked for
/// on a still screen: the stream's own first key frame, or the one a new panel asks for,
/// usually comes by itself.
const START_GRACE: Duration = Duration::from_millis(700);
/// Key frame requests to start a surface decoder without a key frame coming: after these the
/// thread stops asking (the software picture stays) until a key frame arrives.
const MAX_START_ASKS: u32 = 3;

pub(super) struct SurfaceState {
    // enable-android-surface-decode, read once per video thread.
    enabled: bool,
    dec: Option<SurfaceDecoder>,
    // Nothing may be decoded before the next key frame.
    wait_key: bool,
    // The surface generation whose decoder failed.
    blocked_gen: u64,
    // The view must be told that the surface picture is over.
    lost: bool,
    // A key frame must be requested.
    refresh: bool,
    refresh_at: Option<Instant>,
    // Newer frames were waiting when the current one was taken.
    backlog: bool,
    // Time the current frame's decode spent waiting for the decoder's output.
    waited: Duration,
    // When the registered surface was last looked at.
    checked_at: Instant,
    // The last frame's codec is one the surface decoder takes (H.264 / H.265).
    codec_ok: bool,
    // Key frame requests sent since the last key frame.
    asks: u32,
}

impl SurfaceState {
    pub(super) fn new(display: usize) -> Self {
        let enabled =
            LocalConfig::get_option(keys::OPTION_ENABLE_ANDROID_SURFACE_DECODE) != "N";
        log::info!(
            "LATENCY client surface display={} state=option value={}",
            display,
            if enabled { "on" } else { "off" }
        );
        Self {
            enabled,
            dec: None,
            wait_key: false,
            blocked_gen: 0,
            lost: false,
            refresh: false,
            refresh_at: None,
            backlog: false,
            waited: Duration::ZERO,
            checked_at: Instant::now(),
            codec_ok: false,
            asks: 0,
        }
    }
}

/// What the video thread does with the frame it just took.
#[derive(Default)]
pub(super) struct SurfaceSync {
    /// The frame cannot be decoded (no decoder until a key frame).
    pub skip: bool,
    /// Another decoder takes over with this frame.
    pub switched: bool,
}

impl VideoHandler {
    /// Whether the surface decoder decodes this display now.
    pub(super) fn surface_active(&self) -> bool {
        self.surf.dec.is_some()
    }

    /// Before each frame: starts, keeps or drops the surface decoder. `want`: the surface of
    /// the one view that shows this display, if there is exactly one and it registered one.
    pub(super) fn surface_sync(
        &mut self,
        want: Option<SurfaceRef>,
        key: bool,
        format: CodecFormat,
        backlog: bool,
    ) -> SurfaceSync {
        self.surf.backlog = backlog;
        self.surf.waited = Duration::ZERO;
        self.surf.checked_at = Instant::now();
        let hevc = format == CodecFormat::H265;
        let codec_ok = matches!(format, CodecFormat::H264 | CodecFormat::H265);
        self.surf.codec_ok = codec_ok;
        let (enabled, record, blocked) = (self.surf.enabled, self.record, self.surf.blocked_gen);
        let want = want.filter(|w| enabled && !record && codec_ok && w.generation != blocked);
        let running_ok = match (self.surf.dec.as_ref(), want.as_ref()) {
            (Some(d), Some(w)) => d.generation == w.generation && d.hevc == hevc,
            _ => false,
        };
        let mut sync = SurfaceSync::default();
        if key {
            // The key frame a pending request would ask for is here.
            self.surf.refresh = false;
            self.surf.asks = 0;
            if running_ok {
                self.surf.wait_key = false;
                return sync;
            }
            // The old decoder goes first: a surface takes one producer at a time.
            let had = self.surf.dec.take().is_some();
            if let Some(w) = want.as_ref() {
                match SurfaceDecoder::new(hevc, w, self._display) {
                    Ok(d) => {
                        log::info!(
                            "LATENCY client decoder display={} codec={:?} path=mediacodec-surface/{} hw=true use_texture_render=false render=surface size={}x{} cfg={} gen={}",
                            self._display, format, d.name, w.width, w.height, d.config_step, d.generation
                        );
                        self.surf.dec = Some(d);
                        sync.switched = true;
                    }
                    Err(e) => {
                        self.surf.blocked_gen = w.generation;
                        log::error!(
                            "LATENCY client surface display={} state=fallback render=soft codec={:?} reason=\"start: {}\"",
                            self._display, format, e
                        );
                    }
                }
            }
            if self.surf.dec.is_none() {
                if had {
                    self.surf.lost = true;
                }
                if had || self.surf.wait_key {
                    // The software decoder starts again with this key frame.
                    self.reset(Some(format));
                    sync.switched = true;
                }
            }
            self.surf.wait_key = false;
            return sync;
        }
        if self.surf.wait_key {
            self.surf.refresh = true;
            sync.skip = true;
            return sync;
        }
        if self.surf.dec.is_some() {
            if !running_ok {
                self.surf.dec = None;
                self.surf.wait_key = true;
                self.surf.lost = true;
                self.surf.refresh = true;
                sync.skip = true;
                log::info!(
                    "LATENCY client surface display={} state=fallback render=soft codec={:?} reason=\"{}\"",
                    self._display,
                    format,
                    if want.is_some() {
                        "the surface was replaced"
                    } else {
                        "no usable surface (gone, a second viewer, recording, or another codec)"
                    }
                );
            }
        } else if want.is_some() && self.surf.asks < MAX_START_ASKS {
            // The software decoder keeps the picture until the key frame that starts the
            // surface decoder.
            self.surf.refresh = true;
        }
        sync
    }

    /// `handle_frame` while the surface decoder is active: queues the frame's access units and
    /// waits a bounded time for the last one's picture. `Output`: an output buffer is held for
    /// `surface_release`.
    pub(super) fn surface_handle_frame(
        &mut self,
        vf: &VideoFrame,
        pixelbuffer: &mut bool,
        want_output: bool,
    ) -> DecodeOutput {
        use super::video_frame::Union::{H264s, H265s};
        *pixelbuffer = false;
        let frames = match &vf.union {
            Some(H264s(f)) | Some(H265s(f)) => &f.frames,
            _ => return DecodeOutput::NoFrame,
        };
        // With newer frames waiting this one will not be shown: its picture is not waited for.
        let wait = if want_output && !self.surf.backlog {
            OUTPUT_WAIT
        } else {
            Duration::ZERO
        };
        let Some(d) = self.surf.dec.as_mut() else {
            return DecodeOutput::NoFrame;
        };
        let mut held = false;
        let last = frames.len().saturating_sub(1);
        let mut failure = None;
        for (i, f) in frames.iter().enumerate() {
            match d.feed(&f.data, f.pts, if i == last { wait } else { Duration::ZERO }) {
                Ok(h) => held = h,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = failure {
            self.surface_failed("decode", &e.to_string());
            return DecodeOutput::NoFrame;
        }
        self.surf.waited = d.take_waited();
        if !want_output {
            if let Err(e) = d.release(false) {
                self.surface_failed("release", &e.to_string());
                return DecodeOutput::NoFrame;
            }
            return DecodeOutput::DecodedNoOutput;
        }
        if held {
            DecodeOutput::Output
        } else {
            DecodeOutput::DecodedNoOutput
        }
    }

    /// Releases the output buffer `surface_handle_frame` holds, to the surface when `render`.
    /// Returns whether it was rendered, and the picture's description when the view needs it.
    pub(super) fn surface_release(&mut self, render: bool) -> (bool, Option<SurfaceInfo>) {
        let Some(d) = self.surf.dec.as_mut() else {
            return (false, None);
        };
        match d.release(render) {
            Ok(info) => (render, info),
            Err(e) => {
                self.surface_failed("render", &e.to_string());
                (false, None)
            }
        }
    }

    /// How long the thread may wait for the next frame before it must look after the surface
    /// decoder (`idle`); `None`: as long as it takes.
    pub(super) fn surface_wake(&self) -> Option<Duration> {
        match self.surf.dec.as_ref() {
            Some(d) if d.has_pending() => Some(POLL_INTERVAL),
            _ if self.surf.refresh => Some(POLL_INTERVAL),
            _ if self.surf.enabled => Some(HOUSEKEEP_INTERVAL),
            _ => None,
        }
    }

    // Between frames, every `HOUSEKEEP_INTERVAL`, with the surface registered for the display
    // now (as `surface_sync` gets it):
    // - a running decoder whose surface is no longer that one is released. Without a surface
    //   no key frame is asked for: nothing waits to be decoded, and the display's next frame
    //   asks (`wait_key`);
    // - without a decoder, a usable surface gets the key frame its decoder starts on: on a
    //   still screen no frame would ever ask for it (a view that got its texture after the
    //   stream's first key frame, a texture that came back after its activity stopped and
    //   started or after a memory trim). A new surface is given `START_GRACE` first, except
    //   when a decoder was given up before it (`wait_key`): then the view is back with an
    //   empty texture (the stop took the old one's picture), nothing can be decoded before a
    //   key frame anyway, and waiting would only keep the window blank longer.
    fn surface_housekeep(&mut self, want: impl FnOnce() -> Option<SurfaceRef>) {
        let now = Instant::now();
        if !self.surf.enabled
            || now.saturating_duration_since(self.surf.checked_at) < HOUSEKEEP_INTERVAL
        {
            return;
        }
        self.surf.checked_at = now;
        let (record, codec_ok, blocked) = (self.record, self.surf.codec_ok, self.surf.blocked_gen);
        let want = want().filter(|w| !record && codec_ok && w.generation != blocked);
        if let Some(running) = self.surf.dec.as_ref().map(|d| d.generation) {
            if want.as_ref().map(|w| w.generation) == Some(running) {
                return;
            }
            self.surf.dec = None;
            self.surf.wait_key = true;
            self.surf.lost = true;
            log::info!(
                "LATENCY client surface display={} state=released reason=\"its surface is gone or replaced\"",
                self._display
            );
        }
        if let Some(w) = want {
            if self.surf.asks < MAX_START_ASKS
                && (self.surf.wait_key
                    || now.saturating_duration_since(w.registered_at) >= START_GRACE)
            {
                self.surf.refresh = true;
            }
        }
    }

    /// The time the last frame's decode waited for the decoder's output.
    pub(super) fn surface_take_wait(&mut self) -> Duration {
        std::mem::take(&mut self.surf.waited)
    }

    pub(super) fn surface_on_reset(&mut self) {
        if self.surf.dec.take().is_some() {
            // The stream starts again: a new decoder with the next key frame. The view keeps
            // showing the texture's last picture meanwhile.
            self.surf.wait_key = true;
            log::info!(
                "LATENCY client surface display={} state=reset",
                self._display
            );
        }
    }

    fn surface_failed(&mut self, what: &str, e: &str) {
        let name = if let Some(d) = self.surf.dec.take() {
            self.surf.blocked_gen = d.generation;
            d.name.clone()
        } else {
            String::new()
        };
        self.surf.wait_key = true;
        self.surf.lost = true;
        self.surf.refresh = true;
        self.surf.waited = Duration::ZERO;
        log::error!(
            "LATENCY client surface display={} state=fallback render=soft decoder={} reason=\"{}: {}\"",
            self._display,
            name,
            what,
            e
        );
    }

    // A picture that came after its frame's decode stopped waiting: shown now, it is the newest.
    fn surface_poll(&mut self) -> (bool, Option<SurfaceInfo>) {
        let Some(d) = self.surf.dec.as_mut() else {
            return (false, None);
        };
        match d.poll() {
            Ok(true) => self.surface_release(true),
            Ok(false) => (false, None),
            Err(e) => {
                self.surface_failed("output", &e.to_string());
                (false, None)
            }
        }
    }

    fn surface_take_lost(&mut self) -> bool {
        std::mem::take(&mut self.surf.lost)
    }

    fn surface_take_refresh(&mut self, now: Instant) -> bool {
        if !self.surf.refresh {
            return false;
        }
        if self
            .surf
            .refresh_at
            .map_or(false, |at| now.saturating_duration_since(at) < REFRESH_SPACING)
        {
            // Stays pending: `surface_wake` keeps the thread waking.
            return false;
        }
        self.surf.refresh = false;
        self.surf.refresh_at = Some(now);
        self.surf.asks = self.surf.asks.saturating_add(1);
        true
    }
}

/// After anything that may have changed the surface state: tells the view that the surface
/// picture is over and sends a due key frame request.
pub(super) fn notify<T: InvokeUiSession>(
    handler: &mut VideoHandler,
    session: &Session<T>,
    display: usize,
) {
    if handler.surface_take_lost() {
        session.on_video_surface(display, false, None);
    }
    if handler.surface_take_refresh(Instant::now()) {
        log::info!(
            "LATENCY client surface display={} state=refresh",
            display
        );
        session.refresh_video(display as _);
    }
}

/// Tells the view about a rendered picture's size (first picture, format change).
pub(super) fn announce<T: InvokeUiSession>(
    session: &Session<T>,
    display: usize,
    info: Option<SurfaceInfo>,
) {
    if let Some(info) = info {
        session.on_video_surface(display, true, Some(&info));
    }
}

/// Between frames (`VideoHandler::surface_wake`): takes and shows a picture the decoder still
/// owed, and releases a decoder whose surface is gone.
pub(super) fn idle<T, F>(
    handler: &mut VideoHandler,
    session: &Session<T>,
    display: usize,
    counters: &VideoCounters,
    video_callback: &mut F,
) where
    T: InvokeUiSession,
    F: FnMut(usize, &mut scrap::ImageRgb, *mut c_void, bool),
{
    handler.surface_housekeep(|| session.video_surface(display));
    let (rendered, info) = handler.surface_poll();
    if rendered {
        counters.shown.fetch_add(1, Ordering::Relaxed);
        // Counts the frame for the status line's fps; no picture goes to the UI.
        video_callback(display, &mut handler.rgb, std::ptr::null_mut(), false);
    }
    announce(session, display, info);
    notify(handler, session, display);
}
