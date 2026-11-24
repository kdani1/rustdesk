//! Loader, extensions and instance.

use super::{xlog_i, xlog_w};
use openxr as xr;

/// Asked for when the runtime lists them; only the first two are required.
/// - `KHR_android_create_instance`, `KHR_opengl_es_enable`: required.
/// - `KHR_android_surface_swapchain`: the video surface of phase 2 (missing: phase 2 is off).
/// - `KHR_composition_layer_cylinder`: the curved screen (phase 2).
/// - `FB_display_refresh_rate`: the 90 Hz request.
/// - `EXT_performance_settings`: GPU level (CPU left alone, critique #18).
/// - `KHR_android_thread_settings`: the XR thread as the renderer's main thread.
/// - `FB_color_space`: Rec.709 for the desktop picture (critique #10).
/// `META_touch_controller_plus` is NOT enabled: the bindings are for
/// `oculus/touch_controller` only (critique #6).
fn wanted(available: &xr::ExtensionSet) -> xr::ExtensionSet {
    let mut e = xr::ExtensionSet::default();
    e.khr_android_create_instance = available.khr_android_create_instance;
    e.khr_opengl_es_enable = available.khr_opengl_es_enable;
    e.khr_android_surface_swapchain = available.khr_android_surface_swapchain;
    e.khr_composition_layer_cylinder = available.khr_composition_layer_cylinder;
    e.fb_display_refresh_rate = available.fb_display_refresh_rate;
    e.ext_performance_settings = available.ext_performance_settings;
    e.khr_android_thread_settings = available.khr_android_thread_settings;
    e.fb_color_space = available.fb_color_space;
    e
}

fn names(set: &xr::ExtensionSet) -> String {
    let mut v: Vec<String> = set
        .names()
        .into_iter()
        .map(|n| String::from_utf8_lossy(n.strip_suffix(b"\0").unwrap_or(n)).into_owned())
        .collect();
    v.sort();
    v.join(" ")
}

pub(super) struct XrInstance {
    pub instance: xr::Instance,
    pub enabled: xr::ExtensionSet,
}

/// Loads `libopenxr_loader.so` (from the AAR), initialises it with the activity, logs every
/// extension the runtime lists (T4) and creates the instance: API 1.1, else 1.0.
pub(super) fn create(platform: &xr::AndroidPlatformInfo) -> Result<XrInstance, String> {
    let entry =
        unsafe { xr::Entry::load(platform) }.map_err(|e| format!("OpenXR loader: {}", e))?;
    let available = entry
        .enumerate_extensions()
        .map_err(|e| format!("xrEnumerateInstanceExtensionProperties: {:?}", e))?;
    xlog_i(&format!("runtime extensions: {}", names(&available)));
    let enabled = wanted(&available);
    if !enabled.khr_android_create_instance || !enabled.khr_opengl_es_enable {
        return Err(
            "the runtime lacks XR_KHR_android_create_instance or XR_KHR_opengl_es_enable"
                .to_owned(),
        );
    }
    if !enabled.khr_android_surface_swapchain {
        xlog_w("XR_KHR_android_surface_swapchain is not listed: the video of phase 2 cannot work");
    }
    xlog_i(&format!("enabled extensions: {}", names(&enabled)));
    for api_version in [xr::Version::new(1, 1, 0), xr::Version::new(1, 0, 0)] {
        let app = xr::ApplicationInfo {
            application_name: "RustDesk",
            application_version: 1,
            engine_name: "RustDesk",
            engine_version: 1,
            api_version,
        };
        match entry.create_instance(&app, &enabled, &[], platform) {
            Ok(instance) => {
                match instance.properties() {
                    Ok(p) => xlog_i(&format!(
                        "instance: API {}, runtime {} {}",
                        api_version, p.runtime_name, p.runtime_version
                    )),
                    Err(e) => xlog_w(&format!("instance: API {}, properties: {:?}", api_version, e)),
                }
                return Ok(XrInstance { instance, enabled });
            }
            Err(xr::sys::Result::ERROR_API_VERSION_UNSUPPORTED) => {
                xlog_w(&format!("xrCreateInstance: API {} unsupported", api_version));
            }
            Err(e) => return Err(format!("xrCreateInstance (API {}): {:?}", api_version, e)),
        }
    }
    Err("xrCreateInstance: neither API 1.1 nor 1.0 is supported".to_owned())
}

/// The headset's system. The runtime may not have it ready the moment the activity starts.
pub(super) fn system(
    instance: &xr::Instance,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<xr::SystemId, String> {
    let mut tries = 0;
    loop {
        match instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY) {
            Ok(s) => {
                if let Ok(p) = instance.system_properties(s) {
                    xlog_i(&format!(
                        "system: {} (vendor 0x{:x}), max layers {}, orientation {} position {}",
                        p.system_name,
                        p.vendor_id,
                        p.graphics_properties.max_layer_count,
                        p.tracking_properties.orientation_tracking,
                        p.tracking_properties.position_tracking
                    ));
                }
                return Ok(s);
            }
            Err(xr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE)
                if tries < 25 && !stop.load(std::sync::atomic::Ordering::SeqCst) =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => return Err(format!("xrGetSystem: {:?}", e)),
        }
    }
}
