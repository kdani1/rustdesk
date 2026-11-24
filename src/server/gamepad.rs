//! Game mode: a virtual Xbox 360 pad per viewer connection, fed by `Misc.gamepad_state`
//! (Windows only, through the ViGEmBus driver and the `vigem-client` crate).
//!
//! One worker thread owns the bus `Client` and every pad, so no call from a connection ever
//! waits on the driver:
//! - a pad is plugged in on the first state of a connection (Windows plays its connect sound
//!   once per game-mode entry), at most `MAX_PADS` at a time (the XInput limit);
//! - after `NEUTRAL_AFTER` without a state the pad is set neutral but stays plugged in (the
//!   viewer's heartbeat stopped: headset off, Quest menu, a stall), so nothing stays pressed;
//! - it is unplugged only on `off = true` (the viewer left game mode), when the connection
//!   closes or loses the keyboard permission (`release`), or after `UNPLUG_AFTER` without a
//!   state. A long pause in a game must not hot-unplug the pad, so this is minutes, not
//!   seconds.
//!
//! Every value is clamped and the Guide button (0x0400) and the unused bit 0x0800 are masked.
//! `seq` is for logging only: the session stream is ordered, and the viewer restarts the
//! counter at every game-mode entry, so no state is ever dropped by it.
//!
//! The worker never falls behind: the states waiting in its queue are coalesced (the newest
//! per connection wins; `off` and releases keep their order), and after a failed bus connect,
//! plug-in or update it does not touch the driver again for `PLUG_RETRY` (the states of that
//! time are dropped; the viewer sends a full snapshot at least every 200 ms).
//!
//! `availability` answers `Features.gamepad` for the PeerInfo without blocking the login: the
//! driver check runs on its own thread, primed when the connection opens, and the answer is
//! cached (`RECHECK_MISSING` while the driver is missing, `RECHECK_PRESENT` while it is there).
//!
//! Log lines start with "LATENCY gamepad" (plug, unplug, neutral, errors).

use base::message_proto::GamepadState;
use hbb_common::{config::Config, log};
use std::{
    collections::HashMap,
    rc::Rc,
    sync::{
        mpsc::{self, RecvTimeoutError},
        Mutex,
    },
    time::{Duration, Instant},
};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

/// `Features.gamepad`: not offered (option off, or not a remote-control connection).
pub const UNSUPPORTED: u32 = 0;
/// `Features.gamepad`: ViGEmBus answered, pads can be plugged in.
pub const AVAILABLE: u32 = 1;
/// `Features.gamepad`: game mode is supported here but the driver is missing or unusable.
pub const NO_DRIVER: u32 = 2;

/// Host option, default on. "N" turns the virtual pad off (`Features.gamepad` = 0).
const OPTION: &str = "enable-gamepad";
/// XInput allows four controllers.
const MAX_PADS: usize = 4;
/// Without a state for this long the pad goes neutral (the viewer heartbeats every 200 ms).
const NEUTRAL_AFTER: Duration = Duration::from_millis(600);
/// Without a state for this long the pad is unplugged (critique #5: not a short timeout).
const UNPLUG_AFTER: Duration = Duration::from_secs(120);
/// How often the worker looks at its pads while it has any.
const TICK: Duration = Duration::from_millis(100);
/// Driver check: cached this long while it is missing (and the "missing" log repeats at most
/// this often), and this long while it is present.
const RECHECK_MISSING: Duration = Duration::from_secs(30);
const RECHECK_PRESENT: Duration = Duration::from_secs(300);
/// How long `availability` waits for a check that is running (normally a few milliseconds).
const AVAILABILITY_WAIT: Duration = Duration::from_millis(200);
/// After a failed bus connect, plug-in or update: no new attempt for this long.
const PLUG_RETRY: Duration = Duration::from_secs(3);
/// A neutral that keeps failing with `TargetNotReady` this long (any other error at once)
/// unplugs the pad: dropping the target is the sure way to clear a held input.
const NEUTRAL_GIVE_UP: Duration = Duration::from_secs(1);
/// The XInput buttons a viewer may press: everything but Guide (0x0400) and 0x0800 (unused).
const BUTTON_MASK: u32 = 0xF3FF;
/// Repeated errors of one kind are logged at most this often.
const ERROR_LOG_EVERY: Duration = Duration::from_secs(30);

