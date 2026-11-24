use base::platform::windows::is_windows_version_or_greater;
use hbb_common::{bail, config::Config, lazy_static, log, ResultType};
use std::sync::{Mutex, MutexGuard};

// This string is defined here.
//  https://github.com/rustdesk-org/RustDeskIddDriver/blob/b370aad3f50028b039aad211df60c8051c4a64d6/RustDeskIddDriver/RustDeskIddDriver.inf#LL73C1-L73C40
pub const RUSTDESK_IDD_DEVICE_STRING: &'static str = "RustDeskIddDriver Device\0";
pub const AMYUNI_IDD_DEVICE_STRING: &'static str = "USB Mobile Monitor Virtual Display\0";

const IDD_IMPL_RUSTDESK: &str = "rustdesk_idd";
const IDD_IMPL_AMYUNI: &str = "amyuni_idd";
/// Fork: "virtual-display-impl" asks for the RustDesk IDD but its driver is not installed.
const IDD_IMPL_NONE: &str = "none";
pub const NO_RUSTDESK_IDD: &str =
    "The RustDesk virtual display driver is not installed (virtual-display-impl = rustdesk)";
const IDD_PLUG_OUT_ALL_INDEX: i32 = -1;

/// Fork: host option choosing the virtual display backend: "amyuni" (default, unset) or
/// "rustdesk" (the RustDesk indirect display driver, which must already be installed).
pub const OPTION_VIRTUAL_DISPLAY_IMPL: &str = "virtual-display-impl";
/// Fork: refresh rate of new RustDesk IDD displays (60/72/90/120/144, default 90).
pub const OPTION_VIRTUAL_DISPLAY_HZ: &str = "virtual-display-hz";
const DEFAULT_VIRTUAL_DISPLAY_HZ: u32 = 90;
const ALLOWED_VIRTUAL_DISPLAY_HZ: [u32; 5] = [60, 72, 90, 120, 144];
/// Sizes every RustDesk IDD display offers besides the requested one.
const STANDARD_SIZES: [(u32, u32); 2] = [(1920, 1080), (2560, 1440)];

lazy_static::lazy_static! {
    // The backend in effect. It is chosen again from the option only while the backend in
    // effect has no displays of ours, so a running session never has its backend switched.
    static ref SELECTED_IMPL: Mutex<Option<&'static str>> = Mutex::new(None);
}

#[inline]
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn wanted_impl() -> &'static str {
    match Config::get_option(OPTION_VIRTUAL_DISPLAY_IMPL)
        .trim()
        .to_lowercase()
        .as_str()
    {
        "rustdesk" | "rustdesk_idd" | "rustdesk-idd" => IDD_IMPL_RUSTDESK,
        _ => IDD_IMPL_AMYUNI,
    }
}

/// The virtual display backend in effect: the option. Fork: "rustdesk" without a
/// RustDeskIddDriver package in the driver store gives no backend (IDD_IMPL_NONE) instead of
/// falling back to Amyuni, whose displays are 60 Hz only and cannot be plugged out one by one.
fn idd_impl() -> &'static str {
    let mut selected = lock(&SELECTED_IMPL);
    if let Some(cur) = *selected {
        let in_use = match cur {
            IDD_IMPL_RUSTDESK => rustdesk_idd::has_displays(),
            IDD_IMPL_AMYUNI => amyuni_idd::plugged_count() > 0,
            _ => false,
        };
        if in_use {
            return cur;
        }
    }
    let mut next = wanted_impl();
    if next == IDD_IMPL_RUSTDESK && !driver_store::has_rustdesk_idd_package() {
        next = IDD_IMPL_NONE;
    }
    if *selected != Some(next) {
        log::info!(
            "LATENCY vdisplay backend: {} (option {}={:?}, {} Hz)",
            next,
            OPTION_VIRTUAL_DISPLAY_IMPL,
            Config::get_option(OPTION_VIRTUAL_DISPLAY_IMPL),
            virtual_display_hz()
        );
        *selected = Some(next);
    }
    next
}

pub fn is_amyuni_idd() -> bool {
    idd_impl() == IDD_IMPL_AMYUNI
}

/// Fork: the RustDesk IDD backend is in effect.
pub fn is_rustdesk_idd() -> bool {
    idd_impl() == IDD_IMPL_RUSTDESK
}

/// Fork: "virtual-display-impl" = rustdesk, but its driver is not installed: no backend, so no
/// virtual display can be plugged in at all.
pub fn is_no_idd() -> bool {
    idd_impl() == IDD_IMPL_NONE
}

/// Fork: a backend is in effect, so a virtual display can be plugged in for a PC without one.
pub fn can_plug_headless() -> bool {
    !is_no_idd()
}

/// Fork: `name` ("\\.\DISPLAYn") is an active display of the RustDesk IDD or the Amyuni driver,
/// told by the DeviceString Windows reports for it (not by the names this process remembers,
/// which may be unknown for a display whose name could not be resolved at plug-in time).
pub fn is_virtual_device(name: &str) -> bool {
    windows::get_device_names(Some(RUSTDESK_IDD_DEVICE_STRING))
        .iter()
        .chain(windows::get_device_names(Some(AMYUNI_IDD_DEVICE_STRING)).iter())
        .any(|s| windows::is_device_name(s, name))
}

/// Fork: refresh rate for new RustDesk IDD displays, from `OPTION_VIRTUAL_DISPLAY_HZ`.
pub fn virtual_display_hz() -> u32 {
    Config::get_option(OPTION_VIRTUAL_DISPLAY_HZ)
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|hz| ALLOWED_VIRTUAL_DISPLAY_HZ.contains(hz))
        .unwrap_or(DEFAULT_VIRTUAL_DISPLAY_HZ)
}

/// Fork: the modes a RustDesk IDD display offers: `width` x `height` first, then 1920x1080 and
/// 2560x1440, each at `hz`, 72 and 60 Hz, without duplicates. The first one is the wanted mode.
pub fn display_modes(width: u32, height: u32, hz: u32) -> Vec<virtual_display::MonitorMode> {
    let mut modes: Vec<virtual_display::MonitorMode> = Vec::new();
    let sizes = std::iter::once((width, height)).chain(STANDARD_SIZES.iter().copied());
    for (w, h) in sizes {
        if w == 0 || h == 0 {
            continue;
        }
        for sync in [hz, 72, 60] {
            let dup = modes
                .iter()
                .any(|m| m.width as u32 == w && m.height as u32 == h && m.sync as u32 == sync);
            if !dup {
                modes.push(virtual_display::MonitorMode {
                    width: w as _,
                    height: h as _,
                    sync: sync as _,
                });
            }
        }
    }
    modes
}

pub fn get_cur_device_string() -> &'static str {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => RUSTDESK_IDD_DEVICE_STRING,
        IDD_IMPL_AMYUNI => AMYUNI_IDD_DEVICE_STRING,
        _ => "",
    }
}

