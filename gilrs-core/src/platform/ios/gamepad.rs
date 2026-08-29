// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::collections::VecDeque;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
use objc2_game_controller::{GCController, GCDevice, GCExtendedGamepad};
use uuid::Uuid;

use super::FfDevice;
use crate::{AxisInfo, Event, EventType, PlatformError, PowerInfo};

/// Full scale for an axis value handed upward.
///
/// GameController reports `f32` in `[-1, 1]` (sticks) or `[0, 1]` (triggers);
/// gilrs wants an `i32` plus an [`AxisInfo`] range to divide it by. The number
/// itself is arbitrary — it cancels out — but it is **odd-free on purpose**:
/// `gilrs::gamepad::axis_value` adds a correction when `max - min` is odd, and a
/// symmetric `-32767..=32767` avoids ever exercising it.
const AXIS_MAX: i32 = 32767;

/// A stick or trigger reading, in the units [`AXIS_MAX`] defines.
fn scaled(v: f32) -> i32 {
    (v * AXIS_MAX as f32) as i32
}

/// How much an axis must move before it is worth an event.
///
/// GameController re-reports a resting analog stick with tiny float jitter, and
/// this backend polls rather than being edge-driven, so without a floor every
/// poll of an untouched pad would enqueue four `AxisValueChanged` events — which
/// on the station is an idle lock that never fires, since raw input is what the
/// idle timer watches. One count is below what any consumer can resolve
/// (1/32767 of full scale) and still collapses the jitter.
const AXIS_EPSILON: i32 = 1;

/// A button's analog value above which it counts as pressed.
///
/// Apple's own threshold: `GCControllerButtonInput.isPressed` is documented as
/// `value >= 0.0` being released and any positive value pressed, but real pads
/// report small non-zero values at rest on the triggers, so this uses the same
/// half-scale point SDL does rather than trusting `isPressed`.
const PRESS: f32 = 0.5;

#[derive(Debug)]
pub struct Gilrs {
    gamepads: Vec<Gamepad>,
    /// Events produced by the last poll and not yet handed out. `next_event`
    /// drains this before doing any work, exactly as the `wasm` backend does.
    queue: VecDeque<Event>,
    /// Set once, after the first controller is seen. See [`Gilrs::poll`].
    background_events_set: bool,
}

impl Gilrs {
    pub(crate) fn new() -> Result<Self, PlatformError> {
        let mut gilrs =
            Gilrs { gamepads: Vec::new(), queue: VecDeque::new(), background_events_set: false };
        // **The boot touch, and it is load-bearing.** GameController initializes
        // lazily on first contact with its API: measured on macOS 15.3.1, a
        // process that pumps the run loop for 1.2s *before* calling anything
        // still sees zero controllers, while one that calls `controllers()` first
        // sees the attached pad after ~3 run-loop turns. So this call is not the
        // enumeration — it is what makes the enumeration possible. (Same finding
        // as MAME PR #15129, which fixed the identical symptom.)
        let _ = unsafe { GCController::controllers() };
        // Discover synchronously so `last_gamepad_hint` is right before the
        // wrapper's `finish_gamepads_creation` reads it — which is what makes
        // `gilrs/examples/gamepad_info.rs` useful here, unlike on `macos` where
        // discovery is asynchronous and that example prints nothing.
        // GameController answers the boot touch asynchronously — measured at ~3
        // run-loop turns on macOS — so give it a bounded moment before the first
        // enumeration. Discovering synchronously here is what lets
        // `last_gamepad_hint` be right before the wrapper reads it.
        let deadline = Instant::now() + Duration::from_millis(50);
        while unsafe { GCController::controllers() }.is_empty() && Instant::now() < deadline {
            pump_for(Duration::from_millis(2));
        }
        gilrs.poll();
        Ok(gilrs)
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        if let Some(ev) = self.queue.pop_front() {
            return Some(ev);
        }
        self.poll();
        self.queue.pop_front()
    }

