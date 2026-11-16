// 24FPS (actually 23.976FPS) is what video professionals ages ago determined to be the
// slowest playback rate that still looks smooth enough to feel real.
// Our eyes can see a slight difference and even though 30FPS actually shows
// more information and is more realistic.
// 60FPS is commonly used in game, teamviewer 12 support this for video editing user.

// how to capture with mouse cursor:
// https://docs.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api?redirectedfrom=MSDN

// RECORD: The following Project has implemented audio capture, hardware codec and mouse cursor drawn.
// https://github.com/PHZ76/DesktopSharing

// dxgi memory leak issue
// https://stackoverflow.com/questions/47801238/memory-leak-in-creating-direct2d-device
// but per my test, it is more related to AcquireNextFrame,
// https://forums.developer.nvidia.com/t/dxgi-outputduplication-memory-leak-when-using-nv-but-not-amd-drivers/108582

// to-do:
// https://slhck.info/video/2017/03/01/rate-control.html

use super::{display_service::check_display_changed, service::ServiceTmpl, video_qos::VideoQoS, *};
#[cfg(target_os = "linux")]
use crate::common::SimpleCallOnReturn;
#[cfg(target_os = "linux")]
use crate::platform::linux::is_x11;
use crate::privacy_mode::{get_privacy_mode_conn_id, INVALID_PRIVACY_MODE_CONN_ID};
#[cfg(windows)]
use crate::{
    platform::windows::is_process_consent_running,
    privacy_mode::{is_current_privacy_mode_impl, PRIVACY_MODE_IMPL_WIN_MAG},
    ui_interface::is_installed,
};
use hbb_common::{
    anyhow::anyhow,
    config,
    tokio::sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex as TokioMutex,
    },
};
#[cfg(feature = "hwcodec")]
use scrap::hwcodec::{HwRamEncoder, HwRamEncoderConfig};
#[cfg(feature = "vram")]
use scrap::vram::{VRamEncoder, VRamEncoderConfig};
#[cfg(not(windows))]
use scrap::Capturer;
use scrap::{
    aom::AomEncoderConfig,
    codec::{Encoder, EncoderCfg},
    record::{Recorder, RecorderContext},
    vpxcodec::{VpxEncoderConfig, VpxVideoCodecId},
    CodecFormat, Display, EncodeInput, TraitCapturer, TraitPixelBuffer,
};
#[cfg(windows)]
use std::io::ErrorKind::ConnectionReset;
#[cfg(windows)]
use std::sync::Once;
use std::{
    collections::HashSet,
    io::ErrorKind::WouldBlock,
    ops::{Deref, DerefMut},
    time::{self, Duration, Instant},
};

pub const OPTION_REFRESH: &'static str = "refresh";

/// One encoded message as the capture loop saw it: any key frame in it, and its size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Encoded {
    pub key: bool,
    pub bytes: usize,
}

// `bail!("SWITCH")` that names its reason for the `LATENCY switch` line (latency_stats.rs).
macro_rules! bail_switch {
    ($reason:expr) => {{
        super::latency_stats::note_switch($reason);
        bail!("SWITCH");
    }};
}

#[cfg(windows)]
const DXGI_RECOVERY_LIMIT: usize = 3;
#[cfg(windows)]
const DXGI_RECOVERY_WINDOW: Duration = Duration::from_secs(10);
#[cfg(windows)]
const DXGI_RECOVERY_FRAME_GRACE: Duration = Duration::from_secs(2);
// A new DXGI capturer that has produced no image for this long falls back to GDI. Upstream fell
// back after 4 empty waits (~70 ms), which a fullscreen game switching display modes hits on
// every switch, leaving the session on GDI (~36 ms per 1440p grab, ~25 fps) until a restart.
#[cfg(windows)]
const NO_IMAGE_GDI_AFTER: Duration = Duration::from_secs(2);
// After a fallback to GDI (not forced by option or portable service), DXGI is tried again after
// these delays, per display; a DXGI run that lasts `DXGI_RETRY_RESET_AFTER` resets the backoff.
#[cfg(windows)]
const DXGI_RETRY_BACKOFF: [Duration; 4] = [
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
#[cfg(windows)]
const DXGI_RETRY_RESET_AFTER: Duration = Duration::from_secs(30);

// display name -> index into `DXGI_RETRY_BACKOFF`.
#[cfg(windows)]
fn dxgi_retry_level() -> &'static Mutex<HashMap<String, usize>> {
    static LEVEL: std::sync::OnceLock<Mutex<HashMap<String, usize>>> = std::sync::OnceLock::new();
    LEVEL.get_or_init(Default::default)
}

#[cfg(windows)]
struct DxgiRecoveryState {
    attempts: usize,
    window_started: Option<Instant>,
    restart_pending: bool,
    fallback_pending: bool,
}

#[cfg(windows)]
impl DxgiRecoveryState {
    fn new() -> Self {
        Self {
            attempts: 0,
            window_started: None,
            restart_pending: false,
            fallback_pending: false,
        }
    }

    fn next_attempt(&mut self) -> Option<usize> {
        if self
            .window_started
            .map(|started| started.elapsed() > DXGI_RECOVERY_WINDOW)
            .unwrap_or(true)
        {
            self.attempts = 0;
            self.window_started = Some(Instant::now());
        }
        if self.attempts >= DXGI_RECOVERY_LIMIT {
            self.fallback_pending = true;
            return None;
        }
        self.attempts += 1;
        self.restart_pending = true;
        Some(self.attempts)
    }

    fn take_restart_pending(&mut self) -> bool {
        std::mem::take(&mut self.restart_pending)
    }

    fn take_fallback_pending(&mut self) -> bool {
        std::mem::take(&mut self.fallback_pending)
    }
}

type FrameFetchedNotifierSender = UnboundedSender<(i32, Option<Instant>)>;
type FrameFetchedNotifierReceiver = Arc<TokioMutex<UnboundedReceiver<(i32, Option<Instant>)>>>;

lazy_static::lazy_static! {
    static ref FRAME_FETCHED_NOTIFIERS: Mutex<HashMap<usize, (FrameFetchedNotifierSender, FrameFetchedNotifierReceiver)>> = Mutex::new(HashMap::default());

    // display_idx -> set of conn id.
    // Used to record which connections need to be notified when
    // 1. A new frame is received from a web client.
    //   Because web client does not send the display index in message `VideoReceived`.
    // 2. The client is closing.
    static ref DISPLAY_CONN_IDS: Arc<Mutex<HashMap<usize, HashSet<i32>>>> = Default::default();
    pub static ref VIDEO_QOS: Arc<Mutex<VideoQoS>> = Default::default();
    pub static ref IS_UAC_RUNNING: Arc<Mutex<bool>> = Default::default();
    pub static ref IS_FOREGROUND_WINDOW_ELEVATED: Arc<Mutex<bool>> = Default::default();
    static ref SCREENSHOTS: Mutex<HashMap<(VideoSource, usize), Screenshot>> = Default::default();
}

struct Screenshot {
    sid: String,
    tx: Sender,
    restore_vram: bool,
}

#[inline]
pub fn notify_video_frame_fetched(display_idx: usize, conn_id: i32, frame_tm: Option<Instant>) {
    if let Some(notifier) = FRAME_FETCHED_NOTIFIERS.lock().unwrap().get(&display_idx) {
        notifier.0.send((conn_id, frame_tm)).ok();
    }
}

#[inline]
pub fn notify_video_frame_fetched_by_conn_id(conn_id: i32, frame_tm: Option<Instant>) {
    let vec_display_idx: Vec<usize> = {
        let display_conn_ids = DISPLAY_CONN_IDS.lock().unwrap();
        display_conn_ids
            .iter()
            .filter_map(|(display_idx, conn_ids)| {
                if conn_ids.contains(&conn_id) {
                    Some(*display_idx)
                } else {
                    None
                }
            })
            .collect()
    };
    let notifiers = FRAME_FETCHED_NOTIFIERS.lock().unwrap();
    for display_idx in vec_display_idx {
        if let Some(notifier) = notifiers.get(&display_idx) {
            notifier.0.send((conn_id, frame_tm)).ok();
        }
    }
}

struct VideoFrameController {
    display_idx: usize,
    cur: Instant,
    send_conn_ids: HashSet<i32>,
}