pub fn is_virtual_display_supported() -> bool {
    #[cfg(target_os = "windows")]
    {
        is_windows_version_or_greater(10, 0, 19041, 0, 0)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

pub fn plug_in_headless() -> ResultType<()> {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => rustdesk_idd::plug_in_headless(),
        IDD_IMPL_AMYUNI => amyuni_idd::plug_in_headless(),
        IDD_IMPL_NONE => bail!(NO_RUSTDESK_IDD),
        _ => bail!("Unsupported virtual display implementation."),
    }
}

pub fn get_platform_additions() -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    if !crate::platform::windows::is_self_service_running() {
        return map;
    }
    let idd_impl = idd_impl();
    map.insert("idd_impl".into(), serde_json::json!(idd_impl));
    match idd_impl {
        IDD_IMPL_RUSTDESK => {
            // Fork: the plugged indices in the order they were plugged in, the Windows device
            // name of each (so a viewer maps DisplayInfo.name to the index), the refresh rate,
            // and that ToggleVirtualDisplay(on: false, display: idx) unplugs exactly idx.
            let (indices, names) = rustdesk_idd::get_virtual_displays_and_names();
            map.insert(
                "rustdesk_virtual_displays".into(),
                serde_json::json!(indices),
            );
            let names: serde_json::Map<String, serde_json::Value> = names
                .into_iter()
                .map(|(idx, name)| (idx.to_string(), serde_json::json!(name)))
                .collect();
            map.insert(
                "virtual_display_names".into(),
                serde_json::Value::Object(names),
            );
            map.insert(
                "virtual_display_hz".into(),
                serde_json::json!(virtual_display_hz()),
            );
            map.insert(
                "virtual_display_targeted_unplug".into(),
                serde_json::json!(true),
            );
            // Fork: the headless display that stands in for a missing monitor (index 0, not in
            // rustdesk_virtual_displays, plugged out by the host itself once a monitor is back).
            if let Some(name) = rustdesk_idd::headless_name() {
                map.insert(
                    "rustdesk_headless_display".into(),
                    serde_json::json!(name),
                );
            }
        }
        IDD_IMPL_AMYUNI => {
            let c = amyuni_idd::get_monitor_count();
            if c > 0 {
                map.insert("amyuni_virtual_displays".into(), serde_json::json!(c));
            }
        }
        _ => {}
    }
    map
}

/// Fork: (virtual displays of either driver, all active displays), for the cursor hider.
pub fn virtual_display_counts() -> (usize, usize) {
    (
        windows::get_device_names(Some(AMYUNI_IDD_DEVICE_STRING)).len()
            + windows::get_device_names(Some(RUSTDESK_IDD_DEVICE_STRING)).len(),
        windows::get_device_names(None).len(),
    )
}

#[inline]
pub fn plug_in_monitor(idx: u32, modes: Vec<virtual_display::MonitorMode>) -> ResultType<()> {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => rustdesk_idd::plug_in_index_modes(idx, modes),
        IDD_IMPL_AMYUNI => amyuni_idd::plug_in_monitor(),
        IDD_IMPL_NONE => bail!(NO_RUSTDESK_IDD),
        _ => bail!("Unsupported virtual display implementation."),
    }
}

pub fn plug_out_monitor(index: i32, force_all: bool, force_one: bool) -> ResultType<()> {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => {
            let indices = if index == IDD_PLUG_OUT_ALL_INDEX {
                rustdesk_idd::get_virtual_displays()
            } else {
                vec![index as _]
            };
            rustdesk_idd::plug_out_peer_request(&indices)
        }
        IDD_IMPL_AMYUNI => amyuni_idd::plug_out_monitor(index, force_all, force_one),
        IDD_IMPL_NONE => bail!(NO_RUSTDESK_IDD),
        _ => bail!("Unsupported virtual display implementation."),
    }
}

pub fn plug_in_peer_request(modes: Vec<Vec<virtual_display::MonitorMode>>) -> ResultType<Vec<u32>> {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => rustdesk_idd::plug_in_peer_request(modes),
        IDD_IMPL_AMYUNI => {
            amyuni_idd::plug_in_monitor()?;
            Ok(vec![0])
        }
        IDD_IMPL_NONE => bail!(NO_RUSTDESK_IDD),
        _ => bail!("Unsupported virtual display implementation."),
    }
}

pub fn plug_out_monitor_indices(
    indices: &[u32],
    force_all: bool,
    force_one: bool,
) -> ResultType<()> {
    match idd_impl() {
        IDD_IMPL_RUSTDESK => rustdesk_idd::plug_out_peer_request(indices),
        IDD_IMPL_AMYUNI => {
            for _idx in indices.iter() {
                amyuni_idd::plug_out_monitor(0, force_all, force_one)?;
            }
            Ok(())
        }
        IDD_IMPL_NONE => bail!(NO_RUSTDESK_IDD),
        _ => bail!("Unsupported virtual display implementation."),
    }
}

pub fn reset_all() -> ResultType<()> {
    let res = match idd_impl() {
        IDD_IMPL_RUSTDESK | IDD_IMPL_NONE => Ok(()),
        IDD_IMPL_AMYUNI => amyuni_idd::reset_all(),
        _ => bail!("Unsupported virtual display implementation."),
    };
    // Fork: RustDesk IDD displays always go, after any plug request still queued, so that a
    // request that finishes after the last connection ended does not leave its display behind.
    rustdesk_idd::reset_all_queued();
    res
}

/// Fork: whether our RustDesk IDD driver package is installed. The service never installs or
/// updates it (an INSTALLFLAG_FORCE install would replace a locally built, signed driver).
mod driver_store {
    use super::{lock, windows};
    use hbb_common::{lazy_static, log};
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    // A missing package is looked for again after this long (it may be installed meanwhile).
    // Fork: a package found stays found for the life of the process (no rescan of every
    // oem*.inf every 5 minutes); a removed one makes the next plug-in fail with its error.
    const RECHECK_MISSING: Duration = Duration::from_secs(30);

    lazy_static::lazy_static! {
        static ref CHECKED: Mutex<Option<(bool, Instant)>> = Mutex::new(None);
    }

    pub(super) fn has_rustdesk_idd_package() -> bool {
        let mut checked = lock(&CHECKED);
        if let Some((found, at)) = *checked {
            if found || at.elapsed() < RECHECK_MISSING {
                return found;
            }
        }
        let found = if let Some(inf) = find_oem_inf() {
            log::info!(
                "LATENCY vdisplay driver: RustDeskIddDriver package {} is in the driver store",
                inf
            );
            true
        } else if windows::has_display_device(super::RUSTDESK_IDD_DEVICE_STRING) {
            log::info!(
                "LATENCY vdisplay driver: a RustDeskIddDriver Device is bound to its driver"
            );
            true
        } else {
            log::warn!(
                "LATENCY vdisplay driver: no RustDeskIddDriver package in the driver store; \
                 no virtual displays with virtual-display-impl=rustdesk (the service never \
                 installs the driver, install it by hand)"
            );
            false
        };
        *checked = Some((found, Instant::now()));
        found
    }

