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
use crate::ipc::{self, Request, Response, Status};
use crate::sys::{self, EqHandle, TapSession};

/// Two bands count as "the same band" if their frequencies are this close (Hz).
const BAND_MATCH_HZ: f32 = 0.5;
/// Channel count for the processor (stereo).
const CHANNELS: usize = 2;
/// How often the idle loop accepts connections and checks the default device.
const POLL: Duration = Duration::from_millis(100);

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
                self.engine_target_on = true;
                self.reconcile()?; // override: starts even while Low Power Mode is active
                Ok(Response::Ok)
            }
            Request::Disable => {
                self.user_intent = false;
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
                Ok(Response::Ok)
            }
            Request::SetBand { freq, gain_db, q } => {
                let preset = self.active_preset_mut()?;
                if let Some(b) = preset.bands.iter_mut().find(|b| (b.freq - freq).abs() < BAND_MATCH_HZ) {
                    b.gain_db = gain_db;
                    b.q = q;
                } else {
                    preset.bands.push(Band { kind: BandKind::Peaking, freq, gain_db, q });
                    preset.bands.sort_by(|a, b| a.freq.total_cmp(&b.freq));
                }
                self.persist_and_apply()?;
                Ok(Response::Ok)
            }
            Request::RemoveBand { freq } => {
                self.active_preset_mut()?
                    .bands
                    .retain(|b| (b.freq - freq).abs() >= BAND_MATCH_HZ);
                self.persist_and_apply()?;
                Ok(Response::Ok)
            }
            Request::SetPreamp(db) => {
                self.active_preset_mut()?.preamp_db = db;
                self.persist_and_apply()?;
                Ok(Response::Ok)
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
            Request::Reset => {
                self.config = Config::default();
                self.persist_and_apply()?;
                Ok(Response::Ok)
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
        self.engine_target_on = if now { false } else { self.user_intent };
        eprintln!(
            "low power mode {} — eqtune {}",
            if now { "on" } else { "off" },
            if self.engine_target_on { "resuming" } else { "suspended" }
        );
        if let Err(e) = self.reconcile() {
            eprintln!("engine reconcile failed: {e}");
        }
    }

    fn persist_and_apply(&mut self) -> anyhow::Result<()> {
        self.config.save()?;
        if self.engine.is_some() {
            let fs = self.engine_target.map(|(_, r)| r as f32).unwrap_or(48_000.0);
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
        }
    }
}

/// The current default output device and its (rounded) sample rate.
fn current_target() -> (u32, u32) {
    let dev = sys::default_output_device().unwrap_or(0);
    let rate = sys::default_output_sample_rate().unwrap_or(48_000.0).round() as u32;
    (dev, rate)
}
