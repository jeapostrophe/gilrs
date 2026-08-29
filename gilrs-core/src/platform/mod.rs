// Copyright 2016-2018 Mateusz Sieczko and other GilRs Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Module which exports the platform-specific types.
//!
//! Each backend has to provide:
//!
//! * A `FfDevice` (a struct which handles force feedback)
//! * A `Gilrs` context
//! * A `Gamepad` struct
//! * A static `str` which specifies the name of the SDL input mapping
//! * A constant which define whether Y axis of sticks points upwards or downwards
//! * A module with the platform-specific constants for common gamepad buttons
//!   called `native_ev_codes`

#![allow(clippy::module_inception)]

pub use self::platform::*;

#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod platform;

#[cfg(all(target_os = "macos", not(feature = "gc-backend")))]
#[path = "macos/mod.rs"]
mod platform;

/// GameController.framework: iOS/iPadOS always, macOS when `gc-backend` is on.
///
/// **The macOS arm is a test harness, not a product choice.** On iPadOS an app
/// has no IOKit HID access, so `GCController` is the only way in; on macOS the
/// IOKit backend above is better (it sees every stick, not just the ones on
/// Apple's allow-list). But the framework is the same one on both — `GCController`
/// is `API_AVAILABLE(macos(10.9))` and the macOS and iOS `GCController.h` differ
/// by exactly one method — so `--features gc-backend` lets the iPad's input path
/// be developed and run against a real pad on a desktop, months before the device
/// is in the loop.
#[cfg(any(target_os = "ios", all(target_os = "macos", feature = "gc-backend")))]
#[path = "ios/mod.rs"]
mod platform;

// Target-gated, unlike the mutual-exclusion check below it. Without the
// `target_os` guard this fires on *every* target built with neither feature —
// which nothing hit while `wgi` was on by default, but which any consumer
// selecting a backend feature explicitly (`default-features = false, features =
// ["gc-backend"]`) would trip, with a message about Windows.
#[cfg(all(target_os = "windows", not(feature = "xinput"), not(feature = "wgi")))]
compile_error!(
    "Windows needs one of the features `gilrs/xinput` or `gilrs/wgi` enabled. \nEither don't use \
     'default-features = false' or add one of the features back."
);

#[cfg(all(feature = "wgi", feature = "xinput"))]
compile_error!("features `gilrs/xinput` and `gilrs/wgi` are mutually exclusive");

#[cfg(all(target_os = "windows", feature = "xinput", not(feature = "wgi")))]
#[path = "windows_xinput/mod.rs"]
mod platform;

#[cfg(all(target_os = "windows", feature = "wgi"))]
#[path = "windows_wgi/mod.rs"]
mod platform;

#[cfg(target_arch = "wasm32")]
#[path = "wasm/mod.rs"]
mod platform;

// The negative catch-all. It must exclude every arm above it, so adding a
// backend means adding a `not(...)` here too — otherwise that target ends up
// with two `mod platform` items (E0428). `macos` appears without its
// `gc-backend` qualifier because either macOS arm claims the target.
#[cfg(all(
    not(any(target_os = "linux")),
    not(target_os = "macos"),
    not(target_os = "ios"),
    not(target_os = "windows"),
    not(target_arch = "wasm32")
))]
#[path = "default/mod.rs"]
mod platform;
