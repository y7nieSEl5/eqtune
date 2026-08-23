//! The long-running daemon: owns the config and the audio engine, and serves the
//! control socket. `on`/`off` start/stop the Core Audio tap; live edits push fresh
//! settings to the running engine lock-free; and a lightweight poll makes the engine
//! follow the system default output device (so plugging in EarPods/Bluetooth "just
//! works" without manually re-selecting output).

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::config::{
    Config, Preset, validate_band, validate_freq, validate_preamp, validate_preset,
};
use crate::dsp::{Band, BandKind, EqSettings, MAX_BANDS};
use crate::ipc::{self, PresetBackup, Request, Response, Status, Tuning};
use crate::sys::{self, EqHandle, OutputTarget, TapSession};

/// Two bands count as "the same band" if their frequencies are this close (Hz).
const BAND_MATCH_HZ: f32 = 0.5;
/// Channel count for the processor (stereo).
const CHANNELS: usize = 2;
/// Fallback sample rate (Hz) used only when the default output device's nominal rate is
/// unavailable. 48 kHz is the near-universal default for macOS output devices.
const DEFAULT_SAMPLE_RATE_HZ: u32 = 48_000;
/// How often the idle loop accepts connections and checks the default device.
const POLL: Duration = Duration::from_millis(100);
/// Total budget for one client request/response exchange. `read_request_line` spends this
/// down across the whole read (not just each recv), so a client that drips bytes just under
/// the socket timeout still can't hold the single-threaded accept/poll loop open.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest request line the daemon will read before giving up. Requests are single short
/// JSON lines; this only caps a misbehaving client so a newline-less flood can't grow the
/// read buffer without bound.
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// How long captured audio must remain silent before the engine is suspended.
const IDLE_SUSPEND_AFTER: Duration = Duration::from_secs(10);
/// Minimum spacing between session-draft mirror writes. The first edit after a quiet
/// period is mirrored immediately (an isolated edit is never at risk of being lost); a
/// *burst* of edits within this window is coalesced into one write flushed from the poll
/// loop, so dragging a control doesn't rewrite the whole config on every step. The mirror
/// is best-effort, so at most this much of an in-progress burst is at risk on a crash.
const SESSION_MIRROR_MIN_INTERVAL: Duration = Duration::from_millis(500);
const MAX_PRESET_NAME_LEN: usize = 64;
/// Bounded recovery schedule for one engine-failure incident. The first failed desired
/// start is the incident trigger; these are the six subsequent retry delays.
const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

#[derive(Debug, Default)]
struct Recovery {
    retries_attempted: usize,
    next_retry: Option<Instant>,
    exhausted: bool,
}

impl Recovery {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn pause(&mut self) {
        self.next_retry = None;
    }

    /// Schedule after an initial failure, before any retries have run.
    fn schedule_initial(&mut self, now: Instant) {
        self.retries_attempted = 0;
        self.exhausted = false;
        self.next_retry = Some(now + RETRY_DELAYS[0]);
    }

    /// Record one failed retry and either schedule the next or exhaust the incident.
    fn retry_failed(&mut self, now: Instant) {
        self.retries_attempted += 1;
        if self.retries_attempted >= RETRY_DELAYS.len() {
            self.next_retry = None;
            self.exhausted = true;
        } else {
            self.next_retry = Some(now + RETRY_DELAYS[self.retries_attempted]);
        }
    }

    fn due(&self, now: Instant) -> bool {
        self.next_retry.is_some_and(|at| now >= at)
    }
}

/// Process-lifetime advisory lock for the daemon. The lock file intentionally remains on
/// disk after exit: deleting it would let two processes lock different inodes during a
/// startup race. Closing the file releases the kernel lock automatically.
#[derive(Debug)]
struct DaemonLock {
    _file: std::fs::File,
}

impl DaemonLock {
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("could not open daemon lock {}", path.display()))?;
        // SAFETY: `file` owns a valid descriptor for the duration of the call and remains
        // alive inside `DaemonLock` for as long as the daemon is serving.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                anyhow::bail!("another eqtune daemon is already running");
            }
            return Err(error)
                .with_context(|| format!("could not lock daemon lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
}

/// Bind the control socket without deleting a live daemon's endpoint. A connection probe
/// preserves compatibility with older eqtune daemons that predate `DaemonLock`; only a
/// verified stale Unix socket is removed and rebound.
fn bind_control_listener(path: &Path) -> anyhow::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("could not bind control socket {}", path.display()));
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_socket() => anyhow::bail!(
            "control socket path exists but is not a Unix socket: {}",
            path.display()
        ),
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return UnixListener::bind(path)
                .with_context(|| format!("could not bind control socket {}", path.display()));
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("could not inspect control socket {}", path.display()));
        }
    }

    match UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "another eqtune daemon is already listening on {}",
            path.display()
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    std::fs::remove_file(path).with_context(|| {
                        format!("could not remove stale control socket {}", path.display())
                    })?;
                }
                Ok(_) => anyhow::bail!(
                    "control socket path changed to a non-socket while probing: {}",
                    path.display()
                ),
                Err(inspect) if inspect.kind() == ErrorKind::NotFound => {}
                Err(inspect) => {
                    return Err(inspect).with_context(|| {
                        format!("could not recheck control socket {}", path.display())
                    });
                }
            }
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!("could not probe existing control socket {}", path.display())
            });
        }
    }

    UnixListener::bind(path)
        .with_context(|| format!("could not bind control socket {}", path.display()))
}

#[derive(Debug, Serialize, Deserialize)]
struct PresetFile {
    name: String,
    bands: Vec<Band>,
    preamp_db: f32,
}

pub struct Daemon {
    /// Working config: includes live, unsaved tuning edits for the current session.
    config: Config,
    /// Last persisted config on disk. Used to discard drafts or save drafts as a new
    /// preset without overwriting the preset being edited.
    saved_config: Config,
    config_path: PathBuf,
    /// Where the working config is mirrored while it differs from `saved_config`, so an
    /// unsaved session survives a daemon restart. Removed once the session resolves.
    session_path: PathBuf,
    engine: Option<(TapSession, EqHandle)>,
    /// Authoritative metadata for the output the running engine was validated against.
    /// It is adopted only after tap startup succeeds.
    engine_target: Option<OutputTarget>,
    /// The effective target: the audio engine should be running iff this is true.
    /// `reconcile` starts/stops the engine to match it. It folds `user_intent` together
    /// with the automatic suspends (Low Power Mode, idle).
    engine_target_on: bool,
    /// The user's last explicit on/off *intent*, in memory. Seeded from the persisted
    /// `config.enabled` at startup and updated the instant `on`/`off` is handled — before
    /// the persist that records it durably. The automatic-suspend logic (Low Power Mode,
    /// idle) gates on this, not on `config.enabled`, so that a persist failure (which
    /// leaves `config.enabled` on its old on-disk value, reported as a retryable error)
    /// cannot desync the live idle/LPM behavior from what is actually running.
    user_intent: bool,
    /// Last-seen macOS Low Power Mode state (edge-detected in `follow_low_power`).
    low_power: bool,
    /// Last default output ID seen by the device follower. Unlike `engine_target`, this
    /// remains populated while recovery is waiting/exhausted, so a device change can
    /// reset the incident budget even though no tap is running.
    observed_output_id: Option<u32>,
    recovery: Recovery,
    last_engine_error: Option<String>,
    /// Whether the engine is currently off because captured audio was silent long enough
    /// to count as no active media.
    idle_suspended: bool,
    /// A session-draft mirror write is pending: an edit changed the working config within
    /// `SESSION_MIRROR_MIN_INTERVAL` of the last write, so the write was deferred to the
    /// poll loop (`maybe_flush_draft`) to coalesce the burst. See `sync_session_file`.
    draft_dirty: bool,
    /// When the session-draft mirror was last written, for the rate limit above. Reset to
    /// `None` when a session resolves, so the next session's first edit mirrors at once.
    draft_last_write: Option<Instant>,
}

impl Daemon {
    pub fn new() -> anyhow::Result<Self> {
        let saved_config = Config::load()?;
        let session_path = Config::session_path();
        // Restore the unsaved session a previous daemon run left behind (reboot, crash,
        // reinstall), if any — it stays a draft, resolved by the usual `off` prompt.
        let config = Config::load_session(&session_path, &saved_config)
            .unwrap_or_else(|| saved_config.clone());
        // Seed from the real state so the first poll doesn't fire a spurious edge.
        let low_power = sys::low_power_enabled();
        let observed_output_id = sys::default_output_device();
        // Runtime on/off intent, seeded from the persisted `enabled` and then tracked in
        // memory so it stays correct even if a later persist fails (see `set_enabled`).
        let user_intent = config.enabled;
        let lpm_suppressed = config.auto_off_low_power && low_power;
        let (engine_target_on, idle_suspended) =
            initial_engine_state(user_intent, config.auto_off_idle, lpm_suppressed);
        Ok(Self {
            // `run` reconciles once before serving requests; only the eager path needs it.
            engine_target_on,
            idle_suspended,
            user_intent,
            saved_config,
            config,
            config_path: Config::path(),
            session_path,
            engine: None,
            engine_target: None,
            low_power,
            observed_output_id,
            recovery: Recovery::default(),
            last_engine_error: None,
            draft_dirty: false,
            draft_last_write: None,
        })
    }