enum Cmd {
    State(i32, GamepadState),
    Release(i32, &'static str),
}

#[derive(Default)]
struct Availability {
    value: u32,
    checked: Option<Instant>,
    checking: bool,
    missing_logged: Option<Instant>,
}

lazy_static::lazy_static! {
    static ref WORKER: Mutex<Option<mpsc::Sender<Cmd>>> = Mutex::new(None);
    static ref AVAILABILITY: (Mutex<Availability>, std::sync::Condvar) =
        (Mutex::new(Availability::default()), std::sync::Condvar::new());
}

fn enabled() -> bool {
    Config::get_option(OPTION) != "N"
}

/// Starts a driver check in the background if the cached answer is stale. Never blocks.
/// Called when a connection opens, so the answer is ready by the time its PeerInfo is built.
pub fn prime() {
    if enabled() {
        start_check();
    }
}

/// `Features.gamepad` for a remote-control connection: `AVAILABLE`, `NO_DRIVER`, or
/// `UNSUPPORTED` when the host option is off. Waits at most `AVAILABILITY_WAIT` for a check
/// that is running; an unknown answer counts as `NO_DRIVER`.
pub fn availability() -> u32 {
    if !enabled() {
        return UNSUPPORTED;
    }
    start_check();
    let (lock, cvar) = &*AVAILABILITY;
    let mut a = lock.lock().unwrap();
    let deadline = Instant::now() + AVAILABILITY_WAIT;
    while a.checking {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        a = cvar.wait_timeout(a, deadline - now).unwrap().0;
    }
    if a.checked.is_some() {
        a.value
    } else {
        NO_DRIVER
    }
}

fn start_check() {
    {
        let (lock, _) = &*AVAILABILITY;
        let mut a = lock.lock().unwrap();
        let ttl = if a.value == AVAILABLE {
            RECHECK_PRESENT
        } else {
            RECHECK_MISSING
        };
        let fresh = a.checked.map_or(false, |t| t.elapsed() < ttl);
        if a.checking || fresh {
            return;
        }
        a.checking = true;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("gamepad-check".to_owned())
        .spawn(|| {
            let result = Client::connect().map(|_| ());
            record_driver(result);
        })
    {
        log::error!("LATENCY gamepad error: failed to start the driver check: {e}");
        let (lock, cvar) = &*AVAILABILITY;
        lock.lock().unwrap().checking = false;
        cvar.notify_all();
    }
}

/// Stores the result of a driver check (or of the worker's own connect) in the cache.
fn record_driver(result: Result<(), vigem_client::Error>) {
    let (lock, cvar) = &*AVAILABILITY;
    let mut a = lock.lock().unwrap();
    let now = Instant::now();
    let previous = a.checked.map(|_| a.value);
    match result {
        Ok(()) => {
            a.value = AVAILABLE;
            if previous != Some(AVAILABLE) {
                log::info!("LATENCY gamepad driver: ViGEmBus available");
            }
        }
        Err(e) => {
            a.value = NO_DRIVER;
            if a.missing_logged
                .map_or(true, |t| now.duration_since(t) >= RECHECK_MISSING)
            {
                a.missing_logged = Some(now);
                log::warn!("LATENCY gamepad driver: ViGEmBus not usable ({e}), Features.gamepad=2");
            }
        }
    }
    a.checked = Some(now);
    a.checking = false;
    cvar.notify_all();
}

/// A `Misc.gamepad_state` from connection `conn_id`, already checked for the remote-control
/// connection type and the keyboard permission. Returns at once; the worker applies it.
pub fn apply(conn_id: i32, state: GamepadState) {
    if !enabled() {
        return;
    }
    send(Cmd::State(conn_id, state), true);
}

/// Connection `conn_id` closed or lost the keyboard permission: unplug its pad, if it has one.
pub fn release(conn_id: i32, reason: &'static str) {
    // No worker yet means no pad yet: do not start a thread for every closing connection.
    send(Cmd::Release(conn_id, reason), false);
}

fn send(cmd: Cmd, start: bool) {
    let mut worker = WORKER.lock().unwrap();
    let cmd = match worker.as_ref() {
        Some(sender) => match sender.send(cmd) {
            Ok(()) => return,
            Err(mpsc::SendError(cmd)) => cmd,
        },
        None => cmd,
    };
    if !start {
        return;
    }
    let (sender, receiver) = mpsc::channel::<Cmd>();
    match std::thread::Builder::new()
        .name("gamepad".to_owned())
        .spawn(move || Worker::default().run(receiver))
    {
        Ok(_) => {
            sender.send(cmd).ok();
            *worker = Some(sender);
        }
        Err(e) => {
            log::error!("LATENCY gamepad error: failed to start the worker thread: {e}");
            *worker = None;
        }
    }
}

struct Pad {
    target: Xbox360Wired<Rc<Client>>,
    last_seq: Option<u32>,
    last_state: Instant,
    /// The neutral report after `NEUTRAL_AFTER` took effect.
    neutral: bool,
    /// Since when the neutral report keeps failing.
    neutral_failing: Option<Instant>,
    plugged: Instant,
    applied: u64,
    /// States replaced in the queue by a newer one before the worker got to them.
    coalesced: u64,
}

#[derive(Default)]
struct Worker {
    client: Option<Rc<Client>>,
    pads: HashMap<i32, Pad>,
    /// Error kind -> when it was last logged.
    logged: HashMap<&'static str, Instant>,
    /// No bus connect or plug-in before this (set after a failure).
    retry_after: Option<Instant>,
    /// States dropped while `retry_after` held, since the last log of it.
    backoff_dropped: u64,
}

impl Worker {
    fn run(mut self, receiver: mpsc::Receiver<Cmd>) {
        let mut disconnected = false;
        while !disconnected {
            let first = if self.pads.is_empty() {
                match receiver.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => break,
                }
            } else {
                match receiver.recv_timeout(TICK) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };
            if let Some(first) = first {
                // Everything queued meanwhile, coalesced: releases and `off` in arrival order
                // (each drops the states of its connection queued before it), then the newest
                // state of each connection.
                let mut pending: Vec<(i32, GamepadState)> = Vec::new();
                let mut next = Some(first);
                while let Some(cmd) = next.take() {
                    match cmd {
                        Cmd::State(conn_id, state) if !state.off => {
                            if let Some(slot) = pending.iter_mut().find(|(id, _)| *id == conn_id)
                            {
                                slot.1 = state;
                                if let Some(pad) = self.pads.get_mut(&conn_id) {
                                    pad.coalesced += 1;
                                }
                            } else {
                                pending.push((conn_id, state));
                            }
                        }
                        Cmd::State(conn_id, _) => {
                            pending.retain(|(id, _)| *id != conn_id);
                            self.unplug(conn_id, "off");
                        }
                        Cmd::Release(conn_id, reason) => {
                            pending.retain(|(id, _)| *id != conn_id);
                            self.unplug(conn_id, reason);
                        }
                    }
                    next = match receiver.try_recv() {
                        Ok(cmd) => Some(cmd),
                        Err(mpsc::TryRecvError::Empty) => None,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            None
                        }
                    };
                }
                for (conn_id, state) in pending {
                    self.on_state(conn_id, state);
                }
            }
            self.tick();
        }
        let ids: Vec<i32> = self.pads.keys().copied().collect();
        for conn_id in ids {
            self.unplug(conn_id, "worker exit");
        }
    }

