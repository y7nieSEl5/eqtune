//! eqtune — system-wide audio EQ for macOS.
//!
//! Library root. The portable Rust modules own config, IPC, daemon state, DSP, launchd
//! integration, and safe wrappers around the Objective-C Core Audio shim.
//!
//! For the design notes, see [`architecture_zh`].

pub mod architecture_zh;
pub mod config;
pub mod daemon;
pub mod dsp;
pub mod ipc;
pub mod launchd;
pub mod sys;