impl VideoFrameController {
    fn new(display_idx: usize) -> Self {
        Self {
            display_idx,
            cur: Instant::now(),
            send_conn_ids: HashSet::new(),
        }
    }

    fn reset(&mut self) {
        self.send_conn_ids.clear();
    }

    fn set_send(&mut self, tm: Instant, conn_ids: HashSet<i32>) {
        if !conn_ids.is_empty() {
            self.cur = tm;
            self.send_conn_ids = conn_ids;
            DISPLAY_CONN_IDS
                .lock()
                .unwrap()
                .insert(self.display_idx, self.send_conn_ids.clone());
        }
    }

    #[tokio::main(flavor = "current_thread")]
    async fn try_wait_next(&mut self, fetched_conn_ids: &mut HashSet<i32>, timeout_millis: u64) {
        if self.send_conn_ids.is_empty() {
            return;
        }

        let timeout_dur = Duration::from_millis(timeout_millis as u64);
        let receiver = {
            match FRAME_FETCHED_NOTIFIERS
                .lock()
                .unwrap()
                .get(&self.display_idx)
            {
                Some(notifier) => notifier.1.clone(),
                None => {
                    return;
                }
            }
        };
        let mut receiver_guard = receiver.lock().await;
        match tokio::time::timeout(timeout_dur, receiver_guard.recv()).await {
            Err(_) => {
                // break if timeout
                // log::error!("blocking wait frame receiving timeout {}", timeout_millis);
            }
            Ok(Some((id, instant))) => {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
            Ok(None) => {
                // this branch would never be reached
            }
        }
        while !receiver_guard.is_empty() {
            if let Some((id, instant)) = receiver_guard.recv().await {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum VideoSource {
    Monitor,
    Camera,
}

impl VideoSource {
    pub fn service_name_prefix(&self) -> &'static str {
        match self {
            VideoSource::Monitor => "monitor",
            VideoSource::Camera => "camera",
        }
    }

    pub fn is_monitor(&self) -> bool {
        matches!(self, VideoSource::Monitor)
    }

    pub fn is_camera(&self) -> bool {
        matches!(self, VideoSource::Camera)
    }
}

#[derive(Clone)]
pub struct VideoService {
    sp: GenericService,
    idx: usize,
    source: VideoSource,
    #[cfg(windows)]
    dxgi_recovery_state: Arc<Mutex<DxgiRecoveryState>>,
}

impl Deref for VideoService {
    type Target = ServiceTmpl<ConnInner>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for VideoService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

pub fn get_service_name(source: VideoSource, idx: usize) -> String {
    format!("{}{}", source.service_name_prefix(), idx)
}

pub fn new(source: VideoSource, idx: usize) -> GenericService {
    let _ = FRAME_FETCHED_NOTIFIERS
        .lock()
        .unwrap()
        .entry(idx)
        .or_insert_with(|| {
            let (tx, rx) = unbounded_channel();
            (tx, Arc::new(TokioMutex::new(rx)))
        });
    let vs = VideoService {
        sp: GenericService::new(get_service_name(source, idx), true),
        idx,
        source,
        #[cfg(windows)]
        dxgi_recovery_state: Arc::new(Mutex::new(DxgiRecoveryState::new())),
    };
    let (source, display) = (vs.source, vs.idx);
    GenericService::run(&vs, move |vs: VideoService| {
        let res = run(vs);
        if let Err(err) = &res {
            super::latency_stats::on_restart(source, display, err);
        }
        res
    });
    vs.sp
}

// Capturer object is expensive, avoiding to create it frequently.
fn create_capturer(
    privacy_mode_id: i32,
    display: Display,
    _current: usize,
    _portable_service_running: bool,
) -> ResultType<Box<dyn TraitCapturer>> {
    #[cfg(not(windows))]
    let c: Option<Box<dyn TraitCapturer>> = None;
    #[cfg(windows)]
    let mut c: Option<Box<dyn TraitCapturer>> = None;
    if privacy_mode_id > 0 {
        #[cfg(windows)]
        {
            // Windows Mode 1 can cover every local monitor with overlay windows,
            // but the legacy magnifier capture backend is still single-monitor
            // constrained. Keep display-switch gating aligned with that backend
            // limit, not just the overlay coverage.
            if let Some(c1) = crate::privacy_mode::win_mag::create_capturer(
                privacy_mode_id,
                display.origin(),
                display.width(),
                display.height(),
            )? {
                c = Some(Box::new(c1));
            }
        }
    }

    match c {
        Some(c1) => return Ok(c1),
        None => {
            #[cfg(windows)]
            {
                log::debug!("Create capturer dxgi|gdi");
                return crate::portable_service::client::create_capturer(
                    _current,
                    display,
                    _portable_service_running,
                );
            }
            #[cfg(not(windows))]
            {
                log::debug!("Create capturer from scrap");
                return Ok(Box::new(
                    Capturer::new(display).with_context(|| "Failed to create capturer")?,
                ));
            }
        }
    };
}

// This function works on privacy mode. Windows only for now.
pub fn test_create_capturer(
    privacy_mode_id: i32,
    display_idx: usize,
    timeout_millis: u64,
) -> String {
    let test_begin = Instant::now();
    loop {
        let err = match Display::all() {
            Ok(mut displays) => {
                if displays.len() <= display_idx {
                    anyhow!(
                        "Failed to get display {}, the displays' count is {}",
                        display_idx,
                        displays.len()
                    )
                } else {
                    let display = displays.remove(display_idx);
                    match create_capturer(privacy_mode_id, display, display_idx, false) {
                        Ok(_) => return "".to_owned(),
                        Err(e) => e,
                    }
                }
            }
            Err(e) => e.into(),
        };
        if test_begin.elapsed().as_millis() >= timeout_millis as _ {
            return err.to_string();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// Note: This function is extremely expensive, do not call it frequently.
#[cfg(windows)]
fn check_uac_switch(privacy_mode_id: i32, capturer_privacy_mode_id: i32) -> ResultType<()> {
    if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
        && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
    {
        if !is_installed() {
            if privacy_mode_id != capturer_privacy_mode_id {
                if !is_process_consent_running()? {
                    bail!("consent.exe is not running");
                }
            }
            if is_process_consent_running()? {
                bail!("consent.exe is running");
            }
        }
    }
    Ok(())
}

pub(super) struct CapturerInfo {
    pub origin: (i32, i32),
    pub width: usize,
    pub height: usize,
    pub ndisplay: usize,
    pub current: usize,
    pub privacy_mode_id: i32,
    pub _capturer_privacy_mode_id: i32,
    pub capturer: Box<dyn TraitCapturer>,
}

impl Deref for CapturerInfo {
    type Target = Box<dyn TraitCapturer>;

    fn deref(&self) -> &Self::Target {
        &self.capturer
    }
}

impl DerefMut for CapturerInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.capturer
    }
}

fn get_capturer_monitor(
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    #[cfg(target_os = "linux")]
    {
        if !is_x11() {
            return super::wayland::get_capturer_for_display(current);
        }
    }

    let mut displays = Display::all()?;
    let ndisplay = displays.len();
    if ndisplay <= current {
        bail!(
            "Failed to get display {}, displays len: {}",
            current,
            ndisplay
        );
    }

    let display = displays.remove(current);

    #[cfg(target_os = "linux")]
    if let Display::X11(inner) = &display {
        if let Err(err) = inner.get_shm_status() {
            log::warn!(
                "MIT-SHM extension not working properly on select X11 server: {:?}",
                err
            );
        }
    }

    let (origin, width, height) = (display.origin(), display.width(), display.height());
    let name = display.name();
    log::debug!(
        "#displays={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ndisplay,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        &name,
    );

    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    #[cfg(not(windows))]
    let capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    let mut capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    {
        if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
            && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
        {
            if !is_installed() {
                if is_process_consent_running()? {
                    capturer_privacy_mode_id = INVALID_PRIVACY_MODE_CONN_ID;
                }
            }
        }
    }
    log::debug!(
        "Try create capturer with capturer privacy mode id {}",
        capturer_privacy_mode_id,
    );

    if privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID {
        if privacy_mode_id != capturer_privacy_mode_id {
            log::info!("In privacy mode, but show UAC prompt window for now");
        } else {
            log::info!("In privacy mode, the peer side cannot watch the screen");
        }
    }
    let capturer = create_capturer(
        capturer_privacy_mode_id,
        display,
        current,
        portable_service_running,
    )?;
    Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: capturer_privacy_mode_id,
        capturer,
    })
}

fn get_capturer_camera(current: usize) -> ResultType<CapturerInfo> {
    let cameras = camera::Cameras::get_sync_cameras();
    let ncamera = cameras.len();
    if ncamera <= current {
        bail!("Failed to get camera {}, cameras len: {}", current, ncamera,);
    }
    let Some(camera) = cameras.get(current) else {
        bail!(
            "Camera of index {} doesn't exist or platform not supported",
            current
        );
    };
    let capturer = camera::Cameras::get_capturer(current)?;
    let (width, height) = (camera.width as usize, camera.height as usize);
    let origin = (camera.x as i32, camera.y as i32);
    let name = &camera.name;
    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    let _capturer_privacy_mode_id = privacy_mode_id;
    log::debug!(
        "#cameras={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ncamera,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        name,
    );
    return Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay: ncamera,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: privacy_mode_id,
        capturer,
    });
}
fn get_capturer(
    source: VideoSource,
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    match source {
        VideoSource::Monitor => get_capturer_monitor(current, portable_service_running),
        VideoSource::Camera => get_capturer_camera(current),
    }
}

fn run(vs: VideoService) -> ResultType<()> {
    let mut _raii = Raii::new(vs.idx, vs.sp.name());
    // Wayland only support one video capturer for now. It is ok to call ensure_inited() here.
    //
    // ensure_inited() is needed because clear() may be called.
    // to-do: wayland ensure_inited should pass current display index.
    // But for now, we do not support multi-screen capture on wayland.
    #[cfg(target_os = "linux")]
    super::wayland::ensure_inited()?;
    #[cfg(target_os = "linux")]
    let _wayland_call_on_ret = {
        // Increment active display count when starting
        let _display_count = super::wayland::increment_active_display_count();

        SimpleCallOnReturn {
            b: true,
            f: Box::new(|| {
                // Decrement active display count and only clear if this was the last display
                let remaining_count = super::wayland::decrement_active_display_count();
                if remaining_count == 0 {
                    super::wayland::clear();
                }
            }),
        }
    };

    #[cfg(windows)]
    let last_portable_service_running = crate::portable_service::client::running();
    #[cfg(not(windows))]
    let last_portable_service_running = false;

    let display_idx = vs.idx;
    #[cfg(windows)]
    let dxgi_recovery_state = vs.dxgi_recovery_state.clone();
    let sp = vs.sp;
    let mut c = get_capturer(vs.source, display_idx, last_portable_service_running)?;
    #[cfg(windows)]
    // ACCESS_LOST marks the next successful capturer creation as a recovery. This timestamp is
    // consumed once and temporarily holds off the normal WouldBlock-to-GDI fallback, giving the
    // replacement DXGI capturer time to produce its first frame. Normal startup is unaffected.
    let dxgi_recovery_started = dxgi_recovery_state
        .lock()
        .unwrap()
        .take_restart_pending()
        .then(Instant::now);
    #[cfg(windows)]
    if dxgi_recovery_state.lock().unwrap().take_fallback_pending() {
        c.set_gdi();
        log::info!("dxgi recovery exhausted, fall back to gdi");
    }
    #[cfg(windows)]
    if !scrap::codec::enable_directx_capture() && !c.is_gdi() {
        log::info!("disable dxgi with option, fall back to gdi");
        c.set_gdi();
    }
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    let mut spf = video_qos.spf();
    let mut quality = video_qos.ratio();
    let record_incoming = config::option2bool(
        "allow-auto-record-incoming",
        &Config::get_option("allow-auto-record-incoming"),
    );
    let client_record = video_qos.record();
    drop(video_qos);
    let (mut encoder, encoder_cfg, codec_format, use_i444, recorder) = match setup_encoder(
        &c,
        sp.name(),
        quality,
        client_record,
        record_incoming,
        last_portable_service_running,
        vs.source,
        display_idx,
    ) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to create encoder: {err:?}, fallback to VP9");
            Encoder::set_fallback(&EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval: None,
            }));
            setup_encoder(
                &c,
                sp.name(),
                quality,
                client_record,
                record_incoming,
                last_portable_service_running,
                vs.source,
                display_idx,
            )?
        }
    };
    // Capture and encode run on this thread: keep it scheduled under CPU load. Only with a
    // hardware encoder; a software encode at MMCSS priority would take CPU from a game.
    #[cfg(windows)]
    let _mmcss = encoder
        .is_hardware()
        .then(crate::platform::windows::mmcss_capture);
    #[cfg(feature = "vram")]
    c.set_output_texture(encoder.input_texture());
    #[cfg(target_os = "android")]
    if vs.source.is_monitor() {
        if let Err(e) = check_change_scale(encoder.is_hardware()) {
            try_broadcast_display_changed(&sp, display_idx, &c, true).ok();
            bail!(e);
        }
    }
    {
        let mut qos = VIDEO_QOS.lock().unwrap();
        qos.store_bitrate_at(encoder.bitrate(), quality);
        qos.set_bitrate_curve(bitrate_curve(&encoder_cfg));
    }
    VIDEO_QOS
        .lock()
        .unwrap()
        .set_support_changing_quality(&sp.name(), encoder.support_changing_quality());
    log::info!("initial quality: {quality:?}");

    if sp.is_option_true(OPTION_REFRESH) {
        sp.set_option_bool(OPTION_REFRESH, false);
    }

    let mut frame_controller = VideoFrameController::new(display_idx);

    let start = time::Instant::now();
    let mut last_check_displays = time::Instant::now();
    #[cfg(windows)]
    let mut try_gdi = 1;
    #[cfg(windows)]
    log::info!("gdi: {}", c.is_gdi());
    // When this run fell back from DXGI to GDI (`set_gdi` below or the recovery fallback above),
    // for the periodic DXGI retry. Not for GDI forced by option or the portable service.
    #[cfg(windows)]
    let dxgi_retry = !last_portable_service_running
        && scrap::codec::enable_directx_capture()
        && Config::get_option(base::config::keys::OPTION_ENABLE_DXGI_RETRY) != "N";
    #[cfg(windows)]
    let mut gdi_fallback_at: Option<Instant> = (dxgi_retry && c.is_gdi()).then(Instant::now);
    #[cfg(windows)]
    let mut dxgi_retry_reset = false;
    #[cfg(windows)]
    start_uac_elevation_check();

    #[cfg(target_os = "linux")]
    let mut would_block_count = 0u32;
    let mut yuv = Vec::new();
    let mut mid_data = Vec::new();
    let mut repeat_encode_counter = 0;
    let repeat_encode_max = 10;
    let mut encode_fail_counter = 0;
    let mut first_frame = true;
    let capture_width = c.width;
    let capture_height = c.height;
    let (mut second_instant, mut send_counter) = (Instant::now(), 0);
    let mut latency_stats =
        super::latency_stats::LatencyStats::new(vs.source, display_idx, encoder.bitrate());
    // M13: refreshes served by a forced key frame (enable-refresh-keyframe, read per refresh).
    #[cfg(windows)]
    let mut key_refresh = super::key_refresh::KeyRefresh::default();
    // M14: re-encodes of the kept last image on a static screen, only on an encoder that runs
    // with the small VBV (nvenc with `nvenc-vbv-frames` > 0): the two only go on together, and
    // `enable-static-refine=N` turns both off (`nvenc_vbv_frames` is then 0).
    #[cfg(windows)]
    let mut static_refine = super::key_refresh::StaticRefine::new(encoder.small_vbv());
    #[cfg(windows)]
    if static_refine.enabled() {
        log::info!("static refine on");
    }
    // M7: an nvenc encoder opened at `nvenc-nominal-fps` gets its bitrate rescaled to the frame
    // rate the loop runs at (check_qos).
    #[cfg(windows)]
    let mut virtual_fps = encoder.virtual_fps().then(|| {
        super::virtual_fps::VirtualFps::new(
            Config::get_option(base::config::keys::OPTION_NVENC_FPS_FROM_SEND_RATE) == "Y",
        )
    });
    // Absolute frame deadlines instead of sleeping for the rest of each period (Windows only).
    #[cfg(windows)]
    let deadline_pacing =
        Config::get_option(base::config::keys::OPTION_ENABLE_CAPTURE_DEADLINE_PACING) != "N";
    #[cfg(not(windows))]
    let deadline_pacing = false;
    // Only the in-process DXGI capturer gets the longer wait: the portable service takes the
    // timeout as its own frame period, GDI, magnifier and camera capturers ignore it.
    let dxgi_wait_margin =
        deadline_pacing && vs.source.is_monitor() && !last_portable_service_running;
    let mut next_tick = Instant::now();

    while sp.ok() {
        #[cfg(windows)]
        check_uac_switch(c.privacy_mode_id, c._capturer_privacy_mode_id)?;
        let quality_before = quality;
        check_qos(
            &mut encoder,
            &mut quality,
            &mut spf,
            client_record,
            &mut send_counter,
            &mut second_instant,
            &sp.name(),
            #[cfg(windows)]
            &mut virtual_fps,
            &mut latency_stats,
        )?;
        if quality != quality_before && encoder.support_changing_quality() {
            latency_stats.on_quality(encoder.bitrate());
        }
        if sp.is_option_true(OPTION_REFRESH) {
            #[cfg(windows)]
            let keyframe = refresh_keyframe_enabled();
            #[cfg(not(windows))]
            let keyframe = false;
            if !keyframe {
                if vs.source.is_monitor() {
                    let _ = try_broadcast_display_changed(&sp, display_idx, &c, true);
                }
                log::info!("switch to refresh");
                bail_switch!("refresh");
            }
            #[cfg(windows)]
            {
                // Cleared here: the encoder keeps running, so run() does not start over.
                sp.set_option_bool(OPTION_REFRESH, false);
                if vs.source.is_monitor() {
                    // A real display change still restarts capture.
                    try_broadcast_display_changed(&sp, display_idx, &c, true)?;
                }
                // A screenshot is taken from the next captured image, which a static screen
                // only gives after a restart.
                if SCREENSHOTS
                    .lock()
                    .unwrap()
                    .contains_key(&(vs.source, display_idx))
                {
                    log::info!("switch to refresh for a screenshot");
                    bail_switch!("refresh_screenshot");
                }
                match key_refresh.on_refresh(Instant::now()) {
                    super::key_refresh::RefreshAction::Request => {
                        request_refresh_key(&mut encoder, &mut key_refresh)?
                    }
                    super::key_refresh::RefreshAction::Merged => latency_stats.on_refresh_merged(),
                    super::key_refresh::RefreshAction::Switch(reason) => {
                        log::info!("refresh escalated to SWITCH: {reason}");
                        bail_switch!(reason);
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            let now = Instant::now();
            if key_refresh.take_due(now) {
                request_refresh_key(&mut encoder, &mut key_refresh)?;
            }
            if key_refresh.timed_out(now) {
                log::warn!(
                    "refresh: no key frame within {:?}, switch",
                    super::key_refresh::KEY_TIMEOUT
                );
                bail_switch!("refresh_key_timeout");
            }
        }
        if codec_format != Encoder::negotiated_codec() {
            log::info!(
                "switch due to codec changed, {:?} -> {:?}",
                codec_format,
                Encoder::negotiated_codec()
            );
            bail_switch!("codec");
        }
        #[cfg(windows)]
        if last_portable_service_running != crate::portable_service::client::running() {
            log::info!("switch due to portable service running changed");
            bail_switch!("portable_service");
        }
        if Encoder::use_i444(&encoder_cfg) != use_i444 {
            log::info!("switch due to i444 changed");
            bail_switch!("i444");
        }
        #[cfg(windows)]
        if let Some(at) = gdi_fallback_at {
            let level = *dxgi_retry_level().lock().unwrap().get(sp.name().as_str()).unwrap_or(&0);
            let delay = DXGI_RETRY_BACKOFF[level.min(DXGI_RETRY_BACKOFF.len() - 1)];
            if at.elapsed() >= delay {
                dxgi_retry_level()
                    .lock()
                    .unwrap()
                    .insert(sp.name(), (level + 1).min(DXGI_RETRY_BACKOFF.len() - 1));
                #[cfg(feature = "vram")]
                VRamEncoder::set_fallback_gdi(sp.name(), false);
                log::info!("gdi for {:?}, retry dxgi (level {level})", at.elapsed());
                bail_switch!("gdi_retry_dxgi");
            }
        } else if !dxgi_retry_reset && !c.is_gdi() && start.elapsed() >= DXGI_RETRY_RESET_AFTER {
            dxgi_retry_reset = true;
            dxgi_retry_level().lock().unwrap().remove(sp.name().as_str());
        }
        #[cfg(all(windows, feature = "vram"))]
        if c.is_gdi() && encoder.input_texture() {
            log::info!("changed to gdi when using vram");
            VRamEncoder::set_fallback_gdi(sp.name(), true);
            bail_switch!("gdi_vram");
        }
        if vs.source.is_monitor() {
            check_privacy_mode_changed(&sp, display_idx, &c)?;
        }
        #[cfg(windows)]
        {
            if crate::platform::windows::desktop_changed()
                && !crate::portable_service::client::running()
            {
                bail!("Desktop changed");
            }
        }
        let now = time::Instant::now();
        if vs.source.is_monitor() && last_check_displays.elapsed().as_millis() > 1000 {
            last_check_displays = now;
            // This check may be redundant, but it is better to be safe.
            // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
            try_broadcast_display_changed(&sp, display_idx, &c, false)?;
        }

        frame_controller.reset();
        // `wait` is sampled for captured frames only, not for kept-image and refine encodes.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut wait_sample = true;

        let time = now - start;
        let ms = (time.as_secs() * 1000 + time.subsec_millis() as u64) as i64;
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut frame_timeout = if dxgi_wait_margin {
            dxgi_frame_timeout(spf)
        } else {
            spf
        };
        // A requested or deferred refresh key frame is due before the capturer's timeout.
        #[cfg(windows)]
        if let Some(due) = key_refresh.next_due(spf) {
            frame_timeout = frame_timeout
                .min(due.saturating_duration_since(Instant::now()))
                .max(Duration::from_millis(1));
        }
        let grab_begin = Instant::now();
        let res = match c.frame(frame_timeout) {
            Ok(frame) => {
                let grab_done = Instant::now();
                repeat_encode_counter = 0;
                if frame.valid() {
                    let screenshot_key = (vs.source, display_idx);
                    let screenshot = SCREENSHOTS.lock().unwrap().remove(&screenshot_key);
                    if let Some(mut screenshot) = screenshot {
                        let restore_vram = screenshot.restore_vram;
                        let (msg, w, h, data) = match &frame {
                            scrap::Frame::PixelBuffer(f) => match get_rgba_from_pixelbuf(f) {
                                Ok(rgba) => ("".to_owned(), f.width(), f.height(), rgba),
                                Err(e) => {
                                    let serr = e.to_string();
                                    log::error!(
                                        "Failed to convert the pix format into rgba, {}",
                                        &serr
                                    );
                                    (format!("Convert pixfmt: {}", serr), 0, 0, vec![])
                                }
                            },
                            scrap::Frame::Texture(_) => {
                                if restore_vram {
                                    // Already set one time, just ignore to break infinite loop.
                                    // Though it's unreachable, this branch is kept to avoid infinite loop.
                                    (
                                        "Please change codec and try again.".to_owned(),
                                        0,
                                        0,
                                        vec![],
                                    )
                                } else {
                                    #[cfg(all(windows, feature = "vram"))]
                                    VRamEncoder::set_not_use(sp.name(), true);
                                    screenshot.restore_vram = true;
                                    SCREENSHOTS
                                        .lock()
                                        .unwrap()
                                        .insert(screenshot_key, screenshot);
                                    _raii.try_vram = false;
                                    bail_switch!("screenshot");
                                }
                            }
                        };
                        std::thread::spawn(move || {
                            handle_screenshot(screenshot, msg, w, h, data);
                        });
                        if restore_vram {
                            bail_switch!("screenshot");
                        }
                    }

                    let frame = frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                    let (send_conn_ids, _encoded) = handle_one_frame(
                        display_idx,
                        &sp,
                        frame,
                        ms,
                        &mut encoder,
                        recorder.clone(),
                        &mut encode_fail_counter,
                        &mut first_frame,
                        capture_width,
                        capture_height,
                        &mut latency_stats,
                    )?;
                    #[cfg(windows)]
                    if let Some(encoded) = _encoded {
                        static_refine.on_frame(encoded);
                        match key_refresh.on_encoded(encoded, Instant::now(), false) {
                            Ok(true) if !send_conn_ids.is_empty() => {
                                latency_stats.on_refresh_key_sent(false)
                            }
                            Ok(_) => {}
                            Err(reason) => {
                                log::warn!("refresh: the forced key frame did not come, switch");
                                bail_switch!(reason);
                            }
                        }
                    }
                    // The frame no longer borrows the capturer; the age is of the frame just encoded.
                    #[cfg(windows)]
                    let age = c.last_present_age();
                    #[cfg(not(windows))]
                    let age = None;
                    latency_stats.on_frame(
                        grab_done.saturating_duration_since(grab_begin),
                        grab_done.elapsed(),
                        !send_conn_ids.is_empty(),
                        age,
                    );
                    frame_controller.set_send(now, send_conn_ids);
                    send_counter += 1;
                }
                #[cfg(windows)]
                {
                    #[cfg(feature = "vram")]
                    if try_gdi == 1 && !c.is_gdi() {
                        VRamEncoder::set_fallback_gdi(sp.name(), false);
                    }
                    try_gdi = 0;
                }
                Ok(())
            }
            Err(err) => Err(err),
        };

        // No new image within the capturer's timeout, which under deadline pacing is longer
        // than spf (`dxgi_frame_timeout`).
        let idle = matches!(res, Err(ref e) if e.kind() == WouldBlock);
        match res {
            Err(ref e) if e.kind() == WouldBlock => {
                latency_stats.on_idle();
                #[cfg(windows)]
                if dxgi_recovery_started
                    .map(|started| started.elapsed() >= DXGI_RECOVERY_FRAME_GRACE)
                    .unwrap_or(true)
                {
                    if try_gdi > 0 && !c.is_gdi() {
                        // With `dxgi_retry` (default), wait `NO_IMAGE_GDI_AFTER` for the first
                        // image; without it, upstream's 4 empty waits.
                        if try_gdi > 3 && (!dxgi_retry || start.elapsed() >= NO_IMAGE_GDI_AFTER) {
                            c.set_gdi();
                            try_gdi = 0;
                            if dxgi_retry {
                                gdi_fallback_at = Some(Instant::now());
                            }
                            log::info!("No image for {:?}, fall back to gdi", start.elapsed());
                        } else {
                            try_gdi += 1;
                        }
                    }
                }
                #[cfg(target_os = "linux")]
                {
                    would_block_count += 1;
                    if !is_x11() {
                        if would_block_count >= 100 {
                            // to-do: Unknown reason for WouldBlock 100 times (seconds = 100 * 1 / fps)
                            // https://github.com/rustdesk/rustdesk/blob/63e6b2f8ab51743e77a151e2b7ff18816f5fa2fb/libs/scrap/src/common/wayland.rs#L81
                            //
                            // Do not reset the capturer for now, as it will cause the prompt to show every few minutes.
                            // https://github.com/rustdesk/rustdesk/issues/4276
                            //
                            // super::wayland::clear();
                            // bail!("Wayland capturer none 100 times, try restart capture");
                        }
                    }
                }
                // No new image: a requested key frame comes from the kept last image, or a key
                // or large frame on a now static screen is refined.
                #[cfg(windows)]
                {
                    let now = Instant::now();
                    if key_refresh.kept_due(now, spf) {
                        check_new_subscriber(&sp)?;
                        let (send_conn_ids, encoded) = match encode_kept_frame(
                            display_idx,
                            &sp,
                            &mut encoder,
                            &yuv,
                            ms,
                            &recorder,
                            capture_width,
                            capture_height,
                            &mut latency_stats,
                        ) {
                            Ok(v) => v,
                            Err(e) => {
                                log::warn!("refresh: no key frame from the kept image ({e}), switch");
                                bail_switch!("refresh_kept_failed");
                            }
                        };
                        match key_refresh.on_encoded(encoded, now, true) {
                            Ok(_) => {
                                static_refine.on_kept_key();
                                if !send_conn_ids.is_empty() {
                                    latency_stats.on_refresh_key_sent(true);
                                }
                            }
                            Err(reason) => {
                                log::warn!("refresh: the kept image gave no key frame, switch");
                                bail_switch!(reason);
                            }
                        }
                        frame_controller.set_send(now, send_conn_ids);
                        wait_sample = false;
                        send_counter += 1;
                    } else if !key_refresh.pending() && static_refine.on_idle(now) {
                        check_new_subscriber(&sp)?;
                        match encode_kept_frame(
                            display_idx,
                            &sp,
                            &mut encoder,
                            &yuv,
                            ms,
                            &recorder,
                            capture_width,
                            capture_height,
                            &mut latency_stats,
                        ) {
                            Ok((send_conn_ids, encoded)) => {
                                latency_stats.on_refine();
                                let _ = key_refresh.on_encoded(encoded, now, true);
                                frame_controller.set_send(now, send_conn_ids);
                                wait_sample = false;
                                send_counter += 1;
                            }
                            Err(e) => {
                                log::warn!("static refine failed, off until the next restart: {e}");
                                static_refine.disable();
                            }
                        }
                    }
                }
                if !encoder.latency_free() && yuv.len() > 0 {
                    // yun.len() > 0 means the frame is not texture.
                    if repeat_encode_counter < repeat_encode_max {
                        repeat_encode_counter += 1;
                        let (send_conn_ids, _) = handle_one_frame(
                            display_idx,
                            &sp,
                            EncodeInput::YUV(&yuv),
                            ms,
                            &mut encoder,
                            recorder.clone(),
                            &mut encode_fail_counter,
                            &mut first_frame,
                            capture_width,
                            capture_height,
                            &mut latency_stats,
                        )?;
                        frame_controller.set_send(now, send_conn_ids);
                        send_counter += 1;
                    }
                }
            }
            Err(err) => {
                #[cfg(windows)]
                // The display-change check can restart capture before error handling below.
                let recovery_attempt = if !c.is_gdi() && err.kind() == ConnectionReset {
                    dxgi_recovery_state.lock().unwrap().next_attempt()
                } else {
                    None
                };
                // This check may be redundant, but it is better to be safe.
                // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
                if vs.source.is_monitor() {
                    try_broadcast_display_changed(&sp, display_idx, &c, true)?;
                }

                #[cfg(windows)]
                if !c.is_gdi() {
                    if err.kind() == ConnectionReset {
                        if let Some(attempt) = recovery_attempt {
                            log::debug!(
                                "dxgi access lost, restart capture: attempt {attempt}, error: {err:?}"
                            );
                            bail_switch!("dxgi_access_lost");
                        }
                        log::warn!(
                            "dxgi access lost after {DXGI_RECOVERY_LIMIT} restarts in {} seconds, fall back to gdi: {err:?}",
                            DXGI_RECOVERY_WINDOW.as_secs()
                        );
                        dxgi_recovery_state.lock().unwrap().take_fallback_pending();
                    }
                    c.set_gdi();
                    if dxgi_retry {
                        gdi_fallback_at = Some(Instant::now());
                    }
                    log::info!("dxgi error, fall back to gdi: {:?}", err);
                    continue;
                }
                return Err(err.into());
            }
            _ => {
                #[cfg(target_os = "linux")]
                {
                    would_block_count = 0;
                }
            }
        }

        let mut fetched_conn_ids = HashSet::new();
        let timeout_millis = 3_000u64;
        // A fast bitrate estimator runs (enable-fast-abr-shadow): shorter slices, and the
        // connections that keep the frame waiting are reported to it.
        let stall_slice = super::video_qos::fast::stall_slice(spf);
        let stall_after = super::video_qos::fast::stall_threshold(spf);
        let slice_millis = stall_slice.map_or(300, |d| d.as_millis() as u64);
        let wait_begin = Instant::now();
        while wait_begin.elapsed().as_millis() < timeout_millis as _ {
            if vs.source.is_monitor() {
                check_privacy_mode_changed(&sp, display_idx, &c)?;
            }
            frame_controller.try_wait_next(&mut fetched_conn_ids, slice_millis);
            // break if all connections have received current frame
            if fetched_conn_ids.len() >= frame_controller.send_conn_ids.len() {
                break;
            }
            if stall_slice.is_some() {
                let waited = wait_begin.elapsed();
                if waited >= stall_after {
                    super::video_qos::fast::on_send_stall(
                        frame_controller
                            .send_conn_ids
                            .iter()
                            .filter(|id| !fetched_conn_ids.contains(id))
                            .copied(),
                        waited,
                    );
                }
            }
        }
        latency_stats.on_wait(
            wait_sample && !frame_controller.send_conn_ids.is_empty(),
            wait_begin.elapsed(),
        );
        DISPLAY_CONN_IDS.lock().unwrap().remove(&display_idx);

        // A refresh key frame that is due before the end of this period cuts the sleep short.
        #[cfg(windows)]
        let refresh_due = key_refresh.next_due(spf);
        #[cfg(not(windows))]
        let refresh_due: Option<Instant> = None;
        if deadline_pacing {
            next_tick += spf;
            let now = Instant::now();
            if let Some(due) = refresh_due {
                if due < next_tick {
                    // The schedule continues from the refresh deadline.
                    next_tick = due.max(now);
                }
            }
            if now < next_tick {
                latency_stats.on_sleep(next_tick - now);
                std::thread::sleep(next_tick - now);
            } else if idle {
                // Late only by the capturer's longer timeout, with no frame to catch up on:
                // follow it without counting a re-anchor.
                next_tick = now;
            } else if now - next_tick > spf {
                // More than one period late (a long wait, a stall, a camera slower than the
                // target): start a new schedule from now rather than bursting frames to catch up.
                next_tick = now;
                latency_stats.on_reanchor();
            }
        } else {
            let elapsed = now.elapsed();
            // may need to enable frame(timeout)
            log::trace!("{:?} {:?}", time::Instant::now(), elapsed);
            if elapsed < spf {
                let mut rest = spf - elapsed;
                if let Some(due) = refresh_due {
                    rest = rest.min(due.saturating_duration_since(Instant::now()));
                }
                if !rest.is_zero() {
                    latency_stats.on_sleep(rest);
                    std::thread::sleep(rest);
                }
            }
        }
        latency_stats.tick(spf, deadline_pacing);
    }

    Ok(())
}

// How long DXGI may wait for a new image under deadline pacing: ceil(spf) + 2 ms. The plain spf
// truncated to ms (16 ms at 60 fps) is shorter than the present interval (16.67 ms), so a loop
// that keeps pace with vsync would time out just before the next present and skip a tick.
fn dxgi_frame_timeout(spf: Duration) -> Duration {
    Duration::from_millis(((spf.as_micros() + 999) / 1000) as u64 + 2)
}

struct Raii {
    display_idx: usize,
    name: String,
    try_vram: bool,
}

impl Raii {
    fn new(display_idx: usize, name: String) -> Self {
        log::info!("new video service: {}", name);
        VIDEO_QOS.lock().unwrap().new_display(name.clone());
        Raii {
            display_idx,
            name,
            try_vram: true,
        }
    }
}

impl Drop for Raii {
    fn drop(&mut self) {
        log::info!("stop video service: {}", self.name);
        #[cfg(feature = "vram")]
        if self.try_vram {
            VRamEncoder::set_not_use(self.name.clone(), false);
        }
        #[cfg(feature = "vram")]
        Encoder::update(scrap::codec::EncodingUpdate::Check);
        VIDEO_QOS.lock().unwrap().remove_display(&self.name);
        DISPLAY_CONN_IDS.lock().unwrap().remove(&self.display_idx);
    }
}

fn setup_encoder(
    c: &CapturerInfo,
    name: String,
    quality: f32,
    client_record: bool,
    record_incoming: bool,
    last_portable_service_running: bool,
    source: VideoSource,
    display_idx: usize,
) -> ResultType<(
    Encoder,
    EncoderCfg,
    CodecFormat,
    bool,
    Arc<Mutex<Option<Recorder>>>,
)> {
    let encoder_cfg = get_encoder_config(
        &c,
        name.to_string(),
        quality,
        client_record || record_incoming,
        last_portable_service_running,
        source,
    );
    Encoder::set_fallback(&encoder_cfg);
    let codec_format = Encoder::negotiated_codec();
    let recorder = get_recorder(record_incoming, display_idx, source == VideoSource::Camera);
    let use_i444 = Encoder::use_i444(&encoder_cfg);
    let encoder = Encoder::new(encoder_cfg.clone(), use_i444)?;
    Ok((encoder, encoder_cfg, codec_format, use_i444, recorder))
}

// The encoder's configured kbps for a ratio, as its `set_quality` computes it, for the fast
// bitrate estimators' conversions (roadmap M16): hwcodec RAM and VRAM encoders are not linear
// in the ratio. None (vpx, aom): linear. QoS checks the curve against the stored bitrate, so a
// fallback encoder with another curve is not misread.
fn bitrate_curve(cfg: &EncoderCfg) -> Option<super::video_qos::BitrateCurve> {
    match cfg {
        #[cfg(feature = "hwcodec")]
        EncoderCfg::HWRAM(c) => {
            let (name, width, height) = (c.name.clone(), c.width, c.height);
            Some(Box::new(move |r| HwRamEncoder::bitrate(&name, width, height, r)))
        }
        #[cfg(feature = "vram")]
        EncoderCfg::VRAM(c) => {
            let (format, width, height) = (c.feature.data_format, c.width, c.height);
            Some(Box::new(move |r| VRamEncoder::bitrate(format, width, height, r)))
        }
        _ => None,
    }
}

fn get_encoder_config(
    c: &CapturerInfo,
    _name: String,
    quality: f32,
    record: bool,
    _portable_service: bool,
    _source: VideoSource,
) -> EncoderCfg {
    #[cfg(all(windows, feature = "vram"))]
    if _portable_service || c.is_gdi() || _source == VideoSource::Camera {
        log::info!("gdi:{}, portable:{}", c.is_gdi(), _portable_service);
        VRamEncoder::set_not_use(_name, true);
    }
    #[cfg(feature = "vram")]
    Encoder::update(scrap::codec::EncodingUpdate::Check);
    // https://www.wowza.com/community/t/the-correct-keyframe-interval-in-obs-studio/95162
    let keyframe_interval = if record { Some(240) } else { None };
    let negotiated_codec = Encoder::negotiated_codec();
    match negotiated_codec {
        CodecFormat::H264 | CodecFormat::H265 => {
            #[cfg(feature = "vram")]
            if let Some(feature) = VRamEncoder::try_get(&c.device(), negotiated_codec) {
                return EncoderCfg::VRAM(VRamEncoderConfig {
                    device: c.device(),
                    width: c.width,
                    height: c.height,
                    quality,
                    feature,
                    keyframe_interval,
                });
            }
            #[cfg(feature = "hwcodec")]
            if let Some(hw) = HwRamEncoder::try_get(negotiated_codec) {
                return EncoderCfg::HWRAM(HwRamEncoderConfig {
                    name: hw.name,
                    mc_name: hw.mc_name,
                    width: c.width,
                    height: c.height,
                    quality,
                    keyframe_interval,
                });
            }
            EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval,
            })
        }
        format @ (CodecFormat::VP8 | CodecFormat::VP9) => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: if format == CodecFormat::VP8 {
                VpxVideoCodecId::VP8
            } else {
                VpxVideoCodecId::VP9
            },
            keyframe_interval,
        }),
        CodecFormat::AV1 => EncoderCfg::AOM(AomEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            keyframe_interval,
        }),
        _ => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: VpxVideoCodecId::VP9,
            keyframe_interval,
        }),
    }
}

