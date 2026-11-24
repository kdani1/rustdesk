//! Android viewer: H.264 / H.265 decoded by the device's hardware decoder (MediaCodec, through
//! the NDK) straight into a `Surface` that Flutter shows as a `Texture`. No picture is copied
//! to Rust or Dart. See docs/LATENCY-METRICS.md, "Android surface decoding".
//!
//! Kotlin (`VideoTextures.kt`) owns the Flutter texture and hands its `Surface` to
//! [`Java_ffi_FFI_setVideoSurface`]; it is kept here by (UI session id, display). A display's
//! video thread looks its surface up once per frame and owns the [`SurfaceDecoder`]: it is
//! created, used and dropped on that thread only.
//!
//! Raw `ndk-sys`: every function called here is API 21 (the build links at platform 21), and
//! every format key is passed as a literal, because the `AMEDIAFORMAT_KEY_*` symbols of the
//! newer keys are not in an API 21 `libmediandk`.

use hbb_common::{bail, log, ResultType};
use jni::{
    objects::{JClass, JObject, JString},
    sys::{jint, jlong},
    JNIEnv,
};
use ndk_sys as ffi;
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString},
    os::raw::{c_char, c_void},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

macro_rules! c {
    ($s:literal) => {
        concat!($s, "\0").as_ptr() as *const c_char
    };
}

const TRY_AGAIN_LATER: isize = ffi::AMEDIACODEC_INFO_TRY_AGAIN_LATER as isize;
const OUTPUT_FORMAT_CHANGED: isize = ffi::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as isize;
const OUTPUT_BUFFERS_CHANGED: isize = ffi::AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED as isize;

// An access unit is never skipped (later frames reference it): no free input buffer within
// this long is a decoder failure.
const INPUT_TRY: Duration = Duration::from_millis(10);
const INPUT_TRIES: usize = 5;
// A larger access unit is a decoder failure (software takes over).
const MAX_INPUT_SIZE: i32 = 2 * 1024 * 1024;
// The decoder's clock hint ("operating-rate") is the caller's: the stream's expected fps. A
// fixed 60 under-clocked the decoder for the 90 fps stream (decode p95 4.5 -> 8 ms with 2-3
// panels). Set when a decoder starts (a custom-fps change reaches the next decoder). When the
// decoder refuses the configuration (several panels) the step is retried with the old 60
// before the rate is dropped. Bounds for what the caller passes; the floor is the old 60.
const OPERATING_RATE_MIN: i32 = 60;
const OPERATING_RATE_MAX: i32 = 240;
// The rate every decoder was configured with before; the retry when the caller's is refused.
const OPERATING_RATE_FALLBACK: i32 = 60;
// The oldest access unit still waiting for its picture has waited this long: the decoder is
// stuck or holds pictures back.
const STALL: Duration = Duration::from_secs(1);
const LOG_INTERVAL: Duration = Duration::from_secs(5);
// A decoded buffer larger than the picture by at most this is alignment padding.
const MAX_PADDING: i32 = 64;

/// One reference on an `ANativeWindow`.
pub struct Window(*mut ffi::ANativeWindow);
// SAFETY: an ANativeWindow is reference counted and may be used from any thread.
unsafe impl Send for Window {}
unsafe impl Sync for Window {}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe { ffi::ANativeWindow_release(self.0) }
    }
}

/// The surface a UI session registered for one display.
#[derive(Clone)]
pub struct SurfaceRef {
    pub window: Arc<Window>,
    /// Unique per registration, never 0: a new `Surface` (new texture, new size, after a memory
    /// trim) is a new generation, and a decoder never outlives its generation.
    pub generation: u64,
    /// The display's size in pixels.
    pub width: i32,
    pub height: i32,
    /// The hardware decoders Kotlin picked from `MediaCodecList`, empty when it found none.
    pub hevc: String,
    pub avc: String,
    /// Buffers rendered to this surface; Kotlin reads it (`videoSurfaceRendered`).
    pub rendered: Arc<AtomicU64>,
    /// The view's own name for the surface (its Flutter texture id), handed back with
    /// [`SurfaceInfo`] so the view can tell which of its textures got a picture.
    pub texture: i64,
    pub registered_at: Instant,
}

