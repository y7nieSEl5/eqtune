//! Raw FFI to the Objective-C Core Audio shim (`shim/tap_shim.m`) plus safe wrappers.
//! This is the boundary between Rust and the macOS audio system.

use std::ffi::c_void;

use crate::dsp::Equalizer;

/// Matches `eqtune_process_cb` in `tap_shim.h`.
type ProcessCb = extern "C" fn(ctx: *mut c_void, buffer: *mut f32, frames: u32, channels: u32);

#[repr(C)]
struct RawSession {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// AudioObjectID of the current default output device, or 0 on failure.
    fn eqtune_default_output_device() -> u32;
    /// Nominal sample rate of the current default output device, or 0 on failure.
    fn eqtune_default_output_sample_rate() -> f64;
    fn eqtune_tap_start(cb: ProcessCb, ctx: *mut c_void) -> *mut RawSession;
    fn eqtune_tap_stop(session: *mut RawSession);
}

/// The current default output device's `AudioObjectID`, if one exists.
pub fn default_output_device() -> Option<u32> {
    let id = unsafe { eqtune_default_output_device() };
    (id != 0).then_some(id)
}

/// Nominal sample rate (Hz) of the current default output device, if available.
pub fn default_output_sample_rate() -> Option<f64> {
    let rate = unsafe { eqtune_default_output_sample_rate() };
    (rate > 0.0).then_some(rate)
}

/// Real-time callback invoked by the shim's IOProc to EQ one block in place.
extern "C" fn process_trampoline(ctx: *mut c_void, buffer: *mut f32, frames: u32, channels: u32) {
    if ctx.is_null() || buffer.is_null() || frames == 0 || channels == 0 {
        return;
    }
    // SAFETY: `ctx` is the `Box<Equalizer>` interior owned by the live `TapSession`;
    // the audio thread is its only accessor while the session is running.
    let eq = unsafe { &mut *(ctx as *mut Equalizer) };
    let len = frames as usize * channels as usize;
    let buf = unsafe { std::slice::from_raw_parts_mut(buffer, len) };
    eq.process_interleaved(buf, channels as usize);
}

/// A running capture→EQ→replay session. Audio stops when this is dropped.
pub struct TapSession {
    raw: *mut RawSession,
    // Keeps the Equalizer alive (and at a stable heap address) for the audio thread.
    _eq: Box<Equalizer>,
}

impl TapSession {
    /// Start tapping system audio, applying `eq`, and replaying to the default output.
    /// Returns `None` if the tap could not be created (see stderr for the reason).
    pub fn start(mut eq: Box<Equalizer>) -> Option<Self> {
        let ctx = (&mut *eq as *mut Equalizer).cast::<c_void>();
        let raw = unsafe { eqtune_tap_start(process_trampoline, ctx) };
        if raw.is_null() {
            None
        } else {
            Some(Self { raw, _eq: eq })
        }
    }
}

impl Drop for TapSession {
    fn drop(&mut self) {
        // Stops the audio thread before `_eq` is freed (no use-after-free).
        unsafe { eqtune_tap_stop(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_links_and_returns_a_device() {
        // Proves the ObjC shim compiles, links CoreAudio, and is callable from Rust.
        assert!(default_output_device().is_some(), "expected a default output device");
    }
}