    pub(crate) fn next_event_blocking(&mut self, timeout: Option<Duration>) -> Option<Event> {
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(ev) = self.next_event() {
                return Some(ev);
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return None;
            }
            // The run loop is where connect/disconnect notifications and fresh
            // input land, so waiting *is* pumping it. `runMode:beforeDate:`
            // returns as soon as it has serviced anything, so this is not a spin.
            pump_for(Duration::from_millis(4));
        }
    }

    pub fn gamepad(&self, id: usize) -> Option<&Gamepad> {
        self.gamepads.get(id)
    }

    pub fn last_gamepad_hint(&self) -> usize {
        self.gamepads.len()
    }

    /// One pass: service the run loop, reconcile the controller list, then diff
    /// every connected pad's inputs against what it read last time.
    fn poll(&mut self) {
        pump_once();
        let controllers = unsafe { GCController::controllers() };

        if !self.background_events_set && !controllers.is_empty() {
            // **macOS only, and it defaults the wrong way for a cabinet.** With
            // it false — the default since macOS 11.3 — every axis and button
            // reads 0 whenever the app is not frontmost, which is
            // indistinguishable from a broken backend. Apple's header says the
            // property is ignored on iOS and tvOS, so this is unconditional.
            //
            // Set *after* a controller exists rather than in `new`: SDL carries a
            // macOS crash fix for setting it before the framework has any.
            unsafe { GCController::setShouldMonitorBackgroundEvents(true) };
            self.background_events_set = true;
        }

        // Mark everything absent, then un-mark what we find. Slots are never
        // removed — a disconnected pad keeps its index so ids stay dense and
        // ascending, which the wrapper's `next_event_priv` requires (it rejects
        // an out-of-order `Connected` id outright).
        let mut seen = vec![false; self.gamepads.len()];
        for controller in controllers.iter() {
            let Some(gamepad) = (unsafe { controller.extendedGamepad() }) else {
                // No extended profile: a remote, a `GCMicroGamepad`, or a racing
                // wheel. gilrs has no vocabulary for those, so it is not a
                // gamepad as far as this backend is concerned.
                continue;
            };
            match self.gamepads.iter().position(|g| g.is(&controller)) {
                Some(id) => {
                    seen[id] = true;
                    if !self.gamepads[id].is_connected {
                        self.gamepads[id].is_connected = true;
                        self.queue.push_back(Event::new(id, EventType::Connected));
                    }
                }
                None => {
                    let id = self.gamepads.len();
                    self.gamepads.push(Gamepad::new(controller.clone(), gamepad));
                    seen.push(true);
                    self.queue.push_back(Event::new(id, EventType::Connected));
                }
            }
        }
        for (id, present) in seen.iter().enumerate() {
            if !present && self.gamepads[id].is_connected {
                self.gamepads[id].is_connected = false;
                self.queue.push_back(Event::new(id, EventType::Disconnected));
            }
        }

        for id in 0..self.gamepads.len() {
            if self.gamepads[id].is_connected {
                let mut out = Vec::new();
                self.gamepads[id].diff(id, &mut out);
                self.queue.extend(out);
            }
        }
    }
}

/// Service the main run loop once, without blocking.
///
/// GameController delivers everything — device arrival, departure and input —
/// through the run loop, so a poll-based backend has to turn it by hand. This is
/// the whole of this backend's threading model: no worker thread, no callbacks,
/// nothing to synchronize, unlike `platform::macos` which runs an IOKit manager
/// on its own thread and ships events over a channel.
/// `NSDefaultRunLoopMode`, in the one place that has to spell it.
///
/// The `allow` is a per-target difference in the binding, not sloppiness:
/// `objc2-foundation` exposes this as an `unsafe` extern static on macOS and a
/// safe one on iOS, so an `unsafe` block is *required* on one host and *warns*
/// on the other. One wrapper takes the wart instead of two call sites.
#[allow(unused_unsafe)]
fn default_mode() -> &'static objc2_foundation::NSRunLoopMode {
    unsafe { NSDefaultRunLoopMode }
}

fn pump_once() {
    // **Drain, do not tick.** `runMode:beforeDate:` services at most *one* input
    // source per call and returns whether it ran at all, so a single call with a
    // now-date leaves anything queued behind it sitting there — which showed up
    // as `new()` finding zero controllers with a pad plainly attached. Loop until
    // it reports nothing left.
    //
    // The bound is a backstop, not a tuning knob: without it a source that
    // re-arms itself every time it is serviced would spin here forever, and this
    // runs inside the frontend's frame loop.
    let rl = NSRunLoop::currentRunLoop();
    for _ in 0..64 {
        let now = NSDate::dateWithTimeIntervalSinceNow(0.0);
        if !rl.runMode_beforeDate(default_mode(), &now) {
            break;
        }
    }
}

/// Wait up to `d` for the run loop to have something to do.
fn pump_for(d: Duration) {
    let rl = NSRunLoop::currentRunLoop();
    let until = NSDate::dateWithTimeIntervalSinceNow(d.as_secs_f64());
    rl.runMode_beforeDate(default_mode(), &until);
}