lazy_static::lazy_static! {
    static ref SURFACES: RwLock<HashMap<(String, usize), SurfaceRef>> = Default::default();
}
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Writes a surface state line to logcat too (tag "RustDeskSurface"): a release build logs Rust
/// only to a file in the app's private data, and a fallback's reason must be readable with adb.
pub fn logcat(level: log::Level, line: &str) {
    let prio = match level {
        log::Level::Error => 6,
        log::Level::Warn => 5,
        _ => 4,
    };
    let Ok(text) = CString::new(line.replace('\0', " ")) else {
        return;
    };
    unsafe {
        ffi::__android_log_write(prio, c!("RustDeskSurface"), text.as_ptr());
    }
}

/// The surface UI session `sid` (lowercase hyphenated uuid) registered for `display`.
pub fn get_surface(sid: &str, display: usize) -> Option<SurfaceRef> {
    SURFACES
        .read()
        .unwrap()
        .get(&(sid.to_owned(), display))
        .cloned()
}

fn jstring(env: &mut JNIEnv, s: &JString) -> String {
    if s.is_null() {
        return String::new();
    }
    env.get_string(s).map(String::from).unwrap_or_default()
}

/// Kotlin, main thread: the `Surface` of UI session `sid`'s texture for `display`, or null when
/// it is gone. `texture`: the view's id of that texture. `hevc` / `avc`: decoder names to
/// create, empty for the system's default.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_setVideoSurface(
    mut env: JNIEnv,
    _class: JClass,
    sid: JString,
    display: jint,
    surface: JObject,
    texture: jlong,
    width: jint,
    height: jint,
    hevc: JString,
    avc: JString,
) {
    let sid = jstring(&mut env, &sid).to_lowercase();
    let display = display.max(0) as usize;
    let hevc = jstring(&mut env, &hevc);
    let avc = jstring(&mut env, &avc);
    let window = if surface.is_null() {
        std::ptr::null_mut()
    } else {
        // Takes one reference, released by `Window::drop`.
        unsafe { ffi::ANativeWindow_fromSurface(env.get_raw() as _, surface.as_raw() as _) }
    };
    let key = (sid, display);
    let mut generation = 0;
    let old = {
        let mut map = SURFACES.write().unwrap();
        if window.is_null() {
            map.remove(&key)
        } else {
            generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
            map.insert(
                key.clone(),
                SurfaceRef {
                    window: Arc::new(Window(window)),
                    generation,
                    width,
                    height,
                    hevc: hevc.clone(),
                    avc: avc.clone(),
                    rendered: Default::default(),
                    texture,
                    registered_at: Instant::now(),
                },
            )
        }
    };
    // A decoder still using the old window keeps its own reference.
    drop(old);
    if generation != 0 {
        log::info!(
            "LATENCY client surface display={} state=registered gen={} texture={} size={}x{} hevc={:?} avc={:?} sid={}",
            display, generation, texture, width, height, hevc, avc, key.0
        );
    } else {
        log::info!(
            "LATENCY client surface display={} state=unregistered sid={}",
            display,
            key.0
        );
    }
}

/// Kotlin, main thread: buffers rendered to the registered surface so far, -1 without one.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_videoSurfaceRendered(
    mut env: JNIEnv,
    _class: JClass,
    sid: JString,
    display: jint,
) -> jlong {
    let sid = jstring(&mut env, &sid).to_lowercase();
    get_surface(&sid, display.max(0) as usize)
        .map_or(-1, |s| s.rendered.load(Ordering::Relaxed) as jlong)
}

/// What the UI needs to show the texture: sent after the first rendered buffer and after each
/// change of the output format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceInfo {
    /// The picture.
    pub width: i32,
    pub height: i32,
    /// The decoded buffer, at least the picture (alignment padding at the right and bottom).
    pub buf_width: i32,
    pub buf_height: i32,
    pub name: String,
    /// [`SurfaceRef::texture`] of the surface the decoder renders into.
    pub texture: i64,
}

struct Held {
    idx: usize,
    ts: i64,
}

#[derive(Default)]
struct Stats {
    fed: usize,
    out: usize,
    rendered: usize,
    dropped: usize,
    // Feeds that waited and did not get their own picture in time.
    late: usize,
    // Of those, the ones that returned an older picture.
    lag: usize,
    // Access unit queued -> its output buffer dequeued, in microseconds.
    out_us: Vec<u64>,
}