    fn log_error(&mut self, kind: &'static str, text: impl FnOnce() -> String) {
        let now = Instant::now();
        if self
            .logged
            .get(kind)
            .map_or(true, |t| now.duration_since(*t) >= ERROR_LOG_EVERY)
        {
            self.logged.insert(kind, now);
            log::error!("LATENCY gamepad error {kind}: {}", text());
        }
    }

    /// A failed bus connect, plug-in or update: leave the driver alone for `PLUG_RETRY`.
    fn back_off(&mut self) {
        self.retry_after = Some(Instant::now() + PLUG_RETRY);
    }

    fn on_state(&mut self, conn_id: i32, state: GamepadState) {
        if state.off {
            self.unplug(conn_id, "off");
            return;
        }
        if !self.pads.contains_key(&conn_id) && !self.plug(conn_id) {
            return;
        }
        let Some(pad) = self.pads.get_mut(&conn_id) else {
            return;
        };
        pad.last_seq = Some(state.seq);
        pad.last_state = Instant::now();
        let report = to_report(&state);
        match pad.target.update(&report) {
            Ok(()) => {
                pad.applied += 1;
                pad.neutral_failing = None;
                if pad.neutral {
                    pad.neutral = false;
                    log::info!("LATENCY gamepad resume conn={conn_id} seq={}", state.seq);
                }
            }
            // Just plugged in and not ready yet: the next state (or the heartbeat) retries.
            Err(vigem_client::Error::TargetNotReady) => {
                self.log_error("not-ready", || format!("conn={conn_id} target not ready"));
            }
            Err(e) => {
                self.log_error("update", || format!("conn={conn_id} update failed: {e}"));
                // The bus went away or the target broke: plug in again, after the back-off.
                self.unplug(conn_id, "update error");
                self.back_off();
                if self.pads.is_empty() {
                    self.client = None;
                }
            }
        }
    }

