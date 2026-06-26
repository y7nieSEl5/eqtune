//! eqtune — system-wide audio EQ for macOS.
//!
//! Library root. The Core Audio capture/replay layer (process taps) will live in the
//! `daemon` module; the modules here are the portable, unit-testable core.
//!
//! For the design notes, see [`architecture_zh`].

pub mod architecture_zh;
pub mod config;
pub mod daemon;
pub mod dsp;
pub mod ipc;
pub mod launchd;
pub mod sys;
