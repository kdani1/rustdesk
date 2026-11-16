//! Host GPU scheduling priority while a capture runs (roadmap M17, server options
//! `enable-host-gpu-priority` and `enable-host-dwm-mmcss`, both off by default).
//!
//! A game that keeps the GPU at 99% makes DXGI duplication, the BGRA->NV12 conversion and the
//! encoder's input copies wait behind its own work: encode p95 went from about 3 ms to 10-16 ms.
//! What Sunshine does about it, minus REALTIME:
//! - `SeIncreaseBasePriorityPrivilege` on the process token (the `--server` process runs as
//!   SYSTEM and holds it; a portable instance under a normal user may not);
//! - `D3DKMTSetProcessSchedulingPriorityClass(HIGH)` for the whole process (all its GPU contexts,
//!   CUDA/NVENC included), ABOVE_NORMAL when HIGH is refused, never REALTIME;
//! - `IDXGIDevice::SetGPUThreadPriority(7)` on the capture device (shared with VRAM encoders) and,
//!   through hwcodec, on D3D11 devices the encoders open themselves (RAM nvenc);
//! - optionally `DwmEnableMMCSS(TRUE)`, kept only if it measurably helps.
//!
//! Counted: the first capture loop raises the process class (and DWM MMCSS), the last one to end
//! restores them. The device priority dies with the device. Return codes are logged once per
//! capture start (`LATENCY server gpu_priority`), with the HAGS state.

use hbb_common::{config::Config, log};
use std::{
    ffi::c_void,
    sync::{Mutex, OnceLock},
};
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

// D3DKMT_SCHEDULINGPRIORITYCLASS
const PRIORITY_CLASS_NORMAL: i32 = 2;
const PRIORITY_CLASS_ABOVE_NORMAL: i32 = 3;
const PRIORITY_CLASS_HIGH: i32 = 4;

/// IDXGIDevice::SetGPUThreadPriority range is -7..7; Sunshine uses 7.
pub const GPU_THREAD_PRIORITY: i32 = 7;

type SetClassFn = unsafe extern "system" fn(process: *mut c_void, class: i32) -> i32;
type GetClassFn = unsafe extern "system" fn(process: *mut c_void, class: *mut i32) -> i32;
type DwmEnableMmcssFn = unsafe extern "system" fn(enable: i32) -> i32;

struct Kmt {
    set: SetClassFn,
    get: GetClassFn,
}

unsafe fn load_symbol(dll: &str, name: &'static [u8]) -> Option<*mut c_void> {
    let wide: Vec<u16> = dll.encode_utf16().chain(std::iter::once(0)).collect();
    let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
    if module.is_null() {
        return None;
    }
    let f = GetProcAddress(module, name.as_ptr() as _);
    (!f.is_null()).then_some(f as *mut c_void)
}

fn kmt() -> Option<&'static Kmt> {
    static KMT: OnceLock<Option<Kmt>> = OnceLock::new();
    KMT.get_or_init(|| unsafe {
        let set = load_symbol("gdi32.dll", b"D3DKMTSetProcessSchedulingPriorityClass\0")?;
        let get = load_symbol("gdi32.dll", b"D3DKMTGetProcessSchedulingPriorityClass\0")?;
        Some(Kmt {
            set: std::mem::transmute::<*mut c_void, SetClassFn>(set),
            get: std::mem::transmute::<*mut c_void, GetClassFn>(get),
        })
    })
    .as_ref()
}

fn dwm_enable_mmcss() -> Option<DwmEnableMmcssFn> {
    static DWM: OnceLock<Option<DwmEnableMmcssFn>> = OnceLock::new();
    *DWM.get_or_init(|| unsafe {
        load_symbol("dwmapi.dll", b"DwmEnableMMCSS\0")
            .map(|f| std::mem::transmute::<*mut c_void, DwmEnableMmcssFn>(f))
    })
}

fn option_on(key: &str) -> bool {
    Config::get_option(key) == "Y"
}

fn class_name(class: i32) -> String {
    match class {
        0 => "idle".to_owned(),
        1 => "below_normal".to_owned(),
        PRIORITY_CLASS_NORMAL => "normal".to_owned(),
        PRIORITY_CLASS_ABOVE_NORMAL => "above_normal".to_owned(),
        PRIORITY_CLASS_HIGH => "high".to_owned(),
        5 => "realtime".to_owned(),
        c => format!("class{c}"),
    }
}

