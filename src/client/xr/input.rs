//! Touch controllers -> one XInput-style pad (the mapping table of gamemode-plan.md section 3).
//! Phase 1 computes the state and LOGS it (tag RustDeskXr); phase 4 sends it to the host as
//! Misc.gamepad_state (message.proto GamepadState, same fields as `PadState`).
//!
//! | Touch                         | XInput                                              |
//! |-------------------------------|-----------------------------------------------------|
//! | A / B / X / Y                 | A / B / X / Y                                       |
//! | trigger/value                 | LT / RT, round(v * 255)                             |
//! | squeeze/value                 | LB / RB, press above 0.55, release below 0.35       |
//! | thumbsticks                   | LS / RS, radial deadzone 0.06 rescaled, * 32767     |
//! | thumbstick clicks             | L3 / R3                                             |
//! | Menu tap (< 0.5 s, no chord)  | Start, a 100 ms pulse on release                    |
//! | Menu + Y                      | Back                                                |
//! | Menu + left stick             | D-pad, 8-way, threshold 0.5                         |
//! | Menu held 1.5 s, Menu+L3+R3   | leave game mode (a buzz at 0.5 s warns)             |
//! | Menu + A                      | recenter the screen (placeholder: logged)           |
//! | Menu + B                      | passthrough toggle (phase 5: logged)                |
//! | Guide (0x0400)                | never sent                                          |
//!
//! While Menu is held, A, B, Y, L3, R3 and the left stick are not sent as themselves; one
//! that was down during the chord stays suppressed after Menu is released, until it is
//! released too (stick: back inside the deadzone), so a chord never leaks into the game.
//! Bindings: `/interaction_profiles/oculus/touch_controller` only (critique #6); with hand
//! tracking or any other profile the pad is neutral.

use super::{xlog_i, xlog_w};
use openxr as xr;
use std::time::{Duration, Instant};

pub const PROFILE: &str = "/interaction_profiles/oculus/touch_controller";

// XINPUT_GAMEPAD_* (wButtons).
pub const DPAD_UP: u32 = 0x0001;
pub const DPAD_DOWN: u32 = 0x0002;
pub const DPAD_LEFT: u32 = 0x0004;
pub const DPAD_RIGHT: u32 = 0x0008;
pub const START: u32 = 0x0010;
pub const BACK: u32 = 0x0020;
pub const LEFT_THUMB: u32 = 0x0040;
pub const RIGHT_THUMB: u32 = 0x0080;
pub const LEFT_SHOULDER: u32 = 0x0100;
pub const RIGHT_SHOULDER: u32 = 0x0200;
pub const A: u32 = 0x1000;
pub const B: u32 = 0x2000;
pub const X: u32 = 0x4000;
pub const Y: u32 = 0x8000;

const STICK_DEADZONE: f32 = 0.06;
const GRIP_PRESS: f32 = 0.55;
const GRIP_RELEASE: f32 = 0.35;
const DPAD_THRESHOLD: f32 = 0.5;
const MENU_TAP: Duration = Duration::from_millis(500);
const MENU_EXIT_HOLD: Duration = Duration::from_millis(1500);
const START_PULSE: Duration = Duration::from_millis(100);
const LOG_EVERY: Duration = Duration::from_millis(500);

/// The pad as the host would get it (message.proto GamepadState without seq/off).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct PadState {
    pub buttons: u32,
    pub left_trigger: u32,
    pub right_trigger: u32,
    pub lx: i32,
    pub ly: i32,
    pub rx: i32,
    pub ry: i32,
}

impl PadState {
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    fn describe(&self, seq: u32) -> String {
        const NAMES: [(u32, &str); 14] = [
            (A, "A"),
            (B, "B"),
            (X, "X"),
            (Y, "Y"),
            (LEFT_SHOULDER, "LB"),
            (RIGHT_SHOULDER, "RB"),
            (LEFT_THUMB, "L3"),
            (RIGHT_THUMB, "R3"),
            (START, "START"),
            (BACK, "BACK"),
            (DPAD_UP, "UP"),
            (DPAD_DOWN, "DOWN"),
            (DPAD_LEFT, "LEFT"),
            (DPAD_RIGHT, "RIGHT"),
        ];
        let names: Vec<&str> = NAMES
            .iter()
            .filter(|(bit, _)| self.buttons & bit != 0)
            .map(|(_, n)| *n)
            .collect();
        format!(
            "pad seq={} buttons=0x{:04x} [{}] lt={} rt={} l=({},{}) r=({},{})",
            seq,
            self.buttons,
            names.join(" "),
            self.left_trigger,
            self.right_trigger,
            self.lx,
            self.ly,
            self.rx,
            self.ry
        )
    }
}

