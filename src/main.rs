//! eqtune CLI entry point. Either runs the long-lived daemon or acts as a thin client
//! that sends a single control request to it over the Unix socket.

use clap::{Parser, Subcommand};

use eqtune::daemon::Daemon;
use eqtune::ipc::{self, Request, Response};
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
    Presets,
    /// Switch the active preset.
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
            let req = to_request(client_cmd);
            match ipc::send(&req) {
                Ok(resp) => {
                    print_response(&resp);
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

fn to_request(cmd: Command) -> Request {
    match cmd {
        Command::On => Request::Enable,
        Command::Off => Request::Disable,
        Command::Status => Request::Status,
        Command::Presets => Request::ListPresets,
        Command::Preset { name } => Request::SetPreset(name),
        Command::Band { freq, gain_db, q } => Request::SetBand { freq, gain_db, q },
        Command::BandRm { freq } => Request::RemoveBand { freq },
        Command::Preamp { db } => Request::SetPreamp(db),
        Command::Reset => Request::Reset,
        Command::Daemon | Command::Install | Command::Uninstall | Command::Probe | Command::Spike => {
            unreachable!("handled above")
        }
    }
}

fn print_response(resp: &Response) {
    match resp {
        Response::Ok => println!("ok"),
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