fn get_recorder(
    record_incoming: bool,
    display_idx: usize,
    camera: bool,
) -> Arc<Mutex<Option<Recorder>>> {
    #[cfg(windows)]
    let root = crate::platform::is_root();
    #[cfg(not(windows))]
    let root = false;
    let recorder = if record_incoming {
        use crate::hbbs_http::record_upload;

        let tx = if record_upload::is_enable() {
            let (tx, rx) = std::sync::mpsc::channel();
            record_upload::run(rx);
            Some(tx)
        } else {
            None
        };
        Recorder::new(RecorderContext {
            server: true,
            id: Config::get_id(),
            dir: crate::ui_interface::video_save_directory(root),
            display_idx,
            camera,
            tx,
        })
        .map_or(Default::default(), |r| Arc::new(Mutex::new(Some(r))))
    } else {
        Default::default()
    };

    recorder
}

#[cfg(target_os = "android")]
fn check_change_scale(hardware: bool) -> ResultType<()> {
    use base::config::keys::OPTION_ENABLE_ANDROID_SOFTWARE_ENCODING_HALF_SCALE as SCALE_SOFT;

    // isStart flag is set at the end of startCapture() in Android, wait it to be set.
    let n = 60; // 3s
    for i in 0..n {
        if scrap::is_start() == Some(true) {
            log::info!("start flag is set");
            break;
        }
        log::info!("wait for start, {i}");
        std::thread::sleep(Duration::from_millis(50));
        if i == n - 1 {
            log::error!("wait for start timeout");
        }
    }
    let screen_size = scrap::screen_size();
    let scale_soft = hbb_common::config::option2bool(SCALE_SOFT, &Config::get_option(SCALE_SOFT));
    let half_scale = !hardware && scale_soft;
    log::info!("hardware: {hardware}, scale_soft: {scale_soft}, screen_size: {screen_size:?}",);
    scrap::android::call_main_service_set_by_name(
        "half_scale",
        Some(half_scale.to_string().as_str()),
        None,
    )
    .ok();
    let old_scale = screen_size.2;
    let new_scale = scrap::screen_size().2;
    log::info!("old_scale: {old_scale}, new_scale: {new_scale}");
    if old_scale != new_scale {
        log::info!("switch due to scale changed, {old_scale} -> {new_scale}");
        // switch is not a must, but it is better to do so.
        bail_switch!("scale");
    }
    Ok(())
}