/// One frame of controller input, as read from the actions.
#[derive(Clone, Copy, Default, Debug)]
pub struct Raw {
    /// At least one Touch controller is bound and active.
    pub active: bool,
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub menu: bool,
    pub l3: bool,
    pub r3: bool,
    pub lt: f32,
    pub rt: f32,
    pub lgrip: f32,
    pub rgrip: f32,
    pub ls: (f32, f32),
    pub rs: (f32, f32),
}

/// What a Menu chord asks for besides the pad state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chord {
    /// Menu held 0.5 s with no chord: the exit hold is running (buzz).
    HoldWarning,
    /// Leave game mode.
    Exit(&'static str),
    /// Menu + A.
    Recenter,
    /// Menu + B.
    Passthrough,
}

fn trigger(v: f32) -> u32 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u32
}

fn axis(v: f32) -> i32 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i32
}

/// Radial deadzone with rescale, clamped to the unit circle. OpenXR +Y is up, as in XInput.
fn stick((x, y): (f32, f32)) -> (i32, i32) {
    let m = (x * x + y * y).sqrt();
    if !m.is_finite() || m <= STICK_DEADZONE {
        return (0, 0);
    }
    let scale = (m.min(1.0) - STICK_DEADZONE) / (1.0 - STICK_DEADZONE) / m;
    (axis(x * scale), axis(y * scale))
}

/// The chord and hysteresis state machine. Pure: no OpenXR in here.
#[derive(Default)]
pub struct Mapper {
    lb: bool,
    rb: bool,
    menu_down_at: Option<Instant>,
    chord_used: bool,
    warned: bool,
    exit_fired: bool,
    start_until: Option<Instant>,
    prev_a: bool,
    prev_b: bool,
    /// XInput bits (A, B, Y, L3, R3) that were down while Menu was held: not sent until the
    /// button is released.
    latched: u32,
    /// The left stick was out of the deadzone while Menu was held: not sent until it is back.
    ls_latched: bool,
}

impl Mapper {
    /// Focus lost, or the controllers went away: everything released, no Start on release.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn update(&mut self, r: &Raw, now: Instant, chords: &mut Vec<Chord>) -> PadState {
        if !r.active {
            self.reset();
            return PadState::default();
        }
        self.lb = if self.lb { r.lgrip > GRIP_RELEASE } else { r.lgrip > GRIP_PRESS };
        self.rb = if self.rb { r.rgrip > GRIP_RELEASE } else { r.rgrip > GRIP_PRESS };
        let mut st = PadState {
            left_trigger: trigger(r.lt),
            right_trigger: trigger(r.rt),
            ..Default::default()
        };
        if self.lb {
            st.buttons |= LEFT_SHOULDER;
        }
        if self.rb {
            st.buttons |= RIGHT_SHOULDER;
        }
        if r.x {
            st.buttons |= X;
        }
        (st.rx, st.ry) = stick(r.rs);