    fn plug(&mut self, conn_id: i32) -> bool {
        if self.pads.len() >= MAX_PADS {
            self.log_error("too-many", || {
                format!("conn={conn_id} not plugged: {MAX_PADS} pads in use")
            });
            return false;
        }
        // After a failure: no driver call (a connect is a full device enumeration, a failing
        // plug-in may retry up to 65535 serial numbers) until the back-off is over.
        if let Some(at) = self.retry_after {
            if Instant::now() < at {
                self.backoff_dropped += 1;
                return false;
            }
            self.retry_after = None;
            if self.backoff_dropped > 0 {
                log::info!(
                    "LATENCY gamepad retry conn={conn_id} after back-off, states_dropped={}",
                    self.backoff_dropped
                );
                self.backoff_dropped = 0;
            }
        }
        if self.client.is_none() {
            match Client::connect() {
                Ok(client) => {
                    record_driver(Ok(()));
                    self.client = Some(Rc::new(client));
                }
                Err(e) => {
                    record_driver(Err(e));
                    self.log_error("connect", || format!("conn={conn_id} bus connect: {e}"));
                    self.back_off();
                    return false;
                }
            }
        }
        let Some(client) = self.client.clone() else {
            return false;
        };
        let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        if let Err(e) = target.plugin() {
            self.log_error("plugin", || format!("conn={conn_id} plugin: {e}"));
            self.back_off();
            if self.pads.is_empty() {
                self.client = None;
            }
            return false;
        }
        if let Err(e) = target.wait_ready() {
            self.log_error("ready", || format!("conn={conn_id} wait_ready: {e}"));
        }
        let index = target.get_user_index().map(|i| i as i64).unwrap_or(-1);
        log::info!(
            "LATENCY gamepad plug conn={conn_id} xinput_index={index} pads={}",
            self.pads.len() + 1
        );
        let now = Instant::now();
        self.pads.insert(
            conn_id,
            Pad {
                target,
                last_seq: None,
                last_state: now,
                neutral: false,
                neutral_failing: None,
                plugged: now,
                applied: 0,
                coalesced: 0,
            },
        );
        true
    }