fn check_privacy_mode_changed(
    sp: &GenericService,
    display_idx: usize,
    ci: &CapturerInfo,
) -> ResultType<()> {
    let privacy_mode_id_2 = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    if ci.privacy_mode_id != privacy_mode_id_2 {
        if privacy_mode_id_2 != INVALID_PRIVACY_MODE_CONN_ID {
            let msg_out = crate::common::make_privacy_mode_msg(
                back_notification::PrivacyModeState::PrvOnByOther,
                "".to_owned(),
            );
            sp.send_to_others(msg_out, privacy_mode_id_2);
        }
        log::info!("switch due to privacy mode changed");
        try_broadcast_display_changed(&sp, display_idx, ci, true).ok();
        bail_switch!("privacy_mode");
    }
    Ok(())
}

#[inline]
fn handle_one_frame(
    display: usize,
    sp: &GenericService,
    frame: EncodeInput,
    ms: i64,
    encoder: &mut Encoder,
    recorder: Arc<Mutex<Option<Recorder>>>,
    encode_fail_counter: &mut usize,
    first_frame: &mut bool,
    width: usize,
    height: usize,
    latency_stats: &mut super::latency_stats::LatencyStats,
) -> ResultType<(HashSet<i32>, Option<Encoded>)> {
    check_new_subscriber(sp)?;

    let mut send_conn_ids: HashSet<i32> = Default::default();
    let mut encoded = None;
    let first = *first_frame;
    *first_frame = false;
    match encoder.encode_to_message(frame, ms) {
        Ok(vf) => {
            *encode_fail_counter = 0;
            let (ids, info) =
                send_encoded(display, sp, vf, &recorder, width, height, latency_stats);
            send_conn_ids = ids;
            encoded = Some(info);
        }
        Err(e) => {
            *encode_fail_counter += 1;
            // Encoding errors are not frequent except on Android
            if !cfg!(target_os = "android") {
                log::error!("encode fail: {e:?}, times: {}", *encode_fail_counter,);
            }
            let max_fail_times = if cfg!(target_os = "android") && encoder.is_hardware() {
                9
            } else {
                3
            };
            let repeat = !encoder.latency_free();
            // repeat encoders can reach max_fail_times on the first frame
            if (first && !repeat) || *encode_fail_counter >= max_fail_times {
                *encode_fail_counter = 0;
                if encoder.is_hardware() {
                    encoder.disable();
                    log::error!("switch due to encoding fails, first frame: {first}, error: {e:?}");
                    bail_switch!("encode_fail");
                }
            }
            match e.to_string().as_str() {
                scrap::codec::ENCODE_NEED_SWITCH => {
                    encoder.disable();
                    log::error!("switch due to encoder need switch");
                    bail_switch!("encoder_need_switch");
                }
                _ => {}
            }
        }
    }
    Ok((send_conn_ids, encoded))
}