        if r.menu {
            // Whatever is down now belongs to the chord, also after Menu is released.
            for (down, bit) in [(r.a, A), (r.b, B), (r.y, Y), (r.l3, LEFT_THUMB), (r.r3, RIGHT_THUMB)] {
                if down {
                    self.latched |= bit;
                }
            }
            if stick(r.ls) != (0, 0) {
                self.ls_latched = true;
            }
            let down_at = match self.menu_down_at {
                Some(t) => t,
                None => {
                    // A fresh press: only buttons pressed from now on make chords.
                    self.menu_down_at = Some(now);
                    self.chord_used = false;
                    self.warned = false;
                    self.exit_fired = false;
                    self.prev_a = r.a;
                    self.prev_b = r.b;
                    now
                }
            };
            if r.y {
                st.buttons |= BACK;
                self.chord_used = true;
            }
            let (dx, dy) = r.ls;
            let mut dpad = 0;
            if dy > DPAD_THRESHOLD {
                dpad |= DPAD_UP;
            }
            if dy < -DPAD_THRESHOLD {
                dpad |= DPAD_DOWN;
            }
            if dx < -DPAD_THRESHOLD {
                dpad |= DPAD_LEFT;
            }
            if dx > DPAD_THRESHOLD {
                dpad |= DPAD_RIGHT;
            }
            if dpad != 0 {
                st.buttons |= dpad;
                self.chord_used = true;
            }
            if r.a && !self.prev_a {
                chords.push(Chord::Recenter);
            }
            if r.b && !self.prev_b {
                chords.push(Chord::Passthrough);
            }
            if r.a || r.b || r.l3 || r.r3 {
                self.chord_used = true;
            }
            if r.l3 && r.r3 && !self.exit_fired {
                self.exit_fired = true;
                chords.push(Chord::Exit("Menu+L3+R3"));
            }
            let held = now.saturating_duration_since(down_at);
            if !self.chord_used && !self.warned && held >= MENU_TAP {
                self.warned = true;
                chords.push(Chord::HoldWarning);
            }
            if !self.chord_used && !self.exit_fired && held >= MENU_EXIT_HOLD {
                self.exit_fired = true;
                chords.push(Chord::Exit("Menu held 1.5 s"));
            }
            self.prev_a = r.a;
            self.prev_b = r.b;
        } else {
            if let Some(down_at) = self.menu_down_at.take() {
                // A tap with no chord is Start; a release after 0.5 s sends nothing.
                if !self.chord_used
                    && !self.exit_fired
                    && now.saturating_duration_since(down_at) < MENU_TAP
                {
                    self.start_until = Some(now + START_PULSE);
                }
            }
            for (down, bit) in [(r.a, A), (r.b, B), (r.y, Y), (r.l3, LEFT_THUMB), (r.r3, RIGHT_THUMB)] {
                if !down {
                    self.latched &= !bit;
                } else if self.latched & bit == 0 {
                    st.buttons |= bit;
                }
            }
            let ls = stick(r.ls);
            if ls == (0, 0) {
                self.ls_latched = false;
            } else if !self.ls_latched {
                (st.lx, st.ly) = ls;
            }
        }
        if let Some(until) = self.start_until {
            if now < until {
                st.buttons |= START;
            } else {
                self.start_until = None;
            }
        }
        st
    }
}

/// The action set and the logging of the computed pad.
pub struct Input {
    set: xr::ActionSet,
    left: xr::Path,
    right: xr::Path,
    profile: xr::Path,
    a: xr::Action<bool>,
    b: xr::Action<bool>,
    x: xr::Action<bool>,
    y: xr::Action<bool>,
    menu: xr::Action<bool>,
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    stick: xr::Action<xr::Vector2f>,
    stick_click: xr::Action<bool>,
    haptic: xr::Action<xr::Haptic>,
    mapper: Mapper,
    /// Both hands' current interaction profile is the Touch controller one.
    controllers: [bool; 2],
    state: PadState,
    seq: u32,
    logged: PadState,
    logged_at: Option<Instant>,
    chords: Vec<Chord>,
    diag_at: std::cell::Cell<Option<Instant>>,
}

impl Input {
    /// One input diagnostic line every 2 s while focused.
    fn diag_due(&self) -> bool {
        let now = Instant::now();
        let due = self.diag_at.get().map_or(true, |t| now.saturating_duration_since(t) >= Duration::from_secs(2));
        if due {
            self.diag_at.set(Some(now));
        }
        due
    }

