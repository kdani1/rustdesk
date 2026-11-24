//! Quest 2D gamepad mode: `FFI.sendGamepad` (ffi.kt), called by `QuestGamepadCapture.kt` while
//! Horizon OS routes the Touch controllers to our panel as a virtual gamepad. Each call is a
//! full `Misc.gamepad_state` snapshot for UI session `sid`; Kotlin sends one on every change and
//! at least every 150 ms while captured (the host sets the pad neutral after 600 ms of
//! silence), and a last one with `off = true` when capture ends or the session closes.
//!
//! Only sent to a host whose PeerInfo said `Features.gamepad == 1`: a stock host, a host with
//! the option off (0) or one without ViGEmBus (2) gets nothing. The host's value is logged once
//! per session and value, and returned to Kotlin.
//!
//! Kept out of flutter_ffi.rs, which must stay byte-identical to the main checkout's.

use crate::{
    client::{Data, Interface},
    flutter::sessions,
};
use base::message_proto::{GamepadState, Message, Misc};
use hbb_common::log;
use jni::{
    objects::{JClass, JString},
    sys::{jboolean, jint, JNI_FALSE},
    JNIEnv,
};
use std::{collections::HashMap, sync::Mutex};

/// No UI session with this id (closed, or a bad id).
const NO_SESSION: jint = -1;
/// The session has no PeerInfo yet: the host's `Features.gamepad` is not known.
const NOT_KNOWN: jint = -2;

lazy_static::lazy_static! {
    /// The host feature value last logged per UI session id.
    static ref LOGGED: Mutex<HashMap<String, u32>> = Mutex::new(HashMap::new());
}

/// `FFI.sendGamepad(sid, seq, buttons, lt, rt, lx, ly, rx, ry, off)`. Returns the host's
/// `Features.gamepad` (the state was sent only when it is 1), `NOT_KNOWN` or `NO_SESSION`.
#[no_mangle]
pub extern "system" fn Java_ffi_FFI_sendGamepad(
    mut env: JNIEnv,
    _class: JClass,
    sid: JString,
    seq: jint,
    buttons: jint,
    lt: jint,
    rt: jint,
    lx: jint,
    ly: jint,
    rx: jint,
    ry: jint,
    off: jboolean,
) -> jint {
    let sid: String = env
        .get_string(&sid)
        .map(String::from)
        .unwrap_or_default()
        .to_lowercase();
    let Ok(id) = uuid::Uuid::parse_str(&sid) else {
        return NO_SESSION;
    };
    let Some(session) = sessions::get_session_by_session_id(&id) else {
        return NO_SESSION;
    };
    let Some(feature) = session.lc.read().unwrap().gamepad_feature() else {
        return NOT_KNOWN;
    };
    {
        let mut logged = LOGGED.lock().unwrap();
        if logged.get(&sid) != Some(&feature) {
            logged.insert(sid.clone(), feature);
            log::info!(
                "LATENCY gamepad viewer: host Features.gamepad={} sid={} ({})",
                feature,
                sid,
                if feature == 1 { "sending" } else { "not sent" }
            );
        }
    }
    if feature != 1 {
        return feature as jint;
    }
    let off = off != JNI_FALSE;
    let state = GamepadState {
        seq: seq as u32,
        // Guide (0x0400) is never sent; the host masks it too.
        buttons: (buttons as u32) & 0xF3FF,
        left_trigger: lt.clamp(0, 255) as u32,
        right_trigger: rt.clamp(0, 255) as u32,
        lx: lx.clamp(i16::MIN as i32, i16::MAX as i32),
        ly: ly.clamp(i16::MIN as i32, i16::MAX as i32),
        rx: rx.clamp(i16::MIN as i32, i16::MAX as i32),
        ry: ry.clamp(i16::MIN as i32, i16::MAX as i32),
        off,
        ..Default::default()
    };
    if off || seq == 0 {
        log::info!(
            "LATENCY gamepad viewer: {} seq={} sid={}",
            if off { "off" } else { "first state" },
            seq,
            sid
        );
    }
    let mut misc = Misc::new();
    misc.set_gamepad_state(state);
    let mut msg = Message::new();
    msg.set_misc(misc);
    session.send(Data::Message(msg));
    feature as jint
}
