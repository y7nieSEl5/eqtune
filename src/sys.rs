//! Raw FFI to the Objective-C Core Audio shim (`shim/tap_shim.m`) plus safe wrappers.
//! This is the boundary between Rust and the macOS audio system.

use std::ffi::{CStr, c_char, c_void};
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
    /// Nominal sample rate of exactly `dev`, or 0 on failure.
    fn eqtune_output_device_sample_rate(dev: u32) -> f64;
    /// Whether macOS Low Power Mode is currently enabled.
    fn eqtune_low_power_enabled() -> bool;
    /// Whether the current default output device is running somewhere.
    fn eqtune_default_output_device_running() -> bool;
    /// Writes the name of output device `dev` (NUL-terminated UTF-8) into `buf`; returns
    /// false if unavailable or the buffer is too small.
    fn eqtune_output_device_name(dev: u32, buf: *mut c_char, buflen: usize) -> bool;
    fn eqtune_output_device_uid(dev: u32, buf: *mut c_char, buflen: usize) -> bool;
    fn eqtune_output_device_stream_facts(dev: u32, facts: *mut RawStreamFacts) -> bool;
    fn eqtune_tap_start(
        output_device: u32,
        cb: ProcessCb,
        ctx: *mut c_void,
        error_buf: *mut c_char,
        error_buflen: usize,
    ) -> *mut RawSession;
    fn eqtune_tap_runtime_error(session: *mut RawSession) -> u32;
    fn eqtune_tap_stop(session: *mut RawSession);
}