fn check_new_subscriber(sp: &GenericService) -> ResultType<()> {
    sp.snapshot(|sps| {
        // so that new sub and old sub share the same encoder after switch
        if sps.has_subscribes() {
            log::info!("switch due to new subscriber");
            bail_switch!("new_subscriber");
        }
        Ok(())
    })
}

// Records and sends one encoded frame to the display's connections.
fn send_encoded(
    display: usize,
    sp: &GenericService,
    mut vf: VideoFrame,
    recorder: &Arc<Mutex<Option<Recorder>>>,
    width: usize,
    height: usize,
    latency_stats: &mut super::latency_stats::LatencyStats,
) -> (HashSet<i32>, Encoded) {
    vf.display = display as _;
    let mut msg = Message::new();
    msg.set_video_frame(vf);
    let mut encoded = Encoded::default();
    for f in super::latency_stats::encoded_frames(&msg).unwrap_or_default() {
        encoded.key |= f.key;
        encoded.bytes += f.data.len();
    }
    recorder
        .lock()
        .unwrap()
        .as_mut()
        .map(|r| r.write_message(&msg, width, height));
    latency_stats.on_encoded(&msg);
    let send_conn_ids = sp.send_video_frame(msg);
    if !send_conn_ids.is_empty() {
        latency_stats.on_sent();
    }
    (send_conn_ids, encoded)
}

