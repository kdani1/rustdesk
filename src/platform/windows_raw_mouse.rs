//! Raw-input relative mouse for the Windows viewer (relative mouse mode).
//!
//! The Flutter path derives deltas from cursor positions, recenters with SetCursorPos and
//! batches sends on a 16 ms timer. Games such as CS2 then see stalls at the window edge,
//! jumps after a recenter and stutter. Here one thread reads WM_INPUT, which carries the
//! mouse's own counts (no pointer acceleration, no DPI scaling), and sends each drained batch
//! straight to the session. The cursor is confined to one pixel at the centre of the session
//! window, so it cannot leave it.
//!
//! Buttons come from the same packets, in order with the motion. Flutter reports only the first
//! pressed button as a press; a second one (right while left is held) arrives as a move and was
//! lost, so chords and held buttons work only from here.
//!
//! Raw-input registration is per process, so at most one session owns this at a time.

use base::config::keys::OPTION_ENABLE_RAW_RELATIVE_MOUSE;
use hbb_common::{config::LocalConfig, log};
use std::{
    sync::{mpsc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawMouseEvent {
    /// Relative motion in mouse counts.
    Move(i32, i32),
    Button { button: RawButton, down: bool },
}

/// Receives the events in the order the mouse produced them. Called on the raw-input thread.
pub type Sink = Box<dyn Fn(RawMouseEvent) + Send + 'static>;

// Same bound as the server and the Flutter client (kMaxRelativeMouseDelta).
const MAX_DELTA: i64 = 10000;
const WM_STOP: u32 = winapi::um::winuser::WM_APP + 0x52;

struct Owner {
    owner: String,
    thread_id: u32,
    join: JoinHandle<()>,
}

lazy_static::lazy_static! {
    static ref OWNER: Mutex<Option<Owner>> = Mutex::new(None);
}

/// Starts raw-input relative deltas for `owner` (a session id).
///
/// Returns 1 when started (or already owned by `owner`), 0 when the foreground window is not
/// this process's or the cursor is not over it yet (the caller may retry), -2 when another
/// session owns raw input, and -1 when unavailable (turned off, registration failed). On -1
/// and -2 a manual enable keeps its old path.
pub fn start(owner: String, sink: Sink) -> i32 {
    use winapi::{
        shared::windef::POINT,
        um::{processthreadsapi::GetCurrentProcessId, winuser::*},
    };

    if LocalConfig::get_option(OPTION_ENABLE_RAW_RELATIVE_MOUSE) == "N" {
        return -1;
    }
    let mut guard = OWNER.lock().unwrap();
    if let Some(current) = guard.as_ref() {
        return if current.owner == owner { 1 } else { -2 };
    }
    // The session window is the one the user is in when relative mode starts: in front, ours,
    // and under the cursor (a hover over a background window must not latch the front one).
    let hwnd = unsafe { GetForegroundWindow() };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if hwnd.is_null() || pid != unsafe { GetCurrentProcessId() } {
        return 0;
    }
    let mut cursor = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == 0
        || unsafe { GetAncestor(WindowFromPoint(cursor), GA_ROOT) } != hwnd
    {
        return 0;
    }
    let hwnd = hwnd as usize;
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let join = std::thread::spawn(move || run(hwnd, sink, ready_tx));
    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(thread_id)) => {
            log::info!("raw relative mouse started for {owner}");
            *guard = Some(Owner {
                owner,
                thread_id,
                join,
            });
            1
        }
        Ok(Err(e)) => {
            log::warn!("raw relative mouse unavailable: {e}");
            -1
        }
        Err(_) => {
            // The thread has not reported back; it stops on its own once it does.
            log::warn!("raw relative mouse did not start in time");
            -1
        }
    }
}

/// Stops raw-input deltas if `owner` owns them. Idempotent. Waits until the thread has removed
/// its registration, so a following `start` cannot be unregistered by it.
pub fn stop(owner: &str) {
    use winapi::um::winuser::PostThreadMessageW;

    let taken = {
        let mut guard = OWNER.lock().unwrap();
        match guard.as_ref() {
            Some(current) if current.owner == owner => guard.take(),
            _ => None,
        }
    };
    if let Some(current) = taken {
        unsafe { PostThreadMessageW(current.thread_id, WM_STOP, 0, 0) };
        let _ = current.join.join();
        log::info!("raw relative mouse stopped for {owner}");
    }
}

