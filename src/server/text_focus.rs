//! Host side of the viewer's "open the soft keyboard?" decision (Windows only).
//!
//! A touch viewer (the Quest app) opens its soft keyboard after a click when the remote mouse
//! cursor is a text I-beam. Windows shows that cursor over any selectable text, so the keyboard
//! also came up on plain text. The host knows better: after a left click from such a viewer it
//! asks UI Automation what has the keyboard focus and answers with `Misc.text_input_focus`:
//!
//!   EDITABLE      the focus is in an editable text field under the pointer: open the keyboard
//!   NOT_EDITABLE  the focus is in something that takes no text, or in a normally sized text
//!                 field somewhere else than under the pointer: do not open it
//!   UNKNOWN       no usable answer (no accessibility data, secure desktop, timeout): the viewer
//!                 falls back to the cursor shape, as it does with a host that never answers
//!
//! The probe runs on its own thread: it makes cross-process calls that a hung application can
//! delay, and the input threads switch desktops (`try_change_desktop`), which this thread never
//! does. Only a newer click replaces a pending one, so the cost of a slow target is bounded by
//! the UI Automation timeouts set below.

use super::connection::Sender;
use base::message_proto::{Message, Misc};
use hbb_common::log;
use std::{
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{POINT, RECT},
        System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED},
        UI::{
            Accessibility::{
                CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
                IUIAutomationValuePattern, UIA_BoundingRectanglePropertyId,
                UIA_ClassNamePropertyId, UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId,
                UIA_CustomControlTypeId, UIA_DataItemControlTypeId, UIA_DocumentControlTypeId,
                UIA_EditControlTypeId, UIA_E_TIMEOUT, UIA_FrameworkIdPropertyId,
                UIA_GroupControlTypeId, UIA_IsEnabledPropertyId, UIA_PaneControlTypeId,
                UIA_SpinnerControlTypeId, UIA_TitleBarControlTypeId,
                UIA_ValueIsReadOnlyPropertyId, UIA_ValuePatternId, UIA_WindowControlTypeId,
            },
            WindowsAndMessaging::{
                GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowInfo, WINDOWINFO,
            },
        },
    },
};

pub const UNKNOWN: i32 = 0;
pub const NOT_EDITABLE: i32 = 1;
pub const EDITABLE: i32 = 2;

/// Focus moves asynchronously after the click (a browser needs a renderer round trip), so the
/// state is sampled at these times after the click. EDITABLE is answered at the first sample
/// that says so, anything else only after the last one.
const SAMPLE_MS: [u64; 3] = [90, 260, 600];
/// A hung target application must not hold the probe for long.
const UIA_TIMEOUT_MS: u32 = 300;
/// The pointer must be inside the focused field (plus this margin): a click on a toolbar or on
/// plain text that leaves the focus in an editor elsewhere is not a click into that editor.
const RECT_MARGIN: i32 = 16;
/// Some editors take their input through a hidden proxy element they keep at the caret, not
/// through the visible editor: with the pointer outside such an element the host cannot say
/// "not a text field", and the cursor shape decides (UNKNOWN). A proxy is recognised by being
/// tiny in both directions (xterm.js: one character cell; Monaco's and Ace's textarea: about
/// 1 px), or by its class (Monaco's EditContext element is a whole line, which a size test
/// cannot tell from an ordinary one-line input: measured, an ordinary input box that kept the
/// focus while the user clicked plain text elsewhere was 312 x 32 px).
const PROXY_MAX_SIDE: i32 = 48;
const PROXY_CLASSES: [&str; 4] = [
    "native-edit-context",
    "xterm-helper-textarea",
    "inputarea",
    "ace_text-input",
];
/// Console windows have no accessible "edit" but typing is what they are for. Windows Terminal
/// is not listed: its focused element (class TermControl) is found through UI Automation.
const TERMINAL_CLASSES: [&str; 4] = ["ConsoleWindowClass", "PuTTY", "mintty", "VirtualConsoleClass"];

pub fn message(state: i32) -> Message {
    let mut misc = Misc::new();
    misc.set_text_input_focus(state);
    let mut msg = Message::new();
    msg.set_misc(misc);
    msg
}

struct Request {
    tx: Sender,
    at: Instant,
}

lazy_static::lazy_static! {
    static ref PROBE: Mutex<Option<mpsc::Sender<Request>>> = Mutex::new(None);
    static ref CLICK_POS: Mutex<Option<(Instant, POINT)>> = Mutex::new(None);
}

/// Input thread, right after a left button release was injected: where the click landed. The
/// viewer's pointer keeps moving the cursor afterwards, so the probe must not read it later.
pub fn note_left_up_injected() {
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_ok() {
        *CLICK_POS.lock().unwrap() = Some((Instant::now(), p));
    }
}