    /// The published name (oemNN.inf) of an INF in %SystemRoot%\INF that installs
    /// RustDeskIddDriver.dll.
    fn find_oem_inf() -> Option<String> {
        let root = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("windir"))
            .unwrap_or_else(|| "C:\\Windows".into());
        let dir = std::path::PathBuf::from(root).join("INF");
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !(name.starts_with("oem") && name.ends_with(".inf")) {
                continue;
            }
            if entry.metadata().map_or(true, |m| m.len() > 4 * 1024 * 1024) {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if decode(&bytes)
                .to_lowercase()
                .contains("rustdeskidddriver.dll")
            {
                return Some(name);
            }
        }
        None
    }

    fn decode(bytes: &[u8]) -> String {
        let utf16 = |be: bool| {
            let v: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|c| {
                    if be {
                        u16::from_be_bytes([c[0], c[1]])
                    } else {
                        u16::from_le_bytes([c[0], c[1]])
                    }
                })
                .collect();
            String::from_utf16_lossy(&v)
        };
        match bytes {
            [0xFF, 0xFE, ..] => utf16(false),
            [0xFE, 0xFF, ..] => utf16(true),
            _ => String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

pub mod rustdesk_idd {
    use super::{lock, windows};
    use hbb_common::{bail, lazy_static, log, ResultType};
    use std::{
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc, Mutex,
        },
        time::{Duration, Instant},
    };

    // virtual display index range: 0 - 2 are reserved for headless and other special uses.
    const VIRTUAL_DISPLAY_INDEX_FOR_HEADLESS: u32 = 0;
    const VIRTUAL_DISPLAY_START_FOR_PEER: u32 = 1;
    // Peer displays use the indices START_FOR_PEER..MAX_COUNT (1..=4).
    const VIRTUAL_DISPLAY_MAX_COUNT: u32 = 5;
    // How long a new display may take to show up on the desktop.
    const DEVICE_NAME_WAIT: Duration = Duration::from_secs(3);
    // How long Windows may take to offer a mode just sent to the driver.
    const MODE_APPLY_TRIES: usize = 15;
    const MODE_APPLY_STEP: Duration = Duration::from_millis(200);

    struct Plugged {
        // Windows device name ("\\.\DISPLAYnn", as EnumDisplayDevicesW gives it); None while
        // it could not be told apart (never stored as "").
        name: Option<String>,
        // Refresh rate the display was plugged in with; kept on resolution changes.
        hz: u32,
        // The mode to set once the name is known, when it was not known at plug-in time.
        mode_pending: Option<virtual_display::MonitorMode>,
    }

    #[derive(Default)]
    struct State {
        headless: Option<(u32, Plugged)>,
        peers: HashMap<u32, Plugged>,
        // Peer indices in the order they were plugged in (newest last).
        order: Vec<u32>,
    }

    type Job = Box<dyn FnOnce() + Send + 'static>;

    lazy_static::lazy_static! {
        // Short-lived lock: never held across driver calls or waits.
        static ref STATE: Mutex<State> = Default::default();
        // Serialises the driver calls that change the set of displays.
        static ref PLUG_LOCK: Mutex<()> = Mutex::new(());
        // One worker runs the viewers' requests in the order they came.
        static ref WORKER: Mutex<Option<mpsc::Sender<Job>>> = Mutex::new(None);
    }
    // Requests queued for the worker and not finished yet.
    static PENDING: AtomicUsize = AtomicUsize::new(0);

    /// Displays of ours exist, or requests that may plug some in are still queued.
    pub(super) fn has_displays() -> bool {
        if PENDING.load(Ordering::SeqCst) > 0 {
            return true;
        }
        let state = lock(&STATE);
        state.headless.is_some() || !state.peers.is_empty()
    }

    fn run_queued(job: Job) {
        let mut worker = lock(&WORKER);
        let job = match worker.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("virtual-display".to_owned())
            .spawn(move || {
                for job in rx {
                    job();
                }
            });
        match spawned {
            Ok(_) => {
                let _ = tx.send(job);
                *worker = Some(tx);
            }
            Err(e) => {
                log::error!("LATENCY vdisplay: failed to start the worker thread: {}", e);
                *worker = None;
                drop(worker);
                job();
            }
        }
    }

    /// The RustDeskIddDriver package must already be installed; the software device that loads
    /// it is created on first use (it lives as long as this process).
    fn prepare_device() -> ResultType<()> {
        if !super::driver_store::has_rustdesk_idd_package() {
            bail!("RustDeskIddDriver is not installed (no package in the driver store)");
        }
        if virtual_display::is_device_created() {
            return Ok(());
        }
        let started = Instant::now();
        if let Err(e) = virtual_display::create_device() {
            if !e.to_string().contains("Device is already created") {
                log::error!("LATENCY vdisplay device: create failed: {}", e);
                bail!("Create device failed {}", e);
            }
        }
        log::info!(
            "LATENCY vdisplay device: created in {} ms",
            started.elapsed().as_millis()
        );
        Ok(())
    }

    /// Fork: `--install-idd` only. Never forces the bundled INF over an installed package.
    pub fn install_update_driver() -> ResultType<()> {
        if super::driver_store::has_rustdesk_idd_package() {
            log::info!("LATENCY vdisplay driver: already installed, not reinstalling it");
            return Ok(());
        }
        if let Err(e) = virtual_display::create_device() {
            if !e.to_string().contains("Device is already created") {
                bail!("Create device failed {}", e);
            }
        }
        // Reboot is not required for this case.
        let mut _reboot_required = false;
        virtual_display::install_update_driver(&mut _reboot_required)?;
        Ok(())
    }

    #[inline]
    fn get_device_names() -> Vec<String> {
        windows::get_device_names(Some(super::RUSTDESK_IDD_DEVICE_STRING))
    }

    #[inline]
    fn trimmed(name: &str) -> &str {
        name.trim_end_matches('\0')
    }

    fn get_new_device_name(device_names: &HashSet<String>) -> Option<String> {
        let started = Instant::now();
        loop {
            let device_names_af: HashSet<String> = get_device_names().into_iter().collect();
            let diff_names: Vec<_> = device_names_af.difference(&device_names).collect();
            if diff_names.len() == 1 {
                return Some(diff_names[0].clone());
            } else if diff_names.len() > 1 {
                log::error!(
                    "Failed to get diff device names after plugin virtual display, more than one diff names: {:?}",
                    &diff_names
                );
                return None;
            }
            if started.elapsed() >= DEVICE_NAME_WAIT {
                break;
            }
            // Sleep is needed here to wait for the virtual display to be ready.
            std::thread::sleep(Duration::from_millis(100));
        }
        log::error!(
            "LATENCY vdisplay: no new display on the desktop {} ms after plugging in",
            started.elapsed().as_millis()
        );
        None
    }

    /// Sets the Windows mode of `name` to the first of `modes` (with its refresh rate),
    /// retrying while Windows does not offer it yet. Blocking (up to ~3 s).
    fn apply_mode(name: &str, mode: virtual_display::MonitorMode) -> bool {
        let name = trimmed(name).to_owned();
        let (w, h, hz) = (mode.width as u32, mode.height as u32, mode.sync as u32);
        let started = Instant::now();
        let mut last_err = String::new();
        for _ in 0..MODE_APPLY_TRIES {
            if let Ok((cw, ch, chz)) = crate::platform::windows::current_display_mode(&name) {
                if (cw, ch, chz) == (w, h, hz) {
                    log::info!(
                        "LATENCY vdisplay mode: {} is {}x{}@{} ({} ms)",
                        name,
                        w,
                        h,
                        hz,
                        started.elapsed().as_millis()
                    );
                    return true;
                }
            }
            if let Err(e) =
                crate::platform::windows::change_resolution_hz(&name, w as _, h as _, hz)
            {
                last_err = e.to_string();
            }
            std::thread::sleep(MODE_APPLY_STEP);
        }
        let now = crate::platform::windows::current_display_mode(&name)
            .map(|(cw, ch, chz)| format!("{}x{}@{}", cw, ch, chz))
            .unwrap_or_else(|e| e.to_string());
        log::warn!(
            "LATENCY vdisplay mode: {} not set to {}x{}@{} after {} ms (now {}): {}",
            name,
            w,
            h,
            hz,
            started.elapsed().as_millis(),
            now,
            last_err
        );
        false
    }

    /// Plugs in `idx` with `modes` (the first is the wanted one) and sets that mode. The caller
    /// holds PLUG_LOCK and has checked that `idx` is not plugged in. Blocking.
    fn plug_in_locked(idx: u32, modes: &[virtual_display::MonitorMode]) -> ResultType<Plugged> {
        prepare_device()?;
        let Some(first) = modes.first().copied() else {
            bail!("No modes for virtual display {}", idx);
        };
        let started = Instant::now();
        let device_names: HashSet<String> = get_device_names().into_iter().collect();
        if let Err(e) = virtual_display::plug_in_monitor(idx) {
            log::error!("LATENCY vdisplay plug in {}: failed: {}", idx, e);
            bail!("Plug in monitor failed {}", e);
        }
        match virtual_display::update_monitor_modes(idx, modes) {
            Ok(()) => log::info!(
                "LATENCY vdisplay modes {}: {}",
                idx,
                modes
                    .iter()
                    .map(|m| format!("{}x{}@{}", m.width, m.height, m.sync))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            Err(e) => log::error!("LATENCY vdisplay modes {}: update failed: {}", idx, e),
        }
        let name = get_new_device_name(&device_names);
        log::info!(
            "LATENCY vdisplay plug in {}: {} in {} ms",
            idx,
            name.as_deref().map(trimmed).unwrap_or("(name unknown)"),
            started.elapsed().as_millis()
        );
        if let Some(name) = &name {
            apply_mode(name, first);
        }
        Ok(Plugged {
            mode_pending: if name.is_none() { Some(first) } else { None },
            name,
            hz: first.sync as u32,
        })
    }

    /// Plugs out `indices` (peer displays). Unless `force`, refuses to take away the only
    /// displays left on the desktop. The caller holds PLUG_LOCK.
    fn plug_out_locked(indices: &[u32], force: bool) -> ResultType<()> {
        let plugged: Vec<u32> = {
            let state = lock(&STATE);
            indices
                .iter()
                .copied()
                .filter(|i| state.peers.contains_key(i))
                .collect()
        };
        if plugged.is_empty() {
            return Ok(());
        }
        if !force && windows::get_device_names(None).len() <= plugged.len() {
            bail!("The only display cannot be plugged out.");
        }
        let mut first_err: Option<String> = None;
        for idx in plugged {
            let res = virtual_display::plug_out_monitor(idx);
            let name = lock(&STATE).peers.get(&idx).and_then(|p| p.name.clone());
            let shown = name.as_deref().map(trimmed).unwrap_or("").to_owned();
            // A failed unplug keeps the display in our state (it is probably still there, and
            // a later reset retries it), unless it has left the desktop anyway: then the
            // wrapper's bookkeeping is cleared too, so that a later plug-in of the index really
            // calls the driver.
            let gone = match &res {
                Ok(()) => true,
                Err(_) => name.as_deref().map_or(false, |n| {
                    !get_device_names()
                        .iter()
                        .any(|d| trimmed(d) == trimmed(n))
                }),
            };
            match &res {
                Ok(()) => log::info!("LATENCY vdisplay plug out {}: {} done", idx, shown),
                Err(e) if gone => {
                    virtual_display::forget_monitor(idx);
                    log::warn!(
                        "LATENCY vdisplay plug out {}: {} failed ({}), but it is gone",
                        idx,
                        shown,
                        e
                    );
                }
                Err(e) => {
                    log::error!("LATENCY vdisplay plug out {}: {} failed: {}", idx, shown, e);
                    first_err
                        .get_or_insert_with(|| format!("Plug out monitor {} failed: {}", idx, e));
                }
            }
            if gone {
                let mut state = lock(&STATE);
                state.order.retain(|i| *i != idx);
                state.peers.remove(&idx);
            }
        }
        match first_err {
            Some(e) if !force => bail!("{}", e),
            _ => Ok(()),
        }
    }

    fn plug_out_headless_locked() -> bool {
        let headless = lock(&STATE).headless.take();
        if let Some((index, _)) = headless {
            match virtual_display::plug_out_monitor(index) {
                Ok(()) => log::info!("LATENCY vdisplay plug out headless {}: done", index),
                Err(e) => log::error!("Plug out monitor failed {}", e),
            }
            true
        } else {
            false
        }
    }

    pub fn plug_in_headless() -> ResultType<()> {
        let _plug = lock(&PLUG_LOCK);
        if lock(&STATE).headless.is_some() {
            return Ok(());
        }
        let modes = super::display_modes(1920, 1080, super::virtual_display_hz());
        let plugged = plug_in_locked(VIRTUAL_DISPLAY_INDEX_FOR_HEADLESS, &modes)?;
        lock(&STATE).headless = Some((VIRTUAL_DISPLAY_INDEX_FOR_HEADLESS, plugged));
        Ok(())
    }

    pub fn plug_out_headless() -> bool {
        let _plug = lock(&PLUG_LOCK);
        plug_out_headless_locked()
    }

    /// Fork: the headless display (index 0) is plugged in.
    pub fn has_headless() -> bool {
        lock(&STATE).headless.is_some()
    }

    /// Fork: the Windows device name of the headless display, once known.
    pub fn headless_name() -> Option<String> {
        let state = lock(&STATE);
        let name = state.headless.as_ref()?.1.name.as_deref()?;
        Some(trimmed(name).to_owned())
    }

    /// Fork: no request is queued for the worker or running on it.
    pub fn is_idle() -> bool {
        PENDING.load(Ordering::SeqCst) == 0
    }

    /// Fork: waits up to `timeout` until the requests queued before now (a reset queued by the
    /// last connection that closed, say) have finished. False when they still run.
    pub fn wait_idle(timeout: Duration) -> bool {
        let started = Instant::now();
        while !is_idle() {
            if started.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    // A headless plug-in is queued and not done yet.
    static HEADLESS_IN_QUEUED: AtomicBool = AtomicBool::new(false);

    /// Fork: `plug_in_headless` on the worker (after the requests queued before it), when the PC
    /// lost its last display during a session. `still_needed` runs on the worker right before the
    /// plug-in and must say again that a display is missing (a reset or the monitor may have come
    /// in between).
    pub fn plug_in_headless_queued<F>(still_needed: F)
    where
        F: FnOnce() -> bool + Send + 'static,
    {
        if HEADLESS_IN_QUEUED.swap(true, Ordering::SeqCst) {
            return;
        }
        PENDING.fetch_add(1, Ordering::SeqCst);
        run_queued(Box::new(move || {
            if !still_needed() {
                log::info!("LATENCY vdisplay headless: a display is there again, none plugged in");
            } else if let Err(e) = plug_in_headless() {
                log::error!("LATENCY vdisplay headless: plug in failed: {}", e);
            } else {
                log::info!("LATENCY vdisplay headless: plugged in for the missing display");
            }
            HEADLESS_IN_QUEUED.store(false, Ordering::SeqCst);
            PENDING.fetch_sub(1, Ordering::SeqCst);
            crate::server::display_service::resync_displays();
        }));
    }

    // A headless plug-out is queued and not done yet.
    static HEADLESS_OUT_QUEUED: AtomicBool = AtomicBool::new(false);

    /// Fork: `plug_out_headless` on the worker (after the requests queued before it), when a
    /// real display is back. Once per headless display. `still_wanted` runs on the worker right
    /// before the plug-out and must say again that a real display is there, so that a layout
    /// change in between (privacy mode, the monitor gone again) never takes away the only one.
    pub fn plug_out_headless_queued<F>(still_wanted: F)
    where
        F: FnOnce() -> bool + Send + 'static,
    {
        if HEADLESS_OUT_QUEUED.swap(true, Ordering::SeqCst) {
            return;
        }
        PENDING.fetch_add(1, Ordering::SeqCst);
        run_queued(Box::new(move || {
            if !still_wanted() {
                log::info!("LATENCY vdisplay: the real display is gone again, headless display kept");
            } else if plug_out_headless() {
                log::info!("LATENCY vdisplay: a real display is back, headless display plugged out");
            }
            HEADLESS_OUT_QUEUED.store(false, Ordering::SeqCst);
            PENDING.fetch_sub(1, Ordering::SeqCst);
            crate::server::display_service::resync_displays();
        }));
    }

    /// Fills in names that could not be told apart at plug-in time when exactly one display of
    /// ours is unnamed and exactly one RustDesk IDD display is unclaimed.
    fn resolve_missing_names(state: &mut State) {
        let unnamed: Vec<u32> = state
            .peers
            .iter()
            .filter(|(_, p)| p.name.is_none())
            .map(|(i, _)| *i)
            .collect();
        if unnamed.len() != 1
            || state
                .headless
                .as_ref()
                .map_or(false, |h| h.1.name.is_none())
        {
            return;
        }
        let claimed: HashSet<String> = state
            .peers
            .values()
            .chain(state.headless.iter().map(|h| &h.1))
            .filter_map(|p| p.name.clone())
            .collect();
        let free: Vec<String> = get_device_names()
            .into_iter()
            .filter(|n| !claimed.contains(n))
            .collect();
        if free.len() == 1 {
            log::info!(
                "LATENCY vdisplay: display {} is {}",
                unnamed[0],
                trimmed(&free[0])
            );
            if let Some(p) = state.peers.get_mut(&unnamed[0]) {
                p.name = Some(free[0].clone());
                // The wanted mode could not be set without the name: set it now, off the
                // caller's thread (it may wait for Windows to offer the mode).
                if let Some(mode) = p.mode_pending.take() {
                    let name = free[0].clone();
                    let _ = std::thread::Builder::new()
                        .name("virtual-display-mode".to_owned())
                        .spawn(move || {
                            apply_mode(&name, mode);
                        });
                }
            }
        }
    }

    /// Plugged peer indices, in the order they were plugged in.
    pub fn get_virtual_displays() -> Vec<u32> {
        lock(&STATE).order.clone()
    }

    /// Fork: plugged peer indices (oldest first) and the device name of each known one.
    pub fn get_virtual_displays_and_names() -> (Vec<u32>, Vec<(u32, String)>) {
        let mut state = lock(&STATE);
        resolve_missing_names(&mut state);
        let names = state
            .order
            .iter()
            .filter_map(|i| {
                let name = state.peers.get(i)?.name.as_deref()?;
                Some((*i, trimmed(name).to_owned()))
            })
            .collect();
        (state.order.clone(), names)
    }

    fn next_free_index() -> Option<u32> {
        let state = lock(&STATE);
        (VIRTUAL_DISPLAY_START_FOR_PEER..VIRTUAL_DISPLAY_MAX_COUNT)
            .find(|i| !state.peers.contains_key(i))
    }

    /// Plugs in peer display `idx` (`None`: the lowest free index) with `modes`. The caller
    /// holds PLUG_LOCK. Returns the index.
    fn plug_in_peer_locked(
        idx: Option<u32>,
        mut modes: Vec<virtual_display::MonitorMode>,
    ) -> ResultType<u32> {
        let idx = match idx {
            Some(idx) => idx,
            None => match next_free_index() {
                Some(idx) => idx,
                None => bail!(
                    "There are already {} virtual displays plugged in.",
                    VIRTUAL_DISPLAY_MAX_COUNT - VIRTUAL_DISPLAY_START_FOR_PEER
                ),
            },
        };
        if lock(&STATE).peers.contains_key(&idx) {
            log::info!("LATENCY vdisplay plug in {}: already plugged in", idx);
            return Ok(idx);
        }
        if modes.is_empty() {
            modes = super::display_modes(1920, 1080, super::virtual_display_hz());
        }
        let plugged = plug_in_locked(idx, &modes)?;
        let mut state = lock(&STATE);
        state.peers.insert(idx, plugged);
        state.order.retain(|i| *i != idx);
        state.order.push(idx);
        Ok(idx)
    }

    pub fn plug_in_index_modes(
        idx: u32,
        modes: Vec<virtual_display::MonitorMode>,
    ) -> ResultType<()> {
        let _plug = lock(&PLUG_LOCK);
        let idx = if idx == 0 { None } else { Some(idx) };
        plug_in_peer_locked(idx, modes).map(|_| ())
    }

    pub fn reset_all() -> ResultType<()> {
        if !super::is_virtual_display_supported() {
            return Ok(());
        }
        let _plug = lock(&PLUG_LOCK);
        let peers = get_virtual_displays();
        if let Err(e) = plug_out_locked(&peers, true) {
            log::error!("Failed to plug out virtual displays: {}", e);
        }
        let _ = plug_out_headless_locked();
        Ok(())
    }

    /// Fork: `reset_all` on the worker, after the requests queued before it. Does nothing (no
    /// thread either) when there are no displays of ours.
    pub fn reset_all_queued() {
        if !has_displays() {
            return;
        }
        PENDING.fetch_add(1, Ordering::SeqCst);
        run_queued(Box::new(|| {
            log::info!("LATENCY vdisplay: last connection closed, plugging out all");
            if let Err(e) = reset_all() {
                log::error!("LATENCY vdisplay reset failed: {}", e);
            }
            PENDING.fetch_sub(1, Ordering::SeqCst);
            crate::server::display_service::resync_displays();
        }));
    }

    /// Fork: a viewer's ToggleVirtualDisplay, run on the worker so that the connection's message
    /// loop never waits for the driver. `on`: `display` 1..=4 plugs in that index, 0 the lowest
    /// free one (1920x1080 at the configured rate). Off: `display` 1..=4 plugs out exactly that
    /// index, 0 the newest one, -1 all. `on_error` gets the text for the viewer.
    pub fn toggle_queued<F>(on: bool, display: i32, on_error: F)
    where
        F: FnOnce(String) + Send + 'static,
    {
        PENDING.fetch_add(1, Ordering::SeqCst);
        run_queued(Box::new(move || {
            let res = toggle(on, display);
            PENDING.fetch_sub(1, Ordering::SeqCst);
            match res {
                Ok(text) => log::info!("LATENCY vdisplay toggle on={} {}: {}", on, display, text),
                Err(e) => {
                    log::error!("LATENCY vdisplay toggle on={} {}: {}", on, display, e);
                    on_error(e.to_string());
                }
            }
            // The display service sends the new set (and these additions) on its own when it
            // sees the change; resend once the names are known.
            crate::server::display_service::resync_displays();
        }));
    }

    fn peer_index(display: i32) -> ResultType<u32> {
        let range = VIRTUAL_DISPLAY_START_FOR_PEER as i32..VIRTUAL_DISPLAY_MAX_COUNT as i32;
        if range.contains(&display) {
            Ok(display as u32)
        } else {
            bail!("Invalid virtual display index {}", display)
        }
    }

    fn toggle(on: bool, display: i32) -> ResultType<String> {
        let _plug = lock(&PLUG_LOCK);
        if on {
            let idx = if display == 0 {
                None
            } else {
                Some(peer_index(display)?)
            };
            let idx = plug_in_peer_locked(idx, Vec::new())?;
            Ok(format!(
                "plugged in {}, now {:?}",
                idx,
                get_virtual_displays()
            ))
        } else {
            let indices = match display {
                super::IDD_PLUG_OUT_ALL_INDEX => get_virtual_displays(),
                0 => match get_virtual_displays().last() {
                    Some(idx) => vec![*idx],
                    None => bail!("No virtual display to plug out."),
                },
                _ => {
                    let idx = peer_index(display)?;
                    if !lock(&STATE).peers.contains_key(&idx) {
                        bail!("Virtual display {} is not plugged in.", idx);
                    }
                    vec![idx]
                }
            };
            plug_out_locked(&indices, false)?;
            Ok(format!(
                "plugged out {:?}, now {:?}",
                indices,
                get_virtual_displays()
            ))
        }
    }

    pub fn plug_in_peer_request(
        modes: Vec<Vec<virtual_display::MonitorMode>>,
    ) -> ResultType<Vec<u32>> {
        let _plug = lock(&PLUG_LOCK);
        let mut indices: Vec<u32> = Vec::new();
        for m in modes.into_iter() {
            if next_free_index().is_none() {
                break;
            }
            match plug_in_peer_locked(None, m) {
                Ok(idx) => indices.push(idx),
                Err(e) => log::error!("Plug in monitor failed {}", e),
            }
        }
        Ok(indices)
    }

    pub fn plug_out_peer_request(indices: &[u32]) -> ResultType<()> {
        let _plug = lock(&PLUG_LOCK);
        plug_out_locked(indices, true)
    }

    pub fn is_virtual_display(name: &str) -> bool {
        let state = lock(&STATE);
        state
            .headless
            .iter()
            .map(|h| &h.1)
            .chain(state.peers.values())
            .filter_map(|p| p.name.as_deref())
            .any(|device_name| windows::is_device_name(device_name, name))
    }

    pub fn change_resolution_if_is_virtual_display(name: &str, w: u32, h: u32) -> Option<bool> {
        let (index, hz) = {
            let mut state = lock(&STATE);
            resolve_missing_names(&mut state);
            let is_name = |p: &Plugged| {
                p.name
                    .as_deref()
                    .map_or(false, |n| windows::is_device_name(n, name))
            };
            if let Some((index, p)) = state.headless.as_ref().filter(|h| is_name(&h.1)) {
                (*index, p.hz)
            } else {
                let (index, p) = state.peers.iter().find(|(_, p)| is_name(*p))?;
                (*index, p.hz)
            }
        };
        let modes = super::display_modes(w, h, hz);
        match virtual_display::update_monitor_modes(index, &modes) {
            Ok(_) => {
                log::info!(
                    "LATENCY vdisplay resolution {}: {}x{}@{} requested",
                    index,
                    w,
                    h,
                    hz
                );
                let name = name.to_owned();
                let first = modes[0];
                // Windows takes a moment to offer the new modes: set it off the message loop.
                let _ = std::thread::Builder::new()
                    .name("virtual-display-mode".to_owned())
                    .spawn(move || {
                        apply_mode(&name, first);
                    });
                Some(true)
            }
            Err(e) => {
                log::error!("Update monitor {} modes {:?} failed: {}", index, &modes, e);
                Some(false)
            }
        }
    }
}

pub mod amyuni_idd {
    use super::windows;
    use crate::platform::{reg_display_settings, win_device};
    use hbb_common::{bail, lazy_static, log, tokio::time::Instant, ResultType};
    use std::{
        ptr::null_mut,
        sync::{atomic, Arc, Mutex},
        time::Duration,
    };
    use winapi::{
        shared::{guiddef::GUID, winerror::ERROR_NO_MORE_ITEMS},
        um::shellapi::ShellExecuteA,
    };

    const INF_PATH: &str = r#"usbmmidd_v2\usbmmIdd.inf"#;
    const INTERFACE_GUID: GUID = GUID {
        Data1: 0xb5ffd75f,
        Data2: 0xda40,
        Data3: 0x4353,
        Data4: [0x8f, 0xf8, 0xb6, 0xda, 0xf6, 0xf1, 0xd8, 0xca],
    };
    const HARDWARE_ID: &str = "usbmmidd";
    const PLUG_MONITOR_IO_CONTROL_CDOE: u32 = 2307084;
    const INSTALLER_EXE_FILE: &str = "deviceinstaller64.exe";

    lazy_static::lazy_static! {
        static ref LOCK: Arc<Mutex<()>> = Default::default();
        static ref LAST_PLUG_IN_HEADLESS_TIME: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    }
    const VIRTUAL_DISPLAY_MAX_COUNT: usize = 4;
    // The count of virtual displays plugged in.
    // This count is not accurate, because:
    // 1. The virtual display driver may also be controlled by other processes.
    // 2. RustDesk may crash and restart, but the virtual displays are kept.
    //
    // to-do: Maybe a better way is to add an option asking the user if plug out all virtual displays on disconnect.
    static VIRTUAL_DISPLAY_COUNT: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

    fn get_deviceinstaller64_work_dir() -> ResultType<Option<Vec<u8>>> {
        let cur_exe = std::env::current_exe()?;
        let Some(cur_dir) = cur_exe.parent() else {
            bail!("Cannot get parent of current exe file.");
        };
        let work_dir = cur_dir.join("usbmmidd_v2");
        if !work_dir.exists() {
            return Ok(None);
        }
        let exe_path = work_dir.join(INSTALLER_EXE_FILE);
        if !exe_path.exists() {
            return Ok(None);
        }

        let Some(work_dir) = work_dir.to_str() else {
            bail!("Cannot convert work_dir to string.");
        };
        let mut work_dir2 = work_dir.as_bytes().to_vec();
        work_dir2.push(0);
        Ok(Some(work_dir2))
    }

    pub fn uninstall_driver() -> ResultType<()> {
        if let Ok(Some(work_dir)) = get_deviceinstaller64_work_dir() {
            if crate::platform::windows::is_x64() {
                log::info!("Uninstalling driver by deviceinstaller64.exe");
                install_if_x86_on_x64(&work_dir, "remove usbmmidd")?;
                // Sleep some time to wait for the driver to be uninstalled.
                std::thread::sleep(Duration::from_secs(2));
                return Ok(());
            }
        }

        log::info!("Uninstalling driver by SetupAPI");
        let mut reboot_required = false;
        let _ = unsafe { win_device::uninstall_driver(HARDWARE_ID, &mut reboot_required)? };
        Ok(())
    }

    // SetupDiCallClassInstaller() will always fail if current_exe() is built as x86 and running on x64.
    // So we need to call another x64 version exe to install and uninstall the driver.
    fn install_if_x86_on_x64(work_dir: &[u8], args: &str) -> ResultType<()> {
        const SW_HIDE: i32 = 0;
        let mut args = args.bytes().collect::<Vec<_>>();
        args.push(0);
        let mut exe_file = INSTALLER_EXE_FILE.bytes().collect::<Vec<_>>();
        exe_file.push(0);
        let hi = unsafe {
            ShellExecuteA(
                null_mut(),
                "open\0".as_ptr() as _,
                exe_file.as_ptr() as _,
                args.as_ptr() as _,
                work_dir.as_ptr() as _,
                SW_HIDE,
            ) as i32
        };
        if hi <= 32 {
            log::error!("Failed to run deviceinstaller: {}", hi);
            bail!("Failed to run deviceinstaller.")
        }
        Ok(())
    }

    // If the driver is installed by "deviceinstaller64.exe", the driver will be installed asynchronously.
    // The caller must wait some time before using the driver.
    fn check_install_driver(is_async: &mut bool) -> ResultType<()> {
        let _l = LOCK.lock().unwrap();
        let drivers = windows::get_display_drivers();
        if drivers
            .iter()
            .any(|(s, c)| s == super::AMYUNI_IDD_DEVICE_STRING && *c == 0)
        {
            *is_async = false;
            return Ok(());
        }

        if let Ok(Some(work_dir)) = get_deviceinstaller64_work_dir() {
            if crate::platform::windows::is_x64() {
                log::info!("Installing driver by deviceinstaller64.exe");
                install_if_x86_on_x64(&work_dir, "install usbmmidd.inf usbmmidd")?;
                *is_async = true;
                return Ok(());
            }
        }

        let exe_file = std::env::current_exe()?;
        let Some(cur_dir) = exe_file.parent() else {
            bail!("Cannot get parent of current exe file");
        };
        let inf_path = cur_dir.join(INF_PATH);
        if !inf_path.exists() {
            bail!("Driver inf file not found.");
        }
        let inf_path = inf_path.to_string_lossy().to_string();

        log::info!("Installing driver by SetupAPI");
        let mut reboot_required = false;
        let _ =
            unsafe { win_device::install_driver(&inf_path, HARDWARE_ID, &mut reboot_required)? };
        *is_async = false;
        Ok(())
    }

    pub fn reset_all() -> ResultType<()> {
        let _ = crate::privacy_mode::turn_off_privacy(0, None);
        let _ = plug_out_monitor(super::IDD_PLUG_OUT_ALL_INDEX, true, false);
        *LAST_PLUG_IN_HEADLESS_TIME.lock().unwrap() = None;
        Ok(())
    }

    #[inline]
    fn plug_monitor_(
        add: bool,
        wait_timeout: Option<Duration>,
    ) -> Result<(), win_device::DeviceError> {
        let cmd = if add { 0x10 } else { 0x00 };
        let cmd = [cmd, 0x00, 0x00, 0x00];
        let now = Instant::now();
        let c1 = get_monitor_count();
        unsafe {
            win_device::device_io_control(&INTERFACE_GUID, PLUG_MONITOR_IO_CONTROL_CDOE, &cmd, 0)?;
        }
        if let Some(wait_timeout) = wait_timeout {
            while now.elapsed() < wait_timeout {
                if get_monitor_count() != c1 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        // No need to consider concurrency here.
        if add {
            // If the monitor is plugged in, increase the count.
            // Though there's already a check of `VIRTUAL_DISPLAY_MAX_COUNT`, it's still better to check here for double ensure.
            if VIRTUAL_DISPLAY_COUNT.load(atomic::Ordering::SeqCst) < VIRTUAL_DISPLAY_MAX_COUNT {
                VIRTUAL_DISPLAY_COUNT.fetch_add(1, atomic::Ordering::SeqCst);
            }
        } else {
            if VIRTUAL_DISPLAY_COUNT.load(atomic::Ordering::SeqCst) > 0 {
                VIRTUAL_DISPLAY_COUNT.fetch_sub(1, atomic::Ordering::SeqCst);
            }
        }
        Ok(())
    }

    // `std::thread::sleep()` with a timeout is acceptable here.
    // Because user can wait for a while to plug in a monitor.
    fn plug_in_monitor_(
        add: bool,
        is_driver_async_installed: bool,
        wait_timeout: Option<Duration>,
    ) -> ResultType<()> {
        let timeout = Duration::from_secs(3);
        let now = Instant::now();
        let reg_connectivity_old = reg_display_settings::read_reg_connectivity();
        loop {
            match plug_monitor_(add, wait_timeout) {
                Ok(_) => {
                    break;
                }
                Err(e) => {
                    if is_driver_async_installed {
                        if let win_device::DeviceError::WinApiLastErr(_, e2) = &e {
                            if e2.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as _) {
                                if now.elapsed() < timeout {
                                    std::thread::sleep(Duration::from_millis(100));
                                    continue;
                                }
                            }
                        }
                    }
                    return Err(e.into());
                }
            }
        }
        // Workaround for the issue that we can't set the default the resolution.
        if let Ok(old_connectivity_old) = reg_connectivity_old {
            std::thread::spawn(move || {
                try_reset_resolution_on_first_plug_in(old_connectivity_old.len(), 1920, 1080);
            });
        }

        Ok(())
    }

    fn try_reset_resolution_on_first_plug_in(
        old_connectivity_len: usize,
        width: usize,
        height: usize,
    ) {
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(300));
            if let Ok(reg_connectivity_new) = reg_display_settings::read_reg_connectivity() {
                if reg_connectivity_new.len() != old_connectivity_len {
                    for name in
                        windows::get_device_names(Some(super::AMYUNI_IDD_DEVICE_STRING)).iter()
                    {
                        crate::platform::change_resolution(&name, width, height).ok();
                    }
                    break;
                }
            }
        }
    }

    pub fn plug_in_headless() -> ResultType<()> {
        let mut tm = LAST_PLUG_IN_HEADLESS_TIME.lock().unwrap();
        if let Some(tm) = &mut *tm {
            if tm.elapsed() < Duration::from_secs(3) {
                bail!("Plugging in too frequently.");
            }
        }
        *tm = Some(Instant::now());
        drop(tm);

        let mut is_async = false;
        if let Err(e) = check_install_driver(&mut is_async) {
            log::error!("Failed to install driver: {}", e);
            bail!("Failed to install driver.");
        }

        plug_in_monitor_(true, is_async, Some(Duration::from_millis(3_000)))
    }

    pub fn plug_in_monitor() -> ResultType<()> {
        let mut is_async = false;
        if let Err(e) = check_install_driver(&mut is_async) {
            log::error!("Failed to install driver: {}", e);
            bail!("Failed to install driver.");
        }

        if get_monitor_count() == VIRTUAL_DISPLAY_MAX_COUNT {
            bail!("There are already {VIRTUAL_DISPLAY_MAX_COUNT} monitors plugged in.");
        }

        plug_in_monitor_(true, is_async, None)
    }

    // `index` the display index to plug out. -1 means plug out all.
    // `force_all` is used to forcibly plug out all virtual displays.
    // `force_one` is used to forcibly plug out one virtual display managed by other processes
    //             if there're no virtual displays managed by RustDesk.
    pub fn plug_out_monitor(index: i32, force_all: bool, force_one: bool) -> ResultType<()> {
        let plug_out_all = index == super::IDD_PLUG_OUT_ALL_INDEX;
        // If `plug_out_all and force_all` is true, forcibly plug out all virtual displays.
        // Though the driver may be controlled by other processes,
        // we still forcibly plug out all virtual displays.
        //
        // 1. RustDesk plug in 2 virtual displays. (RustDesk)
        // 2. Other process plug out all virtual displays. (User manually)
        // 3. Other process plug in 1 virtual display. (User manually)
        // 4. RustDesk plug out all virtual displays in this call. (RustDesk disconnect)
        //
        // This is not a normal scenario, RustDesk will plug out virtual display unexpectedly.
        let mut plug_in_count = VIRTUAL_DISPLAY_COUNT.load(atomic::Ordering::Relaxed);
        let amyuni_count = get_monitor_count();
        if !plug_out_all {
            if plug_in_count == 0 && amyuni_count > 0 {
                if force_one {
                    plug_in_count = 1;
                } else {
                    bail!("The virtual display is managed by other processes.");
                }
            }
        } else {
            // Ignore the message if trying to plug out all virtual displays.
        }

        let all_count = windows::get_device_names(None).len();
        let mut to_plug_out_count = match all_count {
            0 => return Ok(()),
            1 => {
                if plug_in_count == 0 {
                    bail!("No virtual displays to plug out.")
                } else {
                    if force_all {
                        1
                    } else {
                        bail!("This only virtual display cannot be plugged out.")
                    }
                }
            }
            _ => {
                if all_count == plug_in_count {
                    if force_all {
                        all_count
                    } else {
                        all_count - 1
                    }
                } else {
                    plug_in_count
                }
            }
        };
        if to_plug_out_count != 0 && !plug_out_all {
            to_plug_out_count = 1;
        }

        for _i in 0..to_plug_out_count {
            let _ = plug_monitor_(false, None);
        }
        Ok(())
    }

    #[inline]
    pub fn get_monitor_count() -> usize {
        windows::get_device_names(Some(super::AMYUNI_IDD_DEVICE_STRING)).len()
    }

    /// Fork: the Amyuni displays RustDesk itself plugged in (and has not plugged out).
    #[inline]
    pub fn plugged_count() -> usize {
        VIRTUAL_DISPLAY_COUNT.load(atomic::Ordering::SeqCst)
    }

    /// (active Amyuni virtual displays, all active displays).
    pub fn display_counts() -> (usize, usize) {
        (get_monitor_count(), windows::get_device_names(None).len())
    }

    #[inline]
    pub fn is_my_display(name: &str) -> bool {
        windows::get_device_names(Some(super::AMYUNI_IDD_DEVICE_STRING))
            .iter()
            .any(|s| windows::is_device_name(s, name))
    }
}

mod windows {
    use std::ptr::null_mut;
    use winapi::{
        shared::{
            devguid::GUID_DEVCLASS_DISPLAY,
            minwindef::{DWORD, FALSE},
            ntdef::ULONG,
        },
        um::{
            cfgmgr32::{CM_Get_DevNode_Status, CR_SUCCESS},
            cguid::GUID_NULL,
            setupapi::{
                SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
                SetupDiGetDeviceRegistryPropertyW, SP_DEVINFO_DATA,
            },
            wingdi::{
                DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_MIRRORING_DRIVER,
            },
            winnt::HANDLE,
            winuser::{EnumDisplayDevicesW, EnumDisplaySettingsExW, ENUM_CURRENT_SETTINGS},
        },
    };

    const DIGCF_PRESENT: DWORD = 0x00000002;
    const SPDRP_DEVICEDESC: DWORD = 0x00000000;
    const SPDRP_DRIVER: DWORD = 0x00000009;
    const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

    #[inline]
    pub(super) fn is_device_name(device_name: &str, name: &str) -> bool {
        if name.len() == device_name.len() {
            name == device_name
        } else if name.len() > device_name.len() {
            false
        } else {
            &device_name[..name.len()] == name && device_name.as_bytes()[name.len() as usize] == 0
        }
    }

    pub(super) fn get_device_names(device_string: Option<&str>) -> Vec<String> {
        let mut device_names = Vec::new();
        let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
        dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as DWORD;
        let mut i_dev_num = 0;
        loop {
            let result = unsafe { EnumDisplayDevicesW(null_mut(), i_dev_num, &mut dd, 0) };
            if result == 0 {
                break;
            }
            i_dev_num += 1;

            if 0 == (dd.StateFlags & DISPLAY_DEVICE_ACTIVE)
                || (dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER) > 0
            {
                continue;
            }

            let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
            dm.dmSize = std::mem::size_of::<DEVMODEW>() as _;
            dm.dmDriverExtra = 0;
            let ok = unsafe {
                EnumDisplaySettingsExW(
                    dd.DeviceName.as_ptr(),
                    ENUM_CURRENT_SETTINGS,
                    &mut dm as _,
                    0,
                )
            };
            if ok == FALSE {
                continue;
            }
            if dm.dmPelsHeight == 0 || dm.dmPelsWidth == 0 {
                continue;
            }

            if let (Ok(device_name), Ok(ds)) = (
                String::from_utf16(&dd.DeviceName),
                String::from_utf16(&dd.DeviceString),
            ) {
                if let Some(s) = device_string {
                    if ds.len() >= s.len() && &ds[..s.len()] == s {
                        device_names.push(device_name);
                    }
                } else {
                    device_names.push(device_name);
                }
            }
        }
        device_names
    }

    /// Fork: whether a present display-class device has the description `desc` (a
    /// NUL-terminated prefix such as `RUSTDESK_IDD_DEVICE_STRING`) and a driver bound to it
    /// (a non-empty driver key). A devnode left over after its package was removed keeps its
    /// description but has no driver key, and a non-present one is not listed.
    pub(super) fn has_display_device(desc: &str) -> bool {
        let device_info_set = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVCLASS_DISPLAY,
                null_mut(),
                null_mut(),
                DIGCF_PRESENT,
            )
        };
        if device_info_set == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut device_info_data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ClassGuid: GUID_NULL,
            DevInst: 0,
            Reserved: 0,
        };
        let mut found = false;
        let mut device_index = 0;
        while !found
            && unsafe {
                SetupDiEnumDeviceInfo(device_info_set, device_index, &mut device_info_data)
            } != 0
        {
            device_index += 1;
            let mut buffer = vec![0u16; 256];
            let ok = unsafe {
                SetupDiGetDeviceRegistryPropertyW(
                    device_info_set,
                    &mut device_info_data,
                    SPDRP_DEVICEDESC,
                    null_mut(),
                    buffer.as_mut_ptr() as *mut u8,
                    (buffer.len() * 2) as DWORD,
                    null_mut(),
                )
            };
            if ok == 0 {
                continue;
            }
            let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            let description = String::from_utf16_lossy(&buffer[..len]);
            if description != desc.trim_end_matches('\0') {
                continue;
            }
            let mut driver = vec![0u16; 256];
            let ok = unsafe {
                SetupDiGetDeviceRegistryPropertyW(
                    device_info_set,
                    &mut device_info_data,
                    SPDRP_DRIVER,
                    null_mut(),
                    driver.as_mut_ptr() as *mut u8,
                    (driver.len() * 2) as DWORD,
                    null_mut(),
                )
            };
            found = ok != 0 && driver.first().map_or(false, |&c| c != 0);
        }
        unsafe {
            SetupDiDestroyDeviceInfoList(device_info_set);
        }
        found
    }

    pub(super) fn get_display_drivers() -> Vec<(String, u32)> {
        let mut display_drivers: Vec<(String, u32)> = Vec::new();

        let device_info_set = unsafe {
            SetupDiGetClassDevsW(
                &GUID_DEVCLASS_DISPLAY,
                null_mut(),
                null_mut(),
                DIGCF_PRESENT,
            )
        };

        if device_info_set == INVALID_HANDLE_VALUE {
            println!(
                "Failed to get device information set. Error: {}",
                std::io::Error::last_os_error()
            );
            return display_drivers;
        }

        let mut device_info_data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ClassGuid: GUID_NULL,
            DevInst: 0,
            Reserved: 0,
        };

        let mut device_index = 0;
        loop {
            let result = unsafe {
                SetupDiEnumDeviceInfo(device_info_set, device_index, &mut device_info_data)
            };
            if result == 0 {
                break;
            }

            let mut data_type: DWORD = 0;
            let mut required_size: DWORD = 0;

            // Get the required buffer size for the driver description
            let mut buffer;
            unsafe {
                SetupDiGetDeviceRegistryPropertyW(
                    device_info_set,
                    &mut device_info_data,
                    SPDRP_DEVICEDESC,
                    &mut data_type,
                    null_mut(),
                    0,
                    &mut required_size,
                );

                buffer = vec![0; required_size as usize / 2];
                SetupDiGetDeviceRegistryPropertyW(
                    device_info_set,
                    &mut device_info_data,
                    SPDRP_DEVICEDESC,
                    &mut data_type,
                    buffer.as_mut_ptr() as *mut u8,
                    required_size,
                    null_mut(),
                );
            }

            let Ok(driver_description) = String::from_utf16(&buffer) else {
                println!("Failed to convert driver description to string");
                device_index += 1;
                continue;
            };

            let mut status: ULONG = 0;
            let mut problem_number: ULONG = 0;
            // Get the device status and problem number
            let config_ret = unsafe {
                CM_Get_DevNode_Status(
                    &mut status,
                    &mut problem_number,
                    device_info_data.DevInst,
                    0,
                )
            };
            if config_ret != CR_SUCCESS {
                println!(
                    "Failed to get device status. Error: {}",
                    std::io::Error::last_os_error()
                );
                device_index += 1;
                continue;
            }
            display_drivers.push((driver_description, problem_number));
            device_index += 1;
        }

        display_drivers
    }
}
