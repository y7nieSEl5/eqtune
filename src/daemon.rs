//! The long-running daemon: owns the config and the audio engine, and serves the
//! control socket. `on`/`off` start/stop the Core Audio tap; live edits push fresh
//! settings to the running engine lock-free (no audio restart).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use crate::config::{Config, Preset};
use crate::dsp::{Band, BandKind, EqSettings};
use crate::ipc::{self, Request, Response, Status};
use crate::sys::{self, EqHandle, TapSession};

/// Two bands count as "the same band" if their frequencies are this close (Hz).
const BAND_MATCH_HZ: f32 = 0.5;
/// Channel count for the processor (stereo).
const CHANNELS: usize = 2;

pub struct Daemon {
    config: Config,
    engine: Option<(TapSession, EqHandle)>,
}

impl Daemon {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { config: Config::load()?, engine: None })
    }

    /// Bind the control socket and serve requests until the process is terminated.
    pub fn run(mut self) -> anyhow::Result<()> {
        let path = ipc::socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&path); // clear any stale socket
        let listener = UnixListener::bind(&path)?;
        eprintln!("eqtune daemon listening on {}", path.display());

        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    if let Err(e) = self.handle(stream) {
                        eprintln!("connection error: {e}");
                    }
                }
                Err(e) => eprintln!("accept error: {e}"),
            }
        }
        Ok(())
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
                self.start_engine()?;
                Ok(Response::Ok)
            }
            Request::Disable => {
                self.engine = None; // drops TapSession -> stops the audio thread
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

    /// Build engine settings from the active preset and the output device's rate.
    fn current_settings(&self) -> EqSettings {
        let fs = sys::default_output_sample_rate().unwrap_or(48_000.0) as f32;
        let active = self.config.active();
        let bands: &[Band] = active.map(|p| p.bands.as_slice()).unwrap_or(&[]);
        let preamp = active.map(|p| p.preamp_db).unwrap_or(0.0);
        EqSettings::new(bands, fs, preamp, self.config.limiter)
    }

    fn start_engine(&mut self) -> anyhow::Result<()> {
        if self.engine.is_some() {
            return Ok(());
        }
        let settings = self.current_settings();
        match TapSession::start(CHANNELS, settings) {
            Some(pair) => {
                self.engine = Some(pair);
                Ok(())
            }
            None => Err(anyhow::anyhow!(
                "could not start the audio tap — needs macOS 14.2+ and audio-capture permission"
            )),
        }
    }

    fn persist_and_apply(&mut self) -> anyhow::Result<()> {
        self.config.save()?;
        if self.engine.is_some() {
            let settings = self.current_settings();
            if let Some((_, handle)) = &self.engine {
                handle.store(settings); // lock-free live update
            }
        }
        Ok(())
    }

    fn status(&self) -> Status {
        let active = self.config.active();
        let output_device = self.engine.as_ref().map(|_| match sys::default_output_device() {
            Some(id) => format!("#{id}"),
            None => "unknown".to_string(),
        });
        Status {
            enabled: self.engine.is_some(),
            active_preset: self.config.active_preset.clone(),
            preamp_db: active.map(|p| p.preamp_db).unwrap_or(0.0),
            band_count: active.map(|p| p.bands.len()).unwrap_or(0),
            limiter: self.config.limiter,
            output_device,
        }
    }
}