    /// Bind the control socket and serve requests; also follow default-device changes.
    pub fn run(mut self) -> anyhow::Result<()> {
        let path = ipc::socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _daemon_lock = DaemonLock::acquire(&path.with_extension("lock"))?;
        let listener = bind_control_listener(&path)?;
        listener.set_nonblocking(true)?;
        eprintln!("eqtune daemon listening on {}", path.display());

        // Restore the last run's on state (eager path only; the idle-aware restore starts
        // suspended and lets `follow_idle_activity` start the engine on real playback). A
        // start failure (capture permission not yet granted, unsupported macOS) must not
        // kill the daemon — under launchd KeepAlive that would crash-loop. Reconcile
        // records the failure and starts the bounded recovery schedule instead.
        if self.engine_target_on {
            if let Err(e) = self.reconcile() {
                eprintln!("could not restore the EQ at startup: {e}");
            }
        }

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false); // blocking for the short req/resp
                    // Bound the exchange so a stalled client can't freeze the daemon (which
                    // would also stall the device-follow, low-power, and idle polling below).
                    // The read timeout is the total request budget `read_request_line` spends
                    // down; the write timeout bounds the small response.
                    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                    if let Err(e) = self.handle(stream) {
                        eprintln!("connection error: {e}");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => eprintln!("accept error: {e}"),
            }
            self.follow_engine_health();
            self.follow_low_power();
            self.follow_idle_activity();
            self.follow_default_device();
            self.follow_recovery();
            self.maybe_flush_draft(Instant::now());
            std::thread::sleep(POLL);
        }
    }

    fn handle(&mut self, stream: UnixStream) -> anyhow::Result<()> {
        let line = read_request_line(&stream)?;
        if line.trim().is_empty() {
            return Ok(());
        }
        let resp = match serde_json::from_str::<Request>(line.trim_end()) {
            Ok(req) => self.dispatch(req),
            Err(e) => Response::Error(format!("bad request: {e}")),
        };
        let mut out = stream;
        let mut s = serde_json::to_string(&resp)?;
        s.push('\n');
        out.write_all(s.as_bytes())?;
        out.flush()?;
        Ok(())
    }

    fn dispatch(&mut self, req: Request) -> Response {
        match self.apply(req) {
            Ok(resp) => resp,
            Err(e) => Response::Error(e.to_string()),
        }
    }

    fn apply(&mut self, req: Request) -> anyhow::Result<Response> {
        match req {
            Request::Status => Ok(Response::Status(Box::new(self.status()))),
            Request::Enable => {
                self.user_intent = true;
                self.idle_suspended = false;
                self.engine_target_on = true;
                // An explicit `on` always starts a fresh incident and tries immediately,
                // including while a previous incident was waiting or exhausted.
                self.recovery.reset();
                // Override: starts even while Low Power Mode is active.
                let start = self.reconcile();
                // `enabled` is desired user intent, not a claim that the tap is healthy.
                // Persist it even when the immediate start failed so bounded recovery and
                // daemon restarts continue honoring the explicit command.
                self.set_enabled(true).context(
                    "the on intent could not be saved — it would not survive a daemon \
                     restart; retry `eqtune on`",
                )?;
                start.context(
                    "native output is active; automatic recovery is scheduled after 1 second",
                )?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::Disable => {
                // Stop the engine before anything fallible: `off` must never leave audio
                // processing because a disk write failed. The stop path of `reconcile`
                // is pure assignments and cannot fail. Clearing `user_intent` first keeps
                // a later LPM edge from restoring an EQ the user just turned off, even if
                // the persist below fails and `config.enabled` stays on its stale value.
                self.user_intent = false;
                self.idle_suspended = false;
                self.engine_target_on = false;
                self.recovery.reset();
                self.reconcile()?; // drops the TapSession -> large energy drop
                self.set_enabled(false).context(
                    "the EQ was stopped for this run, but the off state could not be \
                     saved — a daemon restart would turn it back on; retry `eqtune off`",
                )?;
                if self.has_unsaved_session() {
                    Ok(Response::UnsavedSession {
                        tuning: self.tuning(),
                        dirty_presets: self.dirty_preset_names(),
                    })
                } else {
                    Ok(Response::Ok)
                }
            }
            Request::ListPresets => Ok(Response::Presets {
                active: self.config.active_preset.clone(),
                names: self.config.presets.keys().cloned().collect(),
            }),
            Request::ShowPreset(name) => {
                let name = name.as_deref().unwrap_or(&self.config.active_preset);
                let Some(preset) = self.config.presets.get(name) else {
                    return Ok(Response::Error(format!("no such preset: {name}")));
                };
                Ok(Response::Tuning(Tuning {
                    enabled: self.engine.is_some(),
                    preset: name.to_string(),
                    preamp_db: preset.preamp_db,
                    bands: preset.bands.clone(),
                }))
            }
            Request::SetPreset(name) => {
                if !self.config.presets.contains_key(&name) {
                    return Ok(Response::Error(format!("no such preset: {name}")));
                }
                self.set_active_preset(name)?;
                self.apply_current_settings()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SavePreset { name } => {
                self.save_session_as(&name)?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::ClonePreset { source, dest } => {
                // Cloning rebuilds the working config from the saved one, which would
                // silently drop unsaved session edits — block it like the other
                // preset-management commands until the session is resolved.
                self.ensure_no_unsaved_session()?;
                self.clone_preset(&source, &dest)?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::DeletePresets { names } => {
                self.ensure_no_unsaved_session()?;
                self.commit_config(|c| delete_presets(c, &names))?;
                Ok(Response::Presets {
                    active: self.config.active_preset.clone(),
                    names: self.config.presets.keys().cloned().collect(),
                })
            }
            Request::RenamePreset { from, to } => {
                self.ensure_no_unsaved_session()?;
                self.commit_config(|c| rename_preset(c, &from, &to))?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::ExportPreset { name, path } => {
                export_preset(&self.config, &name, &path)?;
                Ok(Response::Ok)
            }
            Request::ImportPreset { path, name } => {
                self.ensure_no_unsaved_session()?;
                self.commit_config(|c| import_preset(c, &path, name.as_deref()))?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetBand { freq, gain_db, q } => {
                validate_band(freq, gain_db, q)?;
                let preset = self.active_preset_mut()?;
                if let Some(b) = preset
                    .bands
                    .iter_mut()
                    .find(|b| (b.freq - freq).abs() < BAND_MATCH_HZ)
                {
                    b.gain_db = gain_db;
                    b.q = q;
                } else {
                    if preset.bands.len() >= MAX_BANDS {
                        return Ok(Response::Error(format!(
                            "cannot add band: preset already has the maximum of {MAX_BANDS} bands"
                        )));
                    }
                    preset.bands.push(Band {
                        kind: BandKind::Peaking,
                        freq,
                        gain_db,
                        q,
                    });
                    preset.bands.sort_by(|a, b| a.freq.total_cmp(&b.freq));
                }
                self.apply_current_settings()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::RemoveBand { freq } => {
                validate_freq(freq)?;
                let removed = {
                    let preset = self.active_preset_mut()?;
                    let (index, nearest) = preset
                        .bands
                        .iter()
                        .enumerate()
                        .min_by(|(_, a), (_, b)| {
                            (a.freq - freq)
                                .abs()
                                .total_cmp(&(b.freq - freq).abs())
                                .then_with(|| a.freq.total_cmp(&b.freq))
                        })
                        .ok_or_else(|| anyhow::anyhow!("active preset has no bands to remove"))?;
                    if (nearest.freq - freq).abs() >= BAND_MATCH_HZ {
                        return Ok(Response::Error(format!(
                            "no band matches {freq} Hz; nearest configured band is {} Hz",
                            nearest.freq
                        )));
                    }
                    preset.bands.remove(index)
                };
                self.apply_current_settings()?;
                Ok(Response::BandRemoved {
                    tuning: self.tuning(),
                    removed,
                })
            }
            Request::SetPreamp(db) => {
                validate_preamp(db)?;
                self.active_preset_mut()?.preamp_db = db;
                self.apply_current_settings()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetLimiter(on) => {
                self.commit_setting(|c| c.limiter = on)?;
                self.apply_current_settings()?;
                Ok(Response::Ok)
            }
            Request::SetAutoOffLowPower(on) => {
                self.commit_setting(|c| c.auto_off_low_power = on)?;
                if on && self.low_power {
                    self.engine_target_on = false; // apply the policy right now
                    self.recovery.pause();
                } else if !on {
                    // Lift any LPM suppression — but an idle suspension is not this
                    // toggle's to lift: restarting the engine here with no media playing
                    // would contradict the idle policy (follow_low_power and
                    // SetAutoOffIdle apply the same guard).
                    self.engine_target_on = self.user_intent && !self.idle_suspended;
                    if self.engine_target_on && self.engine.is_none() {
                        self.recovery.reset();
                    }
                }
                self.reconcile()?;
                Ok(Response::Ok)
            }
            Request::SetAutoOffIdle(on) => {
                self.commit_setting(|c| c.auto_off_idle = on)?;
                if !on && self.idle_suspended {
                    self.idle_suspended = false;
                    if self.user_intent && !(self.config.auto_off_low_power && self.low_power) {
                        self.engine_target_on = true;
                        self.recovery.reset();
                    }
                }
                self.reconcile()?;
                Ok(Response::Ok)
            }
            Request::SaveSessionAs { name } => {
                self.save_session_as(&name)?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SaveSessionOverwrite => {
                // Commit the working config — the session edits — exactly as it stands.
                self.commit_config(|_| Ok(()))?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::DiscardSession => {
                self.discard_session()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::ResetPreset { name } => {
                self.ensure_no_unsaved_session()?;
                let changed =
                    modified_shipped_presets(&self.saved_config, std::slice::from_ref(&name))?;
                if changed.is_empty() {
                    self.confirm_reset_preset(&name, &[])?;
                    Ok(Response::Tuning(self.tuning()))
                } else {
                    Ok(Response::ResetWouldOverwrite { names: changed })
                }
            }
            Request::ConfirmResetPreset { name, backups } => {
                self.ensure_no_unsaved_session()?;
                self.confirm_reset_preset(&name, &backups)?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::Reset => {
                self.ensure_no_unsaved_session()?;
                let names = shipped_preset_names();
                let changed = modified_shipped_presets(&self.saved_config, &names)?;
                if changed.is_empty() {
                    self.confirm_reset_all(&[])?;
                    Ok(Response::Tuning(self.tuning()))
                } else {
                    Ok(Response::ResetWouldOverwrite { names: changed })
                }
            }
            Request::ConfirmReset { backups } => {
                self.ensure_no_unsaved_session()?;
                self.confirm_reset_all(&backups)?;
                Ok(Response::Tuning(self.tuning()))
            }
        }
    }

    fn active_preset_mut(&mut self) -> anyhow::Result<&mut Preset> {
        let name = self.config.active_preset.clone();
        self.config
            .presets
            .get_mut(&name)
            .ok_or_else(|| anyhow::anyhow!("active preset '{name}' is missing"))
    }

    /// Build engine settings from the active preset at sample rate `fs` (Hz).
    fn settings_for(&self, fs: f32) -> EqSettings {
        let active = self.config.active();
        let bands: &[Band] = active.map(|p| p.bands.as_slice()).unwrap_or(&[]);
        let preamp = active.map(|p| p.preamp_db).unwrap_or(0.0);
        EqSettings::new(bands, fs, preamp, self.config.limiter)
    }

    /// Start or stop the audio engine so its running state matches `engine_target_on`.
    /// Called on every state change (commands, Low-Power-Mode edges). Starting can fail
    /// (no tap permission / unsupported macOS); stopping cannot.
    fn reconcile(&mut self) -> anyhow::Result<()> {
        if self.engine_target_on && self.engine.is_none() {
            // A scheduled or exhausted incident is owned by `follow_recovery`; ordinary
            // reconciles must not smuggle in unbounded extra attempts.
            if self.recovery.next_retry.is_none() && !self.recovery.exhausted {
                self.try_start_engine(Instant::now(), false)?;
            }
        } else if !self.engine_target_on && self.engine.is_some() {
            self.engine = None; // drops TapSession -> stops the audio thread
            self.engine_target = None;
        }
        Ok(())
    }

    fn start_engine(&mut self) -> anyhow::Result<()> {
        if self.engine.is_some() {
            return Ok(());
        }
        let target = OutputTarget::resolve_default()?;
        let settings = self.settings_for(target.sample_rate as f32);
        let pair = TapSession::start(&target, CHANNELS, settings)?;
        self.engine = Some(pair);
        // Do not expose a partly-probed target: the snapshot becomes authoritative only
        // after the shim validated the aggregate streams and started its IOProc.
        self.engine_target = Some(target);
        Ok(())
    }

    fn try_start_engine(&mut self, now: Instant, retry: bool) -> anyhow::Result<()> {
        match self.start_engine() {
            Ok(()) => {
                self.recovery.reset();
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                self.last_engine_error = Some(message.clone());
                if self.engine_target_on {
                    if retry {
                        self.recovery.retry_failed(now);
                    } else {
                        self.recovery.schedule_initial(now);
                    }
                }
                Err(anyhow::anyhow!(message))
            }
        }
    }

    /// Lift a realtime stream/layout failure onto the control thread. The callback only
    /// publishes an atomic error and silences the unsafe block; teardown happens here,
    /// outside realtime constraints, and restores the native path while `user_intent`
    /// and `engine_target_on` continue to say the user wants EQ processing.
    fn follow_engine_health(&mut self) {
        let error = self
            .engine
            .as_ref()
            .and_then(|(session, _)| session.runtime_error());
        if let Some(error) = error {
            eprintln!("audio engine failed: {error} — restoring native output");
            self.engine = None;
            self.engine_target = None;
            self.last_engine_error = Some(error.to_string());
            self.recovery.reset();
            if self.engine_target_on {
                self.recovery.schedule_initial(Instant::now());
            }
        }
    }

    /// Rebuild the engine if the system default output device (or its sample rate)
    /// changed, so replay follows wherever audio is now meant to go.
    fn follow_default_device(&mut self) {
        let output_id = sys::default_output_device();
        if output_id != self.observed_output_id {
            self.observed_output_id = output_id;
            eprintln!("default output changed to {output_id:?} — rebuilding engine");
            self.engine = None;
            self.engine_target = None;
            if self.engine_target_on {
                self.recovery.reset();
                if let Err(e) = self.reconcile() {
                    eprintln!("engine rebuild failed: {e}");
                }
            } else {
                self.recovery.pause();
            }
            return;
        }
        if self.engine.is_none() {
            return;
        }
        let Some(target) = self.engine_target.as_ref() else {
            return;
        };
        // The steady-state poll needs only the one mutable target property that changes
        // DSP construction. UID, name, and stream facts are resolved once per startup;
        // runtime layout changes are reported by the IOProc itself.
        let Some(rate) = sys::output_device_sample_rate(target.id) else {
            eprintln!("could not inspect output #{} sample rate", target.id);
            return;
        };
        if rate.round() as u32 != target.sample_rate.round() as u32 {
            eprintln!("output sample rate changed to {rate} Hz — rebuilding engine");
            self.engine = None;
            self.engine_target = None;
            self.recovery.reset();
            if let Err(e) = self.reconcile() {
                eprintln!("engine rebuild failed: {e}");
            }
        }
    }

    fn follow_recovery(&mut self) {
        if !self.engine_target_on || self.engine.is_some() {
            return;
        }
        let now = Instant::now();
        if !self.recovery.due(now) {
            return;
        }
        let attempt = self.recovery.retries_attempted + 1;
        self.recovery.next_retry = None;
        eprintln!("retrying audio engine ({attempt}/{})", RETRY_DELAYS.len());
        if let Err(e) = self.try_start_engine(now, true) {
            if self.recovery.exhausted {
                eprintln!(
                    "audio engine recovery exhausted after {} retries: {e}",
                    RETRY_DELAYS.len()
                );
            } else {
                eprintln!("audio engine retry failed: {e}");
            }
        }
    }

    /// Follow macOS Low Power Mode: on entering LPM, auto-off the engine (a large energy
    /// drop) while remembering the user's intent; on leaving LPM, restore that intent.
    /// Edge-triggered; leaving a real suppression starts a fresh recovery incident.
    fn follow_low_power(&mut self) {
        let now = sys::low_power_enabled();
        if now == self.low_power {
            return;
        }
        self.low_power = now;
        if !self.config.auto_off_low_power {
            return; // policy disabled: track the state but don't act
        }
        self.engine_target_on = if now {
            false
        } else {
            self.user_intent && !self.idle_suspended
        };
        if now {
            self.recovery.pause();
        } else if self.engine_target_on && self.engine.is_none() {
            // Leaving a suppressing policy is a legitimate new resume incident.
            self.recovery.reset();
        }
        eprintln!(
            "low power mode {} — eqtune {}",
            if now { "on" } else { "off" },
            if self.engine_target_on {
                "resuming"
            } else {
                "suspended"
            }
        );
        if let Err(e) = self.reconcile() {
            eprintln!("engine reconcile failed: {e}");
        }
    }

    /// Suspend on sustained captured silence, then resume when Core Audio reports the
    /// default output device running again. The resume probe only runs while suspended;
    /// while the tap is active, eqtune itself keeps the output device running.
    fn follow_idle_activity(&mut self) {
        if !self.config.auto_off_idle || !self.user_intent {
            return;
        }

        if let Some((_, handle)) = &self.engine {
            let rate = self
                .engine_target
                .as_ref()
                .map(|target| target.sample_rate.round() as u32)
                .unwrap_or(DEFAULT_SAMPLE_RATE_HZ);
            let idle_frames = IDLE_SUSPEND_AFTER.as_secs().saturating_mul(rate as u64);
            if handle.silent_frames() >= idle_frames {
                self.idle_suspended = true;
                self.engine_target_on = false;
                self.recovery.pause();
                eprintln!("no active media detected — eqtune suspended");
                if let Err(e) = self.reconcile() {
                    eprintln!("engine idle-suspend failed: {e}");
                }
            }
            return;
        }

        if !self.idle_suspended {
            return;
        }
        if self.config.auto_off_low_power && self.low_power {
            return;
        }
        if sys::default_output_device_running() {
            self.idle_suspended = false;
            self.engine_target_on = true;
            self.recovery.reset();
            eprintln!("default output active — eqtune resuming");
            if let Err(e) = self.reconcile() {
                eprintln!("engine idle-resume failed: {e}");
            }
        }
    }

    /// Commit an immediately-persisted setting (a global toggle or the preset switch):
    /// apply `set` to a copy of the saved config, persist that copy, and only then adopt
    /// it into both in-memory configs. Persist-first is the invariant — on a failed
    /// write nothing in memory changes, so `status` and the engine keep matching what is
    /// actually on disk, and a retried command re-attempts the write instead of hitting
    /// the no-op skip. Applying `set` to both configs keeps the field equal in `config`
    /// and `saved_config`, so an immediate-commit change never shows up as an
    /// unsaved-session diff. A change that alters nothing skips the disk write.
    fn commit_setting(&mut self, set: impl Fn(&mut Config)) -> anyhow::Result<()> {
        let mut next = self.saved_config.clone();
        set(&mut next);
        if next == self.saved_config {
            return Ok(());
        }
        next.save_to(&self.config_path)?;
        set(&mut self.config);
        self.saved_config = next;
        Ok(())
    }

    /// Commit a preset switch. Switching is a selection, not a tuning edit: like the
    /// global toggles it is persisted immediately, so it survives restarts and never
    /// counts as an unsaved session by itself — `eqtune off` right after a switch must
    /// not raise the save prompt. Draft edits to any preset's contents stay uncommitted;
    /// only `active_preset` changes in the saved config.
    fn set_active_preset(&mut self, name: String) -> anyhow::Result<()> {
        self.commit_setting(|c| c.active_preset = name.clone())
    }

    /// Persist the user's explicit on/off into `config.enabled`, so the state is restored
    /// at the next daemon startup. This is the *durable* record only; the live runtime
    /// intent is `self.user_intent`, updated by the `on`/`off` handlers before this call,
    /// so a failed persist here costs durability but not correct idle/LPM behavior.
    /// `enabled` is a global toggle, not session state: drafts are not committed with it.
    fn set_enabled(&mut self, on: bool) -> anyhow::Result<()> {
        self.commit_setting(|c| c.enabled = on)
    }

    /// Commit a working-config mutation: mutate a clone, persist it, and adopt it in
    /// memory only on success — the whole-config sibling of `commit_setting`. A failed
    /// save leaves both configs (and so `status` and the engine) on the state the disk
    /// still has, and a retry of the command re-attempts the write. Committing the
    /// working config as it stands (resolving a session) is the identity mutation.
    fn commit_config(
        &mut self,
        mutate: impl FnOnce(&mut Config) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut next = self.config.clone();
        mutate(&mut next)?;
        next.save_to(&self.config_path)?;
        self.saved_config = next.clone();
        self.config = next;
        self.apply_current_settings()
    }

    /// Push the working config to the running engine (if any) and mirror the session
    /// draft to disk. Every mutation of the working config funnels through here. The
    /// on-disk draft tracks `has_unsaved_session`: written while a draft exists (writes
    /// coalesced by a rate limit, see `sync_session_file`), removed the moment the session
    /// resolves (save, overwrite, discard, or any persist). The engine push cannot fail;
    /// the returned error is `sync_session_file`'s.
    fn apply_current_settings(&mut self) -> anyhow::Result<()> {
        let synced = self.sync_session_file();
        if self.engine.is_some() {
            let fs = self
                .engine_target
                .as_ref()
                .map(|target| target.sample_rate as f32)
                .unwrap_or(DEFAULT_SAMPLE_RATE_HZ as f32);
            let settings = self.settings_for(fs);
            if let Some((_, handle)) = &self.engine {
                handle.store(settings); // lock-free live update
            }
        }
        synced
    }

    /// Mirror the session-draft state to disk so it survives a daemon restart.
    ///
    /// While an unsaved session exists the draft is written, but *rate-limited*: the first
    /// edit after a quiet period mirrors immediately, and further edits within
    /// `SESSION_MIRROR_MIN_INTERVAL` only mark `draft_dirty`, to be flushed once by
    /// `maybe_flush_draft` in the poll loop. This coalesces a burst (dragging a control)
    /// into a few writes instead of rewriting the whole config on every step. A *write*
    /// failure is logged, not propagated: the edit already applied live, and failing the
    /// command over a degraded best-effort mirror would be worse than a draft that only
    /// lives in memory (the pre-mirror behavior).
    ///
    /// A *removal* (session resolved) is neither deferred nor best-effort: it happens now,
    /// and a failure is an error. The leftover draft is authoritative restore state, so
    /// the next daemon startup would resurrect the very session the command just resolved
    /// (a discarded tuning coming back, or a stale draft shadowing a fresh save) — the
    /// resolving command must not report success over that. Resolving also cancels any
    /// pending deferred write and resets the rate limit.
    fn sync_session_file(&mut self) -> anyhow::Result<()> {
        if self.has_unsaved_session() {
            let now = Instant::now();
            if self.draft_write_due(now) {
                self.write_draft(now);
            } else {
                self.draft_dirty = true; // flushed by `maybe_flush_draft`
            }
            return Ok(());
        }
        // Session resolved: cancel any pending mirror write and drop the file now.
        self.draft_dirty = false;
        self.draft_last_write = None;
        if let Err(e) = std::fs::remove_file(&self.session_path) {
            if e.kind() != ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "the tuning was applied, but the resolved session draft at {} could \
                     not be removed ({e}); a daemon restart would restore it as unsaved \
                     tuning — remove the file manually",
                    self.session_path.display()
                ));
            }
        }
        Ok(())
    }

    /// Whether enough time has passed since the last mirror write to write again now.
    /// `None` (fresh session, or just resolved) always writes — the leading edge that
    /// keeps an isolated edit mirrored immediately.
    fn draft_write_due(&self, now: Instant) -> bool {
        self.draft_last_write
            .is_none_or(|last| now.duration_since(last) >= SESSION_MIRROR_MIN_INTERVAL)
    }

    /// Write the session-draft mirror (best-effort) and record the time for the rate
    /// limit. Clears `draft_dirty` regardless of outcome; a failure leaves
    /// `draft_last_write` untouched so the next edit retries promptly.
    fn write_draft(&mut self, now: Instant) {
        self.draft_dirty = false;
        match self.config.write_draft_to(&self.session_path) {
            Ok(()) => self.draft_last_write = Some(now),
            Err(e) => eprintln!(
                "could not mirror the session draft to {}: {e}",
                self.session_path.display()
            ),
        }
    }

    /// Flush a deferred session-draft write once the rate-limit interval has elapsed.
    /// Called from the poll loop, so a burst of edits lands as one coalesced write.
    fn maybe_flush_draft(&mut self, now: Instant) {
        if self.draft_dirty && self.draft_write_due(now) {
            self.write_draft(now);
        }
    }

    fn has_unsaved_session(&self) -> bool {
        self.config != self.saved_config
    }

    /// Names of every preset whose working contents differ from the saved config — the
    /// actual substance of an unsaved session. Immediate-commit fields (the preset
    /// switch, global toggles) are equalized in both configs by `commit_setting` and so
    /// never appear here; edits left on a previously active preset do.
    fn dirty_preset_names(&self) -> Vec<String> {
        self.config
            .presets
            .iter()
            .filter(|(name, contents)| {
                self.saved_config.presets.get(name.as_str()) != Some(contents)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn ensure_no_unsaved_session(&self) -> anyhow::Result<()> {
        if self.has_unsaved_session() {
            Err(anyhow::anyhow!(
                "unsaved tuning changes are active; run `eqtune off` and save or discard them first"
            ))
        } else {
            Ok(())
        }
    }

    /// Save the *active* preset's working tuning under `name` and switch to it. This
    /// save consumes the active preset's unsaved edit, and — when `name` is an explicit
    /// overwrite of an existing preset — supersedes any pending edit of that preset.
    /// Unsaved edits to every other preset (left behind by a preset switch) are neither:
    /// they are carried over into the new working config and stay an open session,
    /// re-raised by the next `off` prompt, instead of being silently reverted to their
    /// saved contents.
    fn save_session_as(&mut self, name: &str) -> anyhow::Result<()> {
        validate_session_save_name(&self.saved_config, &self.config.active_preset, name)?;
        let active_name = self.config.active_preset.clone();
        let preset = self
            .config
            .active()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no active preset to save"))?;
        let mut next = self.saved_config.clone();
        next.presets.insert(name.to_string(), preset);
        next.active_preset = name.to_string();
        // Persist first (commit_setting's invariant): a failed write changes nothing
        // in memory.
        next.save_to(&self.config_path)?;
        let mut working = next.clone();
        for (preset_name, contents) in &self.config.presets {
            if *preset_name != active_name && preset_name != name {
                working
                    .presets
                    .insert(preset_name.clone(), contents.clone());
            }
        }
        self.saved_config = next;
        self.config = working;
        self.apply_current_settings()?;
        Ok(())
    }

    fn clone_preset(&mut self, source: &str, dest: &str) -> anyhow::Result<()> {
        validate_new_preset_name(&self.saved_config, dest)?;
        let preset = self
            .config
            .presets
            .get(source)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such preset: {source}"))?;
        let mut next = self.saved_config.clone();
        next.presets.insert(dest.to_string(), preset);
        next.active_preset = dest.to_string();
        // Persist first: a failed write must not leave a half-applied clone in the
        // working config (which would read as a phantom unsaved session).
        next.save_to(&self.config_path)?;
        self.saved_config = next.clone();
        self.config = next;
        self.apply_current_settings()?;
        Ok(())
    }

    fn discard_session(&mut self) -> anyhow::Result<()> {
        self.config = self.saved_config.clone();
        self.apply_current_settings()
    }

    // Every reset entry point ensures no unsaved session first, so the working config
    // these two mutate equals the saved one. Both rewrite only preset *contents* and
    // leave the engine-lifecycle state (idle_suspended, engine_target_on) untouched: if
    // the engine was idle-suspended, it stays suspended and the resume probe restarts it
    // with the reset tuning when playback returns. Clearing the suspension here without
    // reconciling would instead strand the engine off (the resume probe only runs while
    // suspended); reconciling would restart it just to process silence — wasteful.
    fn confirm_reset_preset(&mut self, name: &str, backups: &[PresetBackup]) -> anyhow::Result<()> {
        self.commit_config(|c| {
            apply_reset_backups(c, backups)?;
            reset_preset(c, name)
        })
    }

    fn confirm_reset_all(&mut self, backups: &[PresetBackup]) -> anyhow::Result<()> {
        self.commit_config(|c| {
            apply_reset_backups(c, backups)?;
            reset_shipped_presets(c);
            Ok(())
        })
    }

    fn status(&self) -> Status {
        let active = self.config.active();
        // Metadata comes only from the target whose stream validation and startup
        // succeeded. Never relabel a running engine from the current system default.
        let target = self
            .engine_target
            .as_ref()
            .filter(|_| self.engine.is_some());
        let now = Instant::now();
        let retry_in_seconds = self.recovery.next_retry.map(|at| {
            let millis = at.saturating_duration_since(now).as_millis();
            millis.div_ceil(1_000) as u64
        });
        Status {
            user_intent: self.user_intent,
            engine_running: self.engine.is_some(),
            suspension_reason: self.suspension_reason().map(str::to_owned),
            active_preset: self.config.active_preset.clone(),
            preamp_db: active.map(|p| p.preamp_db).unwrap_or(0.0),
            band_count: active.map(|p| p.bands.len()).unwrap_or(0),
            limiter: self.config.limiter,
            output_uid: target.map(|target| target.uid.clone()),
            output_name: target.map(|target| target.name.clone()),
            output_rate_hz: target.map(|target| target.sample_rate),
            output_stream: target.map(|target| target.stream.description()),
            last_engine_error: self.last_engine_error.clone(),
            retry_attempts: self.recovery.retries_attempted,
            retry_limit: RETRY_DELAYS.len(),
            retry_in_seconds,
            retry_exhausted: self.recovery.exhausted,
            bypassed: false,
            dirty_presets: self.dirty_preset_names(),
            low_power: self.low_power,
            auto_off_low_power: self.config.auto_off_low_power,
            auto_off_idle: self.config.auto_off_idle,
        }
    }

    fn suspension_reason(&self) -> Option<&'static str> {
        if self.engine.is_some() {
            None
        } else if !self.user_intent {
            Some("user-off")
        } else if self.config.auto_off_low_power && self.low_power && !self.engine_target_on {
            Some("low-power")
        } else if self.idle_suspended {
            Some("idle")
        } else if self.recovery.exhausted {
            Some("recovery-exhausted")
        } else if self.recovery.next_retry.is_some() {
            Some("recovering")
        } else {
            Some("starting")
        }
    }

    /// A snapshot of the active EQ tuning, returned after `on` and edits so the CLI can
    /// print the resulting curve.
    fn tuning(&self) -> Tuning {
        let active = self.config.active();
        Tuning {
            enabled: self.engine.is_some(),
            preset: self.config.active_preset.clone(),
            preamp_db: active.map(|p| p.preamp_db).unwrap_or(0.0),
            bands: active.map(|p| p.bands.clone()).unwrap_or_default(),
        }
    }
}

/// Read one newline-terminated request line, bounded by a total wall-clock budget (the
/// socket's read timeout, i.e. [`REQUEST_TIMEOUT`]) and a [`MAX_REQUEST_BYTES`] size cap.
///
/// `BufRead::read_line`'s only bound is the socket's per-recv timeout, so a client dripping
/// one byte just inside that window keeps it looping forever — wedging the single-threaded
/// loop — and grows the buffer without bound on a newline-less flood. Enforcing an overall
/// deadline and a size cap closes both.
fn read_request_line(stream: &UnixStream) -> anyhow::Result<String> {
    // Bound each recv (so a silent client can't block the loop forever) and the read as a
    // whole (so a client dripping bytes just under that per-recv bound still can't hold it
    // open). `budget` is the socket's configured read timeout — set by `run` to
    // REQUEST_TIMEOUT — falling back to REQUEST_TIMEOUT if none is set; we (re)apply it once
    // here so this stays correct even if a caller forgot to. The overall bound is then a
    // deadline checked after each read rather than a per-recv timeout re-armed every
    // iteration: macOS can reject a rapid re-arm of SO_RCVTIMEO under an active flood.
    let budget = stream.read_timeout()?.unwrap_or(REQUEST_TIMEOUT);
    stream.set_read_timeout(Some(budget))?;
    let deadline = Instant::now() + budget;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = Vec::new();
    loop {
        match reader.fill_buf() {
            Ok([]) => break, // EOF before a newline
            Ok(available) => {
                let newline = available.iter().position(|&b| b == b'\n');
                let upto = newline.unwrap_or(available.len());
                line.extend_from_slice(&available[..upto]);
                reader.consume(newline.map_or(upto, |i| i + 1));
                // Enforce both bounds after every read, before accepting — including the
                // read that carries the terminating newline. Checking them only on the
                // no-newline path let a request whose '\n' landed in this buffer slip past
                // the size cap or the deadline, keeping the single-threaded loop blocked
                // past REQUEST_TIMEOUT.
                check_request_bounds(line.len(), deadline)?;
                if newline.is_some() {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                anyhow::bail!("client did not send a complete request in time");
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

/// The size and time bounds a request read must satisfy, checked after every buffer is
/// appended so neither can be bypassed at the read that carries the terminating newline.
fn check_request_bounds(line_len: usize, deadline: Instant) -> anyhow::Result<()> {
    if line_len > MAX_REQUEST_BYTES {
        anyhow::bail!("request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    if Instant::now() >= deadline {
        anyhow::bail!("client did not send a complete request in time");
    }
    Ok(())
}

/// Initial `(engine_target_on, idle_suspended)` for a daemon restoring the persisted
/// on/off at startup. When the EQ was on and idle auto-off is enabled, restore *suspended*
/// so `follow_idle_activity` starts the engine only once the output device is actually
/// playing — a login/restart with nothing playing then never runs the tap through startup
/// silence. Otherwise restore eagerly, honoring Low-Power-Mode suppression. The suspended
/// flag is kept even under LPM so that when LPM clears the idle probe (not an eager
/// LPM-restore) governs the first start.
fn initial_engine_state(
    user_intent: bool,
    auto_off_idle: bool,
    lpm_suppressed: bool,
) -> (bool, bool) {
    let lazy_start = user_intent && auto_off_idle;
    let engine_target_on = user_intent && !lpm_suppressed && !lazy_start;
    (engine_target_on, lazy_start)
}

fn delete_presets(config: &mut Config, names: &[String]) -> anyhow::Result<()> {
    if names.is_empty() {
        return Err(anyhow::anyhow!("at least one preset name is required"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(anyhow::anyhow!("duplicate preset name: {name}"));
        }
        if !config.presets.contains_key(name) {
            return Err(anyhow::anyhow!("no such preset: {name}"));
        }
    }
    if names.len() >= config.presets.len() {
        return Err(anyhow::anyhow!("cannot delete every preset"));
    }
    for name in names {
        config.presets.remove(name);
    }
    if names.iter().any(|name| name == &config.active_preset) {
        config.active_preset = config
            .presets
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no presets remain"))?;
    }
    Ok(())
}

fn rename_preset(config: &mut Config, from: &str, to: &str) -> anyhow::Result<()> {
    let preset = config
        .presets
        .remove(from)
        .ok_or_else(|| anyhow::anyhow!("no such preset: {from}"))?;
    if let Err(e) = validate_new_preset_name(config, to) {
        config.presets.insert(from.to_string(), preset);
        return Err(e);
    }
    config.presets.insert(to.to_string(), preset);
    if config.active_preset == from {
        config.active_preset = to.to_string();
    }
    Ok(())
}

fn export_preset(config: &Config, name: &str, path: &Path) -> anyhow::Result<()> {
    let preset = config
        .presets
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no such preset: {name}"))?;
    let file = PresetFile {
        name: name.to_string(),
        bands: preset.bands.clone(),
        preamp_db: preset.preamp_db,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&file)?)?;
    Ok(())
}

fn import_preset(
    config: &mut Config,
    path: &Path,
    name_override: Option<&str>,
) -> anyhow::Result<()> {
    let file: PresetFile = toml::from_str(&std::fs::read_to_string(path)?)?;
    let name = name_override.unwrap_or(&file.name);
    validate_new_preset_name(config, name)?;
    validate_preset(&file.preset())?;
    config.presets.insert(name.to_string(), file.preset());
    config.active_preset = name.to_string();
    Ok(())
}

fn reset_preset(config: &mut Config, name: &str) -> anyhow::Result<()> {
    let defaults = Config::default();
    let preset = defaults
        .presets
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no shipped preset: {name}"))?;
    config.presets.insert(name.to_string(), preset);
    if !config.presets.contains_key(&config.active_preset) {
        config.active_preset = name.to_string();
    }
    Ok(())
}

fn reset_shipped_presets(config: &mut Config) {
    let defaults = Config::default();
    for (name, preset) in defaults.presets {
        config.presets.insert(name, preset);
    }
    config.active_preset = defaults.active_preset;
}

fn apply_reset_backups(config: &mut Config, backups: &[PresetBackup]) -> anyhow::Result<()> {
    for backup in backups {
        if !is_shipped_preset_name(&backup.source) {
            return Err(anyhow::anyhow!(
                "can only back up shipped presets during reset: {}",
                backup.source
            ));
        }
        validate_new_preset_name(config, &backup.dest)?;
        let preset = config
            .presets
            .get(&backup.source)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such preset: {}", backup.source))?;
        config.presets.insert(backup.dest.clone(), preset);
    }
    Ok(())
}

fn modified_shipped_presets(config: &Config, names: &[String]) -> anyhow::Result<Vec<String>> {
    let defaults = Config::default();
    let mut changed = Vec::new();
    for name in names {
        let default = defaults
            .presets
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no shipped preset: {name}"))?;
        if matches!(config.presets.get(name), Some(current) if current != default) {
            changed.push(name.clone());
        }
    }
    Ok(changed)
}

fn shipped_preset_names() -> Vec<String> {
    Config::default().presets.keys().cloned().collect()
}

impl PresetFile {
    fn preset(&self) -> Preset {
        Preset {
            bands: self.bands.clone(),
            preamp_db: self.preamp_db,
        }
    }
}

fn validate_new_preset_name(config: &Config, name: &str) -> anyhow::Result<()> {
    validate_preset_name(name)?;
    if config.presets.contains_key(name) {
        return Err(anyhow::anyhow!("preset already exists: {name}"));
    }
    Ok(())
}

/// A session may be saved under a new name, a shipped name (a deliberate overwrite of a
/// built-in), or the active preset's own name — saving the tuning back into the preset
/// being edited is exactly the overwrite action, not an accident to prevent. Only the
/// names of *other* custom presets are rejected, to prevent accidental loss.
fn validate_session_save_name(config: &Config, active: &str, name: &str) -> anyhow::Result<()> {
    validate_preset_name(name)?;
    if config.presets.contains_key(name) && !is_shipped_preset_name(name) && name != active {
        return Err(anyhow::anyhow!("preset already exists: {name}"));
    }
    Ok(())
}

fn is_shipped_preset_name(name: &str) -> bool {
    Config::default().presets.contains_key(name)
}

fn validate_preset_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        return Err(anyhow::anyhow!("preset name must not be empty"));
    }
    if name.len() > MAX_PRESET_NAME_LEN {
        return Err(anyhow::anyhow!(
            "preset name must be at most {MAX_PRESET_NAME_LEN} characters"
        ));
    }
    if name != name.trim() {
        return Err(anyhow::anyhow!(
            "preset name must not have leading or trailing whitespace"
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(anyhow::anyhow!(
            "preset name may only contain ASCII letters, digits, '-', '_', or '.'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_show_reads_without_switching_or_persisting() {
        let mut d = daemon_with(Config::default());
        let before = d.config.clone();
        let response = d.apply(Request::ShowPreset(Some("mellow".into()))).unwrap();

        let Response::Tuning(tuning) = response else {
            panic!("expected tuning response");
        };
        assert_eq!(tuning.preset, "mellow");
        assert_eq!(tuning.preamp_db, before.presets["mellow"].preamp_db);
        assert_eq!(tuning.bands, before.presets["mellow"].bands);
        assert_eq!(d.config, before);
        assert_eq!(d.saved_config, before);
    }

    #[test]
    fn preset_show_without_a_name_uses_the_active_working_tuning() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        let Response::Tuning(tuning) = d.apply(Request::ShowPreset(None)).unwrap() else {
            panic!("expected tuning response");
        };
        assert_eq!(tuning.preset, "bright");
        assert_eq!(tuning.preamp_db, -3.0);
        assert_eq!(d.saved_config.presets["bright"].preamp_db, -8.0);
    }

    #[test]
    fn preset_show_rejects_an_unknown_name() {
        let mut d = daemon_with(Config::default());
        assert_eq!(
            d.apply(Request::ShowPreset(Some("missing".into())))
                .unwrap(),
            Response::Error("no such preset: missing".into())
        );
    }

    #[test]
    fn limiter_toggle_persists_without_committing_tuning_edits() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        d.apply(Request::SetLimiter(false)).unwrap();

        assert!(!d.config.limiter);
        assert!(!d.saved_config.limiter);
        assert!(d.has_unsaved_session());
        assert_eq!(d.config.presets["bright"].preamp_db, -3.0);
        assert_eq!(d.saved_config.presets["bright"].preamp_db, -8.0);
        let on_disk = Config::load_from(&d.config_path).unwrap();
        assert!(!on_disk.limiter);
        assert_eq!(on_disk.presets["bright"].preamp_db, -8.0);
    }

    #[test]
    fn limiter_save_failure_leaves_state_untouched_and_retryable() {
        let mut d = daemon_with(Config::default());
        let blocker = tmp_path("limiter-not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::SetLimiter(false)).is_err());
        assert!(d.config.limiter);
        assert!(d.saved_config.limiter);

        d.config_path = good_path;
        d.apply(Request::SetLimiter(false)).unwrap();
        assert!(!d.config.limiter);
        assert!(!Config::load_from(&d.config_path).unwrap().limiter);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn preset_clone_copies_source_contents_and_selects_it() {
        let mut d = daemon_with(Config::default());
        let source = d.saved_config.presets["mellow"].clone();

        d.apply(Request::ClonePreset {
            source: "mellow".into(),
            dest: "night".into(),
        })
        .unwrap();

        // The clone copies the source verbatim, selects it, and commits immediately — no
        // unsaved session, and it is on disk.
        assert_eq!(d.config.active_preset, "night");
        assert_eq!(d.config.presets["night"], source);
        assert!(!d.has_unsaved_session());
        assert_eq!(
            Config::load_from(&d.config_path).unwrap().presets["night"],
            source
        );
    }

    #[test]
    fn preset_delete_removes_active_and_selects_another() {
        let mut c = Config {
            active_preset: "mellow".into(),
            ..Config::default()
        };
        delete_presets(&mut c, &["mellow".into()]).unwrap();
        assert!(!c.presets.contains_key("mellow"));
        assert!(c.presets.contains_key(&c.active_preset));
    }

    #[test]
    fn preset_delete_rejects_last_preset() {
        let mut c = Config::default();
        c.presets.retain(|name, _| name == "bright");
        c.active_preset = "bright".into();
        assert!(delete_presets(&mut c, &["bright".into()]).is_err());
        assert!(c.presets.contains_key("bright"));
    }

    #[test]
    fn preset_delete_removes_multiple_presets_atomically() {
        let mut c = Config::default();
        c.presets.insert(
            "daily".into(),
            Preset {
                bands: vec![],
                preamp_db: 0.0,
            },
        );
        c.active_preset = "daily".into();

        delete_presets(&mut c, &["daily".into(), "mellow".into()]).unwrap();

        assert!(!c.presets.contains_key("daily"));
        assert!(!c.presets.contains_key("mellow"));
        assert!(c.presets.contains_key(&c.active_preset));
    }

    #[test]
    fn preset_delete_rejects_duplicate_or_all_names_without_mutating() {
        let mut c = Config::default();
        let before = c.clone();

        assert!(delete_presets(&mut c, &["bright".into(), "bright".into()]).is_err());
        assert_eq!(c, before);

        let all = c.presets.keys().cloned().collect::<Vec<_>>();
        assert!(delete_presets(&mut c, &all).is_err());
        assert_eq!(c, before);
    }

    #[test]
    fn preset_rename_moves_preset_and_updates_active_name() {
        let mut c = Config {
            active_preset: "bright".into(),
            ..Config::default()
        };
        let bright = c.presets["bright"].clone();
        rename_preset(&mut c, "bright", "daily").unwrap();
        assert!(!c.presets.contains_key("bright"));
        assert_eq!(c.active_preset, "daily");
        assert_eq!(c.presets["daily"], bright);
    }

    #[test]
    fn preset_names_are_restricted_for_new_presets() {
        let c = Config::default();
        for name in ["", "two words", " lead", "trail ", "emoji-☃"] {
            assert!(validate_new_preset_name(&c, name).is_err());
        }
        assert!(validate_new_preset_name(&c, "bright").is_err());
        assert!(validate_new_preset_name(&c, "daily.v2").is_ok());
    }

    #[test]
    fn preset_export_writes_shareable_toml() {
        let c = Config::default();
        let path = tmp_path("export.toml");
        export_preset(&c, "bright", &path).unwrap();
        let file: PresetFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(file.name, "bright");
        assert_eq!(file.preset(), c.presets["bright"]);
    }

    #[test]
    fn preset_import_reads_file_and_selects_preset() {
        let mut c = Config::default();
        let path = tmp_path("import.toml");
        let file = PresetFile {
            name: "shared".into(),
            bands: c.presets["mellow"].bands.clone(),
            preamp_db: c.presets["mellow"].preamp_db,
        };
        std::fs::write(&path, toml::to_string_pretty(&file).unwrap()).unwrap();

        import_preset(&mut c, &path, None).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(c.active_preset, "shared");
        assert_eq!(c.presets["shared"], file.preset());
    }

    #[test]
    fn preset_import_name_override_wins() {
        let mut c = Config::default();
        let path = tmp_path("import-override.toml");
        let file = PresetFile {
            name: "shared".into(),
            bands: c.presets["mellow"].bands.clone(),
            preamp_db: c.presets["mellow"].preamp_db,
        };
        std::fs::write(&path, toml::to_string_pretty(&file).unwrap()).unwrap();

        import_preset(&mut c, &path, Some("renamed-share")).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(!c.presets.contains_key("shared"));
        assert_eq!(c.active_preset, "renamed-share");
        assert_eq!(c.presets["renamed-share"], file.preset());
    }

    #[test]
    fn preset_import_rejects_duplicate_or_invalid_values() {
        let mut c = Config::default();
        let duplicate = tmp_path("import-duplicate.toml");
        let invalid = tmp_path("import-invalid.toml");
        let file = PresetFile {
            name: "bright".into(),
            bands: c.presets["mellow"].bands.clone(),
            preamp_db: c.presets["mellow"].preamp_db,
        };
        std::fs::write(&duplicate, toml::to_string_pretty(&file).unwrap()).unwrap();

        let invalid_file = PresetFile {
            name: "too-loud".into(),
            bands: c.presets["mellow"].bands.clone(),
            preamp_db: 99.0,
        };
        std::fs::write(&invalid, toml::to_string_pretty(&invalid_file).unwrap()).unwrap();

        assert!(import_preset(&mut c, &duplicate, None).is_err());
        assert!(import_preset(&mut c, &invalid, None).is_err());
        let _ = std::fs::remove_file(&duplicate);
        let _ = std::fs::remove_file(&invalid);
    }

    #[test]
    fn tuning_edits_are_unsaved_until_resolved() {
        let mut d = daemon_with(Config::default());
        let saved_bright = d.saved_config.presets["bright"].clone();

        d.apply(Request::SetPreamp(-3.0)).unwrap();
        assert_ne!(d.config, d.saved_config);

        d.apply(Request::SaveSessionAs {
            name: "daily".into(),
        })
        .unwrap();

        assert_eq!(d.config, d.saved_config);
        assert_eq!(d.config.active_preset, "daily");
        assert_eq!(d.config.presets["bright"], saved_bright);
        assert_eq!(d.config.presets["daily"].preamp_db, -3.0);
    }

    #[test]
    fn session_save_as_can_overwrite_shipped_preset_name() {
        let mut d = daemon_with(Config::default());
        let original_bright = d.saved_config.presets["bright"].clone();

        d.apply(Request::SetPreset("mellow".into())).unwrap();
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        d.apply(Request::SaveSessionAs {
            name: "bright".into(),
        })
        .unwrap();

        assert_eq!(d.config, d.saved_config);
        assert_eq!(d.config.active_preset, "bright");
        assert_ne!(d.saved_config.presets["bright"], original_bright);
        assert_eq!(d.saved_config.presets["bright"].preamp_db, -3.0);
    }

    #[test]
    fn session_save_as_rejects_another_custom_preset_name() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SavePreset {
            name: "daily".into(),
        })
        .unwrap();
        d.apply(Request::SavePreset {
            name: "desk".into(),
        })
        .unwrap(); // active is now "desk"
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        // "daily" is someone else's preset — overwriting it by save-as would be the
        // accidental loss the check exists to prevent.
        let err = d
            .apply(Request::SaveSessionAs {
                name: "daily".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("preset already exists"));
    }

    #[test]
    fn session_save_as_own_name_overwrites_the_active_custom_preset() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SavePreset {
            name: "daily".into(),
        })
        .unwrap();
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        // Saving the session under the preset being edited is the overwrite action, not
        // a name collision — it must not dead-end with "preset already exists".
        d.apply(Request::SaveSessionAs {
            name: "daily".into(),
        })
        .unwrap();
        assert_eq!(d.config, d.saved_config);
        assert_eq!(d.config.active_preset, "daily");
        assert_eq!(d.saved_config.presets["daily"].preamp_db, -3.0);
    }

    #[test]
    fn session_overwrite_commits_active_preset_name() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        d.apply(Request::SaveSessionOverwrite).unwrap();

        assert_eq!(d.config, d.saved_config);
        assert_eq!(d.config.active_preset, "bright");
        assert_eq!(d.saved_config.presets["bright"].preamp_db, -3.0);
    }

    #[test]
    fn session_discard_reverts_to_saved_config() {
        let mut d = daemon_with(Config::default());
        let saved = d.saved_config.clone();
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        d.apply(Request::DiscardSession).unwrap();

        assert_eq!(d.config, saved);
        assert_eq!(d.saved_config, saved);
    }

    #[test]
    fn off_returns_unsaved_session_when_tuning_is_dirty() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        let resp = d.apply(Request::Disable).unwrap();
        assert!(matches!(resp, Response::UnsavedSession { .. }));
    }

    #[test]
    fn off_reports_the_presets_that_actually_carry_unsaved_edits() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap(); // edit bright (active)
        d.apply(Request::SetPreset("mellow".into())).unwrap(); // switch commits

        // The unsaved diff lives on bright, not on the now-active mellow: the prompt
        // data must name what would actually be overwritten or discarded.
        match d.apply(Request::Disable).unwrap() {
            Response::UnsavedSession {
                tuning,
                dirty_presets,
            } => {
                assert_eq!(tuning.preset, "mellow");
                assert_eq!(dirty_presets, vec!["bright".to_string()]);
            }
            other => panic!("expected UnsavedSession, got {other:?}"),
        }
    }

    #[test]
    fn save_session_as_keeps_unsaved_edits_to_other_presets_open() {
        let mut d = daemon_with(Config::default());
        // Edit bright (active), then switch away: the switch commits immediately, the
        // edit stays attached to bright as the open session.
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        d.apply(Request::SetPreset("mellow".into())).unwrap();
        assert!(d.has_unsaved_session());

        // Saving the (pristine) active tuning under a new name consumes only the active
        // preset's edits — bright's unsaved edit must not be silently reverted.
        d.apply(Request::SaveSessionAs {
            name: "party".into(),
        })
        .unwrap();

        assert_eq!(d.saved_config.active_preset, "party");
        assert_eq!(
            d.saved_config.presets["party"],
            d.saved_config.presets["mellow"]
        );
        assert_eq!(
            d.config.presets["bright"].preamp_db, -3.0,
            "bright's unsaved edit must survive the save-as"
        );
        assert_eq!(d.saved_config.presets["bright"].preamp_db, -8.0);
        assert!(
            d.has_unsaved_session(),
            "the remaining edit stays an open session"
        );
        assert!(
            d.session_path.exists(),
            "…and stays mirrored for a daemon restart"
        );
    }

    #[test]
    fn preset_clone_is_blocked_by_an_unsaved_session() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();

        // Cloning rebuilds the working config from the saved one; with a session open
        // that would silently drop the working edit.
        let err = d
            .apply(Request::ClonePreset {
                source: "mellow".into(),
                dest: "night".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("unsaved tuning changes"));
        assert!(!d.config.presets.contains_key("night"));
        assert_eq!(
            d.config.presets["bright"].preamp_db, -3.0,
            "the working edit must not be dropped"
        );
    }

    #[test]
    fn reset_preset_restores_shipped_preset() {
        let mut c = Config::default();
        let defaults = Config::default();
        for name in ["bright", "mellow", "pro"] {
            c.presets.get_mut(name).unwrap().preamp_db = -3.0;
            reset_preset(&mut c, name).unwrap();
            assert_eq!(c.presets[name], defaults.presets[name]);

            c.presets.remove(name);
            reset_preset(&mut c, name).unwrap();
            assert_eq!(c.presets[name], defaults.presets[name]);
        }
    }

    #[test]
    fn reset_all_restores_shipped_presets_and_preserves_custom_presets() {
        let mut c = Config::default();
        let defaults = Config::default();
        c.presets.insert(
            "daily".into(),
            Preset {
                bands: vec![],
                preamp_db: -4.0,
            },
        );
        for name in ["bright", "mellow", "pro"] {
            c.presets.get_mut(name).unwrap().preamp_db = -3.0;
        }

        reset_shipped_presets(&mut c);

        for name in ["bright", "mellow", "pro"] {
            assert_eq!(c.presets[name], defaults.presets[name]);
        }
        assert!(c.presets.contains_key("daily"));
        assert_eq!(c.active_preset, "bright");
    }

    #[test]
    fn reset_requests_restore_shipped_presets_from_saved_config() {
        let mut config = Config::default();
        config.presets.insert(
            "daily".into(),
            Preset {
                bands: vec![],
                preamp_db: -4.0,
            },
        );
        for name in ["bright", "mellow", "pro"] {
            config.presets.get_mut(name).unwrap().preamp_db = -3.0;
        }
        let mut d = daemon_with(config);
        let defaults = Config::default();

        d.apply(Request::SetPreamp(1.0)).unwrap(); // unsaved draft must block reset.
        let blocked = d
            .apply(Request::ResetPreset {
                name: "bright".into(),
            })
            .unwrap_err();
        assert!(blocked.to_string().contains("unsaved tuning changes"));

        d.apply(Request::DiscardSession).unwrap();
        let resp = d
            .apply(Request::ResetPreset {
                name: "bright".into(),
            })
            .unwrap();
        assert!(matches!(resp, Response::ResetWouldOverwrite { .. }));
        d.apply(Request::ConfirmResetPreset {
            name: "bright".into(),
            backups: vec![],
        })
        .unwrap();
        assert_eq!(d.saved_config.presets["bright"], defaults.presets["bright"]);
        assert_eq!(d.saved_config.presets["mellow"].preamp_db, -3.0);
        assert!(d.saved_config.presets.contains_key("daily"));

        let resp = d.apply(Request::Reset).unwrap();
        assert!(matches!(resp, Response::ResetWouldOverwrite { .. }));
        d.apply(Request::ConfirmReset { backups: vec![] }).unwrap();
        for name in ["bright", "mellow", "pro"] {
            assert_eq!(d.saved_config.presets[name], defaults.presets[name]);
        }
        assert!(d.saved_config.presets.contains_key("daily"));
        assert_eq!(d.saved_config.active_preset, "bright");
    }

    #[test]
    fn confirmed_reset_can_save_modified_builtin_copy_first() {
        let mut config = Config::default();
        config.presets.get_mut("bright").unwrap().preamp_db = -3.0;
        let modified_bright = config.presets["bright"].clone();
        let mut d = daemon_with(config);

        let resp = d
            .apply(Request::ResetPreset {
                name: "bright".into(),
            })
            .unwrap();
        assert!(matches!(resp, Response::ResetWouldOverwrite { .. }));
        d.apply(Request::ConfirmResetPreset {
            name: "bright".into(),
            backups: vec![PresetBackup {
                source: "bright".into(),
                dest: "my-bright".into(),
            }],
        })
        .unwrap();

        assert_eq!(
            d.saved_config.presets["bright"],
            Config::default().presets["bright"]
        );
        assert_eq!(d.saved_config.presets["my-bright"], modified_bright);
    }

    #[test]
    fn reset_recreates_deleted_shipped_preset_without_overwrite_warning() {
        let mut config = Config::default();
        config.presets.remove("bright");
        let mut d = daemon_with(config);

        let resp = d
            .apply(Request::ResetPreset {
                name: "bright".into(),
            })
            .unwrap();

        assert!(matches!(resp, Response::Tuning(_)));
        assert_eq!(
            d.saved_config.presets["bright"],
            Config::default().presets["bright"]
        );
    }

    #[test]
    fn set_band_rejects_exceeding_max_bands() {
        let mut d = daemon_with(Config::default());
        // Start from an empty preset so the count is controlled exactly.
        d.config.presets.insert(
            "empty".into(),
            Preset {
                bands: vec![],
                preamp_db: 0.0,
            },
        );
        d.config.active_preset = "empty".into();
        // Fill to MAX_BANDS with distinct frequencies (spaced > BAND_MATCH_HZ apart).
        for i in 0..MAX_BANDS {
            let freq = 20.0 + i as f32;
            d.apply(Request::SetBand {
                freq,
                gain_db: 3.0,
                q: 1.0,
            })
            .unwrap();
        }
        assert_eq!(d.config.presets["empty"].bands.len(), MAX_BANDS);
        // One more *new* band must be rejected without mutating the preset.
        let resp = d
            .apply(Request::SetBand {
                freq: 19_000.0,
                gain_db: 3.0,
                q: 1.0,
            })
            .unwrap();
        assert!(matches!(resp, Response::Error(_)));
        assert_eq!(d.config.presets["empty"].bands.len(), MAX_BANDS);
        // Editing an existing band is still allowed at the cap.
        d.apply(Request::SetBand {
            freq: 20.0,
            gain_db: -3.0,
            q: 2.0,
        })
        .unwrap();
        assert_eq!(d.config.presets["empty"].bands.len(), MAX_BANDS);
    }

    #[test]
    fn remove_band_removes_exactly_one_matching_band() {
        let mut config = Config::default();
        let original = config.presets["bright"].bands.clone();
        let expected = original
            .iter()
            .copied()
            .find(|band| band.freq == 2_000.0)
            .unwrap();
        config.presets.get_mut("bright").unwrap().bands = original.clone();
        let mut d = daemon_with(config);

        let response = d
            .apply(Request::RemoveBand {
                freq: expected.freq + 0.25,
            })
            .unwrap();

        let Response::BandRemoved { tuning, removed } = response else {
            panic!("expected a band-removed response");
        };
        assert_eq!(removed, expected);
        assert_eq!(tuning.bands.len(), original.len() - 1);
        assert!(!tuning.bands.contains(&expected));
        assert!(d.has_unsaved_session());
    }

    #[test]
    fn remove_band_rejects_a_distant_frequency_without_mutating() {
        let mut d = daemon_with(Config::default());
        let before = d.config.clone();

        let response = d.dispatch(Request::RemoveBand { freq: 2_900.0 });

        assert_eq!(
            response,
            Response::Error("no band matches 2900 Hz; nearest configured band is 2000 Hz".into())
        );
        assert_eq!(d.config, before);
        assert!(!d.has_unsaved_session());
    }

    #[test]
    fn remove_band_rejects_an_empty_preset() {
        let mut config = Config::default();
        config.presets.get_mut("bright").unwrap().bands.clear();
        let mut d = daemon_with(config.clone());

        let response = d.dispatch(Request::RemoveBand { freq: 1_000.0 });

        assert_eq!(
            response,
            Response::Error("active preset has no bands to remove".into())
        );
        assert_eq!(d.config, config);
    }

    #[test]
    fn import_rejects_preset_exceeding_max_bands() {
        let mut c = Config::default();
        let bands: Vec<Band> = (0..=MAX_BANDS)
            .map(|i| Band {
                kind: BandKind::Peaking,
                freq: 20.0 + i as f32,
                gain_db: 1.0,
                q: 1.0,
            })
            .collect();
        assert_eq!(bands.len(), MAX_BANDS + 1);
        let path = tmp_path("import-toomany.toml");
        let file = PresetFile {
            name: "big".into(),
            bands,
            preamp_db: 0.0,
        };
        std::fs::write(&path, toml::to_string_pretty(&file).unwrap()).unwrap();
        let res = import_preset(&mut c, &path, None);
        let _ = std::fs::remove_file(&path);
        assert!(res.is_err());
        assert!(!c.presets.contains_key("big"));
    }

    #[test]
    fn disable_persists_enabled_off() {
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });

        let resp = d.apply(Request::Disable).unwrap();
        assert!(matches!(resp, Response::Ok));

        assert!(!d.config.enabled);
        assert!(!d.saved_config.enabled);
        let on_disk = Config::load_from(&d.config_path).unwrap();
        assert!(
            !on_disk.enabled,
            "off must be persisted for the next startup"
        );
    }

    #[test]
    fn disabling_lowpower_auto_off_does_not_lift_an_idle_suspension() {
        // EQ enabled but idle-suspended (no media): turning the LPM policy off must not
        // restart the engine — only new device activity (or an explicit `on`) lifts an
        // idle suspension.
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });
        d.idle_suspended = true;
        d.engine_target_on = false;

        d.apply(Request::SetAutoOffLowPower(false)).unwrap();

        assert!(
            !d.engine_target_on,
            "lifting LPM suppression must not override an idle suspension"
        );
        assert!(d.idle_suspended);
    }

    #[test]
    fn disable_with_a_failing_save_still_stops_the_engine_and_a_retry_persists() {
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });
        d.engine_target_on = true;
        let blocker = tmp_path("off-not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::Disable).is_err());
        // The engine must be stopped regardless of the persist failure…
        assert!(
            !d.engine_target_on,
            "off must stop the engine even if the config write fails"
        );
        // …while the recorded intent stays truthful to disk (still on), so a retry
        // re-attempts the write instead of hitting the no-op skip.
        assert!(d.config.enabled);
        assert!(d.saved_config.enabled);

        d.config_path = good_path;
        d.apply(Request::Disable).unwrap();
        assert!(!d.config.enabled);
        assert!(!Config::load_from(&d.config_path).unwrap().enabled);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn a_failed_off_persist_does_not_let_a_later_reconcile_restart_the_engine() {
        // `eqtune off` stops the engine, but the persist fails, so `config.enabled` stays
        // stale-on on disk (retryable). The live `user_intent` is off, and the automatic
        // -suspend logic gates on that intent — so a later trigger (here, lifting the LPM
        // policy) must not resurrect the EQ the user just turned off.
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });
        d.engine_target_on = true;
        let blocker = tmp_path("off-lpm-not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::Disable).is_err());
        assert!(
            !d.user_intent,
            "off clears the live intent even when the persist fails"
        );
        assert!(
            d.config.enabled,
            "the stale on-disk value is left untouched for a retry"
        );

        // Give the next command a writable path so the toggle itself can persist.
        d.config_path = tmp_path("off-lpm-good.toml");
        d.apply(Request::SetAutoOffLowPower(false)).unwrap();
        assert!(
            !d.engine_target_on,
            "lifting the LPM policy must read the live off-intent, not the stale enabled=true"
        );
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn redundant_disable_skips_the_config_write() {
        // Already off: `eqtune off` must not churn the disk (and on a first run must not
        // manufacture a config file).
        let mut d = daemon_with(Config::default());
        d.apply(Request::Disable).unwrap();
        assert!(!d.config_path.exists());
    }

    #[test]
    fn preset_switch_save_failure_leaves_state_untouched_and_a_retry_persists() {
        let mut d = daemon_with(Config::default());
        // An unwritable config path: its parent is a regular file, so save_to must fail.
        let blocker = tmp_path("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::SetPreset("mellow".into())).is_err());
        // On a failed save nothing in memory may change, or `status` would report a
        // preset the disk (and after the skipped apply, the engine) does not have.
        assert_eq!(d.config.active_preset, "bright");
        assert_eq!(d.saved_config.active_preset, "bright");

        // A retry must actually retry the write, not hit the no-op skip.
        d.config_path = good_path;
        d.apply(Request::SetPreset("mellow".into())).unwrap();
        assert_eq!(d.config.active_preset, "mellow");
        assert_eq!(
            Config::load_from(&d.config_path).unwrap().active_preset,
            "mellow"
        );
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn preset_delete_save_failure_leaves_state_untouched_and_a_retry_persists() {
        let mut d = daemon_with(Config::default());
        let blocker = tmp_path("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        let names = vec!["mellow".to_string()];
        assert!(
            d.apply(Request::DeletePresets {
                names: names.clone()
            })
            .is_err()
        );
        // The delete must not survive in memory: the disk still has the preset, and a
        // half-applied working config would read as a phantom unsaved session whose
        // retry ("no such preset") could never persist the deletion.
        assert!(d.config.presets.contains_key("mellow"));
        assert!(d.saved_config.presets.contains_key("mellow"));
        assert!(!d.has_unsaved_session());

        d.config_path = good_path;
        d.apply(Request::DeletePresets { names }).unwrap();
        assert!(!d.config.presets.contains_key("mellow"));
        assert!(
            !Config::load_from(&d.config_path)
                .unwrap()
                .presets
                .contains_key("mellow")
        );
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn session_overwrite_save_failure_keeps_the_session_open() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        assert!(d.has_unsaved_session());
        let blocker = tmp_path("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::SaveSessionOverwrite).is_err());
        // The edits were not committed, so they must stay an open session (draft mirror
        // included) rather than being adopted as saved while the disk disagrees.
        assert!(d.has_unsaved_session());
        assert!(d.session_path.exists());

        d.config_path = good_path;
        d.apply(Request::SaveSessionOverwrite).unwrap();
        assert!(!d.has_unsaved_session());
        assert!(!d.session_path.exists());
        let on_disk = Config::load_from(&d.config_path).unwrap();
        assert_eq!(on_disk.active().unwrap().preamp_db, -3.0);
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn reset_all_leaves_an_idle_suspension_intact() {
        // Resetting presets only rewrites saved tuning; it must not touch the engine
        // lifecycle. An idle-suspended engine stays suspended on both the failed and the
        // successful save — the resume probe brings it back (with the reset tuning) when
        // playback returns, rather than the reset stranding it off or restarting it just
        // to process silence.
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });
        d.idle_suspended = true;
        let blocker = tmp_path("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::ConfirmReset { backups: vec![] }).is_err());
        assert!(
            d.idle_suspended,
            "a failed reset must not disturb the suspension"
        );

        d.config_path = good_path;
        d.apply(Request::ConfirmReset { backups: vec![] }).unwrap();
        assert!(
            d.idle_suspended,
            "a successful reset leaves the suspension for the resume probe to lift"
        );
        assert!(
            !d.engine_target_on,
            "reset must not turn the engine target back on"
        );
        let _ = std::fs::remove_file(&blocker);
    }

    #[test]
    fn preset_switch_commits_immediately_and_is_not_an_unsaved_session() {
        let mut d = daemon_with(Config::default());

        // Re-selecting the already-active preset must not churn the disk.
        d.apply(Request::SetPreset("bright".into())).unwrap();
        assert!(!d.config_path.exists());

        // A real switch commits: no unsaved session, no draft mirror, persisted on disk
        // — `eqtune off` right after `eqtune p mellow` must not raise the save prompt,
        // and a daemon restart must come back on mellow.
        d.apply(Request::SetPreset("mellow".into())).unwrap();
        assert_eq!(d.saved_config.active_preset, "mellow");
        assert!(!d.has_unsaved_session());
        assert!(!d.session_path.exists());
        let on_disk = Config::load_from(&d.config_path).unwrap();
        assert_eq!(on_disk.active_preset, "mellow");
    }

    #[test]
    fn live_edits_mirror_a_session_draft_and_committing_removes_it() {
        let mut d = daemon_with(Config::default());

        d.apply(Request::SetPreamp(-3.0)).unwrap();
        let draft = Config::load_from(&d.session_path).unwrap();
        assert_eq!(
            draft.presets[&draft.active_preset].preamp_db, -3.0,
            "the unsaved edit must be mirrored to the session file"
        );

        d.apply(Request::SaveSessionOverwrite).unwrap();
        assert!(
            !d.session_path.exists(),
            "a committed session must remove the draft mirror"
        );
    }

    #[test]
    fn a_burst_of_edits_coalesces_into_one_deferred_mirror_write() {
        let mut d = daemon_with(Config::default());
        let t0 = Instant::now();

        // The first edit after a quiet period mirrors immediately, so an isolated edit is
        // never at risk of being lost across a restart.
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        assert!(
            d.session_path.exists(),
            "the first edit mirrors immediately"
        );
        assert!(!d.draft_dirty);

        // Further edits within the min interval defer rather than rewrite the whole config
        // on every step — they only mark the mirror dirty, leaving the earlier write on disk.
        d.apply(Request::SetPreamp(-4.0)).unwrap();
        d.apply(Request::SetPreamp(-5.0)).unwrap();
        assert!(
            d.draft_dirty,
            "a burst within the interval defers the write"
        );
        assert_eq!(
            Config::load_from(&d.session_path)
                .unwrap()
                .active()
                .unwrap()
                .preamp_db,
            -3.0,
            "the deferred edits are not flushed yet"
        );

        // A poll before the interval elapses is a no-op; once it elapses the latest state
        // is flushed in a single coalesced write.
        d.maybe_flush_draft(t0 + SESSION_MIRROR_MIN_INTERVAL / 2);
        assert!(d.draft_dirty, "still within the interval — nothing flushed");
        d.maybe_flush_draft(t0 + SESSION_MIRROR_MIN_INTERVAL * 2);
        assert!(
            !d.draft_dirty,
            "the interval elapsed — the coalesced write lands"
        );
        assert_eq!(
            Config::load_from(&d.session_path)
                .unwrap()
                .active()
                .unwrap()
                .preamp_db,
            -5.0,
            "the mirror now holds the latest edit"
        );
    }

    #[test]
    fn discarding_a_session_removes_the_draft_mirror() {
        let mut d = daemon_with(Config::default());

        d.apply(Request::SetPreamp(-3.0)).unwrap();
        assert!(d.session_path.exists());

        d.apply(Request::DiscardSession).unwrap();
        assert!(
            !d.session_path.exists(),
            "a discarded session must remove the draft mirror"
        );
    }

    #[test]
    fn discard_surfaces_a_draft_that_could_not_be_removed() {
        let mut d = daemon_with(Config::default());
        d.apply(Request::SetPreamp(-3.0)).unwrap();
        assert!(d.session_path.exists());

        // Make the draft unremovable: remove_file on a directory fails. A leftover
        // draft would restore the discarded tuning at the next startup, so the discard
        // must not report success over it.
        std::fs::remove_file(&d.session_path).unwrap();
        std::fs::create_dir(&d.session_path).unwrap();

        let err = d.apply(Request::DiscardSession).unwrap_err();
        assert!(err.to_string().contains("session draft"));
        // The in-memory discard itself still applied (a retry is idempotent).
        assert_eq!(d.config, d.saved_config);
    }

    #[test]
    fn handle_does_not_block_on_a_silent_client() {
        // A client that connects but never sends a full request line must not wedge the
        // single-threaded daemon: the read timeout `run` sets turns it into an error.
        let (client, server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut d = daemon_with(Config::default());

        let start = std::time::Instant::now();
        let res = d.handle(server);
        assert!(
            res.is_err(),
            "a silent client must surface as an error, not a hang"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "handle must return promptly once the read times out"
        );
        drop(client);
    }

    #[test]
    fn read_request_line_caps_a_newlineless_flood() {
        // A client that sends bytes without a newline must hit the size cap, not grow the
        // read buffer without bound.
        let (client, server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // Write from another thread: once the daemon stops reading, the socket buffer fills
        // and the write blocks, so it can't run inline without deadlocking the test.
        let writer = std::thread::spawn(move || {
            let mut w = client;
            let _ = w.write_all(&vec![b'x'; MAX_REQUEST_BYTES + 16]);
        });
        let err = read_request_line(&server).unwrap_err();
        assert!(
            err.to_string().contains("exceeds"),
            "a newline-less flood must surface as the size-cap error, got: {err}"
        );
        drop(server);
        let _ = writer.join();
    }

    #[test]
    fn check_request_bounds_rejects_over_the_size_cap() {
        // With plenty of time left, only the size cap decides — a line whose terminating
        // newline pushes it one byte over must be rejected, not accepted at the break.
        let far = Instant::now() + Duration::from_secs(3600);
        assert!(check_request_bounds(MAX_REQUEST_BYTES, far).is_ok());
        assert!(
            check_request_bounds(MAX_REQUEST_BYTES + 1, far)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn check_request_bounds_rejects_a_reached_deadline() {
        // A deadline of "now" is already reached when the check reads the clock, so even a
        // within-cap line that completes at/after the deadline is rejected.
        assert!(
            check_request_bounds(0, Instant::now())
                .unwrap_err()
                .to_string()
                .contains("in time")
        );
    }

    #[test]
    fn initial_engine_state_starts_suspended_when_idle_auto_off_is_on() {
        // On + idle auto-off on: restore suspended (engine off, idle_suspended set) so the
        // resume probe starts the engine only when playback actually begins — no tap run
        // through startup silence.
        assert_eq!(initial_engine_state(true, true, false), (false, true));
        // On + idle auto-off off: no resume probe to lean on, so restore eagerly.
        assert_eq!(initial_engine_state(true, false, false), (true, false));
        // Off: nothing to restore, either way.
        assert_eq!(initial_engine_state(false, true, false), (false, false));
        assert_eq!(initial_engine_state(false, false, false), (false, false));
        // Low Power Mode suppresses the eager start; the suspended path is already off but
        // keeps idle_suspended so the idle probe (not an LPM-restore) owns the first start
        // once LPM clears.
        assert_eq!(initial_engine_state(true, false, true), (false, false));
        assert_eq!(initial_engine_state(true, true, true), (false, true));
    }

    #[test]
    fn recovery_uses_the_bounded_backoff_schedule() {
        let start = Instant::now();
        let mut recovery = Recovery::default();

        recovery.schedule_initial(start);
        assert_eq!(recovery.next_retry, Some(start + RETRY_DELAYS[0]));
        assert_eq!(recovery.retries_attempted, 0);

        for (attempt, delay) in RETRY_DELAYS.iter().enumerate().skip(1) {
            let failure = start + Duration::from_secs(attempt as u64 * 100);
            recovery.retry_failed(failure);
            assert_eq!(recovery.retries_attempted, attempt);
            assert_eq!(recovery.next_retry, Some(failure + *delay));
            assert!(!recovery.exhausted);
        }

        recovery.retry_failed(start + Duration::from_secs(1_000));
        assert_eq!(recovery.retries_attempted, RETRY_DELAYS.len());
        assert!(recovery.next_retry.is_none());
        assert!(recovery.exhausted);
    }

    #[test]
    fn recovery_reset_opens_a_fresh_incident_budget() {
        let start = Instant::now();
        let mut recovery = Recovery::default();
        recovery.schedule_initial(start);
        for attempt in 0..RETRY_DELAYS.len() {
            recovery.retry_failed(start + Duration::from_secs(attempt as u64));
        }
        assert!(recovery.exhausted);

        recovery.reset();
        assert_eq!(recovery.retries_attempted, 0);
        assert!(recovery.next_retry.is_none());
        assert!(!recovery.exhausted);

        recovery.schedule_initial(start);
        assert_eq!(recovery.next_retry, Some(start + Duration::from_secs(1)));
    }

    #[test]
    fn exhausted_recovery_cannot_reconcile_an_extra_attempt() {
        let mut d = daemon_with(Config::default());
        d.user_intent = true;
        d.engine_target_on = true;
        d.recovery.exhausted = true;
        d.recovery.retries_attempted = RETRY_DELAYS.len();

        d.reconcile().unwrap();

        assert!(d.engine.is_none());
        assert!(d.last_engine_error.is_none());
        assert!(d.recovery.exhausted);
    }

    #[test]
    fn status_explains_recovery_and_unsaved_presets() {
        let mut d = daemon_with(Config::default());
        d.user_intent = true;
        d.engine_target_on = true;
        d.last_engine_error = Some("runtime input stream layout changed".into());
        d.recovery.schedule_initial(Instant::now());
        d.config.presets.get_mut("bright").unwrap().preamp_db = -3.0;

        let status = d.status();

        assert!(status.user_intent);
        assert!(!status.engine_running);
        assert_eq!(status.suspension_reason.as_deref(), Some("recovering"));
        assert_eq!(status.retry_attempts, 0);
        assert_eq!(status.retry_limit, 6);
        assert_eq!(status.retry_in_seconds, Some(1));
        assert!(!status.retry_exhausted);
        assert_eq!(
            status.last_engine_error.as_deref(),
            Some("runtime input stream layout changed")
        );
        assert_eq!(status.dirty_presets, ["bright"]);
        assert!(!status.bypassed);
        assert!(status.output_uid.is_none());
    }

    #[test]
    fn status_distinguishes_policy_and_exhaustion_suspensions() {
        let mut d = daemon_with(Config {
            enabled: true,
            ..Config::default()
        });
        d.user_intent = true;
        d.engine_target_on = false;
        d.low_power = true;
        assert_eq!(d.suspension_reason(), Some("low-power"));

        d.low_power = false;
        d.idle_suspended = true;
        assert_eq!(d.suspension_reason(), Some("idle"));

        d.idle_suspended = false;
        d.engine_target_on = true;
        d.recovery.exhausted = true;
        assert_eq!(d.suspension_reason(), Some("recovery-exhausted"));
    }

    #[test]
    fn daemon_lock_allows_only_one_holder() -> anyhow::Result<()> {
        let path = tmp_path("daemon.lock");
        let first = DaemonLock::acquire(&path)?;

        let error = DaemonLock::acquire(&path).unwrap_err();
        assert!(error.to_string().contains("already running"));

        drop(first);
        let next = DaemonLock::acquire(&path)?;
        drop(next);
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn control_socket_replaces_stale_socket_but_not_live_listener() -> anyhow::Result<()> {
        let stale_path = tmp_path("stale.sock");
        let stale = UnixListener::bind(&stale_path)?;
        drop(stale);

        let replacement = bind_control_listener(&stale_path)?;
        drop(replacement);
        std::fs::remove_file(&stale_path)?;

        let live_path = tmp_path("live.sock");
        let live = UnixListener::bind(&live_path)?;
        let error = bind_control_listener(&live_path).unwrap_err();
        assert!(error.to_string().contains("already listening"));
        drop(live);
        std::fs::remove_file(live_path)?;
        Ok(())
    }

    #[test]
    fn control_socket_does_not_delete_an_unrelated_file() -> anyhow::Result<()> {
        let path = tmp_path("not-a-socket");
        std::fs::write(&path, b"keep me")?;

        let error = bind_control_listener(&path).unwrap_err();

        assert!(error.to_string().contains("not a Unix socket"));
        assert_eq!(std::fs::read(&path)?, b"keep me");
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// A `Daemon` plus RAII cleanup of its on-disk footprint (the config and
    /// session-draft files), so no test can leak temp files by asserting early or by
    /// exercising a path that leaves the draft behind on purpose.
    struct TestDaemon(Daemon);

    impl Drop for TestDaemon {
        fn drop(&mut self) {
            for path in [&self.0.config_path, &self.0.session_path] {
                if std::fs::remove_file(path).is_err() {
                    // Some tests plant a directory there to force removal failures.
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }
    }

    impl std::ops::Deref for TestDaemon {
        type Target = Daemon;
        fn deref(&self) -> &Daemon {
            &self.0
        }
    }

    impl std::ops::DerefMut for TestDaemon {
        fn deref_mut(&mut self) -> &mut Daemon {
            &mut self.0
        }
    }

    fn daemon_with(config: Config) -> TestDaemon {
        TestDaemon(Daemon {
            user_intent: config.enabled,
            saved_config: config.clone(),
            config,
            config_path: tmp_path("daemon-config.toml"),
            session_path: tmp_path("daemon-session.toml"),
            engine: None,
            engine_target: None,
            engine_target_on: false,
            low_power: false,
            observed_output_id: None,
            recovery: Recovery::default(),
            last_engine_error: None,
            idle_suspended: false,
            draft_dirty: false,
            draft_last_write: None,
        })
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A process-wide counter guarantees uniqueness across parallel test threads even
        // when the clock is too coarse to distinguish two calls (otherwise colliding paths
        // race each other's file ops and flake). The timestamp is kept only to keep any
        // leftover file greppable/ordered if a test ever fails to clean up.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "eqtune-test-{}-{nanos}-{seq}-{name}",
            std::process::id()
        ))
    }
}
