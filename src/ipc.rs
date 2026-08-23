//! Client↔daemon control protocol over a Unix domain socket (newline-delimited JSON).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::dsp::{Band, BandKind};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);
/// Tuning and preset-list responses can be larger than requests, but still have a firm
/// ceiling so a wedged or replaced daemon cannot grow the short-lived CLI without bound.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PresetBackup {
    pub source: String,
    pub dest: String,
}

/// A command sent from the CLI client to the running daemon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Request {
    Status,
    Enable,
    Disable,
    ListPresets,
    ShowPreset(Option<String>),
    SetPreset(String),
    SavePreset {
        name: String,
    },
    ClonePreset {
        source: String,
        dest: String,
    },
    DeletePresets {
        names: Vec<String>,
    },
    RenamePreset {
        from: String,
        to: String,
    },
    ExportPreset {
        name: String,
        path: PathBuf,
    },
    ImportPreset {
        path: PathBuf,
        name: Option<String>,
    },
    SetBand {
        kind: BandKind,
        freq: f32,
        gain_db: f32,
        q: f32,
    },
    RemoveBand {
        freq: f32,
    },
    SetPreamp(f32),
    SetPreampAuto,
    GetResponse,
    SetLimiter(bool),
    SetAutoOffLowPower(bool),
    SetAutoOffIdle(bool),
    SaveSessionAs {
        name: String,
    },
    SaveSessionOverwrite,
    DiscardSession,
    ResetPreset {
        name: String,
    },
    ConfirmResetPreset {
        name: String,
        backups: Vec<PresetBackup>,
    },
    Reset,
    ConfirmReset {
        backups: Vec<PresetBackup>,
    },
}

/// The daemon's reply to a [`Request`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Response {
    Ok,
    Status(Box<Status>),
    /// The active tuning after `on` or any EQ edit, so the client can show the resulting
    /// curve (preset, preamp, and bands).
    Tuning(Tuning),
    /// Result of removing the band matching the requested frequency. Returning the removed
    /// band lets the CLI report the action precisely rather than echoing only the request.
    BandRemoved {
        tuning: Tuning,
        removed: Band,
    },
    FrequencyResponse(FrequencyResponse),
    Presets {
        active: String,
        names: Vec<String>,
    },
    /// Returned by reset commands when modified shipped presets would be replaced.
    ResetWouldOverwrite {
        names: Vec<String>,
    },
    /// Returned by `off` when live tuning edits have not been persisted yet.
    UnsavedSession {
        /// The active tuning — what `[s]ave` acts on.
        tuning: Tuning,
        /// Names of every preset whose working contents differ from the saved config —
        /// the actual substance of the unsaved session. Edits stay attached to the
        /// preset they were made on across preset switches, so this can name presets
        /// other than the active one; the prompt must not imply the active curve is
        /// all there is to save or discard.
        dirty_presets: Vec<String>,
    },
    Error(String),
}

/// The active EQ tuning, returned so the CLI can print the current equalizer params
/// after `eqtune on` and after each edit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Tuning {
    /// Whether the audio engine is currently running.
    pub enabled: bool,
    /// Name of the active preset.
    pub preset: String,
    /// The preset's preamp make-up gain (dB).
    pub preamp_db: f32,
    /// The preset's EQ bands, in frequency order.
    pub bands: Vec<Band>,
}

/// The active tuning's linear response at the output device's sample rate.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FrequencyResponse {
    pub sample_rate_hz: f64,
    pub preamp_db: f32,
    pub points: Vec<ResponsePoint>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ResponsePoint {
    pub frequency_hz: f32,
    pub gain_db: f32,
}

/// A snapshot of daemon state, returned for `eqtune status`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Status {
    /// The user's durable desired on/off state, independent of temporary suspension or
    /// engine failure.
    pub user_intent: bool,
    /// Whether the Core Audio tap is actually running now.
    pub engine_running: bool,
    /// Why desired processing is not running, if applicable.
    pub suspension_reason: Option<String>,
    pub active_preset: String,
    pub preamp_db: f32,
    pub band_count: usize,
    pub limiter: bool,
    /// Validated output metadata. All fields remain `None` until startup succeeds.
    pub output_uid: Option<String>,
    pub output_name: Option<String>,
    pub output_rate_hz: Option<f64>,
    /// Compact sample-rate/channel/format/layout description.
    pub output_stream: Option<String>,
    pub last_engine_error: Option<String>,
    /// Number of retries already attempted in the current incident.
    pub retry_attempts: usize,
    pub retry_limit: usize,
    pub retry_in_seconds: Option<u64>,
    pub retry_exhausted: bool,
    /// Runtime dry-path bypass. It remains false until the 0.7.0 bypass command exists.
    pub bypassed: bool,
    /// Presets carrying session edits that have not been saved or discarded.
    pub dirty_presets: Vec<String>,
    /// Whether macOS Low Power Mode is currently active.
    pub low_power: bool,
    /// Whether the auto-off-on-Low-Power-Mode policy is enabled.
    pub auto_off_low_power: bool,
    /// Whether sustained no-media/no-signal idle suspension is enabled.
    pub auto_off_idle: bool,
}

/// Location of the control socket.
pub fn socket_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/eqtune/eqtune.sock")
}

/// Connect to the daemon, send one request, and read one response.
pub fn send(req: &Request) -> anyhow::Result<Response> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        anyhow::anyhow!(
            "could not reach the eqtune daemon ({e}). Is it running? Try `eqtune install` then `eqtune on`."
        )
    })?;
    stream.set_read_timeout(Some(CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_TIMEOUT))?;

    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    read_response(stream)
}