/// Encodes the kept last image again without a new capture: VRAM re-encodes its NV12 surface,
/// YUV encoders the kept YUV buffer; a pending forced key frame applies. Errors never count as
/// encode failures and never disable the encoder: the caller restarts or stops refining.
#[cfg(windows)]
fn encode_kept_frame(
    display: usize,
    sp: &GenericService,
    encoder: &mut Encoder,
    yuv: &[u8],
    ms: i64,
    recorder: &Arc<Mutex<Option<Recorder>>>,
    width: usize,
    height: usize,
    latency_stats: &mut super::latency_stats::LatencyStats,
) -> ResultType<(HashSet<i32>, Encoded)> {
    #[cfg(feature = "vram")]
    let texture = encoder.input_texture();
    #[cfg(not(feature = "vram"))]
    let texture = false;
    let vf = if texture {
        encoder.encode_last(ms)?
    } else if !yuv.is_empty() {
        encoder.encode_to_message(EncodeInput::YUV(yuv), ms)?
    } else {
        bail!("no kept image");
    };
    Ok(send_encoded(
        display,
        sp,
        vf,
        recorder,
        width,
        height,
        latency_stats,
    ))
}

#[cfg(windows)]
fn refresh_keyframe_enabled() -> bool {
    Config::get_option(base::config::keys::OPTION_ENABLE_REFRESH_KEYFRAME) == "Y"
}

