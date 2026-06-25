//! The long-running daemon: owns the config and the audio engine, and serves the
//! control socket. `on`/`off` start/stop the Core Audio tap; live edits push fresh
//! settings to the running engine lock-free; and a lightweight poll makes the engine
//! follow the system default output device (so plugging in EarPods/Bluetooth "just
//! works" without manually re-selecting output).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use crate::config::{Config, Preset};
use crate::dsp::{Band, BandKind, EqSettings};
use crate::ipc::{self, Request, Response, Status, Tuning};
use crate::sys::{self, EqHandle, TapSession};

/// Two bands count as "the same band" if their frequencies are this close (Hz).
const BAND_MATCH_HZ: f32 = 0.5;
/// Channel count for the processor (stereo).
const CHANNELS: usize = 2;
/// How often the idle loop accepts connections and checks the default device.
const POLL: Duration = Duration::from_millis(100);
/// How long captured audio must remain silent before the engine is suspended.
const IDLE_SUSPEND_AFTER: Duration = Duration::from_secs(10);
const MIN_BAND_FREQ_HZ: f32 = 20.0;
const MAX_BAND_FREQ_HZ: f32 = 20_000.0;
const MIN_BAND_GAIN_DB: f32 = -24.0;
const MAX_BAND_GAIN_DB: f32 = 24.0;
const MIN_Q: f32 = 0.1;
const MAX_Q: f32 = 10.0;
const MIN_PREAMP_DB: f32 = -60.0;
const MAX_PREAMP_DB: f32 = 12.0;
const MAX_PRESET_NAME_LEN: usize = 64;

pub struct Daemon {
    config: Config,
    engine: Option<(TapSession, EqHandle)>,
    /// (output device id, sample rate Hz) the running engine was built for.
    engine_target: Option<(u32, u32)>,
    /// The effective target: the audio engine should be running iff this is true.
    /// `reconcile` starts/stops the engine to match it.
    engine_target_on: bool,
    /// The user's last explicit on/off, remembered across a Low-Power-Mode auto-off so it
    /// can be restored when Low Power Mode clears.
    user_intent: bool,
    /// Last-seen macOS Low Power Mode state (edge-detected in `follow_low_power`).
    low_power: bool,
    /// Whether the engine is currently off because captured audio was silent long enough
    /// to count as no active media.
    idle_suspended: bool,
}

