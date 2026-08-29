// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! GameController.framework backend — iOS/iPadOS, and macOS behind the
//! `gc-backend` feature so it can be developed and tested on a desktop.
//!
//! **Why this exists rather than an IOKit backend**: on iPadOS an app has no
//! IOKit HID access at all. `GCController` is the only way in, and it is an
//! allow-list — Apple's `gamecontrollerd` decides which pads exist and hands
//! them over already normalized into a `GCExtendedGamepad` profile. So this
//! backend does no HID report parsing, no element-tree walking and no SDL
//! mapping: it reads named properties off a profile Apple has already mapped.
//!
//! **Shape: it is the `wasm` backend, not the `macos` one.** Both this and the
//! Web Gamepad API are pre-mapped, poll-based, fixed-layout APIs, so the code
//! here mirrors `platform::wasm` — a fixed `native_ev_codes` table, `Uuid::nil()`,
//! an internal event queue that `next_event` drains before doing any polling
//! work — rather than `platform::macos`'s callback-driven IOKit machinery.
mod ff;
mod gamepad;

pub use self::ff::Device as FfDevice;
pub use self::gamepad::{native_ev_codes, EvCode, Gamepad, Gilrs};

/// `false`: GameController's stick Y axis is **up-positive** already
/// (`GCControllerDirectionPad.yAxis` is documented as `-1` down / `1` up), which
/// is the convention `gilrs` wants, so nothing needs flipping. The `macos`
/// backend sets `true` because raw HID reports the opposite way round.
pub const IS_Y_AXIS_REVERSED: bool = false;
