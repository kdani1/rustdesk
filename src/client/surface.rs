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
//!   software decoder until the view registers a new surface, or until the block's retry is
//!   due (`RETRY_BACKOFF`) while the view still shows that surface; after `MAX_SURFACE_FAILS`
//!   failures of one surface it is not tried again. A failure never reaches
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
/// How long a frame that is to be shown waits for its own picture while newer frames are
/// waiting. Without a wait its picture (about 4 ms behind, more with several panels) was not
/// out when the present decision came, nothing was rendered, and the next frame's decode
/// dropped the late picture: with 2-3 panels most frames of a burst were lost. Short, so a
/// backlog still drains (option enable-android-surface-backlog-wait, "N": no wait).
const BACKLOG_OUTPUT_WAIT: Duration = Duration::from_millis(9);
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
/// After a surface decoder failed, its surface is tried again after these delays (the last one
/// repeats), through the normal start: a key frame asked for within the request spacing, the
/// decoder starting with it. A failure may pass (a decoder busy for a moment, a stream
/// restart); without a retry the display stayed on software until its view was rebuilt.
const RETRY_BACKOFF: [Duration; 4] = [
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Surface output this long after a decoder's first picture starts the backoff over. Counted
/// from the first picture to any later one: a still screen in between counts as stable.
const RETRY_STABLE: Duration = Duration::from_secs(60);
/// Failures of one surface generation, without stable output in between, after which it is not
/// tried again (`state=retry_exhausted`): retries after 3, 10, 30, 60 and 60 s. Each retry costs
/// a key frame request, a picture frozen up to the decoder's stall limit and the decoder setup
/// on the video thread. A new texture (generation) starts with a fresh count.
const MAX_SURFACE_FAILS: u32 = 6;

/// A surface state line: to the log as before, and to logcat (`surface_decoder::logcat`).
macro_rules! surface_log {
    ($lvl:expr, $($arg:tt)+) => {{
        let line = format!($($arg)+);
        log::log!($lvl, "{}", line);
        scrap::android::surface_decoder::logcat($lvl, &line);
    }};
}

/// Option `enable-android-surface-backlog-wait` (default on; "N": a frame shown during a backlog
/// does not wait for its picture, as before), read once per process.
fn backlog_wait() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let v = LocalConfig::get_option("enable-android-surface-backlog-wait") != "N";
        log::info!(
            "LATENCY client surface backlog_wait={} wait_ms={}",
            v,
            BACKLOG_OUTPUT_WAIT.as_millis()
        );
        v
    })
}

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
    // `want_output` is final (pacing off, or decided before the decode): a frame that waits
    // for its picture during a backlog is one that will be shown.
    shown_known: bool,
    // Time the current frame's decode spent waiting for the decoder's output.
    waited: Duration,
    // When the registered surface was last looked at.
    checked_at: Instant,
    // The last frame's codec is one the surface decoder takes (H.264 / H.265).
    codec_ok: bool,
    // Key frame requests sent since the last key frame.
    asks: u32,
    // Failures of surface generation `failed_gen` since the surface output was last stable for
    // `RETRY_STABLE`: picks the delay.
    fails: u32,
    failed_gen: u64,
    // When the blocked generation is tried again.
    retry_at: Option<Instant>,
    // The running decoder's first rendered picture.
    output_since: Option<Instant>,
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
            shown_known: true,
            waited: Duration::ZERO,
            checked_at: Instant::now(),
            codec_ok: false,
            asks: 0,
            fails: 0,
            failed_gen: 0,
            retry_at: None,
            output_since: None,
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

    /// Quest game mode parked this display (frames dropped undecoded while it had no
    /// surface): nothing may be decoded before the next key frame, and one is asked for.
    #[cfg(feature = "quest-xr")]
    pub(super) fn surface_unpark(&mut self) {
        self.surf.wait_key = true;
        self.surf.refresh = true;
    }

    /// Before each frame: starts, keeps or drops the surface decoder. `want`: the surface of
    /// the one view that shows this display, if there is exactly one and it registered one.
    /// `shown_known`: `want_output` will be the final present decision (see `SurfaceState`).
    /// `operating_rate`: the stream's expected fps for a decoder that starts now.
    pub(super) fn surface_sync(
        &mut self,
        want: Option<SurfaceRef>,
        key: bool,
        format: CodecFormat,
        backlog: bool,
        shown_known: bool,
        operating_rate: impl FnOnce() -> i32,
    ) -> SurfaceSync {
        self.surf.backlog = backlog;
        self.surf.shown_known = shown_known;
        self.surf.waited = Duration::ZERO;
        self.surf.checked_at = Instant::now();
        let hevc = format == CodecFormat::H265;
        let codec_ok = matches!(format, CodecFormat::H264 | CodecFormat::H265);
        self.surf.codec_ok = codec_ok;
        self.surface_retry(want.as_ref(), self.surf.checked_at);
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
                match SurfaceDecoder::new(hevc, w, self._display, operating_rate()) {
                    Ok(d) => {
                        // rate: the operating-rate configured, 0 when the config step dropped it.
                        surface_log!(
                            log::Level::Info,
                            "LATENCY client decoder display={} codec={:?} path=mediacodec-surface/{} hw=true use_texture_render=false render=surface size={}x{} cfg={} rate={} gen={}",
                            self._display, format, d.name, w.width, w.height, d.config_step, d.operating_rate, d.generation
                        );
                        self.surf.dec = Some(d);
                        self.surf.retry_at = None;
                        self.surf.output_since = None;
                        sync.switched = true;
                    }
                    Err(e) => {
                        // A surface without a size never starts: no retry for it.
                        let lasting = w.width <= 0 || w.height <= 0;
                        let retry = self.surface_block(w.generation, lasting);
                        surface_log!(
                            log::Level::Error,
                            "LATENCY client surface display={} state=fallback render=soft codec={:?} reason=\"start: {}\" retry_in_s={}",
                            self._display, format, e, retry.map_or(-1, |d| d.as_secs() as i64)
                        );
                        self.surface_log_exhausted(retry, w.generation);
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
                surface_log!(
                    log::Level::Info,
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
        // Only a frame present pacing will show waits for its picture: up to `OUTPUT_WAIT`
        // when it is the newest, up to `BACKLOG_OUTPUT_WAIT` while newer frames are waiting
        // (the pacer still shows frames of a burst; not when the decision comes only after
        // the decode, enable-viewer-decode-no-output "N": most of those frames are rejected).
        // Either wait is excluded from the busy time (`surface_take_wait`).
        let wait = if !want_output {
            Duration::ZERO
        } else if !self.surf.backlog {
            OUTPUT_WAIT
        } else if self.surf.shown_known && backlog_wait() {
            BACKLOG_OUTPUT_WAIT
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
            Ok(info) => {
                if render {
                    self.surface_output_stable();
                }
                (render, info)
            }
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
        let want = want();
        self.surface_retry(want.as_ref(), now);
        let (record, codec_ok, blocked) = (self.record, self.surf.codec_ok, self.surf.blocked_gen);
        let want = want.filter(|w| !record && codec_ok && w.generation != blocked);
        if let Some(running) = self.surf.dec.as_ref().map(|d| d.generation) {
            if want.as_ref().map(|w| w.generation) == Some(running) {
                return;
            }
            self.surf.dec = None;
            self.surf.wait_key = true;
            self.surf.lost = true;
            surface_log!(
                log::Level::Info,
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
        let dec = self.surf.dec.take();
        let generation = dec.as_ref().map_or(0, |d| d.generation);
        let retry = dec.as_ref().and_then(|d| self.surface_block(d.generation, false));
        self.surf.wait_key = true;
        self.surf.lost = true;
        self.surf.refresh = true;
        self.surf.waited = Duration::ZERO;
        surface_log!(
            log::Level::Error,
            "LATENCY client surface display={} state=fallback render=soft decoder={} reason=\"{}: {}\" retry_in_s={}",
            self._display,
            dec.as_ref().map_or("", |d| d.name.as_str()),
            what,
            e,
            retry.map_or(-1, |d| d.as_secs() as i64)
        );
        if dec.is_some() {
            self.surface_log_exhausted(retry, generation);
        }
    }

    // A failed decoder's surface: blocked until `RETRY_BACKOFF`'s next delay is over, which is
    // returned, or for good (`None`, `state=retry_exhausted`) after `MAX_SURFACE_FAILS` failures
    // of that generation or a `lasting` failure. The count starts over for another generation:
    // an earlier texture's failures do not lengthen a new one's delays.
    fn surface_block(&mut self, generation: u64, lasting: bool) -> Option<Duration> {
        if generation != self.surf.failed_gen {
            self.surf.failed_gen = generation;
            self.surf.fails = 0;
        }
        let delay = RETRY_BACKOFF[(self.surf.fails as usize).min(RETRY_BACKOFF.len() - 1)];
        self.surf.blocked_gen = generation;
        self.surf.fails = self.surf.fails.saturating_add(1);
        self.surf.output_since = None;
        if lasting || self.surf.fails >= MAX_SURFACE_FAILS {
            self.surf.retry_at = None;
            return None;
        }
        self.surf.retry_at = Some(Instant::now() + delay);
        Some(delay)
    }

    // Logs a block's outcome after its `state=fallback` line.
    fn surface_log_exhausted(&self, retry: Option<Duration>, generation: u64) {
        if retry.is_none() {
            surface_log!(
                log::Level::Warn,
                "LATENCY client surface display={} state=retry_exhausted gen={} fails={}",
                self._display,
                generation,
                self.surf.fails
            );
        }
    }

    // Unblocks the blocked surface once its retry is due, only while the view shows it now
    // (`want`) and surface decoding may run (option on, no recording, H.264 / H.265): the
    // normal start then asks for a key frame and starts a decoder with it. A failure there
    // blocks the surface again with the next, longer delay.
    fn surface_retry(&mut self, want: Option<&SurfaceRef>, now: Instant) {
        let (Some(at), Some(w)) = (self.surf.retry_at, want) else {
            return;
        };
        if now < at
            || w.generation != self.surf.blocked_gen
            || !self.surf.enabled
            || self.record
            || !self.surf.codec_ok
        {
            return;
        }
        self.surf.retry_at = None;
        self.surf.blocked_gen = 0;
        self.surf.asks = 0;
        surface_log!(
            log::Level::Info,
            "LATENCY client surface display={} state=retry attempt={} gen={}",
            self._display,
            self.surf.fails,
            w.generation
        );
    }

    // A picture was rendered: after `RETRY_STABLE` of surface output the backoff starts over.
    fn surface_output_stable(&mut self) {
        let now = Instant::now();
        let since = *self.surf.output_since.get_or_insert(now);
        if self.surf.fails > 0 && now.saturating_duration_since(since) >= RETRY_STABLE {
            self.surf.fails = 0;
            surface_log!(
                log::Level::Info,
                "LATENCY client surface display={} state=stable backoff=reset",
                self._display
            );
        }
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
