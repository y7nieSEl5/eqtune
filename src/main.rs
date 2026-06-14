//! eqtune CLI entry point. Either runs the long-lived daemon or acts as a thin client
//! that sends a single control request to it over the Unix socket.

use clap::{Parser, Subcommand, ValueEnum};

use eqtune::daemon::Daemon;
use eqtune::ipc::{self, Request, Response, Tuning};
use eqtune::{dsp, sys::TapSession};

#[derive(Parser)]
#[command(name = "eqtune", version, about = "System-wide audio EQ for macOS")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enable audio processing.
    On,
    /// Disable audio processing.
    Off,
    /// Show current status.
    Status,
    /// List available presets (active one marked with *).
    #[command(visible_alias = "ls")]
    Presets,
    /// Switch the active preset.
    #[command(visible_alias = "p")]
    Preset { name: String },
    /// Set or update a band: <freq_hz> <gain_db> [q].
    #[command(allow_negative_numbers = true)]
    Band {
        freq: f32,
        gain_db: f32,
        #[arg(default_value_t = 1.0)]
        q: f32,
    },
    /// Remove the band nearest <freq_hz>.
    #[command(name = "band-rm")]
    BandRm { freq: f32 },
    /// Set the preamp make-up gain, in dB.
    #[command(allow_negative_numbers = true)]
    Preamp { db: f32 },
    /// Toggle auto-off while macOS Low Power Mode is active (on/off).
    Lowpower { state: Toggle },
    /// Reset all settings to the built-in default curve.
    Reset,
    /// Run the audio daemon in the foreground (used by the LaunchAgent).
    #[command(hide = true)]
    Daemon,
    /// Print low-level audio probe info (debug).
    #[command(hide = true)]
    Probe,
    /// Run the capture→EQ→replay tap in the foreground (Spike 0 listen test).
    #[command(hide = true)]
    Spike,
    /// Install the LaunchAgent and start the daemon.
    Install,
    /// Stop and remove the LaunchAgent.
    Uninstall,
}