fn read_response(reader: impl Read) -> anyhow::Result<Response> {
    let mut reader = BufReader::new(reader).take((MAX_RESPONSE_BYTES + 1) as u64);
    let mut resp = Vec::new();
    reader.read_until(b'\n', &mut resp)?;
    if resp.len() > MAX_RESPONSE_BYTES {
        anyhow::bail!("daemon response exceeds {MAX_RESPONSE_BYTES} bytes");
    }
    if resp.is_empty() {
        anyhow::bail!("daemon closed the connection without a response");
    }
    Ok(serde_json::from_slice(&resp)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn request_round_trips() {
        let reqs = [
            Request::Status,
            Request::Enable,
            Request::Disable,
            Request::Reset,
            Request::ListPresets,
            Request::ShowPreset(None),
            Request::ShowPreset(Some("mellow".into())),
            Request::SetPreset("flat".into()),
            Request::SavePreset { name: "car".into() },
            Request::ClonePreset {
                source: "bright".into(),
                dest: "desk".into(),
            },
            Request::DeletePresets {
                names: vec!["desk".into(), "car".into()],
            },
            Request::RenamePreset {
                from: "car".into(),
                to: "car-v2".into(),
            },
            Request::ExportPreset {
                name: "car-v2".into(),
                path: PathBuf::from("/tmp/car-v2.toml"),
            },
            Request::ImportPreset {
                path: PathBuf::from("/tmp/car-v2.toml"),
                name: Some("shared-car".into()),
            },
            Request::SetBand {
                kind: BandKind::Peaking,
                freq: 1000.0,
                gain_db: -10.0,
                q: 1.0,
            },
            Request::RemoveBand { freq: 2000.0 },
            Request::SetPreamp(7.0),
            Request::SetPreampAuto,
            Request::GetResponse,
            Request::SetLimiter(false),
            Request::SetAutoOffLowPower(false),
            Request::SetAutoOffIdle(false),
            Request::SaveSessionAs {
                name: "daily".into(),
            },
            Request::SaveSessionOverwrite,
            Request::DiscardSession,
            Request::ResetPreset {
                name: "bright".into(),
            },
            Request::ConfirmResetPreset {
                name: "bright".into(),
                backups: vec![PresetBackup {
                    source: "bright".into(),
                    dest: "my-bright".into(),
                }],
            },
            Request::ConfirmReset {
                backups: vec![PresetBackup {
                    source: "mellow".into(),
                    dest: "my-mellow".into(),
                }],
            },
        ];
        for r in reqs {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn response_round_trips() {
        let st = Status {
            user_intent: true,
            engine_running: true,
            suspension_reason: None,
            active_preset: "default".into(),
            preamp_db: 7.0,
            band_count: 3,
            limiter: true,
            output_uid: Some("BuiltInSpeakerDevice".into()),
            output_name: Some("MacBook Pro Speakers".into()),
            output_rate_hz: Some(48_000.0),
            output_stream: Some("48000 Hz, 2 ch, Float32, interleaved".into()),
            last_engine_error: None,
            retry_attempts: 0,
            retry_limit: 6,
            retry_in_seconds: None,
            retry_exhausted: false,
            bypassed: false,
            dirty_presets: vec!["bright".into()],
            low_power: false,
            auto_off_low_power: true,
            auto_off_idle: true,
        };
        let resps = [
            Response::Ok,
            Response::Status(Box::new(st)),
            Response::Tuning(Tuning {
                enabled: true,
                preset: "bright".into(),
                preamp_db: -8.0,
                bands: vec![
                    crate::dsp::Band {
                        kind: crate::dsp::BandKind::Peaking,
                        freq: 1000.0,
                        gain_db: 4.5,
                        q: 1.41,
                    },
                    crate::dsp::Band {
                        kind: crate::dsp::BandKind::Peaking,
                        freq: 8000.0,
                        gain_db: 9.5,
                        q: 1.41,
                    },
                ],
            }),
            Response::BandRemoved {
                tuning: Tuning {
                    enabled: true,
                    preset: "bright".into(),
                    preamp_db: -8.0,
                    bands: vec![],
                },
                removed: crate::dsp::Band {
                    kind: crate::dsp::BandKind::Peaking,
                    freq: 1000.0,
                    gain_db: 4.5,
                    q: 1.41,
                },
            },
            Response::FrequencyResponse(FrequencyResponse {
                sample_rate_hz: 48_000.0,
                preamp_db: -3.0,
                points: vec![ResponsePoint {
                    frequency_hz: 1_000.0,
                    gain_db: 2.5,
                }],
            }),
            Response::Presets {
                active: "default".into(),
                names: vec!["default".into(), "flat".into()],
            },
            Response::ResetWouldOverwrite {
                names: vec!["bright".into()],
            },
            Response::UnsavedSession {
                tuning: Tuning {
                    enabled: false,
                    preset: "bright".into(),
                    preamp_db: -8.0,
                    bands: vec![],
                },
                dirty_presets: vec!["bright".into(), "mellow".into()],
            },
            Response::Error("nope".into()),
        ];
        for r in resps {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
        }
    }

    #[test]
    fn response_reader_rejects_an_oversized_line() {
        let oversized = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        let error = read_response(Cursor::new(oversized)).unwrap_err();
        assert!(error.to_string().contains("response exceeds"));
    }

    #[test]
    fn response_reader_rejects_an_empty_reply() {
        let error = read_response(Cursor::new(Vec::<u8>::new())).unwrap_err();
        assert!(error.to_string().contains("without a response"));
    }
}
