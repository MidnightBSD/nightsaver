//! nightsaver: OLED-friendly Xorg screensaver and PAM screen locker that is a
//! drop-in replacement for xscreensaver's daemon and command-line protocol.
//!
//! Module ownership (see plan): each module is self-contained; cross-module
//! contracts are the public types defined in this file's submodules.

pub mod cli;
pub mod compat;
pub mod config;
pub mod daemon;
pub mod control;
pub mod display;
pub mod input;
pub mod lock;
pub mod logging;
pub mod patterns;
pub mod render;
pub mod x11;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// xscreensaver version we claim compatibility with in `_SCREENSAVER_VERSION`.
pub const COMPAT_XSCREENSAVER_VERSION: &str = "6.15";