/// A left button release from a viewer that uses the answer was received on connection `tx`.
pub fn on_left_up(tx: Sender) {
    let req = Request {
        tx,
        at: Instant::now(),
    };
    let mut probe = PROBE.lock().unwrap();
    let req = match probe.as_ref() {
        Some(sender) => match sender.send(req) {
            Ok(()) => return,
            Err(e) => e.0, // the thread is gone: start a new one
        },
        None => req,
    };
    let (sender, receiver) = mpsc::channel::<Request>();
    match std::thread::Builder::new()
        .name("text-focus-probe".to_owned())
        .spawn(move || run(receiver))
    {
        Ok(_) => {
            sender.send(req).ok();
            *probe = Some(sender);
        }
        Err(e) => {
            log::error!("text focus probe: failed to start the thread: {e}");
            *probe = None;
        }
    }
}

struct Uia {
    automation: IUIAutomation,
    cache: IUIAutomationCacheRequest,
}

fn create_uia() -> windows::core::Result<Uia> {
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        if let Ok(v2) = automation.cast::<IUIAutomation2>() {
            v2.SetConnectionTimeout(UIA_TIMEOUT_MS).ok();
            v2.SetTransactionTimeout(UIA_TIMEOUT_MS).ok();
        }
        // Everything the rule needs comes back with the one cross-process call that fetches
        // the focused element. No text pattern: asking a browser for it switches on its
        // heaviest accessibility mode.
        let cache = automation.CreateCacheRequest()?;
        cache.AddProperty(UIA_ControlTypePropertyId)?;
        cache.AddProperty(UIA_IsEnabledPropertyId)?;
        cache.AddProperty(UIA_FrameworkIdPropertyId)?;
        cache.AddProperty(UIA_ClassNamePropertyId)?;
        cache.AddProperty(UIA_BoundingRectanglePropertyId)?;
        cache.AddProperty(UIA_ValueIsReadOnlyPropertyId)?;
        cache.AddPattern(UIA_ValuePatternId)?;
        Ok(Uia { automation, cache })
    }
}