/// A MediaCodec video decoder rendering into one surface. Not `Send`: it lives on the display's
/// video thread.
pub struct SurfaceDecoder {
    codec: *mut ffi::AMediaCodec,
    // Keeps the window alive while the codec uses it, whatever Kotlin does with the texture.
    _window: Arc<Window>,
    rendered_total: Arc<AtomicU64>,
    texture: i64,
    pub generation: u64,
    pub hevc: bool,
    pub name: String,
    /// The configuration step that worked (0: every key).
    pub config_step: usize,
    /// The "operating-rate" the decoder was configured with, 0 when that step dropped it.
    pub operating_rate: i32,
    display: usize,
    width: i32,
    height: i32,
    buf_width: i32,
    buf_height: i32,
    vis_width: i32,
    vis_height: i32,
    info_dirty: bool,
    // The newest output buffer, not yet released.
    held: Option<Held>,
    // Access units queued and not yet out, oldest first: (timestamp, queued at).
    pending: VecDeque<(i64, Instant)>,
    last_ts: i64,
    waited: Duration,
    stats: Stats,
    log_at: Instant,
}

unsafe fn create(wanted: &str, mime: &CStr) -> Option<(*mut ffi::AMediaCodec, String)> {
    if !wanted.is_empty() {
        if let Ok(name) = CString::new(wanted) {
            let codec = ffi::AMediaCodec_createCodecByName(name.as_ptr());
            if !codec.is_null() {
                return Some((codec, wanted.to_owned()));
            }
        }
    }
    let codec = ffi::AMediaCodec_createDecoderByType(mime.as_ptr());
    if codec.is_null() {
        None
    } else {
        Some((codec, "default".to_owned()))
    }
}

#[cfg(target_pointer_width = "64")]
const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
#[cfg(not(target_pointer_width = "64"))]
const RTLD_DEFAULT: *mut c_void = 0xffff_ffffusize as *mut c_void;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

// The picture inside the decoded buffer: (left, top, right, bottom), inclusive. The NDK format
// holds it as a rect ("crop"), which only `AMediaFormat_getRect` (API 28) reads: looked up at
// run time, as the library links at API 21. The Java layer's split keys are tried first.
unsafe fn crop_rect(fmt: *mut ffi::AMediaFormat) -> Option<(i32, i32, i32, i32)> {
    let (mut l, mut t, mut r, mut b) = (0, 0, 0, 0);
    if ffi::AMediaFormat_getInt32(fmt, c!("crop-left"), &mut l)
        && ffi::AMediaFormat_getInt32(fmt, c!("crop-top"), &mut t)
        && ffi::AMediaFormat_getInt32(fmt, c!("crop-right"), &mut r)
        && ffi::AMediaFormat_getInt32(fmt, c!("crop-bottom"), &mut b)
    {
        return Some((l, t, r, b));
    }
    type GetRect = unsafe extern "C" fn(
        *mut ffi::AMediaFormat,
        *const c_char,
        *mut i32,
        *mut i32,
        *mut i32,
        *mut i32,
    ) -> bool;
    let sym = dlsym(RTLD_DEFAULT, c!("AMediaFormat_getRect"));
    if sym.is_null() {
        return None;
    }
    let get_rect: GetRect = std::mem::transmute(sym);
    if get_rect(fmt, c!("crop"), &mut l, &mut t, &mut r, &mut b) {
        Some((l, t, r, b))
    } else {
        None
    }
}

