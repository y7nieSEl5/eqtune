//! Raw FFI to the Objective-C Core Audio shim (`shim/tap_shim.m`) plus safe wrappers.
//! This is the boundary between Rust and the macOS audio system.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;

use crate::dsp::{EqSettings, Processor};

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
    /// Whether macOS Low Power Mode is currently enabled.
    fn eqtune_low_power_enabled() -> bool;
    /// Whether the current default output device is running somewhere.
    fn eqtune_default_output_device_running() -> bool;
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

/// Whether macOS Low Power Mode is currently enabled.
pub fn low_power_enabled() -> bool {
    unsafe { eqtune_low_power_enabled() }
}

/// Whether the current default output device reports active I/O.
pub fn default_output_device_running() -> bool {
    unsafe { eqtune_default_output_device_running() }
}

#[derive(Default)]
struct AudioActivity {
    silent_frames: AtomicU64,
}

/// Owned by the audio thread (via the raw pointer handed to the shim): the filter
/// state plus a reader of the atomically-swappable settings.
struct AudioState {
    processor: Processor,
    settings: Arc<ArcSwap<EqSettings>>,
    activity: Arc<AudioActivity>,
}

/// Real-time callback invoked by the shim's IOProc to EQ one block in place.
extern "C" fn process_trampoline(ctx: *mut c_void, buffer: *mut f32, frames: u32, channels: u32) {
    if ctx.is_null() || buffer.is_null() || frames == 0 || channels == 0 {
        return;
    }
    // SAFETY: `ctx` is the `Box<AudioState>` owned by the live `TapSession`; the audio
    // thread is the only accessor of `processor` while the session is running.
    let state = unsafe { &mut *(ctx as *mut AudioState) };
    let settings = state.settings.load(); // arc-swap Guard: borrow, no per-block Arc clone
    let len = frames as usize * channels as usize;
    let buf = unsafe { std::slice::from_raw_parts_mut(buffer, len) };
    if crate::dsp::block_is_silent(buf) {
        state
            .activity
            .silent_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    } else {
        state.activity.silent_frames.store(0, Ordering::Relaxed);
    }
    state.processor.run(&settings, buf, channels as usize);
}

/// Control-thread handle used to push fresh EQ settings to a running session,
/// lock-free, without restarting the audio engine.
#[derive(Clone)]
pub struct EqHandle {
    settings: Arc<ArcSwap<EqSettings>>,
    activity: Arc<AudioActivity>,
}

impl EqHandle {
    pub fn store(&self, settings: EqSettings) {
        self.settings.store(Arc::new(settings));
    }

    pub fn silent_frames(&self) -> u64 {
        self.activity.silent_frames.load(Ordering::Relaxed)
    }
}

/// A running capture→EQ→replay session. Audio stops when this is dropped.
pub struct TapSession {
    raw: *mut RawSession,
    // Keeps the audio-thread state alive (at a stable heap address) until stop.
    _state: Box<AudioState>,
}

impl TapSession {
    /// Start tapping system audio, applying `initial` settings, and replaying to the
    /// default output. Returns the session plus an [`EqHandle`] for live updates, or
    /// `None` if the tap could not be created (reason logged to stderr).
    pub fn start(channels: usize, initial: EqSettings) -> Option<(Self, EqHandle)> {
        let shared = Arc::new(ArcSwap::from_pointee(initial));
        let activity = Arc::new(AudioActivity::default());
        let handle = EqHandle {
            settings: shared.clone(),
            activity: activity.clone(),
        };
        let mut state = Box::new(AudioState {
            processor: Processor::new(channels),
            settings: shared,
            activity,
        });
        let ctx = (&mut *state as *mut AudioState).cast::<c_void>();
        let raw = unsafe { eqtune_tap_start(process_trampoline, ctx) };
        if raw.is_null() {
            None
        } else {
            Some((Self { raw, _state: state }, handle))
        }
    }
}

impl Drop for TapSession {
    fn drop(&mut self) {
        // Stops the audio thread before `_state` is freed (no use-after-free).
        unsafe { eqtune_tap_stop(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_links_and_is_callable() {
        // Proves the ObjC shim compiles, links CoreAudio, and is callable from Rust.
        let _ = default_output_device();
        let _ = default_output_sample_rate();
        let _ = low_power_enabled();
        let _ = default_output_device_running();
    }
}