/// Enables SeIncreaseBasePriorityPrivilege on the process token: `ok`, `not_held` (the token
/// lacks it; HIGH is then usually refused) or `failed(<code>)`.
fn enable_increase_base_priority() -> String {
    use windows::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID},
        Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_INC_BASE_PRIORITY_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
            TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    unsafe {
        let mut token = HANDLE::default();
        if let Err(e) = OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) {
            return format!("failed({:#x})", e.code().0);
        }
        let mut luid = LUID::default();
        let result = match LookupPrivilegeValueW(
            windows::core::PCWSTR::null(),
            SE_INC_BASE_PRIORITY_NAME,
            &mut luid,
        ) {
            Err(e) => format!("failed({:#x})", e.code().0),
            Ok(()) => {
                let privileges = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                match AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) {
                    // Succeeds without the privilege in the token; only the last error tells.
                    Ok(()) if GetLastError() == ERROR_NOT_ALL_ASSIGNED => "not_held".to_owned(),
                    Ok(()) => "ok".to_owned(),
                    Err(e) => format!("failed({:#x})", e.code().0),
                }
            }
        };
        let _ = CloseHandle(token);
        result
    }
}

/// Hardware-accelerated GPU scheduling, from the registry value the Settings switch writes:
/// `on` (HwSchMode 2), `off` (1), `default` (no value: the driver's default) or `?`.
fn hags_state() -> &'static str {
    use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};
    let key = match RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers")
    {
        Ok(key) => key,
        Err(_) => return "?",
    };
    match key.get_value::<u32, _>("HwSchMode") {
        Ok(2) => "on",
        Ok(1) => "off",
        Ok(_) => "?",
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "default",
        Err(_) => "?",
    }
}

struct State {
    holders: usize,
    // The class the first holder found, when it changed it.
    raised_from: Option<i32>,
    dwm_mmcss: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    holders: 0,
    raised_from: None,
    dwm_mmcss: false,
});

/// Raises the process class to HIGH (ABOVE_NORMAL when refused). Returns the log text, the
/// class it found when it changed it, and whether the class is now ABOVE_NORMAL or more.
fn raise_process_class() -> (String, Option<i32>, bool) {
    let Some(kmt) = kmt() else {
        return ("unavailable".to_owned(), None, false);
    };
    let process = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
    let process = process.0 as *mut c_void;
    let mut previous = PRIORITY_CLASS_NORMAL;
    if unsafe { (kmt.get)(process, &mut previous) } != 0 {
        previous = PRIORITY_CLASS_NORMAL;
    }
    if previous >= PRIORITY_CLASS_HIGH {
        return (format!("kept:{}", class_name(previous)), None, true);
    }
    let status = unsafe { (kmt.set)(process, PRIORITY_CLASS_HIGH) };
    if status == 0 {
        return ("high".to_owned(), Some(previous), true);
    }
    if previous < PRIORITY_CLASS_ABOVE_NORMAL {
        let fallback = unsafe { (kmt.set)(process, PRIORITY_CLASS_ABOVE_NORMAL) };
        if fallback == 0 {
            return (
                format!("above_normal(high:{:#010x})", status as u32),
                Some(previous),
                true,
            );
        }
        return (
            format!(
                "failed(high:{:#010x},above_normal:{:#010x})",
                status as u32, fallback as u32
            ),
            None,
            false,
        );
    }
    // Already ABOVE_NORMAL and HIGH refused: unchanged, but above normal.
    (format!("failed(high:{:#010x})", status as u32), None, true)
}

fn restore_process_class(class: i32) -> String {
    let Some(kmt) = kmt() else {
        return "unavailable".to_owned();
    };
    let process = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
    let status = unsafe { (kmt.set)(process.0 as *mut c_void, class) };
    if status == 0 {
        format!("restored:{}", class_name(class))
    } else {
        format!("restore_failed({:#010x})", status as u32)
    }
}

/// One capture loop's share of the GPU priority; the last one dropped restores the process class
/// and DWM MMCSS. Created and dropped on the capture thread.
pub struct HostGpuPriority {
    held: bool,
    label: &'static str,
}

