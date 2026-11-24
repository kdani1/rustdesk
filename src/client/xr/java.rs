//! JNI entry points of game mode (`ffi.FFI` in ffi.kt). Kept here, not in flutter_ffi.rs,
//! which must stay byte-identical to the main checkout's.

use super::StartArgs;
use jni::{
    objects::{JClass, JObject, JString},
    sys::{jboolean, jint, JNI_FALSE, JNI_TRUE},
    JNIEnv,
};

/// `FFI.xrStart(activity, sid, display, width, height, pad)`: starts the XR thread for
/// `XrActivity` [activity]. Returns at once; false when it could not start.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_xrStart(
    mut env: JNIEnv,
    _class: JClass,
    activity: JObject,
    sid: JString,
    display: jint,
    width: jint,
    height: jint,
    pad: jint,
) -> jboolean {
    let sid: String = env
        .get_string(&sid)
        .map(|s| s.into())
        .unwrap_or_default();
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            super::xlog_e(&format!("xrStart: no JavaVM: {}", e));
            return JNI_FALSE;
        }
    };
    let activity = match env.new_global_ref(&activity) {
        Ok(a) => a,
        Err(e) => {
            super::xlog_e(&format!("xrStart: no global ref to the activity: {}", e));
            return JNI_FALSE;
        }
    };
    let args = StartArgs {
        sid,
        display,
        width,
        height,
        pad,
    };
    if super::start(vm, activity, args) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

/// `FFI.xrStop()`, from `XrActivity.onDestroy`: asks the XR thread to end and returns at once.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_xrStop(_env: JNIEnv, _class: JClass) {
    super::xlog_i("xrStop (activity destroyed)");
    super::stop();
}

/// `FFI.xrRequestExit()`: leave game mode the normal way (xrRequestExitSession).
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_xrRequestExit(_env: JNIEnv, _class: JClass) {
    super::xlog_i("xrRequestExit");
    super::request_exit();
}

/// `FFI.xrLifecycle(resumed)`: `XrActivity.onResume` / `onPause`, logged for T1/T5.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_xrLifecycle(_env: JNIEnv, _class: JClass, resumed: jboolean) {
    let resumed = resumed != JNI_FALSE;
    super::xlog_i(if resumed {
        "activity resumed"
    } else {
        "activity paused"
    });
    super::set_resumed(resumed);
}