fn run(hwnd: usize, sink: Sink, ready: mpsc::Sender<Result<u32, String>>) {
    use std::ptr::null_mut;
    use winapi::um::{libloaderapi::GetModuleHandleW, processthreadsapi::GetCurrentThreadId, winuser::*};

    let target = hwnd as winapi::shared::windef::HWND;
    unsafe {
        let class: Vec<u16> = "RustDeskRawMouse\0".encode_utf16().collect();
        let instance = GetModuleHandleW(null_mut());
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as _;
        wc.lpfnWndProc = Some(DefWindowProcW);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        // Fails harmlessly with ERROR_CLASS_ALREADY_EXISTS after the first start.
        RegisterClassExW(&wc);
        let window = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            instance,
            null_mut(),
        );
        if window.is_null() {
            let _ = ready.send(Err(format!(
                "CreateWindowExW failed: {}",
                std::io::Error::last_os_error()
            )));
            return;
        }
        if mouse_registered_elsewhere() {
            DestroyWindow(window);
            let _ = ready.send(Err("raw mouse input already registered in this process".into()));
            return;
        }
        let device = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            // No RIDEV_NOLEGACY: Flutter keeps getting button and move messages.
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        if RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as _) == 0 {
            let _ = ready.send(Err(format!(
                "RegisterRawInputDevices failed: {}",
                std::io::Error::last_os_error()
            )));
            DestroyWindow(window);
            return;
        }
        let mut clip = Clip::default();
        clip.ensure(target);
        if ready.send(Ok(GetCurrentThreadId())).is_err() {
            // start() gave up waiting: undo and leave.
            clip.release();
            unregister(window);
            return;
        }

        let mut pump = Pump {
            // The local "swap primary and secondary buttons" setting, as legacy messages apply it.
            swap: GetSystemMetrics(SM_SWAPBUTTON) != 0,
            ..Default::default()
        };
        // Buttons already held now were pressed on the peer by the Flutter path, and Dart
        // mutes its buttons from here on: this thread owns their release. (An extra release for
        // a button the peer never saw pressed does nothing.)
        for (vk, button) in [
            (VK_LBUTTON, RawButton::Left),
            (VK_RBUTTON, RawButton::Right),
            (VK_MBUTTON, RawButton::Middle),
            (VK_XBUTTON1, RawButton::Back),
            (VK_XBUTTON2, RawButton::Forward),
        ] {
            // GetAsyncKeyState reports physical buttons; map them like the raw flags.
            if (GetAsyncKeyState(vk) as u16) & 0x8000 != 0 {
                pump.pressed |= 1u8 << pump.logical(button) as u8;
            }
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.message == WM_STOP {
                break;
            }
            if msg.message != WM_INPUT {
                DispatchMessageW(&msg);
                continue;
            }
            let front = GetForegroundWindow() == target;
            if front {
                clip.ensure(target);
            } else {
                // Alt+Tab or another window: free the cursor, keep the game still.
                clip.release();
            }
            pump.packet(msg.lParam, front, &sink);
            DispatchMessageW(&msg);
            // Coalesce whatever motion arrived meanwhile into one send.
            while PeekMessageW(&mut msg, null_mut(), WM_INPUT, WM_INPUT, PM_REMOVE) != 0 {
                pump.packet(msg.lParam, front, &sink);
                DispatchMessageW(&msg);
            }
            pump.flush_move(front, &sink);
        }
        // Never leave a button held on the peer.
        pump.release_all(&sink);
        clip.release();
        unregister(window);
    }
}

/// Motion accumulated since the last send, and the buttons this thread has pressed on the peer.
#[derive(Default)]
struct Pump {
    dx: i64,
    dy: i64,
    pressed: u8,
    swap: bool,
}

