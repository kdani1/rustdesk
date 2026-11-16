//! WLAN media streaming mode while video streams (host) or a remote session is open (viewer).
//! The Wi-Fi driver then defers background scans and power saving, which otherwise stall
//! packets for tens of milliseconds. Windows reverts the mode when our WLAN handle closes and
//! when the interface disconnects, so one worker thread keeps a single handle open while anyone
//! holds it and re-checks every few seconds. wlanapi.dll is loaded at run time: it is missing on
//! Windows Server without the Wireless LAN Service, where a static import would stop RustDesk
//! from starting.

use base::config::keys::OPTION_ENABLE_WLAN_STREAMING_MODE;
use hbb_common::{
    config::{Config, LocalConfig},
    log,
};
use std::{
    collections::HashSet,
    ffi::c_void,
    ptr::{null, null_mut},
    sync::{mpsc, Mutex, OnceLock},
    time::Duration,
};
use winapi::um::libloaderapi::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::{
    core::GUID,
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::WiFi::{
            wlan_interface_state_connected, wlan_intf_opcode_media_streaming_mode,
            WLAN_API_VERSION_2_0, WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST,
            WLAN_INTF_OPCODE,
        },
    },
};

/// Re-check while held: a reconnect or roam drops the mode, and the options may change.
const RECHECK: Duration = Duration::from_secs(10);
const LOG_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy)]
pub enum Side {
    Host,
    Viewer,
}

enum Msg {
    Acquire(Side),
    Release(Side),
}

pub fn acquire(side: Side) {
    send(Msg::Acquire(side));
}

pub fn release(side: Side) {
    send(Msg::Release(side));
}

/// Holds the mode while alive (viewer session).
pub struct Hold(Side);

pub fn hold(side: Side) -> Hold {
    acquire(side);
    Hold(side)
}

impl Drop for Hold {
    fn drop(&mut self) {
        release(self.0);
    }
}

fn send(msg: Msg) {
    static TX: OnceLock<Mutex<mpsc::Sender<Msg>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        if let Err(e) = std::thread::Builder::new()
            .name("wlan-streaming".into())
            .spawn(move || worker(rx))
        {
            log::warn!("WLAN media streaming mode thread not started: {e}");
        }
        Mutex::new(tx)
    });
    tx.lock().unwrap().send(msg).ok();
}

fn enabled(side: Side) -> bool {
    match side {
        Side::Host => Config::get_option(OPTION_ENABLE_WLAN_STREAMING_MODE) != "N",
        Side::Viewer => LocalConfig::get_option(OPTION_ENABLE_WLAN_STREAMING_MODE) != "N",
    }
}

fn worker(rx: mpsc::Receiver<Msg>) {
    let (mut host, mut viewer) = (0usize, 0usize);
    let mut client: Option<Client> = None;
    let mut unavailable = false; // wlanapi.dll or its exports missing: never retried
    loop {
        let msg = if host + viewer > 0 {
            match rx.recv_timeout(RECHECK) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(_) => break,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };
        match msg {
            Some(Msg::Acquire(Side::Host)) => host += 1,
            Some(Msg::Release(Side::Host)) => host = host.saturating_sub(1),
            Some(Msg::Acquire(Side::Viewer)) => viewer += 1,
            Some(Msg::Release(Side::Viewer)) => viewer = viewer.saturating_sub(1),
            None => {}
        }
        let want = (host > 0 && enabled(Side::Host)) || (viewer > 0 && enabled(Side::Viewer));
        if !want {
            if client.take().is_some() {
                log::info!("WLAN media streaming mode released");
            }
            if host + viewer == 0 {
                unavailable = false;
            }
            continue;
        }
        if client.is_none() && !unavailable {
            client = Client::open();
            // A failed WlanOpenHandle (service stopped or restarting) is retried every round.
            unavailable = client.is_none() && api().is_none();
        }
        if let Some(c) = client.as_mut() {
            if !c.apply() {
                // Handle went stale (for example the WLAN service restarted): reopen next round.
                client = None;
            }
        }
    }
}

type OpenFn = unsafe extern "system" fn(u32, *const c_void, *mut u32, *mut HANDLE) -> u32;
type CloseFn = unsafe extern "system" fn(HANDLE, *const c_void) -> u32;
type EnumFn =
    unsafe extern "system" fn(HANDLE, *const c_void, *mut *mut WLAN_INTERFACE_INFO_LIST) -> u32;
type QueryFn = unsafe extern "system" fn(
    HANDLE,
    *const GUID,
    WLAN_INTF_OPCODE,
    *const c_void,
    *mut u32,
    *mut *mut c_void,
    *mut c_void,
) -> u32;
type SetFn = unsafe extern "system" fn(
    HANDLE,
    *const GUID,
    WLAN_INTF_OPCODE,
    u32,
    *const c_void,
    *const c_void,
) -> u32;
type FreeFn = unsafe extern "system" fn(*const c_void);