#[cfg(windows)]
fn request_refresh_key(
    encoder: &mut Encoder,
    key_refresh: &mut super::key_refresh::KeyRefresh,
) -> ResultType<()> {
    if !encoder.request_keyframe() {
        log::info!("refresh: this encoder has no forced key frame, switch");
        bail_switch!("refresh_unsupported");
    }
    key_refresh.on_requested(Instant::now());
    Ok(())
}

#[inline]
pub fn refresh() {
    #[cfg(target_os = "android")]
    Display::refresh_size();
}

#[cfg(windows)]
fn start_uac_elevation_check() {
    static START: Once = Once::new();
    START.call_once(|| {
        if !crate::platform::is_installed() && !crate::platform::is_root() {
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Ok(uac) = is_process_consent_running() {
                    *IS_UAC_RUNNING.lock().unwrap() = uac;
                }
                if !crate::platform::is_elevated(None).unwrap_or(false) {
                    if let Ok(elevated) = crate::platform::is_foreground_window_elevated() {
                        *IS_FOREGROUND_WINDOW_ELEVATED.lock().unwrap() = elevated;
                    }
                }
            });
        }
    });
}

#[inline]
fn try_broadcast_display_changed(
    sp: &GenericService,
    display_idx: usize,
    cap: &CapturerInfo,
    refresh: bool,
) -> ResultType<()> {
    if refresh {
        // Get display information immediately.
        crate::display_service::check_displays_changed().ok();
    }
    if let Some(display) = check_display_changed(
        cap.ndisplay,
        cap.current,
        (cap.origin.0, cap.origin.1, cap.width, cap.height),
    ) {
        log::info!("Display {} changed", display);
        if let Some(msg_out) =
            make_display_changed_msg(display_idx, Some(display), VideoSource::Monitor)
        {
            let msg_out = Arc::new(msg_out);
            sp.send_shared(msg_out.clone());
            // switch display may occur before the first video frame, add snapshot to send to new subscribers
            sp.snapshot(move |sps| {
                sps.send_shared(msg_out.clone());
                Ok(())
            })?;
            bail_switch!("display_changed");
        }
    }
    Ok(())
}