impl SurfaceDecoder {
    /// Creates, configures and starts a decoder that renders into `s`. Tries the full low
    /// latency configuration first and drops keys step by step when the decoder refuses it.
    /// `operating_rate`: the stream's expected frames per second (the decoder's clock hint),
    /// clamped to 60..=240.
    pub fn new(
        hevc: bool,
        s: &SurfaceRef,
        display: usize,
        operating_rate: i32,
    ) -> ResultType<Self> {
        if s.width <= 0 || s.height <= 0 {
            bail!("surface size {}x{}", s.width, s.height);
        }
        let operating_rate = operating_rate.clamp(OPERATING_RATE_MIN, OPERATING_RATE_MAX);
        let mime = if hevc { "video/hevc" } else { "video/avc" };
        let cmime = CString::new(mime)?;
        let wanted = if hevc { &s.hevc } else { &s.avc };
        let qti = wanted.starts_with("c2.qti.") || wanted.starts_with("OMX.qcom.");
        let mut last = String::new();
        // 0: every key; 1: no vendor keys; 2: also no operating-rate; 3: also no low-latency;
        // 4: also no priority. Steps 0 and 1 are tried with the caller's rate, then with the old
        // 60 (admission of one more realtime decoder depends on size x rate: a higher rate
        // refused must not cost the rate and the vendor keys), (step, rate):
        // (0,R) (0,60) (1,R) (1,60) (2,-) (3,-) (4,-); the 60 retries only when R > 60.
        let mut tries: Vec<(usize, i32)> = Vec::with_capacity(7);
        for step in 0..5usize {
            if step == 1 && !qti {
                continue;
            }
            if step < 2 {
                tries.push((step, operating_rate));
                if operating_rate > OPERATING_RATE_FALLBACK {
                    tries.push((step, OPERATING_RATE_FALLBACK));
                }
            } else {
                tries.push((step, 0));
            }
        }
        for (step, rate) in tries {
            unsafe {
                let Some((codec, name)) = create(wanted, &cmime) else {
                    bail!("no {mime} decoder (wanted {wanted:?})");
                };
                let fmt = ffi::AMediaFormat_new();
                if fmt.is_null() {
                    ffi::AMediaCodec_delete(codec);
                    bail!("AMediaFormat_new failed");
                }
                ffi::AMediaFormat_setString(fmt, c!("mime"), cmime.as_ptr());
                ffi::AMediaFormat_setInt32(fmt, c!("width"), s.width);
                ffi::AMediaFormat_setInt32(fmt, c!("height"), s.height);
                ffi::AMediaFormat_setInt32(fmt, c!("max-input-size"), MAX_INPUT_SIZE);
                if step < 4 {
                    // 0: realtime. Kept as before for every decoder (the first panel's
                    // included); a decoder that cannot admit one more realtime session
                    // refuses the configuration and the ladder drops the keys below.
                    ffi::AMediaFormat_setInt32(fmt, c!("priority"), 0);
                }
                if step < 3 {
                    ffi::AMediaFormat_setInt32(fmt, c!("low-latency"), 1);
                }
                if step < 2 {
                    ffi::AMediaFormat_setInt32(fmt, c!("operating-rate"), rate);
                }
                if step < 1 && qti {
                    ffi::AMediaFormat_setInt32(
                        fmt,
                        c!("vendor.qti-ext-dec-low-latency.enable"),
                        1,
                    );
                    // Pictures leave in decode order: no reordering delay (the stream has no
                    // B-frames).
                    ffi::AMediaFormat_setInt32(
                        fmt,
                        c!("vendor.qti-ext-dec-picture-order.enable"),
                        1,
                    );
                }
                let status =
                    ffi::AMediaCodec_configure(codec, fmt, s.window.0, std::ptr::null_mut(), 0);
                ffi::AMediaFormat_delete(fmt);
                if status.0 == 0 {
                    let status = ffi::AMediaCodec_start(codec);
                    if status.0 == 0 {
                        return Ok(Self {
                            codec,
                            _window: s.window.clone(),
                            rendered_total: s.rendered.clone(),
                            texture: s.texture,
                            generation: s.generation,
                            hevc,
                            name,
                            config_step: step,
                            operating_rate: rate,
                            display,
                            width: s.width,
                            height: s.height,
                            buf_width: s.width,
                            buf_height: s.height,
                            vis_width: s.width,
                            vis_height: s.height,
                            info_dirty: true,
                            held: None,
                            pending: Default::default(),
                            last_ts: -1,
                            waited: Duration::ZERO,
                            stats: Default::default(),
                            log_at: Instant::now(),
                        });
                    }
                    last = format!("start failed ({})", status.0);
                } else {
                    last = format!("configure failed ({})", status.0);
                }
                ffi::AMediaCodec_delete(codec);
            }
            let line = format!(
                "LATENCY client surface display={} state=config_retry step={} rate={} decoder={:?} {}",
                display,
                step,
                rate,
                wanted,
                last
            );
            log::warn!("{}", line);
            logcat(log::Level::Warn, &line);
        }
        bail!("{mime} decoder {wanted:?}: {last}")
    }