impl Pump {
    unsafe fn packet(&mut self, lparam: isize, front: bool, sink: &Sink) {
        use winapi::um::winuser::*;
        // (down flag, up flag, physical button)
        const BUTTONS: [(u16, u16, RawButton); 5] = [
            (RI_MOUSE_LEFT_BUTTON_DOWN, RI_MOUSE_LEFT_BUTTON_UP, RawButton::Left),
            (RI_MOUSE_RIGHT_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_UP, RawButton::Right),
            (RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_MIDDLE_BUTTON_UP, RawButton::Middle),
            (RI_MOUSE_BUTTON_4_DOWN, RI_MOUSE_BUTTON_4_UP, RawButton::Back),
            (RI_MOUSE_BUTTON_5_DOWN, RI_MOUSE_BUTTON_5_UP, RawButton::Forward),
        ];

        let mut raw: RAWINPUT = std::mem::zeroed();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let read = GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            &mut raw as *mut RAWINPUT as *mut _,
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as _,
        );
        if read == u32::MAX || raw.header.dwType != RIM_TYPEMOUSE {
            return;
        }
        let mouse = raw.data.mouse();
        // Absolute devices (pen, tablet, a session driven over RDP) carry no relative motion.
        if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
            self.dx += mouse.lLastX as i64;
            self.dy += mouse.lLastY as i64;
        }
        let flags = mouse.usButtonFlags;
        if BUTTONS.iter().all(|(down, up, _)| flags & (down | up) == 0) {
            return;
        }
        // The motion before this packet's buttons goes first.
        self.flush_move(front, sink);
        for (down, up, button) in BUTTONS {
            let button = self.logical(button);
            let bit = 1u8 << button as u8;
            if flags & down != 0 && front && self.pressed & bit == 0 {
                self.pressed |= bit;
                sink(RawMouseEvent::Button { button, down: true });
            }
            // A release always goes out for a button pressed on the peer, even after Alt+Tab.
            if flags & up != 0 && self.pressed & bit != 0 {
                self.pressed &= !bit;
                sink(RawMouseEvent::Button { button, down: false });
            }
        }
    }

    fn flush_move(&mut self, front: bool, sink: &Sink) {
        if front && (self.dx != 0 || self.dy != 0) {
            sink(RawMouseEvent::Move(
                self.dx.clamp(-MAX_DELTA, MAX_DELTA) as i32,
                self.dy.clamp(-MAX_DELTA, MAX_DELTA) as i32,
            ));
        }
        self.dx = 0;
        self.dy = 0;
    }

    fn release_all(&mut self, sink: &Sink) {
        for button in [
            RawButton::Left,
            RawButton::Right,
            RawButton::Middle,
            RawButton::Back,
            RawButton::Forward,
        ] {
            let bit = 1u8 << button as u8;
            if self.pressed & bit != 0 {
                self.pressed &= !bit;
                sink(RawMouseEvent::Button { button, down: false });
            }
        }
    }

    fn logical(&self, button: RawButton) -> RawButton {
        match (self.swap, button) {
            (true, RawButton::Left) => RawButton::Right,
            (true, RawButton::Right) => RawButton::Left,
            _ => button,
        }
    }
}

/// True if something else in this process already receives raw mouse input. Registration is
/// per process and usage, so registering would take it away from that owner.
unsafe fn mouse_registered_elsewhere() -> bool {
    use winapi::um::winuser::*;

    let item = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
    let mut count = 0u32;
    GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count, item);
    if count == 0 {
        return false;
    }
    let mut devices = vec![std::mem::zeroed::<RAWINPUTDEVICE>(); count as usize];
    if GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, item) == u32::MAX {
        return false;
    }
    devices
        .iter()
        .take(count as usize)
        .any(|d| d.usUsagePage == 1 && d.usUsage == 2)
}

unsafe fn unregister(window: winapi::shared::windef::HWND) {
    use winapi::um::winuser::*;

    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: RIDEV_REMOVE,
        hwndTarget: std::ptr::null_mut(),
    };
    RegisterRawInputDevices(&device, 1, std::mem::size_of::<RAWINPUTDEVICE>() as _);
    DestroyWindow(window);
}

/// The one-pixel confinement at the centre of the session window's client area.
#[derive(Default)]
struct Clip {
    applied: Option<winapi::shared::windef::RECT>,
}

impl Clip {
    /// Applies the clip if it is missing or out of date (window moved, Windows dropped it).
    unsafe fn ensure(&mut self, hwnd: winapi::shared::windef::HWND) {
        use winapi::{shared::windef::*, um::winuser::*};

        let mut client: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut client) == 0 {
            return;
        }
        let mut centre = POINT {
            x: (client.right - client.left) / 2,
            y: (client.bottom - client.top) / 2,
        };
        if ClientToScreen(hwnd, &mut centre) == 0 {
            return;
        }
        let want = RECT {
            left: centre.x,
            top: centre.y,
            right: centre.x + 1,
            bottom: centre.y + 1,
        };
        let mut current: RECT = std::mem::zeroed();
        let current_ok = GetClipCursor(&mut current) != 0;
        if !current_ok || !same_rect(&current, &want) {
            if ClipCursor(&want) != 0 {
                // Windows clamps the rect to the virtual screen (window partly off-screen or
                // minimised): remember what it applied, so release() recognises it.
                let mut actual = want;
                if GetClipCursor(&mut actual) == 0 {
                    actual = want;
                }
                self.applied = Some(actual);
            }
        } else {
            self.applied = Some(want);
        }
    }

    /// Releases the clip only if it is still ours (a game or the OS may have replaced it).
    unsafe fn release(&mut self) {
        use winapi::um::winuser::*;

        if let Some(ours) = self.applied.take() {
            let mut current: winapi::shared::windef::RECT = std::mem::zeroed();
            if GetClipCursor(&mut current) != 0 && same_rect(&current, &ours) {
                ClipCursor(std::ptr::null());
            }
        }
    }
}

fn same_rect(a: &winapi::shared::windef::RECT, b: &winapi::shared::windef::RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}