/// Restore the default `SIGPIPE` disposition, so the process dies quietly — like any
/// Unix filter — when its output pipe closes early (e.g. `eqtune status | head`). Rust
/// ignores `SIGPIPE` by default, which turns the closed pipe into a `println!` panic
/// ("failed printing to stdout: Broken pipe") instead.
///
/// Client commands only: the daemon must keep `SIGPIPE` ignored, because a control-socket
/// client disconnecting mid-response would otherwise kill the daemon; with the signal
/// ignored that surfaces as a handled `EPIPE` write error. (Known trade-off on the client:
/// a socket write racing a dying daemon now exits silently instead of printing an error —
/// the common daemon-not-running case still fails at `connect` with the friendly message.)
pub fn restore_default_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// The current default output device's `AudioObjectID`, if one exists.
pub fn default_output_device() -> Option<u32> {
    let id = unsafe { eqtune_default_output_device() };
    (id != 0).then_some(id)
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct RawStreamFacts {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamFacts {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_frame: u32,
    pub channels: u32,
    pub bits_per_channel: u32,
}

impl StreamFacts {
    pub fn format_name(&self) -> String {
        const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
        const IS_FLOAT: u32 = 1;
        if self.format_id == LINEAR_PCM && self.format_flags & IS_FLOAT != 0 {
            return format!("Float{}", self.bits_per_channel);
        }
        let fourcc = self.format_id.to_be_bytes();
        if fourcc.iter().all(|byte| byte.is_ascii_graphic()) {
            String::from_utf8_lossy(&fourcc).into_owned()
        } else {
            format!("0x{:08x}", self.format_id)
        }
    }

    pub fn interleaved(&self) -> bool {
        // kAudioFormatFlagIsNonInterleaved
        self.format_flags & (1 << 5) == 0
    }
}

/// One coherent description of a prospective output. The default ID is resolved once;
/// every remaining property is queried by that exact ID, so a device switch between
/// property reads cannot construct a mixed snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputTarget {
    pub id: u32,
    pub uid: String,
    pub name: String,
    pub sample_rate: f64,
    pub stream: StreamFacts,
}

impl OutputTarget {
    pub fn resolve_default() -> anyhow::Result<Self> {
        let id =
            default_output_device().ok_or_else(|| anyhow::anyhow!("no default output device"))?;
        let uid = output_device_string(id, eqtune_output_device_uid)
            .ok_or_else(|| anyhow::anyhow!("could not read output device UID for #{id}"))?;
        // A display name is diagnostic, not a prerequisite for safe audio. Keep the
        // exact numeric target useful if Core Audio cannot provide the friendly label.
        let name = output_device_name(id).unwrap_or_else(|| format!("#{id}"));
        let sample_rate = output_device_sample_rate(id)
            .ok_or_else(|| anyhow::anyhow!("could not read output device sample rate for #{id}"))?;
        let mut raw = RawStreamFacts::default();
        if !unsafe { eqtune_output_device_stream_facts(id, &mut raw) } {
            anyhow::bail!("could not read output stream format for #{id}");
        }
        Ok(Self {
            id,
            uid,
            name,
            sample_rate,
            stream: StreamFacts {
                sample_rate: raw.sample_rate,
                format_id: raw.format_id,
                format_flags: raw.format_flags,
                bytes_per_frame: raw.bytes_per_frame,
                channels: raw.channels_per_frame,
                bits_per_channel: raw.bits_per_channel,
            },
        })
    }
}

/// Nominal sample rate (Hz) of exactly `dev`, if available.
pub fn output_device_sample_rate(dev: u32) -> Option<f64> {
    let rate = unsafe { eqtune_output_device_sample_rate(dev) };
    (rate.is_finite() && rate > 0.0).then_some(rate)
}

/// The human-readable name of output device `dev` (an `AudioObjectID`), if available.
/// Resolving by id — rather than "whatever the default is right now" — keeps a label
/// truthful for a device the caller is already attached to, even if the system default
/// has since moved elsewhere.
pub fn output_device_name(dev: u32) -> Option<String> {
    output_device_string(dev, eqtune_output_device_name)
}

fn output_device_string(
    dev: u32,
    query: unsafe extern "C" fn(u32, *mut c_char, usize) -> bool,
) -> Option<String> {
    // CoreAudio device names are short; 256 bytes is ample for the UTF-8 form.
    let mut buf = [0u8; 256];
    let ok = unsafe { query(dev, buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
    if !ok {
        return None;
    }
    // The shim NUL-terminates on success; decode the bytes up to it. Treat an empty name
    // as "no name" so the caller falls back to the numeric id rather than showing a blank.
    let name = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    (!name.is_empty()).then(|| name.to_owned())
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
    // `run` scans the block for input silence once and returns it, so idle accounting
    // reuses that result instead of walking the buffer a second time here.
    let silent = state.processor.run(&settings, buf, channels as usize);
    if silent {
        state
            .activity
            .silent_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
    } else {
        state.activity.silent_frames.store(0, Ordering::Relaxed);
    }
}

/// Control-thread handle used to push fresh EQ settings to a running session,
/// lock-free, without restarting the audio engine.
#[derive(Clone)]
pub struct EqHandle {
    settings: Arc<ArcSwap<EqSettings>>,
    activity: Arc<AudioActivity>,
}

impl EqHandle {
    /// Publish a new snapshot to the audio thread. Its construction-time generation stamp
    /// is what the audio thread's change detection compares, so a new snapshot at a reused
    /// heap address can never be mistaken for the previous one.
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
    /// exact snapshotted output. Returns the session plus an [`EqHandle`] for live updates,
    /// or an error if the tap could not be created.
    pub fn start(
        target: &OutputTarget,
        channels: usize,
        initial: EqSettings,
    ) -> anyhow::Result<(Self, EqHandle)> {
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
        let mut error = [0u8; 512];
        let raw = unsafe {
            eqtune_tap_start(
                target.id,
                process_trampoline,
                ctx,
                error.as_mut_ptr().cast::<c_char>(),
                error.len(),
            )
        };
        if raw.is_null() {
            let message = CStr::from_bytes_until_nul(&error)
                .ok()
                .and_then(|s| s.to_str().ok())
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown Core Audio error");
            Err(anyhow::anyhow!("could not start the audio tap: {message}"))
        } else {
            Ok((Self { raw, _state: state }, handle))
        }
    }

    pub fn runtime_error(&self) -> Option<&'static str> {
        match unsafe { eqtune_tap_runtime_error(self.raw) } {
            0 => None,
            1 => Some("runtime output stream layout changed"),
            2 => Some("runtime input stream layout changed"),
            3 => Some("runtime stream buffer sizes no longer match"),
            _ => Some("unknown runtime stream failure"),
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
        let _ = OutputTarget::resolve_default();
        let _ = default_output_device().and_then(output_device_name);
        let _ = low_power_enabled();
        let _ = default_output_device_running();
    }

    #[test]
    fn stream_facts_describe_format_and_interleaving() {
        let mut facts = StreamFacts {
            sample_rate: 48_000.0,
            format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: 1,
            bytes_per_frame: 8,
            channels: 2,
            bits_per_channel: 32,
        };
        assert_eq!(facts.format_name(), "Float32");
        assert!(facts.interleaved());

        facts.format_flags |= 1 << 5;
        assert!(!facts.interleaved());
    }
}
