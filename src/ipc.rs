//! Client↔daemon control protocol over a Unix domain socket (newline-delimited JSON).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A command sent from the CLI client to the running daemon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Request {
    Status,
    Enable,
    Disable,
    ListPresets,
    SetPreset(String),
    SetBand { freq: f32, gain_db: f32, q: f32 },
    RemoveBand { freq: f32 },
    SetPreamp(f32),
    Reset,
}

/// The daemon's reply to a [`Request`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Response {
    Ok,
    Status(Status),
    Presets { active: String, names: Vec<String> },
    Error(String),
}

/// A snapshot of daemon state, returned for `eqtune status`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Status {
    pub enabled: bool,
    pub active_preset: String,
    pub preamp_db: f32,
    pub band_count: usize,
    pub limiter: bool,
    /// The real output device audio is being sent to (None until the engine runs).
    pub output_device: Option<String>,
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

    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(serde_json::from_str(resp.trim_end())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let reqs = [
            Request::Status,
            Request::Enable,
            Request::Disable,
            Request::Reset,
            Request::ListPresets,
            Request::SetPreset("flat".into()),
            Request::SetBand { freq: 1000.0, gain_db: -10.0, q: 1.0 },
            Request::RemoveBand { freq: 2000.0 },
            Request::SetPreamp(7.0),
        ];
        for r in reqs {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        }
    }

    #[test]
    fn response_round_trips() {
        let st = Status {
            enabled: true,
            active_preset: "default".into(),
            preamp_db: 7.0,
            band_count: 3,
            limiter: true,
            output_device: Some("MacBook Pro Speakers".into()),
        };
        let resps = [
            Response::Ok,
            Response::Status(st),
            Response::Presets { active: "default".into(), names: vec!["default".into(), "flat".into()] },
            Response::Error("nope".into()),
        ];
        for r in resps {
            let s = serde_json::to_string(&r).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), r);
        }
    }
}