    /// Queues one access unit and waits up to `wait` for its picture. Returns whether an
    /// output buffer is held now (the newest one out; older ones were released unrendered):
    /// [`Self::release`] renders or drops it. An error means the decoder is unusable.
    pub fn feed(&mut self, data: &[u8], pts_ms: i64, wait: Duration) -> ResultType<bool> {
        let mut idx = TRY_AGAIN_LATER;
        for _ in 0..INPUT_TRIES {
            idx = unsafe {
                ffi::AMediaCodec_dequeueInputBuffer(self.codec, INPUT_TRY.as_micros() as i64)
            } as isize;
            if idx >= 0 {
                break;
            }
            if idx != TRY_AGAIN_LATER {
                bail!("dequeueInputBuffer failed ({idx})");
            }
            // The decoder may be waiting for its output buffers to be taken.
            self.drain(Duration::ZERO, None)?;
        }
        if idx < 0 {
            bail!(
                "no input buffer for {} ms",
                (INPUT_TRY * INPUT_TRIES as u32).as_millis()
            );
        }
        let ts = pts_ms.saturating_mul(1000).max(self.last_ts + 1);
        unsafe {
            let mut capacity: ffi::size_t = 0;
            let buf = ffi::AMediaCodec_getInputBuffer(self.codec, idx as _, &mut capacity);
            if buf.is_null() || (capacity as usize) < data.len() {
                bail!(
                    "input buffer of {} bytes for an access unit of {}",
                    capacity,
                    data.len()
                );
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
            let status = ffi::AMediaCodec_queueInputBuffer(
                self.codec,
                idx as _,
                0,
                data.len() as _,
                ts as u64,
                0,
            );
            if status.0 != 0 {
                bail!("queueInputBuffer failed ({})", status.0);
            }
        }
        self.last_ts = ts;
        let queued = Instant::now();
        self.pending.push_back((ts, queued));
        self.stats.fed += 1;
        self.drain(wait, Some(ts))?;
        let now = Instant::now();
        self.waited += now.saturating_duration_since(queued);
        self.check_stall(now)?;
        if !wait.is_zero() && !self.held.as_ref().map_or(false, |h| h.ts >= ts) {
            self.stats.late += 1;
            if self.held.is_some() {
                self.stats.lag += 1;
            }
        }
        self.maybe_log(now);
        Ok(self.held.is_some())
    }

    /// Between frames: takes the output buffers that came after their `feed` gave up waiting.
    /// Returns whether one is held now.
    pub fn poll(&mut self) -> ResultType<bool> {
        self.drain(Duration::ZERO, None)?;
        let now = Instant::now();
        self.check_stall(now)?;
        self.maybe_log(now);
        Ok(self.held.is_some())
    }

    /// Whether an access unit is still waiting for its picture.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The time `feed` spent waiting for output since the last call.
    pub fn take_waited(&mut self) -> Duration {
        std::mem::take(&mut self.waited)
    }

    /// Releases the held output buffer, to the surface when `render`. Returns the picture's
    /// description with the first rendered buffer and after each change of the output format.
    pub fn release(&mut self, render: bool) -> ResultType<Option<SurfaceInfo>> {
        let Some(held) = self.held.take() else {
            return Ok(None);
        };
        let status = unsafe { ffi::AMediaCodec_releaseOutputBuffer(self.codec, held.idx as _, render) };
        if status.0 != 0 {
            bail!("releaseOutputBuffer failed ({})", status.0);
        }
        if !render {
            self.stats.dropped += 1;
            return Ok(None);
        }
        self.stats.rendered += 1;
        self.rendered_total.fetch_add(1, Ordering::Relaxed);
        if std::mem::take(&mut self.info_dirty) {
            return Ok(Some(self.info()));
        }
        Ok(None)
    }

    pub fn info(&self) -> SurfaceInfo {
        SurfaceInfo {
            width: self.vis_width,
            height: self.vis_height,
            buf_width: self.buf_width,
            buf_height: self.buf_height,
            name: self.name.clone(),
            texture: self.texture,
        }
    }

    // Takes output buffers until the one of `until` (or a newer one) is held or `wait` is over;
    // `until == None`: everything that is ready.
    fn drain(&mut self, wait: Duration, until: Option<i64>) -> ResultType<()> {
        let deadline = Instant::now() + wait;
        // Bounded: a decoder cannot have more than a handful of buffers and events ready.
        for _ in 0..256 {
            if let (Some(until), Some(held)) = (until, self.held.as_ref()) {
                if held.ts >= until {
                    return Ok(());
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut info = ffi::AMediaCodecBufferInfo {
                offset: 0,
                size: 0,
                presentationTimeUs: 0,
                flags: 0,
            };
            let r = unsafe {
                ffi::AMediaCodec_dequeueOutputBuffer(
                    self.codec,
                    &mut info,
                    remaining.as_micros() as i64,
                )
            } as isize;
            if r >= 0 {
                self.on_output(r as usize, info.presentationTimeUs);
            } else if r == TRY_AGAIN_LATER {
                return Ok(());
            } else if r == OUTPUT_FORMAT_CHANGED {
                unsafe { self.read_format() };
            } else if r != OUTPUT_BUFFERS_CHANGED {
                bail!("dequeueOutputBuffer failed ({r})");
            }
        }
        Ok(())
    }

    fn on_output(&mut self, idx: usize, ts: i64) {
        if let Some(old) = self.held.take() {
            // A newer picture is out: the older one is never shown.
            unsafe { ffi::AMediaCodec_releaseOutputBuffer(self.codec, old.idx as _, false) };
            self.stats.dropped += 1;
        }
        let now = Instant::now();
        while let Some((pending_ts, queued)) = self.pending.front().copied() {
            if pending_ts > ts {
                break;
            }
            self.pending.pop_front();
            if pending_ts == ts {
                self.stats
                    .out_us
                    .push(now.saturating_duration_since(queued).as_micros() as u64);
            }
        }
        self.held = Some(Held { idx, ts });
        self.stats.out += 1;
    }

    fn check_stall(&self, now: Instant) -> ResultType<()> {
        if let Some((_, queued)) = self.pending.front() {
            let waiting = now.saturating_duration_since(*queued);
            if waiting >= STALL {
                bail!(
                    "no output for {} ms ({} access units waiting)",
                    waiting.as_millis(),
                    self.pending.len()
                );
            }
        }
        Ok(())
    }

    unsafe fn read_format(&mut self) {
        let fmt = ffi::AMediaCodec_getOutputFormat(self.codec);
        if fmt.is_null() {
            return;
        }
        let (mut w, mut h) = (0, 0);
        let has_size = ffi::AMediaFormat_getInt32(fmt, c!("width"), &mut w)
            && ffi::AMediaFormat_getInt32(fmt, c!("height"), &mut h)
            && w > 0
            && h > 0;
        let crop = crop_rect(fmt);
        // Owned by the format.
        let text = ffi::AMediaFormat_toString(fmt);
        let text = if text.is_null() {
            String::new()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        };
        ffi::AMediaFormat_delete(fmt);
        let (buf_w, buf_h) = if has_size {
            (w, h)
        } else {
            (self.width, self.height)
        };
        let padded = |buf: i32, picture: i32| buf >= picture && buf - picture < MAX_PADDING;
        let (vis_w, vis_h) = match crop {
            Some((l, t, r, b)) if r > l && b > t => (r - l + 1, b - t + 1),
            // No crop known: a buffer a little larger than the display is the display's
            // picture plus alignment padding (1920x1088 for 1920x1080).
            _ if padded(buf_w, self.width) && padded(buf_h, self.height) => {
                (self.width, self.height)
            }
            _ => (buf_w, buf_h),
        };
        if (buf_w, buf_h, vis_w, vis_h)
            != (
                self.buf_width,
                self.buf_height,
                self.vis_width,
                self.vis_height,
            )
        {
            self.info_dirty = true;
        }
        self.buf_width = buf_w;
        self.buf_height = buf_h;
        self.vis_width = vis_w;
        self.vis_height = vis_h;
        log::info!(
            "LATENCY client surface display={} state=format decoder={} buffer={}x{} picture={}x{} crop={:?} format={}",
            self.display, self.name, buf_w, buf_h, vis_w, vis_h, crop, text
        );
    }

    // One line every 5 s while frames flow: what went in and came out, and how long a picture
    // took from its access unit being queued to its output buffer being taken.
    fn maybe_log(&mut self, now: Instant) {
        if now.saturating_duration_since(self.log_at) < LOG_INTERVAL {
            return;
        }
        self.log_at = now;
        let s = std::mem::take(&mut self.stats);
        if s.fed == 0 && s.out == 0 {
            return;
        }
        let mut out = s.out_us;
        out.sort_unstable();
        let at = |q: f64| {
            if out.is_empty() {
                "n/a".to_owned()
            } else {
                let i = ((out.len() - 1) as f64 * q).round() as usize;
                format!("{:.1}", out[i] as f64 / 1000.0)
            }
        };
        log::info!(
            "SURFDEC display={} name={} in={} out={} rendered={} dropped={} out_ms={}/{}/{} late={} lag={} waiting={}",
            self.display,
            self.name,
            s.fed,
            s.out,
            s.rendered,
            s.dropped,
            at(0.5),
            at(0.95),
            at(1.0),
            s.late,
            s.lag,
            self.pending.len()
        );
    }
}

impl Drop for SurfaceDecoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(held) = self.held.take() {
                ffi::AMediaCodec_releaseOutputBuffer(self.codec, held.idx as _, false);
            }
            ffi::AMediaCodec_stop(self.codec);
            ffi::AMediaCodec_delete(self.codec);
        }
    }
}
