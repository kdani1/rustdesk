//! Quest 3 immersive "game mode" (feature `quest-xr`, Android only). Phase 1 of
//! C:\RustDeskBuild\gamemode-plan.md: the lifecycle spike.
//!
//! `XrActivity` (a plain Android Activity with the VR / IMMERSIVE_HMD categories, see
//! XrActivity.kt) calls `FFI.xrStart`; this module runs one OpenXR session on a thread of its
//! own, attached to the JVM:
//! - the Khronos loader (AAR org.khronos.openxr:openxr_loader_for_android) is loaded at run
//!   time with the activity as the loader and instance context (`instance.rs`);
//! - an OpenGL ES session on a 1x1 EGL pbuffer context (`egl.rs`), LOCAL space, 90 Hz asked;
//! - a frame loop that submits NO layers yet: the user sees black (`session.rs`);
//! - the Touch controllers turned into an XInput-style pad state, logged only, never sent
//!   (`input.rs`). Menu held 1.5 s or Menu+L3+R3 leaves game mode.
//!
//! When the session ends for any reason the thread calls `XrActivity.onXrExit(reason)`, which
//! posts to the UI thread and returns at once; the activity then takes the user back to the
//! 2D panel. `FFI.xrStop` (from `onDestroy`) only raises a flag and never waits.
//!
//! Everything is logged to logcat with the tag `RustDeskXr`.

mod egl;
mod input;
mod instance;
mod java;
mod session;

use std::{
    ffi::CString,
    os::raw::{c_char, c_int},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const TAG: &[u8] = b"RustDeskXr\0";
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_WARN: c_int = 5;
const ANDROID_LOG_ERROR: c_int = 6;

#[link(name = "log")]
extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

fn xlog(prio: c_int, text: &str) {
    if let Ok(c) = CString::new(text.replace('\0', " ")) {
        unsafe {
            __android_log_write(prio, TAG.as_ptr() as *const c_char, c.as_ptr());
        }
    }
}

pub(crate) fn xlog_i(text: &str) {
    xlog(ANDROID_LOG_INFO, text);
}

pub(crate) fn xlog_w(text: &str) {
    xlog(ANDROID_LOG_WARN, text);
}

pub(crate) fn xlog_e(text: &str) {
    xlog(ANDROID_LOG_ERROR, text);
}

/// What the activity asks of the XR thread. Flags only: the activity never waits for it.
#[derive(Default)]
pub(crate) struct Control {
    /// `onDestroy`: leave as soon as possible (exit the session if it runs, else just stop).
    pub stop: AtomicBool,
    /// Leave game mode the normal way (`xrRequestExitSession`).
    pub exit: AtomicBool,
    /// Between `onResume` and `onPause` (logged with the session state, for T1/T5).
    pub resumed: AtomicBool,
}

/// What `XrActivity` passes in. In phase 1 only logged: the XR view shows no picture yet and
/// the pad is not sent.
#[derive(Debug, Clone)]
pub(crate) struct StartArgs {
    /// The UI session that asked for game mode.
    pub sid: String,
    pub display: i32,
    pub width: i32,
    pub height: i32,
    /// The host's Features.gamepad (0 unsupported, 1 available, 2 no driver).
    pub pad: i32,
}

struct Running {
    ctl: Arc<Control>,
    done: Arc<AtomicBool>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Sets `done` when the XR thread ends, however it ends.
struct DoneGuard(Arc<AtomicBool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Starts the XR thread. False when one is still running (the previous game mode has not
/// finished tearing down) or the thread cannot start.
pub(crate) fn start(vm: jni::JavaVM, activity: jni::objects::GlobalRef, args: StartArgs) -> bool {
    let Ok(mut running) = RUNNING.lock() else {
        return false;
    };
    if let Some(r) = running.as_ref() {
        if !r.done.load(Ordering::SeqCst) {
            xlog_w("start refused: the previous XR session is still shutting down");
            return false;
        }
    }
    let ctl = Arc::new(Control::default());
    ctl.resumed.store(true, Ordering::SeqCst);
    let done = Arc::new(AtomicBool::new(false));
    let (thread_ctl, guard) = (ctl.clone(), DoneGuard(done.clone()));
    xlog_i(&format!("start {:?}", args));
    match std::thread::Builder::new()
        .name("rustdesk-xr".to_owned())
        .spawn(move || {
            let _guard = guard;
            session::thread_main(vm, activity, args, thread_ctl);
        }) {
        Ok(_) => {
            *running = Some(Running { ctl, done });
            true
        }
        Err(e) => {
            xlog_e(&format!("XR thread did not start: {}", e));
            false
        }
    }
}

/// An XR session thread is running (game mode is on, the 2D views are hidden). The video
/// thread parks a display that has no surface meanwhile: its frames are dropped undecoded
/// instead of going to a software decoder nobody sees.
pub(crate) fn game_mode_active() -> bool {
    match RUNNING.try_lock() {
        Ok(running) => running
            .as_ref()
            .map_or(false, |r| !r.done.load(Ordering::SeqCst)),
        // `start` holds the lock only briefly; count that moment as on.
        Err(std::sync::TryLockError::WouldBlock) => true,
        Err(std::sync::TryLockError::Poisoned(_)) => false,
    }
}

fn with_control(f: impl FnOnce(&Control)) {
    if let Ok(running) = RUNNING.lock() {
        if let Some(r) = running.as_ref() {
            if !r.done.load(Ordering::SeqCst) {
                f(&r.ctl);
            }
        }
    }
}

/// The activity is being destroyed: end the session without waiting for it.
pub(crate) fn stop() {
    with_control(|c| c.stop.store(true, Ordering::SeqCst));
}

/// Leave game mode the normal way (`xrRequestExitSession`), e.g. Back on the XR activity.
pub(crate) fn request_exit() {
    with_control(|c| c.exit.store(true, Ordering::SeqCst));
}

pub(crate) fn set_resumed(resumed: bool) {
    with_control(|c| c.resumed.store(resumed, Ordering::SeqCst));
}
