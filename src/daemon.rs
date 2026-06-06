//! The long-running daemon.
//!
//! For now it owns the [`Config`] plus an enabled flag and serves the control socket.
//! The Core Audio capture/replay engine (Spike 0) plugs into the marked TODO points:
//! `enable`/`disable` will start/stop it, and `persist_and_apply` will push fresh
//! filter coefficients to it live.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use crate::config::{Config, Preset};
use crate::dsp::{Band, BandKind};
use crate::ipc::{self, Request, Response, Status};

/// Two bands count as "the same band" if their frequencies are this close (Hz).
const BAND_MATCH_HZ: f32 = 0.5;

pub struct Daemon {
    config: Config,
    enabled: bool,
}

impl Daemon {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { config: Config::load()?, enabled: false })
    }

    /// Bind the control socket and serve requests until the process is terminated.
    pub fn run(mut self) -> anyhow::Result<()> {
        let path = ipc::socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Clear any stale socket left by a previous run before binding.
        let _ = std::fs::remove_file(&path);
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
                self.enabled = true;
                // TODO(Spike 0): start the Core Audio tap + replay engine.
                Ok(Response::Ok)
            }
            Request::Disable => {
                self.enabled = false;
                // TODO(Spike 0): stop the engine; CATapMutedWhenTapped restores audio.
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

    fn persist_and_apply(&mut self) -> anyhow::Result<()> {
        self.config.save()?;
        // TODO(Spike 0): if the engine is running, re-design coefficients live.
        Ok(())
    }

    fn status(&self) -> Status {
        let active = self.config.active();
        Status {
            enabled: self.enabled,
            active_preset: self.config.active_preset.clone(),
            preamp_db: active.map(|p| p.preamp_db).unwrap_or(0.0),
            band_count: active.map(|p| p.bands.len()).unwrap_or(0),
            limiter: self.config.limiter,
            output_device: None,
        }
    }
}