pub fn make_display_changed_msg(
    display_idx: usize,
    opt_display: Option<DisplayInfo>,
    source: VideoSource,
) -> Option<Message> {
    let display = match opt_display {
        Some(d) => d,
        None => match source {
            VideoSource::Monitor => display_service::get_display_info(display_idx)?,
            VideoSource::Camera => camera::Cameras::get_sync_cameras()
                .get(display_idx)?
                .clone(),
        },
    };
    let mut misc = Misc::new();
    misc.set_switch_display(SwitchDisplay {
        display: display_idx as _,
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
        cursor_embedded: match source {
            VideoSource::Monitor => display_service::capture_cursor_embedded(),
            VideoSource::Camera => false,
        },
        #[cfg(not(target_os = "android"))]
        resolutions: Some(SupportedResolutions {
            resolutions: match source {
                VideoSource::Monitor => {
                    if display.name.is_empty() {
                        vec![]
                    } else {
                        crate::platform::resolutions(&display.name)
                    }
                }
                VideoSource::Camera => camera::Cameras::get_camera_resolution(display_idx)
                    .ok()
                    .into_iter()
                    .collect(),
            },
            ..SupportedResolutions::default()
        })
        .into(),
        original_resolution: display.original_resolution,
        ..Default::default()
    });
    let mut msg_out = Message::new();
    msg_out.set_misc(misc);
    Some(msg_out)
}

fn check_qos(
    encoder: &mut Encoder,
    ratio: &mut f32,
    spf: &mut Duration,
    client_record: bool,
    send_counter: &mut usize,
    second_instant: &mut Instant,
    name: &str,
    #[cfg(windows)] virtual_fps: &mut Option<super::virtual_fps::VirtualFps>,
    latency_stats: &mut super::latency_stats::LatencyStats,
) -> ResultType<()> {
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    *spf = video_qos.spf();
    let fps_cap = video_qos.fps();
    let mut measured_fps = None;
    // Fast mode (roadmap M16): a cut the encoder takes now, for its reaction time.
    let mut fast_cut = None;
    if *ratio != video_qos.ratio() {
        *ratio = video_qos.ratio();
        if encoder.support_changing_quality() {
            allow_err!(encoder.set_quality(*ratio));
            video_qos.store_bitrate_at(encoder.bitrate(), *ratio);
            fast_cut = video_qos.take_fast_cut();
        } else {
            // Now only vaapi doesn't support changing quality
            if !video_qos.in_vbr_state() && !video_qos.latest_quality().is_custom() {
                log::info!("switch to change quality");
                bail_switch!("quality");
            }
        }
    }
    if client_record != video_qos.record() {
        log::info!("switch due to record changed");
        bail_switch!("record");
    }
    let window = second_instant.elapsed();
    if window > Duration::from_secs(1) {
        measured_fps = Some(*send_counter as f64 / window.as_secs_f64());
        *second_instant = Instant::now();
        video_qos.update_display_data(&name, *send_counter);
        *send_counter = 0;
    }
    drop(video_qos);
    if let Some((conn, onset, unlogged)) = fast_cut {
        log::info!(
            "FASTABR applied conn={} display={} ratio={:.3} kbps={} reaction_ms={} unlogged={}",
            conn,
            name,
            *ratio,
            encoder.bitrate(),
            onset.elapsed().as_millis(),
            unlogged
        );
    }
    // Virtual framerate, after the lock: the NVENC SDK driver reconfigures the encoder here.
    #[cfg(windows)]
    if let Some(vf) = virtual_fps.as_mut() {
        if let Some(fps) = vf.next(fps_cap, measured_fps) {
            let first = vf.applied() == 0;
            if encoder.set_fps(fps) {
                vf.on_applied(fps);
                latency_stats.on_virtual_fps(fps);
                if first {
                    log::info!("virtual fps: {name} encoder runs at {fps} fps (cap {fps_cap})");
                }
            } else {
                // Back to neutral before giving up: at the nominal rate FFmpeg nvenc's scale is 1
                // and the NVENC SDK driver runs at the rate it was opened with, i.e. "off".
                let applied = vf.applied();
                let nominal = scrap::codec::nvenc_nominal_fps();
                if applied == 0 || applied == nominal || (nominal > 0 && encoder.set_fps(nominal)) {
                    log::warn!("virtual fps: {name} encoder refused {fps} fps, off until restart");
                    latency_stats.on_virtual_fps(0);
                } else {
                    // `vfps` keeps reporting the rate the encoder is stuck at.
                    log::warn!(
                        "virtual fps: {name} encoder refused {fps} fps and the reset to {nominal}, stays at {applied} fps until restart"
                    );
                }
                *virtual_fps = None;
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (fps_cap, measured_fps, latency_stats);
    Ok(())
}

pub fn set_take_screenshot(source: VideoSource, display_idx: usize, sid: String, tx: Sender) {
    SCREENSHOTS.lock().unwrap().insert(
        (source, display_idx),
        Screenshot {
            sid,
            tx,
            restore_vram: false,
        },
    );
}

// We need to this function, because the `stride` may be larger than `width * 4`.
fn get_rgba_from_pixelbuf<'a>(pixbuf: &scrap::PixelBuffer<'a>) -> ResultType<Vec<u8>> {
    let w = pixbuf.width();
    let h = pixbuf.height();
    let stride = pixbuf.stride();
    let Some(s) = stride.get(0) else {
        bail!("Invalid pixel buf stride.")
    };

    if *s == w * 4 {
        let mut rgba = vec![];
        scrap::convert(pixbuf, scrap::Pixfmt::RGBA, &mut rgba)?;
        Ok(rgba)
    } else {
        let bgra = pixbuf.data();
        let mut bit_flipped = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let i = s * y + 4 * x;
                bit_flipped.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i], bgra[i + 3]]);
            }
        }
        Ok(bit_flipped)
    }
}

fn handle_screenshot(screenshot: Screenshot, msg: String, w: usize, h: usize, data: Vec<u8>) {
    let mut response = ScreenshotResponse::new();
    response.sid = screenshot.sid;
    if msg.is_empty() {
        if data.is_empty() {
            response.msg = "Failed to take screenshot, please try again later.".to_owned();
        } else {
            fn encode_png(width: usize, height: usize, rgba: Vec<u8>) -> ResultType<Vec<u8>> {
                let mut png = Vec::new();
                let mut encoder =
                    repng::Options::smallest(width as _, height as _).build(&mut png)?;
                encoder.write(&rgba)?;
                encoder.finish()?;
                Ok(png)
            }
            match encode_png(w as _, h as _, data) {
                Ok(png) => {
                    response.data = png.into();
                }
                Err(e) => {
                    response.msg = format!("Error encoding png: {}", e);
                }
            }
        }
    } else {
        response.msg = msg;
    }
    let mut msg_out = Message::new();
    msg_out.set_screenshot_response(response);
    if let Err(e) = screenshot
        .tx
        .send((hbb_common::tokio::time::Instant::now(), Arc::new(msg_out)))
    {
        log::error!("Failed to send screenshot, {}", e);
    }
}
