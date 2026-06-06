//! eqtune CLI entry point. Either runs the long-lived daemon or acts as a thin client
//! that sends a single control request to it over the Unix socket.

use clap::{Parser, Subcommand};

use eqtune::daemon::Daemon;
use eqtune::ipc::{self, Request, Response};

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
            println!("install: not implemented yet (LaunchAgent setup) — task #8");
            Ok(())
        }
        Command::Uninstall => {
            println!("uninstall: not implemented yet (LaunchAgent teardown) — task #8");
            Ok(())
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
        Command::Daemon | Command::Install | Command::Uninstall => unreachable!("handled above"),
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