struct Api {
    open: OpenFn,
    close: CloseFn,
    enum_interfaces: EnumFn,
    query: QueryFn,
    set: SetFn,
    free: FreeFn,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe { load() }).as_ref()
}

unsafe fn load() -> Option<Api> {
    let name: Vec<u16> = "wlanapi.dll\0".encode_utf16().collect();
    let module = LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32);
    if module.is_null() {
        log::info!("wlanapi.dll not available, WLAN media streaming mode off");
        return None;
    }
    macro_rules! sym {
        ($n:literal) => {{
            let f = GetProcAddress(module, concat!($n, "\0").as_ptr() as _);
            if f.is_null() {
                log::warn!("wlanapi.dll lacks {}", $n);
                return None;
            }
            std::mem::transmute(f)
        }};
    }
    Some(Api {
        open: sym!("WlanOpenHandle"),
        close: sym!("WlanCloseHandle"),
        enum_interfaces: sym!("WlanEnumInterfaces"),
        query: sym!("WlanQueryInterface"),
        set: sym!("WlanSetInterface"),
        free: sym!("WlanFreeMemory"),
    })
}

/// One WLAN client handle; closing it lets Windows revert our media streaming mode.
struct Client {
    api: &'static Api,
    handle: HANDLE,
    /// Interfaces this handle has turned the mode on for.
    set: HashSet<GUID>,
}

impl Client {
    fn open() -> Option<Self> {
        let api = api()?;
        let (mut version, mut handle) = (0u32, HANDLE::default());
        let r = unsafe { (api.open)(WLAN_API_VERSION_2_0, null(), &mut version, &mut handle) };
        if r != 0 {
            // Service stopped or not installed (Ethernet-only PCs): stay off quietly.
            hbb_common::throttled_log!(
                LOG_INTERVAL,
                info,
                "WLAN service not available ({r}), media streaming mode off"
            );
            return None;
        }
        Some(Self {
            api,
            handle,
            set: HashSet::new(),
        })
    }

    fn is_on(&self, guid: &GUID) -> bool {
        let (mut size, mut data) = (0u32, null_mut());
        let r = unsafe {
            (self.api.query)(
                self.handle,
                guid,
                wlan_intf_opcode_media_streaming_mode,
                null(),
                &mut size,
                &mut data,
                null_mut(),
            )
        };
        if r != 0 || data.is_null() {
            return false;
        }
        let on = size >= 4 && unsafe { *(data as *const i32) } != 0;
        unsafe { (self.api.free)(data) };
        on
    }

    /// Turns the mode on for each connected WLAN interface where it is not on yet.
    /// Returns false when the handle no longer works.
    fn apply(&mut self) -> bool {
        let mut list = null_mut();
        let r = unsafe { (self.api.enum_interfaces)(self.handle, null(), &mut list) };
        if r != 0 || list.is_null() {
            hbb_common::throttled_log!(LOG_INTERVAL, info, "WLAN interface list failed: {r}");
            return false;
        }
        let items: &[WLAN_INTERFACE_INFO] = unsafe {
            // Not through a reference to the declared one-element array.
            let first = std::ptr::addr_of!((*list).InterfaceInfo) as *const WLAN_INTERFACE_INFO;
            std::slice::from_raw_parts(first, (*list).dwNumberOfItems as usize)
        };
        let connected: Vec<&WLAN_INTERFACE_INFO> = items
            .iter()
            .filter(|i| i.isState == wlan_interface_state_connected)
            .collect();
        // Windows drops the mode on disconnect: forget interfaces that are gone or disconnected.
        self.set
            .retain(|g| connected.iter().any(|i| i.InterfaceGuid == *g));
        for i in connected {
            let guid = i.InterfaceGuid;
            if self.set.contains(&guid) && self.is_on(&guid) {
                continue;
            }
            let on: i32 = 1;
            let r = unsafe {
                (self.api.set)(
                    self.handle,
                    &guid,
                    wlan_intf_opcode_media_streaming_mode,
                    4,
                    &on as *const i32 as _,
                    null(),
                )
            };
            let name = String::from_utf16_lossy(&i.strInterfaceDescription);
            let name = name.trim_end_matches('\0');
            if r == 0 {
                if self.set.insert(guid) {
                    log::info!("WLAN media streaming mode on: {name}");
                }
            } else {
                hbb_common::throttled_log!(
                    LOG_INTERVAL,
                    warn,
                    "WLAN media streaming mode failed on {name}: {r}"
                );
            }
        }
        unsafe { (self.api.free)(list as _) };
        true
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe { (self.api.close)(self.handle, null()) };
    }
}
