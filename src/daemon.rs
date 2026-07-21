//! The long-running daemon: owns the config and the audio engine, and serves the
//! control socket. `on`/`off` start/stop the Core Audio tap; live edits push fresh
//! settings to the running engine lock-free; and a lightweight poll makes the engine
//! follow the system default output device (so plugging in EarPods/Bluetooth "just
//! works" without manually re-selecting output).

use std::io::{BufRead, BufReader, ErrorKind, Write};
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
use crate::sys::{self, EqHandle, TapSession};

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
const MAX_PRESET_NAME_LEN: usize = 64;

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
    /// (output device id, sample rate Hz) the running engine was built for.
    engine_target: Option<(u32, u32)>,
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
    /// Whether the engine is currently off because captured audio was silent long enough
    /// to count as no active media.
    idle_suspended: bool,
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
        // Runtime on/off intent, seeded from the persisted `enabled` and then tracked in
        // memory so it stays correct even if a later persist fails (see `set_enabled`).
        let user_intent = config.enabled;
        Ok(Self {
            // Restore the persisted on/off, respecting the Low-Power-Mode policy the same
            // way a live LPM edge would. `run` reconciles once before serving requests.
            engine_target_on: user_intent && !(config.auto_off_low_power && low_power),
            user_intent,
            saved_config,
            config,
            config_path: Config::path(),
            session_path,
            engine: None,
            engine_target: None,
            low_power,
            idle_suspended: false,
        })
    }

    /// Bind the control socket and serve requests; also follow default-device changes.
    pub fn run(mut self) -> anyhow::Result<()> {
        let path = ipc::socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&path); // clear any stale socket
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        eprintln!("eqtune daemon listening on {}", path.display());

        // Restore the last run's on state. A start failure (capture permission not yet
        // granted, unsupported macOS) must not kill the daemon — under launchd KeepAlive
        // that would crash-loop — so log it; `eqtune on` retries on demand.
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
            self.follow_low_power();
            self.follow_idle_activity();
            self.follow_default_device();
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
            Request::Status => Ok(Response::Status(self.status())),
            Request::Enable => {
                self.user_intent = true;
                self.idle_suspended = false;
                self.engine_target_on = true;
                // Override: starts even while Low Power Mode is active.
                if let Err(e) = self.reconcile() {
                    // A failed start is a failed enable: leave no half-on target or intent
                    // behind, or a later unrelated reconcile (an LPM edge) would start the
                    // EQ as a side effect of a command that never succeeded.
                    self.user_intent = false;
                    self.engine_target_on = false;
                    return Err(e);
                }
                // Persist only after the engine actually started: recording enabled=true
                // for a start that failed (capture permission not granted yet) would make
                // a later daemon restart silently start the EQ although no `eqtune on`
                // ever succeeded. The live `user_intent` above already reflects the on
                // state, so a persist failure here costs only durability, not correct
                // runtime idle/LPM behavior.
                self.set_enabled(true).context(
                    "the EQ is running, but the on state could not be saved — it would \
                     not survive a daemon restart; retry `eqtune on`",
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
                self.active_preset_mut()?
                    .bands
                    .retain(|b| (b.freq - freq).abs() >= BAND_MATCH_HZ);
                self.apply_current_settings()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetPreamp(db) => {
                validate_preamp(db)?;
                self.active_preset_mut()?.preamp_db = db;
                self.apply_current_settings()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetAutoOffLowPower(on) => {
                self.commit_setting(|c| c.auto_off_low_power = on)?;
                if on && self.low_power {
                    self.engine_target_on = false; // apply the policy right now
                } else if !on {
                    // Lift any LPM suppression — but an idle suspension is not this
                    // toggle's to lift: restarting the engine here with no media playing
                    // would contradict the idle policy (follow_low_power and
                    // SetAutoOffIdle apply the same guard).
                    self.engine_target_on = self.user_intent && !self.idle_suspended;
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
            self.start_engine()?;
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
        let (dev, rate) = current_target();
        let settings = self.settings_for(rate as f32);
        match TapSession::start(CHANNELS, settings) {
            Some(pair) => {
                self.engine = Some(pair);
                self.engine_target = Some((dev, rate));
                Ok(())
            }
            None => Err(anyhow::anyhow!(
                "could not start the audio tap — needs macOS 14.2+ and audio-capture permission"
            )),
        }
    }

    /// Rebuild the engine if the system default output device (or its sample rate)
    /// changed, so replay follows wherever audio is now meant to go.
    fn follow_default_device(&mut self) {
        if self.engine.is_none() {
            return;
        }
        let current = current_target();
        if self.engine_target != Some(current) {
            eprintln!("default output changed to {current:?} — rebuilding engine");
            self.engine = None;
            self.engine_target = None;
            if let Err(e) = self.start_engine() {
                eprintln!("engine rebuild failed: {e}");
            }
        }
    }

    /// Follow macOS Low Power Mode: on entering LPM, auto-off the engine (a large energy
    /// drop) while remembering the user's intent; on leaving LPM, restore that intent.
    /// Edge-triggered, so a persistent start failure isn't retried every poll.
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
                .map(|(_, r)| r)
                .unwrap_or(DEFAULT_SAMPLE_RATE_HZ);
            let idle_frames = IDLE_SUSPEND_AFTER.as_secs().saturating_mul(rate as u64);
            if handle.silent_frames() >= idle_frames {
                self.idle_suspended = true;
                self.engine_target_on = false;
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
    /// draft to disk. Every mutation of the working config funnels through here, so the
    /// on-disk draft always matches `has_unsaved_session`: written while a draft exists,
    /// removed once the session resolves (save, overwrite, discard, or any persist).
    /// The engine push cannot fail; the returned error is `sync_session_file`'s.
    fn apply_current_settings(&mut self) -> anyhow::Result<()> {
        let synced = self.sync_session_file();
        if self.engine.is_some() {
            let fs = self
                .engine_target
                .map(|(_, r)| r as f32)
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
    /// A *write* failure is logged, not propagated: the edit already applied live, and
    /// failing the command over a degraded draft mirror would be worse than a draft
    /// that only lives in memory (the pre-mirror behavior). A *removal* failure is an
    /// error: the leftover draft is authoritative restore state, so the next daemon
    /// startup would resurrect the very session the command just resolved (a discarded
    /// tuning coming back, or a stale draft shadowing a fresh save) — the resolving
    /// command must not report success over that.
    fn sync_session_file(&self) -> anyhow::Result<()> {
        if self.has_unsaved_session() {
            if let Err(e) = self.config.write_draft_to(&self.session_path) {
                eprintln!(
                    "could not mirror the session draft to {}: {e}",
                    self.session_path.display()
                );
            }
        } else if let Err(e) = std::fs::remove_file(&self.session_path) {
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
    // these two mutate equals the saved one.
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
        })?;
        // Only after the reset actually landed: a failed save must not lift the idle
        // suspension and let the next reconcile restart the engine with nothing playing.
        self.idle_suspended = false;
        Ok(())
    }

    fn status(&self) -> Status {
        let active = self.config.active();
        // Resolve the name of the device the engine is actually attached to. Between a
        // default-device change and the next `follow_default_device` tick, `engine_target`
        // still points at the old device, so naming the *current default* here would label
        // the running engine with a device it is not on.
        let output_device = self
            .engine_target
            .filter(|_| self.engine.is_some())
            .map(|(dev, _)| sys::output_device_name(dev).unwrap_or_else(|| format!("#{dev}")));
        Status {
            enabled: self.engine.is_some(),
            active_preset: self.config.active_preset.clone(),
            preamp_db: active.map(|p| p.preamp_db).unwrap_or(0.0),
            band_count: active.map(|p| p.bands.len()).unwrap_or(0),
            limiter: self.config.limiter,
            output_device,
            low_power: self.low_power,
            auto_off_low_power: self.config.auto_off_low_power,
            auto_off_idle: self.config.auto_off_idle,
            idle_suspended: self.idle_suspended,
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

/// The current default output device and its (rounded) sample rate.
fn current_target() -> (u32, u32) {
    let dev = sys::default_output_device().unwrap_or(0);
    let rate = sys::default_output_sample_rate()
        .unwrap_or(DEFAULT_SAMPLE_RATE_HZ as f64)
        .round() as u32;
    (dev, rate)
}

#[cfg(test)]
fn save_active_preset(config: &mut Config, name: &str) -> anyhow::Result<()> {
    validate_new_preset_name(config, name)?;
    let preset = config
        .active()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no active preset to save"))?;
    config.presets.insert(name.to_string(), preset);
    config.active_preset = name.to_string();
    Ok(())
}

#[cfg(test)]
fn clone_preset(config: &mut Config, source: &str, dest: &str) -> anyhow::Result<()> {
    validate_new_preset_name(config, dest)?;
    let preset = config
        .presets
        .get(source)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no such preset: {source}"))?;
    config.presets.insert(dest.to_string(), preset);
    config.active_preset = dest.to_string();
    Ok(())
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
    fn preset_save_clones_active_and_selects_new_preset() {
        let mut c = Config::default();
        let active = c.active().cloned().unwrap();
        save_active_preset(&mut c, "car").unwrap();
        assert_eq!(c.active_preset, "car");
        assert_eq!(c.presets["car"], active);
    }

    #[test]
    fn preset_clone_copies_source_and_selects_dest() {
        let mut c = Config::default();
        let source = c.presets["mellow"].clone();
        clone_preset(&mut c, "mellow", "night").unwrap();
        assert_eq!(c.active_preset, "night");
        assert_eq!(c.presets["night"], source);
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
    fn reset_all_save_failure_keeps_the_idle_suspension() {
        let mut d = daemon_with(Config::default());
        d.idle_suspended = true;
        let blocker = tmp_path("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let good_path = d.config_path.clone();
        d.config_path = blocker.join("config.toml");

        assert!(d.apply(Request::ConfirmReset { backups: vec![] }).is_err());
        // A reset that did not land must not lift the idle suspension: the next
        // reconcile would restart the engine with nothing playing.
        assert!(d.idle_suspended);

        d.config_path = good_path;
        d.apply(Request::ConfirmReset { backups: vec![] }).unwrap();
        assert!(!d.idle_suspended);
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
            idle_suspended: false,
        })
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "eqtune-test-{}-{unique}-{name}",
            std::process::id()
        ))
    }
}
