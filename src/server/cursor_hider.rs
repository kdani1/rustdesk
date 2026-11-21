//! Hides the Windows mouse cursor while a touch viewer controls a virtual display (Windows only).
//!
//! A virtual (indirect) display without hardware-cursor support gets the cursor drawn into its
//! image by Windows: desktop duplication reports no separate pointer there, so the cursor is in
//! every captured frame. A viewer whose own pointer already shows where the mouse is (the Quest
//! app: its laser moves the remote mouse absolutely) then sees a second cursor glued to it, and
//! neither the capturer nor the viewer can take it out of the picture.
//!
//! `MagShowSystemCursor(FALSE)` takes it out at the source. Measured on the Amyuni display with
//! a desktop-duplication capture around the cursor: the arrow is in the frame, and gone after
//! the call. The system keeps reporting the cursor as showing (`GetCursorInfo`), so the cursor
//! shape and position that viewers receive are not affected. The hide belongs to this process:
//! Windows undoes it when the process ends, so a killed server cannot leave the PC without a
//! cursor.
//!
//! The call has no display parameter: the cursor goes away on every display of the session, a
//! real monitor included. So:
//! - every active display is virtual (nobody can be looking at a real monitor): hidden from the
//!   viewer's first mouse event until it disconnects or loses mouse control;
//! - a real monitor is active too: hidden only while the viewer is moving the mouse (`LEASE`),
//!   so a person at the PC gets the cursor back as soon as the viewer rests;
//! - no virtual display: never (the cursor is not in the picture of a real monitor).

use hbb_common::{config::Config, log};
use std::{
    collections::HashMap,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Host option, default on. "N" keeps the cursor in the picture.
const OPTION: &str = "enable-hide-cursor-on-virtual-display";
/// With a real monitor active, the cursor comes back this long after the viewer's last mouse
/// event.
const LEASE: Duration = Duration::from_secs(3);
/// How often the worker looks while a viewer holds.
const TICK: Duration = Duration::from_millis(500);
/// The displays are counted again, and a wanted hide is applied again, this often: a display
/// may appear or go while a viewer is connected, and a show from this process (the legacy
/// privacy mode uses the same API) must not leave the cursor visible.
const RECHECK: Duration = Duration::from_secs(2);

lazy_static::lazy_static! {
    // Connection id -> its last mouse event.
    static ref HOLDERS: Mutex<HashMap<i32, Instant>> = Mutex::new(HashMap::new());
    static ref WORKER: Mutex<Option<mpsc::Sender<()>>> = Mutex::new(None);
}

/// A mouse event of connection `conn_id`, a touch viewer in control of the mouse: keep the
/// cursor out of the picture of virtual displays (see the module comment for how long).
/// Called for every mouse event: cheap unless the state may change.
pub fn hold(conn_id: i32) {
    let now = Instant::now();
    let previous = HOLDERS.lock().unwrap().insert(conn_id, now);
    // A new holder, or one whose lease may have run out: the worker must look now.
    if previous.map_or(true, |t| now.duration_since(t) >= LEASE) {
        wake();
    }
}

/// The connection closed or lost mouse control (or never held).
pub fn release(conn_id: i32) {
    if HOLDERS.lock().unwrap().remove(&conn_id).is_some() {
        wake();
    }
}

fn wake() {
    let mut worker = WORKER.lock().unwrap();
    if let Some(sender) = worker.as_ref() {
        if sender.send(()).is_ok() {
            return;
        }
    }
    let (sender, receiver) = mpsc::channel::<()>();
    match std::thread::Builder::new()
        .name("cursor-hider".to_owned())
        .spawn(move || run(receiver))
    {
        Ok(_) => {
            sender.send(()).ok();
            *worker = Some(sender);
        }
        Err(e) => {
            log::error!("cursor hider: failed to start the thread: {e}");
            *worker = None;
        }
    }
}

fn run(receiver: mpsc::Receiver<()>) {
    let mut hidden = false;
    let mut failed_logged = false;
    // (virtual displays, all displays) and when they were counted.
    let mut displays = (0usize, 0usize);
    let mut counted: Option<Instant> = None;
    let mut applied: Option<Instant> = None;
    loop {
        let (holders, newest) = {
            let map = HOLDERS.lock().unwrap();
            (map.len(), map.values().max().copied())
        };
        let enabled = holders > 0 && Config::get_option(OPTION) != "N";
        if enabled && counted.map_or(true, |t| t.elapsed() >= RECHECK) {
            displays = crate::virtual_display_manager::amyuni_idd::display_counts();
            counted = Some(Instant::now());
        }
        let (virtual_displays, all_displays) = displays;
        let all_virtual = virtual_displays > 0 && virtual_displays == all_displays;
        let moving = newest.map_or(false, |t| t.elapsed() < LEASE);
        let want = enabled && virtual_displays > 0 && (all_virtual || moving);
        // Showing is done once; a wanted hide is repeated every RECHECK.
        if want != hidden || (want && applied.map_or(true, |t| t.elapsed() >= RECHECK)) {
            match scrap::dxgi::mag::show_system_cursor(!want) {
                Ok(()) => {
                    applied = Some(Instant::now());
                    if want != hidden {
                        hidden = want;
                        log::info!(
                            "cursor hider: system cursor {} (holders: {}, displays: {} virtual of {})",
                            if hidden { "hidden" } else { "shown" },
                            holders,
                            virtual_displays,
                            all_displays
                        );
                    }
                }
                Err(e) => {
                    if !failed_logged {
                        failed_logged = true;
                        log::error!("cursor hider: MagShowSystemCursor failed: {e}");
                    }
                }
            }
        }
        if holders == 0 {
            counted = None; // count afresh for the next holder
        }
        // While somebody holds, look again regularly; otherwise sleep until the next hold.
        if holders > 0 || hidden {
            match receiver.recv_timeout(TICK) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else if receiver.recv().is_err() {
            break;
        }
    }
    if hidden {
        scrap::dxgi::mag::show_system_cursor(true).ok();
    }
}