    /// Creates the actions and suggests the Touch controller bindings. Must run before the
    /// action set is attached to the session ([`Input::attach`], once per session).
    pub fn new(instance: &xr::Instance) -> xr::Result<Self> {
        let left = instance.string_to_path(xr::USER_HAND_LEFT)?;
        let right = instance.string_to_path(xr::USER_HAND_RIGHT)?;
        let profile = instance.string_to_path(PROFILE)?;
        let set = instance.create_action_set("gamepad", "Gamepad", 0)?;
        let hands = [left, right];
        let a = set.create_action::<bool>("a", "A", &[])?;
        let b = set.create_action::<bool>("b", "B", &[])?;
        let x = set.create_action::<bool>("x", "X", &[])?;
        let y = set.create_action::<bool>("y", "Y", &[])?;
        let menu = set.create_action::<bool>("menu", "Menu", &[])?;
        let trigger = set.create_action::<f32>("trigger", "Trigger", &hands)?;
        let squeeze = set.create_action::<f32>("squeeze", "Grip", &hands)?;
        let stick = set.create_action::<xr::Vector2f>("stick", "Thumbstick", &hands)?;
        let stick_click = set.create_action::<bool>("stick_click", "Thumbstick click", &hands)?;
        let haptic = set.create_action::<xr::Haptic>("haptic", "Vibration", &hands)?;
        let p = |s: &str| instance.string_to_path(s);
        xlog_i("suggesting Touch controller bindings");
        instance.suggest_interaction_profile_bindings(
            profile,
            &[
                xr::Binding::new(&a, p("/user/hand/right/input/a/click")?),
                xr::Binding::new(&b, p("/user/hand/right/input/b/click")?),
                xr::Binding::new(&x, p("/user/hand/left/input/x/click")?),
                xr::Binding::new(&y, p("/user/hand/left/input/y/click")?),
                xr::Binding::new(&menu, p("/user/hand/left/input/menu/click")?),
                xr::Binding::new(&trigger, p("/user/hand/left/input/trigger/value")?),
                xr::Binding::new(&trigger, p("/user/hand/right/input/trigger/value")?),
                xr::Binding::new(&squeeze, p("/user/hand/left/input/squeeze/value")?),
                xr::Binding::new(&squeeze, p("/user/hand/right/input/squeeze/value")?),
                xr::Binding::new(&stick, p("/user/hand/left/input/thumbstick")?),
                xr::Binding::new(&stick, p("/user/hand/right/input/thumbstick")?),
                xr::Binding::new(&stick_click, p("/user/hand/left/input/thumbstick/click")?),
                xr::Binding::new(&stick_click, p("/user/hand/right/input/thumbstick/click")?),
                xr::Binding::new(&haptic, p("/user/hand/left/output/haptic")?),
                xr::Binding::new(&haptic, p("/user/hand/right/output/haptic")?),
            ],
        )?;
        Ok(Self {
            set,
            left,
            right,
            profile,
            a,
            b,
            x,
            y,
            menu,
            trigger,
            squeeze,
            stick,
            stick_click,
            haptic,
            mapper: Mapper::default(),
            controllers: [false; 2],
            state: PadState::default(),
            seq: 0,
            logged: PadState::default(),
            logged_at: None,
            chords: Vec::new(),
            diag_at: std::cell::Cell::new(None),
        })
    }

    pub fn attach<G>(&self, session: &xr::Session<G>) -> xr::Result<()> {
        session.attach_action_sets(&[&self.set])
    }

    /// INTERACTION_PROFILE_CHANGED: which profile each hand has now. Anything but the Touch
    /// controller (hand tracking: NULL) makes that hand neutral.
    pub fn profile_changed<G>(&mut self, session: &xr::Session<G>) {
        for (i, hand) in [self.left, self.right].into_iter().enumerate() {
            let p = session.current_interaction_profile(hand);
            let name = match p {
                Ok(p) if p == xr::Path::NULL => "none".to_owned(),
                Ok(p) => session
                    .instance()
                    .path_to_string(p)
                    .unwrap_or_else(|e| format!("{:?}", e)),
                Err(e) => format!("error {:?}", e),
            };
            self.controllers[i] = matches!(p, Ok(p) if p == self.profile);
            xlog_i(&format!(
                "interaction profile {}: {}",
                if i == 0 { "left" } else { "right" },
                name
            ));
        }
    }

    fn read<G>(&self, session: &xr::Session<G>) -> xr::Result<Raw> {
        let n = xr::Path::NULL;
        let (l, r) = (self.left, self.right);
        let trig_l = self.trigger.state(session, l)?;
        let trig_r = self.trigger.state(session, r)?;
        let v2 = |s: xr::ActionState<xr::Vector2f>| (s.current_state.x, s.current_state.y);
        let any_active = trig_l.is_active || trig_r.is_active;
        if self.diag_due() {
            xlog_i(&format!(
                "input diag: trigger active l={} r={} controllers={:?} lt={:.2} rt={:.2}",
                trig_l.is_active, trig_r.is_active, self.controllers,
                trig_l.current_state, trig_r.current_state
            ));
        }
        Ok(Raw {
            // Any active binding counts: on Horizon v2.10 the profile check alone let the
            // whole pad stay neutral (no button ever logged with the controllers bound).
            active: any_active,
            a: self.a.state(session, n)?.current_state,
            b: self.b.state(session, n)?.current_state,
            x: self.x.state(session, n)?.current_state,
            y: self.y.state(session, n)?.current_state,
            menu: self.menu.state(session, n)?.current_state,
            l3: self.stick_click.state(session, l)?.current_state,
            r3: self.stick_click.state(session, r)?.current_state,
            lt: trig_l.current_state,
            rt: trig_r.current_state,
            lgrip: self.squeeze.state(session, l)?.current_state,
            rgrip: self.squeeze.state(session, r)?.current_state,
            ls: v2(self.stick.state(session, l)?),
            rs: v2(self.stick.state(session, r)?),
        })
    }

