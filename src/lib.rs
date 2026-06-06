//! eqtune — system-wide audio EQ for macOS.
//!
//! Library root. The Core Audio capture/replay layer (process taps) will live in the
//! `daemon` module; the modules here are the portable, unit-testable core.

pub mod config;
pub mod daemon;
pub mod dsp;
pub mod ipc;
pub mod sys;