impl Daemon {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            config: Config::load()?,
            engine: None,
            engine_target: None,
            engine_target_on: false,
            user_intent: false,
            // Seed from the real state so the first poll doesn't fire a spurious edge.
            low_power: sys::low_power_enabled(),
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

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false); // blocking for the short req/resp
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
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
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
                self.reconcile()?; // override: starts even while Low Power Mode is active
                Ok(Response::Tuning(self.tuning()))
            }
            Request::Disable => {
                self.user_intent = false;
                self.idle_suspended = false;
                self.engine_target_on = false;
                self.reconcile()?; // drops the TapSession -> large energy drop
                Ok(Response::Ok)
            }
            Request::ListPresets => Ok(Response::Presets {
                active: self.config.active_preset.clone(),
                names: self.config.presets.keys().cloned().collect(),
            }),
            Request::SetPreset(name) => {
                if !self.config.presets.contains_key(&name) {
                    return Ok(Response::Error(format!("no such preset: {name}")));
                }
                self.config.active_preset = name;
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SavePreset { name } => {
                save_active_preset(&mut self.config, &name)?;
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::ClonePreset { source, dest } => {
                clone_preset(&mut self.config, &source, &dest)?;
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::DeletePreset { name } => {
                delete_preset(&mut self.config, &name)?;
                self.persist_and_apply()?;
                Ok(Response::Presets {
                    active: self.config.active_preset.clone(),
                    names: self.config.presets.keys().cloned().collect(),
                })
            }
            Request::RenamePreset { from, to } => {
                rename_preset(&mut self.config, &from, &to)?;
                self.persist_and_apply()?;
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
                    preset.bands.push(Band {
                        kind: BandKind::Peaking,
                        freq,
                        gain_db,
                        q,
                    });
                    preset.bands.sort_by(|a, b| a.freq.total_cmp(&b.freq));
                }
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::RemoveBand { freq } => {
                validate_freq(freq)?;
                self.active_preset_mut()?
                    .bands
                    .retain(|b| (b.freq - freq).abs() >= BAND_MATCH_HZ);
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetPreamp(db) => {
                validate_preamp(db)?;
                self.active_preset_mut()?.preamp_db = db;
                self.persist_and_apply()?;
                Ok(Response::Tuning(self.tuning()))
            }
            Request::SetAutoOffLowPower(on) => {
                self.config.auto_off_low_power = on;
                self.config.save()?;
                if on && self.low_power {
                    self.engine_target_on = false; // apply the policy right now
                } else if !on {
                    self.engine_target_on = self.user_intent; // lift any LPM suppression
                }
                self.reconcile()?;
                Ok(Response::Ok)
            }
            Request::SetAutoOffIdle(on) => {
                self.config.auto_off_idle = on;
                self.config.save()?;
                if !on && self.idle_suspended {
                    self.idle_suspended = false;
                    if self.user_intent && !(self.config.auto_off_low_power && self.low_power) {
                        self.engine_target_on = true;
                    }
                }
                self.reconcile()?;
                Ok(Response::Ok)
            }
            Request::Reset => {
                self.config = Config::default();
                self.idle_suspended = false;
                self.persist_and_apply()?;
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
            let rate = self.engine_target.map(|(_, r)| r).unwrap_or(48_000);
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

    fn persist_and_apply(&mut self) -> anyhow::Result<()> {
        self.config.save()?;
        if self.engine.is_some() {
            let fs = self
                .engine_target
                .map(|(_, r)| r as f32)
                .unwrap_or(48_000.0);
            let settings = self.settings_for(fs);
            if let Some((_, handle)) = &self.engine {
                handle.store(settings); // lock-free live update
            }
        }
        Ok(())
    }

    fn status(&self) -> Status {
        let active = self.config.active();
        let output_device = self
            .engine_target
            .filter(|_| self.engine.is_some())
            .map(|(dev, _)| format!("#{dev}"));
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

/// The current default output device and its (rounded) sample rate.
fn current_target() -> (u32, u32) {
    let dev = sys::default_output_device().unwrap_or(0);
    let rate = sys::default_output_sample_rate()
        .unwrap_or(48_000.0)
        .round() as u32;
    (dev, rate)
}

fn validate_band(freq: f32, gain_db: f32, q: f32) -> anyhow::Result<()> {
    validate_freq(freq)?;
    validate_range("gain", gain_db, MIN_BAND_GAIN_DB, MAX_BAND_GAIN_DB, "dB")?;
    validate_range("Q", q, MIN_Q, MAX_Q, "")?;
    Ok(())
}

fn validate_freq(freq: f32) -> anyhow::Result<()> {
    validate_range("frequency", freq, MIN_BAND_FREQ_HZ, MAX_BAND_FREQ_HZ, "Hz")
}

fn validate_preamp(db: f32) -> anyhow::Result<()> {
    validate_range("preamp", db, MIN_PREAMP_DB, MAX_PREAMP_DB, "dB")
}

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

fn delete_preset(config: &mut Config, name: &str) -> anyhow::Result<()> {
    if !config.presets.contains_key(name) {
        return Err(anyhow::anyhow!("no such preset: {name}"));
    }
    if config.presets.len() == 1 {
        return Err(anyhow::anyhow!("cannot delete the last preset"));
    }
    config.presets.remove(name);
    if config.active_preset == name {
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

fn validate_new_preset_name(config: &Config, name: &str) -> anyhow::Result<()> {
    validate_preset_name(name)?;
    if config.presets.contains_key(name) {
        return Err(anyhow::anyhow!("preset already exists: {name}"));
    }
    Ok(())
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

fn validate_range(name: &str, value: f32, min: f32, max: f32, unit: &str) -> anyhow::Result<()> {
    if !value.is_finite() {
        return Err(anyhow::anyhow!("{name} must be a finite number"));
    }
    if !(min..=max).contains(&value) {
        return Err(anyhow::anyhow!(
            "{name} must be between {} and {}",
            format_bound(min, unit),
            format_bound(max, unit)
        ));
    }
    Ok(())
}

fn format_bound(value: f32, unit: &str) -> String {
    if unit.is_empty() {
        trim_number(value)
    } else {
        format!("{} {unit}", trim_number(value))
    }
}

fn trim_number(n: f32) -> String {
    let s = format!("{n:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_validation_accepts_practical_values() {
        validate_band(20.0, -24.0, 0.1).unwrap();
        validate_band(20_000.0, 24.0, 10.0).unwrap();
        validate_band(1000.0, 0.0, 1.41).unwrap();
    }

    #[test]
    fn band_validation_rejects_invalid_values() {
        for (freq, gain, q) in [
            (0.0, 0.0, 1.0),
            (20_001.0, 0.0, 1.0),
            (1000.0, -24.1, 1.0),
            (1000.0, 24.1, 1.0),
            (1000.0, 0.0, 0.0),
            (1000.0, 0.0, 10.1),
            (f32::NAN, 0.0, 1.0),
            (1000.0, f32::INFINITY, 1.0),
            (1000.0, 0.0, f32::NEG_INFINITY),
        ] {
            assert!(validate_band(freq, gain, q).is_err());
        }
    }

    #[test]
    fn preamp_validation_accepts_safe_range() {
        validate_preamp(-60.0).unwrap();
        validate_preamp(0.0).unwrap();
        validate_preamp(12.0).unwrap();
    }

    #[test]
    fn preamp_validation_rejects_invalid_values() {
        for db in [-60.1, 12.1, f32::NAN, f32::INFINITY] {
            assert!(validate_preamp(db).is_err());
        }
    }

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
        let mut c = Config::default();
        c.active_preset = "mellow".into();
        delete_preset(&mut c, "mellow").unwrap();
        assert!(!c.presets.contains_key("mellow"));
        assert!(c.presets.contains_key(&c.active_preset));
    }

    #[test]
    fn preset_delete_rejects_last_preset() {
        let mut c = Config::default();
        c.presets.retain(|name, _| name == "bright");
        c.active_preset = "bright".into();
        assert!(delete_preset(&mut c, "bright").is_err());
        assert!(c.presets.contains_key("bright"));
    }

    #[test]
    fn preset_rename_moves_preset_and_updates_active_name() {
        let mut c = Config::default();
        c.active_preset = "bright".into();
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
}