/// An on/off argument for toggle subcommands (parsed as `on` / `off`).
#[derive(Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => Daemon::new()?.run(),
        Command::Install => {
            eqtune::launchd::install()?;
            println!("eqtune installed; the daemon runs now and at login.");
            println!("Grant audio-capture permission when prompted (or in System Settings ›");
            println!("Privacy & Security), then run `eqtune on`.");
            Ok(())
        }
        Command::Uninstall => {
            eqtune::launchd::uninstall()?;
            println!("eqtune daemon removed. (Config kept; delete");
            println!("~/Library/Application Support/eqtune to remove everything.)");
            Ok(())
        }
        Command::Probe => {
            match eqtune::sys::default_output_device() {
                Some(id) => println!("default output device id: {id}"),
                None => println!("no default output device found"),
            }
            Ok(())
        }
        Command::Spike => {
            let fs = eqtune::sys::default_output_sample_rate().unwrap_or(48_000.0) as f32;
            let settings = dsp::EqSettings::new(&dsp::default_bands(), fs, dsp::DEFAULT_PREAMP_DB, true);
            match TapSession::start(2, settings) {
                Some((_session, _handle)) => {
                    println!("eqtune spike: system audio -> default-curve EQ -> output ({fs} Hz).");
                    println!("Play some audio. Press Ctrl-C to stop.");
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
                None => {
                    eprintln!(
                        "failed to start the audio tap — needs macOS 14.2+ and audio-capture permission."
                    );
                    std::process::exit(1);
                }
            }
        }
        client_cmd => {
            let req = to_request(&client_cmd);
            match ipc::send(&req) {
                Ok(resp) => {
                    print_response(&client_cmd, &resp);
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn to_request(cmd: &Command) -> Request {
    match cmd {
        Command::On => Request::Enable,
        Command::Off => Request::Disable,
        Command::Status => Request::Status,
        Command::Presets => Request::ListPresets,
        Command::Preset { name } => Request::SetPreset(name.clone()),
        Command::Band { freq, gain_db, q } => {
            Request::SetBand { freq: *freq, gain_db: *gain_db, q: *q }
        }
        Command::BandRm { freq } => Request::RemoveBand { freq: *freq },
        Command::Preamp { db } => Request::SetPreamp(*db),
        Command::Lowpower { state } => Request::SetAutoOffLowPower(matches!(state, Toggle::On)),
        Command::Reset => Request::Reset,
        Command::Daemon | Command::Install | Command::Uninstall | Command::Probe | Command::Spike => {
            unreachable!("handled above")
        }
    }
}

/// Render the daemon's reply, tailored to the command that produced it: `on` and edits
/// echo what changed and print the resulting curve; `off` and `lowpower` confirm the
/// action; `status`/`presets` print their own views.
fn print_response(cmd: &Command, resp: &Response) {
    match resp {
        Response::Tuning(t) => {
            // A one-line echo of what just changed, then the full resulting curve.
            let changed = match cmd {
                Command::On => {
                    println!("eqtune on");
                    None
                }
                Command::Preset { name } => {
                    println!("preset → {name}");
                    None
                }
                Command::Band { freq, gain_db, q } => {
                    println!("band {} → {} (Q{})", fmt_freq(*freq), fmt_gain(*gain_db), fmt_q(*q));
                    Some(*freq)
                }
                Command::BandRm { freq } => {
                    println!("removed band near {}", fmt_freq(*freq));
                    None
                }
                Command::Preamp { db } => {
                    println!("preamp → {}", fmt_gain(*db));
                    None
                }
                Command::Reset => {
                    println!("reset to shipped defaults");
                    None
                }
                _ => None,
            };
            print_curve(t, changed);
        }
        Response::Ok => match cmd {
            Command::Off => println!("eqtune off — native Apple audio restored"),
            Command::Lowpower { state } => {
                println!("auto-off in Low Power Mode: {}", if matches!(state, Toggle::On) { "on" } else { "off" });
            }
            _ => println!("ok"),
        },
        Response::Status(s) => {
            println!("enabled:       {}", s.enabled);
            println!("preset:        {}", s.active_preset);
            println!("preamp:        {:+} dB", s.preamp_db);
            println!("bands:         {}", s.band_count);
            println!("limiter:       {}", s.limiter);
            println!(
                "output device: {}",
                s.output_device.as_deref().unwrap_or("(engine not running)")
            );
            println!("low power:     {}", if s.low_power { "on" } else { "off" });
            println!("auto-off LPM:  {}", if s.auto_off_low_power { "on" } else { "off" });
        }
        Response::Presets { active, names } => {
            for n in names {
                let marker = if n == active { "*" } else { " " };
                println!("{marker} {n}");
            }
        }
        Response::Error(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// Print the active tuning: a `preset (state) · preamp` header, then one line per band.
/// The band nearest `changed` (if any) is flagged, so an edit's effect is easy to spot.
fn print_curve(t: &Tuning, changed: Option<f32>) {
    let state = if t.enabled { "enabled" } else { "disabled" };
    println!("{} ({state}) · preamp {}", t.preset, fmt_gain(t.preamp_db));
    if t.bands.is_empty() {
        println!("  (no bands — flat)");
        return;
    }
    // The single band closest to the edited frequency gets the "← changed" marker.
    let marked = changed.and_then(|f| {
        t.bands
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (a.freq - f).abs().total_cmp(&(b.freq - f).abs()))
            .map(|(i, _)| i)
    });
    for (i, b) in t.bands.iter().enumerate() {
        let mark = if Some(i) == marked { "   ← changed" } else { "" };
        println!("  {:>8}  {:>8}  Q{}{mark}", fmt_freq(b.freq), fmt_gain(b.gain_db), trim(b.q));
    }
}

/// Format a frequency for display: kHz at/above 1 kHz, otherwise Hz, trailing `.0`
/// trimmed (e.g. `2 kHz`, `1.25 kHz`, `125 Hz`, `31.5 Hz`).
fn fmt_freq(hz: f32) -> String {
    if hz >= 1000.0 {
        format!("{} kHz", trim(hz / 1000.0))
    } else {
        format!("{} Hz", trim(hz))
    }
}

/// Format a gain in dB with an explicit sign and one decimal (e.g. `+7.5 dB`, `-6.0 dB`).
fn fmt_gain(db: f32) -> String {
    format!("{db:+.1} dB")
}

/// Format a Q value with trailing `.0` trimmed (e.g. `1.41`, `2`).
fn fmt_q(q: f32) -> String {
    trim(q)
}

/// Render a float compactly: drop a trailing `.0` but keep real fractional digits
/// (`32.0 → "32"`, `1.25 → "1.25"`, `1.41 → "1.41"`).
fn trim(v: f32) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_freq_uses_hz_below_1k_and_khz_above() {
        assert_eq!(fmt_freq(32.0), "32 Hz");
        assert_eq!(fmt_freq(31.5), "31.5 Hz");
        assert_eq!(fmt_freq(125.0), "125 Hz");
        assert_eq!(fmt_freq(1000.0), "1 kHz");
        assert_eq!(fmt_freq(2000.0), "2 kHz");
        assert_eq!(fmt_freq(1250.0), "1.25 kHz");
        assert_eq!(fmt_freq(16000.0), "16 kHz");
    }

    #[test]
    fn fmt_gain_always_signed_one_decimal() {
        assert_eq!(fmt_gain(7.5), "+7.5 dB");
        assert_eq!(fmt_gain(-6.0), "-6.0 dB");
        assert_eq!(fmt_gain(0.0), "+0.0 dB");
    }

    #[test]
    fn fmt_q_trims_trailing_zeros() {
        assert_eq!(fmt_q(1.41), "1.41");
        assert_eq!(fmt_q(2.0), "2");
        assert_eq!(fmt_q(0.7), "0.7");
    }
}