    fn unplug(&mut self, conn_id: i32, reason: &'static str) {
        let Some(mut pad) = self.pads.remove(&conn_id) else {
            return;
        };
        // Release everything first, so a game never sees a button held across the unplug.
        pad.target.update(&XGamepad::default()).ok();
        let result = pad.target.unplug();
        match &result {
            Ok(()) => log::info!(
                "LATENCY gamepad unplug conn={conn_id} reason={reason} plugged_s={} applied={} coalesced={} last_seq={:?}",
                pad.plugged.elapsed().as_secs(),
                pad.applied,
                pad.coalesced,
                pad.last_seq
            ),
            Err(e) => log::error!(
                "LATENCY gamepad error unplug conn={conn_id} reason={reason}: {e}"
            ),
        }
        drop(pad);
        if self.pads.is_empty() {
            // Reconnect on the next plug: the driver may have been reinstalled meanwhile.
            self.client = None;
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let mut silent = Vec::new();
        let mut broken = Vec::new();
        let mut errors = Vec::new();
        for (conn_id, pad) in self.pads.iter_mut() {
            let idle = now.duration_since(pad.last_state);
            if idle >= UNPLUG_AFTER {
                silent.push(*conn_id);
            } else if idle >= NEUTRAL_AFTER && !pad.neutral {
                // `neutral` only once the report took effect: a failed one is tried again on
                // the next tick, so nothing stays pressed.
                match pad.target.update(&XGamepad::default()) {
                    Ok(()) => {
                        pad.neutral = true;
                        pad.neutral_failing = None;
                        log::info!(
                            "LATENCY gamepad neutral conn={conn_id} idle_ms={}",
                            idle.as_millis()
                        );
                    }
                    Err(e) => {
                        let since = *pad.neutral_failing.get_or_insert(now);
                        let give_up = !matches!(e, vigem_client::Error::TargetNotReady)
                            || now.duration_since(since) >= NEUTRAL_GIVE_UP;
                        errors.push(format!("conn={conn_id} neutral failed: {e}"));
                        if give_up {
                            broken.push(*conn_id);
                        }
                    }
                }
            }
        }
        for text in errors {
            self.log_error("neutral", || text);
        }
        for conn_id in silent {
            self.unplug(conn_id, "silence");
        }
        if !broken.is_empty() {
            for conn_id in broken {
                // Unplugging removes the device: the sure way to clear a held input.
                self.unplug(conn_id, "neutral failed");
            }
            self.back_off();
        }
    }
}

fn to_report(state: &GamepadState) -> XGamepad {
    let axis = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let trigger = |v: u32| v.min(u8::MAX as u32) as u8;
    XGamepad {
        buttons: XButtons((state.buttons & BUTTON_MASK) as u16),
        left_trigger: trigger(state.left_trigger),
        right_trigger: trigger(state.right_trigger),
        thumb_lx: axis(state.lx),
        thumb_ly: axis(state.ly),
        thumb_rx: axis(state.rx),
        thumb_ry: axis(state.ry),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_clamped_and_masked() {
        let state = GamepadState {
            buttons: 0xFFFF_FFFF,
            left_trigger: 1000,
            right_trigger: 7,
            lx: 40_000,
            ly: -40_000,
            rx: -32768,
            ry: 32767,
            ..Default::default()
        };
        let r = to_report(&state);
        assert_eq!(r.buttons.raw, 0xF3FF);
        assert_eq!(r.buttons.raw & 0x0400, 0);
        assert_eq!((r.left_trigger, r.right_trigger), (255, 7));
        assert_eq!((r.thumb_lx, r.thumb_ly), (32767, -32768));
        assert_eq!((r.thumb_rx, r.thumb_ry), (-32768, 32767));
    }
}