    fn buzz<G>(&self, session: &xr::Session<G>, ms: i64, amplitude: f32) {
        let v = xr::HapticVibration::new()
            .amplitude(amplitude)
            .frequency(xr::FREQUENCY_UNSPECIFIED)
            .duration(xr::Duration::from_nanos(ms * 1_000_000));
        for hand in [self.left, self.right] {
            if let Err(e) = self.haptic.apply_feedback(session, hand, &v) {
                xlog_w(&format!("haptic: {:?}", e));
            }
        }
    }

    fn set_state(&mut self, st: PadState, now: Instant, why: &str) {
        if st == self.state {
            return;
        }
        let buttons_changed = st.buttons != self.state.buttons;
        self.state = st;
        self.seq = self.seq.wrapping_add(1);
        // Every button change, and the axes at most twice a second.
        let due = self
            .logged_at
            .map_or(true, |t| now.saturating_duration_since(t) >= LOG_EVERY);
        if buttons_changed || due || !why.is_empty() {
            self.log(now, why);
        }
    }

    fn log(&mut self, now: Instant, why: &str) {
        let mut line = self.state.describe(self.seq);
        if !why.is_empty() {
            line.push_str(" (");
            line.push_str(why);
            line.push(')');
        }
        xlog_i(&line);
        self.logged = self.state;
        self.logged_at = Some(now);
    }

    /// The pad goes neutral (focus lost, session stopping). Phase 4 sends it once.
    pub fn neutral(&mut self, why: &str) {
        self.mapper.reset();
        let now = Instant::now();
        if !self.state.is_neutral() {
            self.set_state(PadState::default(), now, why);
        }
    }

    /// Once per frame. [focused]: the session is FOCUSED (input reaches the app only then).
    /// Returns the exit reason when a chord asks to leave game mode.
    pub fn frame<G>(&mut self, session: &xr::Session<G>, focused: bool) -> Option<&'static str> {
        let now = Instant::now();
        if !focused {
            self.neutral("not focused");
            return None;
        }
        if let Err(e) = session.sync_actions(&[xr::ActiveActionSet::new(&self.set)]) {
            xlog_w(&format!("xrSyncActions: {:?}", e));
            return None;
        }
        let raw = match self.read(session) {
            Ok(r) => r,
            Err(e) => {
                xlog_w(&format!("action state: {:?}", e));
                return None;
            }
        };
        let mut chords = std::mem::take(&mut self.chords);
        let st = self.mapper.update(&raw, now, &mut chords);
        let mut exit = None;
        for c in chords.drain(..) {
            match c {
                Chord::HoldWarning => {
                    xlog_i("Menu held 0.5 s: keep holding to leave game mode");
                    self.buzz(session, 30, 0.5);
                }
                Chord::Exit(why) => {
                    xlog_i(&format!("exit chord: {}", why));
                    self.buzz(session, 120, 0.9);
                    exit = Some(why);
                }
                Chord::Recenter => {
                    xlog_i("Menu+A: recenter (placeholder, the screen comes in phase 2)");
                    self.buzz(session, 20, 0.4);
                }
                Chord::Passthrough => xlog_i("Menu+B: passthrough toggle (phase 5)"),
            }
        }
        self.chords = chords;
        self.set_state(st, now, "");
        // A held non-neutral state is logged again every 2 s, even without a change.
        if !self.state.is_neutral()
            && self.logged == self.state
            && self
                .logged_at
                .map_or(true, |t| now.saturating_duration_since(t) >= LOG_EVERY * 4)
        {
            self.log(now, "held");
        }
        exit
    }
}