/// Everything this backend reads off one pad, as of the last poll.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct State {
    buttons: [bool; native_ev_codes::BUTTONS.len()],
    axes: [i32; native_ev_codes::AXES.len()],
}

pub struct Gamepad {
    controller: Retained<GCController>,
    profile: Retained<GCExtendedGamepad>,
    name: String,
    is_connected: bool,
    state: State,
}

// SAFETY (and the honest limit of it): `gilrs` asserts `Gilrs: Send` at crate
// level (`const _: () = assert_send::<Gilrs>()`), and `Retained<T>` is `Send`
// only when `T: Send + Sync`, which no `objc2` framework type claims. The same
// obstacle is why `platform::macos` writes `unsafe impl Send/Sync for
// IOHIDDevice`.
//
// It is sound as far as `Send` actually promises — memory safety. These are
// refcounted Objective-C objects whose retain/release is atomic, so moving one
// between threads cannot corrupt anything, and reading element values off the
// main thread was measured to work.
//
// **What it does NOT promise, and what a caller must know**: GameController
// delivers device arrival, departure and input through the *main* run loop
// (`GCDevice.handlerQueue` defaults to main), while `Gilrs::poll` pumps
// `NSRunLoop::currentRunLoop()`. Move a `Gilrs` to another thread and it stays
// memory-safe but goes deaf — it pumps a run loop nothing is scheduled on. So
// this backend must be created and polled on the main thread. That is what the
// frontend does anyway, and it is not a constraint the type system can carry
// while `gilrs` demands `Send`.
unsafe impl Send for Gamepad {}
unsafe impl Sync for Gamepad {}

impl std::fmt::Debug for Gamepad {
    /// Hand-written because `Retained<GCController>` is not `Debug`, and
    /// `crate::Gamepad` derives it.
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("Gamepad")
            .field("name", &self.name)
            .field("is_connected", &self.is_connected)
            .finish_non_exhaustive()
    }
}

impl Gamepad {
    fn new(controller: Retained<GCController>, profile: Retained<GCExtendedGamepad>) -> Gamepad {
        let name = unsafe { controller.vendorName() }
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Unknown Controller".to_owned());
        let mut gamepad =
            Gamepad { controller, profile, name, is_connected: true, state: State::default() };
        // Seed from the live pad rather than from `State::default`, so a button
        // already held when the app starts does not arrive as a press.
        gamepad.state = gamepad.read();
        gamepad
    }

    /// Whether `other` is the same physical pad as this one.
    ///
    /// Pointer identity on the `GCController`: the framework hands back the same
    /// instance for the lifetime of a connection, and there is nothing better to
    /// key on — `vendorName` is not unique and Apple's header says explicitly it
    /// "should not be used as a key in a dictionary", and the real VID/PID are
    /// masked by GameController (SDL fabricates them from the product category).
    fn is(&self, other: &GCController) -> bool {
        std::ptr::eq(Retained::as_ptr(&self.controller), other)
    }

    /// Every input, right now.
    fn read(&self) -> State {
        let g = &self.profile;
        let mut state = State::default();
        unsafe {
            let dpad = g.dpad();
            let pressed = [
                g.buttonA().value(),
                g.buttonB().value(),
                g.buttonX().value(),
                g.buttonY().value(),
                g.leftShoulder().value(),
                g.rightShoulder().value(),
                g.buttonOptions().map(|b| b.value()).unwrap_or(0.0),
                g.buttonMenu().value(),
                g.buttonHome().map(|b| b.value()).unwrap_or(0.0),
                g.leftThumbstickButton().map(|b| b.value()).unwrap_or(0.0),
                g.rightThumbstickButton().map(|b| b.value()).unwrap_or(0.0),
                dpad.up().value(),
                dpad.down().value(),
                dpad.left().value(),
                dpad.right().value(),
            ];
            for (slot, v) in state.buttons.iter_mut().zip(pressed) {
                *slot = v >= PRESS;
            }
            let (ls, rs) = (g.leftThumbstick(), g.rightThumbstick());
            state.axes = [
                scaled(ls.xAxis().value()),
                scaled(ls.yAxis().value()),
                scaled(rs.xAxis().value()),
                scaled(rs.yAxis().value()),
                scaled(g.leftTrigger().value()),
                scaled(g.rightTrigger().value()),
            ];
        }
        state
    }