fn run(receiver: mpsc::Receiver<Request>) {
    // MTA, as the other COM user in this process (microphone forwarding).
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        log::error!("text focus probe: CoInitializeEx failed: {com:?}");
    }
    let mut uia: Option<Uia> = None;
    let mut logged_create_error = false;
    let mut next: Option<Request> = None;
    loop {
        let mut req = match next.take() {
            Some(r) => r,
            None => match receiver.recv() {
                Ok(r) => r,
                Err(_) => return,
            },
        };
        // Only the newest click matters.
        while let Ok(newer) = receiver.try_recv() {
            req = newer;
        }
        if uia.is_none() {
            match create_uia() {
                Ok(u) => uia = Some(u),
                Err(e) => {
                    if !logged_create_error {
                        logged_create_error = true;
                        log::error!("text focus probe: UI Automation is not available: {e}");
                    }
                }
            }
        }
        let mut answer = UNKNOWN;
        let mut why = String::new();
        let mut pointer: Option<POINT> = None;
        let mut superseded = false;
        for ms in SAMPLE_MS {
            let due = req.at + Duration::from_millis(ms);
            let now = Instant::now();
            if due > now {
                match receiver.recv_timeout(due - now) {
                    Ok(newer) => {
                        next = Some(newer);
                        superseded = true;
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if pointer.is_none() {
                // Where the click landed, recorded when it was injected. Only a record made
                // after this request counts: an older one belongs to an earlier click.
                pointer = match *CLICK_POS.lock().unwrap() {
                    Some((t, p)) if t >= req.at => Some(p),
                    _ => None,
                };
            }
            if pointer.is_none() && ms == SAMPLE_MS[SAMPLE_MS.len() - 1] {
                // Never recorded (the click was injected by the portable service process):
                // the pointer now is the best there is.
                let mut p = POINT::default();
                if unsafe { GetCursorPos(&mut p) }.is_ok() {
                    pointer = Some(p);
                }
            }
            let (state, reason, settled) = probe(uia.as_ref(), pointer);
            answer = state;
            why = reason;
            // Anything else may still change while the focus settles.
            if state == EDITABLE || settled {
                break;
            }
        }
        if !superseded {
            // A click that arrived during the last sample: the viewer waits for its answer.
            while let Ok(newer) = receiver.try_recv() {
                next = Some(newer);
            }
            superseded = next.is_some();
        }
        if superseded {
            // The viewer re-arms on its newer click and waits for that click's answer.
            continue;
        }
        // Types and class names only, never any text of the element.
        log::info!(
            "text focus: {answer} after {} ms ({why})",
            req.at.elapsed().as_millis()
        );
        // The connection's channel stamps its messages with tokio's clock.
        req.tx
            .send((
                hbb_common::tokio::time::Instant::now(),
                Arc::new(message(answer)),
            ))
            .ok();
    }
}

/// One sample: the state, why (for the log), and whether a later sample could not differ.
fn probe(uia: Option<&Uia>, pointer: Option<POINT>) -> (i32, String, bool) {
    // The lock screen and UAC run on another desktop: nothing to say about those.
    if crate::platform::windows::desktop_changed() {
        return (UNKNOWN, "not on the input desktop".to_owned(), true);
    }
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_invalid() {
        return (UNKNOWN, "no foreground window".to_owned(), false);
    }
    let mut class = [0u16; 128];
    let len = unsafe { GetClassNameW(foreground, &mut class) }.max(0) as usize;
    let window_class = String::from_utf16_lossy(&class[..len.min(class.len())]);
    if TERMINAL_CLASSES.iter().any(|c| *c == window_class) {
        // Only a click into the terminal itself, not on its title bar, buttons or scroll bar.
        let mut info = WINDOWINFO {
            cbSize: std::mem::size_of::<WINDOWINFO>() as u32,
            ..Default::default()
        };
        let inside = match pointer {
            Some(p) if unsafe { GetWindowInfo(foreground, &mut info) }.is_ok() => {
                let r = info.rcClient; // screen coordinates
                p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
            }
            _ => true, // no pointer or no rectangle: the window class alone decides
        };
        return if inside {
            (EDITABLE, format!("terminal window {window_class}"), false)
        } else {
            (
                NOT_EDITABLE,
                format!("terminal window {window_class}: pointer outside the client area"),
                false,
            )
        };
    }
    let Some(uia) = uia else {
        return (UNKNOWN, "no UI Automation".to_owned(), true);
    };
    unsafe {
        let element = match uia.automation.GetFocusedElementBuildCache(&uia.cache) {
            Ok(e) => e,
            Err(e) => {
                // A target that timed out once will not answer the next sample either.
                let timed_out = e.code().0 as u32 == UIA_E_TIMEOUT;
                return (UNKNOWN, format!("no focused element: {e}"), timed_out);
            }
        };
        let control_type = element.CachedControlType().unwrap_or_default();
        let framework = element
            .CachedFrameworkId()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let class_name = element
            .CachedClassName()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let what = format!(
            "type {} fw {framework} in {window_class}",
            control_type.0
        );
        if !element.CachedIsEnabled().map(|b| b.as_bool()).unwrap_or(true) {
            return (NOT_EDITABLE, format!("{what}: disabled"), false);
        }
        // Some(read only?) when the element has a value at all.
        let value_read_only = element
            .GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .ok()
            .map(|p| p.CachedIsReadOnly().map(|b| b.as_bool()).unwrap_or(true));
        let takes_text = [
            UIA_EditControlTypeId,
            UIA_ComboBoxControlTypeId,
            UIA_DocumentControlTypeId,
            UIA_DataItemControlTypeId,
            UIA_SpinnerControlTypeId,
            UIA_CustomControlTypeId,
            UIA_GroupControlTypeId,
            UIA_PaneControlTypeId,
        ]
        .contains(&control_type);
        let editable = match value_read_only {
            Some(read_only) => takes_text && !read_only,
            // An edit control without a value pattern (some custom ones): take its word.
            None => control_type == UIA_EditControlTypeId,
        } || class_name == "TermControl";
        if editable {
            return match (pointer, element.CachedBoundingRectangle()) {
                (Some(p), Ok(r)) if r.right > r.left && r.bottom > r.top => {
                    // Numbers only, to check the pointer test from the log.
                    let what = format!(
                        "{what} pointer {},{} field {},{} {}x{}",
                        p.x,
                        p.y,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top
                    );
                    if contains(&r, &p) {
                        (EDITABLE, format!("{what}: editable"), false)
                    } else if (r.right - r.left < PROXY_MAX_SIDE
                        && r.bottom - r.top < PROXY_MAX_SIDE)
                        || PROXY_CLASSES.iter().any(|c| class_name.contains(c))
                    {
                        (
                            UNKNOWN,
                            format!("{what}: editable, input proxy, pointer elsewhere"),
                            false,
                        )
                    } else {
                        (
                            NOT_EDITABLE,
                            format!("{what}: editable, pointer elsewhere"),
                            false,
                        )
                    }
                }
                // No rectangle to check against: the focus alone decides.
                _ => (EDITABLE, format!("{what}: editable"), false),
            };
        }
        // Read-only or not a text control. Elements that say nothing about their content
        // (a bare window or pane, a custom control, a document of a toolkit other than the
        // browser's) leave the decision to the viewer's cursor rule.
        let vague = [
            UIA_PaneControlTypeId,
            UIA_WindowControlTypeId,
            UIA_CustomControlTypeId,
            UIA_TitleBarControlTypeId,
        ]
        .contains(&control_type)
            || (control_type == UIA_DocumentControlTypeId && framework != "Chrome");
        if vague && value_read_only != Some(true) {
            (UNKNOWN, format!("{what}: no detail"), false)
        } else {
            (NOT_EDITABLE, format!("{what}: not editable"), false)
        }
    }
}

fn contains(r: &RECT, p: &POINT) -> bool {
    p.x >= r.left - RECT_MARGIN
        && p.x < r.right + RECT_MARGIN
        && p.y >= r.top - RECT_MARGIN
        && p.y < r.bottom + RECT_MARGIN
}