impl HostGpuPriority {
    /// Applies the options for a capture loop that is about to create its encoder: process class
    /// and DWM MMCSS (first holder), `capture` sets the capture device's GPU thread priority and
    /// returns its HRESULT (`None` without such a device). Logs one line. Also tells hwcodec which
    /// priority encoders opened from now on give their own devices.
    pub fn acquire(display: usize, capture: impl FnOnce(i32) -> Option<i32>) -> Self {
        let on = option_on(base::config::keys::OPTION_ENABLE_HOST_GPU_PRIORITY);
        let dwm = option_on(base::config::keys::OPTION_ENABLE_HOST_DWM_MMCSS);
        #[cfg(feature = "hwcodec")]
        scrap::hwcodec::set_encode_gpu_thread_priority(if on { GPU_THREAD_PRIORITY } else { 0 });
        if !on && !dwm {
            // Nothing changed: only with latency logging on, so a default host logs as before.
            if option_on(base::config::keys::OPTION_ENABLE_LATENCY_LOG) {
                log::info!("LATENCY server gpu_priority display={display} off");
            }
            return Self {
                held: false,
                label: "off",
            };
        }
        let mut state = STATE.lock().unwrap();
        state.holders += 1;
        let (process, privilege, process_raised) = if !on {
            ("off".to_owned(), "-".to_owned(), false)
        } else if state.raised_from.is_some() {
            ("held".to_owned(), "-".to_owned(), true)
        } else {
            let privilege = enable_increase_base_priority();
            let (text, raised_from, raised) = raise_process_class();
            state.raised_from = raised_from;
            (text, privilege, raised)
        };
        let dwm_text = if !dwm {
            "off".to_owned()
        } else if state.dwm_mmcss {
            "held".to_owned()
        } else {
            match dwm_enable_mmcss() {
                None => "unavailable".to_owned(),
                Some(f) => {
                    let hr = unsafe { f(1) };
                    state.dwm_mmcss = hr >= 0;
                    format!("{:#010x}", hr as u32)
                }
            }
        };
        let dwm_applied = dwm && state.dwm_mmcss;
        drop(state);
        let mut capture_ok = false;
        let capture_text = if on {
            match capture(GPU_THREAD_PRIORITY) {
                Some(hr) => {
                    capture_ok = hr >= 0;
                    format!("{GPU_THREAD_PRIORITY}:{:#010x}", hr as u32)
                }
                None => "n/a".to_owned(),
            }
        } else {
            "off".to_owned()
        };
        log::info!(
            "LATENCY server gpu_priority display={display} process={process} privilege={privilege} capture={capture_text} encoder={} hags={} dwm_mmcss={dwm_text}",
            if on { GPU_THREAD_PRIORITY } else { 0 },
            hags_state(),
        );
        Self {
            held: true,
            label: label(on.then_some(process_raised || capture_ok), dwm.then_some(dwm_applied)),
        }
    }

    /// For the `gpuprio=` field of the `LATENCY server` line: what this capture start applied.
    pub fn label(&self) -> &'static str {
        self.label
    }
}

impl Drop for HostGpuPriority {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        let mut state = STATE.lock().unwrap();
        state.holders = state.holders.saturating_sub(1);
        if state.holders > 0 {
            return;
        }
        let process = state
            .raised_from
            .take()
            .map(restore_process_class)
            .unwrap_or_else(|| "-".to_owned());
        let dwm = if std::mem::take(&mut state.dwm_mmcss) {
            match dwm_enable_mmcss() {
                Some(f) => format!("{:#010x}", unsafe { f(0) } as u32),
                None => "unavailable".to_owned(),
            }
        } else {
            "-".to_owned()
        };
        log::info!("LATENCY server gpu_priority released process={process} dwm_mmcss={dwm}");
    }
}

/// `gpuprio=` text from what was asked and applied: `gpu` / `dwm` parts (joined by `+`), each
/// `-failed` when its option is on but nothing took effect; `off` when neither option is on.
/// `gpu` counts as applied when the process class is ABOVE_NORMAL or more, or the capture
/// device's `SetGPUThreadPriority` succeeded.
fn label(gpu: Option<bool>, dwm: Option<bool>) -> &'static str {
    match (gpu, dwm) {
        (None, None) => "off",
        (Some(true), None) => "gpu",
        (Some(false), None) => "gpu-failed",
        (None, Some(true)) => "dwm",
        (None, Some(false)) => "dwm-failed",
        (Some(true), Some(true)) => "gpu+dwm",
        (Some(true), Some(false)) => "gpu+dwm-failed",
        (Some(false), Some(true)) => "gpu-failed+dwm",
        (Some(false), Some(false)) => "gpu-failed+dwm-failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_gpu_priority_labels() {
        assert_eq!(label(None, None), "off");
        assert_eq!(label(Some(true), None), "gpu");
        assert_eq!(label(Some(false), None), "gpu-failed");
        assert_eq!(label(None, Some(true)), "dwm");
        assert_eq!(label(Some(true), Some(true)), "gpu+dwm");
        assert_eq!(label(Some(false), Some(true)), "gpu-failed+dwm");
    }

    #[test]
    fn latency_gpu_priority_class_names() {
        assert_eq!(class_name(PRIORITY_CLASS_HIGH), "high");
        assert_eq!(class_name(PRIORITY_CLASS_ABOVE_NORMAL), "above_normal");
        assert_eq!(class_name(PRIORITY_CLASS_NORMAL), "normal");
        assert_eq!(class_name(9), "class9");
    }

    #[test]
    fn latency_gpu_priority_hags_state_is_known() {
        assert!(["on", "off", "default", "?"].contains(&hags_state()));
    }
}