    /// Read once and append an event for everything that moved.
    fn diff(&mut self, id: usize, out: &mut Vec<Event>) {
        let now = self.read();
        for (i, (&was, &is)) in self.state.buttons.iter().zip(now.buttons.iter()).enumerate() {
            if was != is {
                let code = native_ev_codes::BUTTONS[i];
                out.push(Event::new(
                    id,
                    if is {
                        EventType::ButtonPressed(crate::EvCode(code))
                    } else {
                        EventType::ButtonReleased(crate::EvCode(code))
                    },
                ));
            }
        }
        for (i, (&was, &is)) in self.state.axes.iter().zip(now.axes.iter()).enumerate() {
            if (was - is).abs() > AXIS_EPSILON {
                out.push(Event::new(
                    id,
                    EventType::AxisValueChanged(is, crate::EvCode(native_ev_codes::AXES[i])),
                ));
            }
        }
        self.state = now;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// **Deliberately nil**, which forces `gilrs` onto `Mapping::default`.
    ///
    /// That is the whole reason this backend needs no SDL mapping entry: the
    /// codes below *are* `gilrs`'s native numbering, so the default mapping is
    /// already correct, and a lookup that missed is exactly what selects it.
    ///
    /// It also removes a hazard that only appears on the macOS test build. There
    /// `SDL_PLATFORM_NAME` is `"Mac OS X"` and the bundled database is populated,
    /// so a *real* SDL-style UUID could hit an entry — and `parse_sdl_mapping`
    /// resolves that entry's `bN`/`aN` as indices into `buttons()`/`axes()`.
    /// Those entries were authored against IOKit's element order, not this
    /// backend's, so the buttons would come out scrambled on the Mac and correct
    /// on the iPad. Nil on both hosts means one code path on both hosts.
    pub fn uuid(&self) -> Uuid {
        Uuid::nil()
    }

    /// `None`: GameController masks the real identifiers. SDL fabricates values
    /// from the product category rather than reading them, and Chromium
    /// documents the same masking; inventing a number here would be a lie that
    /// downstream mapping code could act on.
    pub fn vendor_id(&self) -> Option<u16> {
        None
    }

    pub fn product_id(&self) -> Option<u16> {
        None
    }

    pub fn power_info(&self) -> PowerInfo {
        // `GCDeviceBattery` exists but is `nil` on plenty of pads and needs
        // another feature of the binding crate; a wired pad — which is what a
        // cabinet uses — reports `isAttachedToDevice` and nothing else useful.
        if unsafe { self.controller.isAttachedToDevice() } {
            PowerInfo::Wired
        } else {
            PowerInfo::Unknown
        }
    }

    pub fn is_ff_supported(&self) -> bool {
        false
    }

    pub fn ff_device(&self) -> Option<FfDevice> {
        Some(FfDevice)
    }

    pub fn buttons(&self) -> &[EvCode] {
        &native_ev_codes::BUTTONS
    }

    pub fn axes(&self) -> &[EvCode] {
        &native_ev_codes::AXES
    }

    /// **Must answer for every code this backend ever emits**, or the wrapper
    /// panics: `next_event_priv` does `axis_info(nec).unwrap()` on every
    /// `AxisValueChanged`.
    ///
    /// The two ranges are different on purpose. A stick is bipolar and goes
    /// through `axis_value`, which maps `min..max` onto `-1.0..1.0`. A trigger is
    /// unipolar and is mapped to `Button::{Left,Right}Trigger2`, so it goes
    /// through `btn_value`, which maps `min..max` onto `0.0..1.0` — giving it
    /// `-AXIS_MAX` as a floor would report a resting trigger as half-pressed.
    pub(crate) fn axis_info(&self, nec: EvCode) -> Option<&AxisInfo> {
        const STICK: AxisInfo = AxisInfo { min: -AXIS_MAX, max: AXIS_MAX, deadzone: None };
        const TRIGGER: AxisInfo = AxisInfo { min: 0, max: AXIS_MAX, deadzone: None };
        match nec {
            native_ev_codes::AXIS_LT2 | native_ev_codes::AXIS_RT2 => Some(&TRIGGER),
            _ if native_ev_codes::AXES.contains(&nec) => Some(&STICK),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected
    }
}

#[cfg_attr(feature = "serde-serialize", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EvCode(u8);

impl EvCode {
    pub fn into_u32(self) -> u32 {
        self.0 as u32
    }
}

impl Display for EvCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        self.0.fmt(f)
    }
}

/// **The same numbering every pre-mapped backend uses**, and the reason this
/// backend needs no SDL entry: `gilrs::Mapping::default` is built from exactly
/// these constants, so emitting them yields `Button::South`, `RightTrigger`,
/// `Start` and the rest correctly, with `MappingSource::Driver`.
pub mod native_ev_codes {
    use super::EvCode;

    pub const AXIS_LSTICKX: EvCode = EvCode(0);
    pub const AXIS_LSTICKY: EvCode = EvCode(1);
    pub const AXIS_LEFTZ: EvCode = EvCode(2);
    pub const AXIS_RSTICKX: EvCode = EvCode(3);
    pub const AXIS_RSTICKY: EvCode = EvCode(4);
    pub const AXIS_RIGHTZ: EvCode = EvCode(5);
    pub const AXIS_DPADX: EvCode = EvCode(6);
    pub const AXIS_DPADY: EvCode = EvCode(7);
    pub const AXIS_RT: EvCode = EvCode(8);
    pub const AXIS_LT: EvCode = EvCode(9);
    pub const AXIS_RT2: EvCode = EvCode(10);
    pub const AXIS_LT2: EvCode = EvCode(11);

    pub const BTN_SOUTH: EvCode = EvCode(12);
    pub const BTN_EAST: EvCode = EvCode(13);
    pub const BTN_C: EvCode = EvCode(14);
    pub const BTN_NORTH: EvCode = EvCode(15);
    pub const BTN_WEST: EvCode = EvCode(16);
    pub const BTN_Z: EvCode = EvCode(17);
    pub const BTN_LT: EvCode = EvCode(18);
    pub const BTN_RT: EvCode = EvCode(19);
    pub const BTN_LT2: EvCode = EvCode(20);
    pub const BTN_RT2: EvCode = EvCode(21);
    pub const BTN_SELECT: EvCode = EvCode(22);
    pub const BTN_START: EvCode = EvCode(23);
    pub const BTN_MODE: EvCode = EvCode(24);
    pub const BTN_LTHUMB: EvCode = EvCode(25);
    pub const BTN_RTHUMB: EvCode = EvCode(26);

    pub const BTN_DPAD_UP: EvCode = EvCode(27);
    pub const BTN_DPAD_DOWN: EvCode = EvCode(28);
    pub const BTN_DPAD_LEFT: EvCode = EvCode(29);
    pub const BTN_DPAD_RIGHT: EvCode = EvCode(30);

    /// **Order is the contract**: `Gamepad::read` fills its button array in this
    /// order, so the two must be edited together.
    ///
    /// **The D-pad is four buttons, not two axes.** GameController's `dpad` is
    /// four `GCControllerButtonInput`s, and emitting them directly makes
    /// `gilrs`'s `axis_dpad_to_button` filter inert — it only synthesizes
    /// `Button::DPad*` when no D-pad *buttons* are mapped. One representation,
    /// no filter, no chance of both firing.
    ///
    /// **Shoulders are `BTN_LT`/`BTN_RT` and the analog triggers are
    /// `AXIS_LT2`/`AXIS_RT2`, never both.** `Mapping::default` maps `BTN_LT` and
    /// `AXIS_LT` to the *same* `Button::LeftTrigger`, and `Mapping::map_rev` —
    /// which backs `is_pressed(Button::LeftTrigger)` — resolves ties by
    /// `iter().find()`, i.e. hash order. Listing both would make that lookup
    /// nondeterministic. This split is also what `retro-trainer` expects:
    /// `RightTrigger` is its `D`, `RightTrigger2` its `C`.
    pub(super) static BUTTONS: [EvCode; 15] = [
        BTN_SOUTH,
        BTN_EAST,
        BTN_WEST,
        BTN_NORTH,
        BTN_LT,
        BTN_RT,
        BTN_SELECT,
        BTN_START,
        BTN_MODE,
        BTN_LTHUMB,
        BTN_RTHUMB,
        BTN_DPAD_UP,
        BTN_DPAD_DOWN,
        BTN_DPAD_LEFT,
        BTN_DPAD_RIGHT,
    ];

    /// Same rule as [`BUTTONS`]: this order is `Gamepad::read`'s axis order.
    pub(super) static AXES: [EvCode; 6] =
        [AXIS_LSTICKX, AXIS_LSTICKY, AXIS_RSTICKX, AXIS_RSTICKY, AXIS_LT2, AXIS_RT2];
}
