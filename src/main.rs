//! eqtune CLI entry point. Either runs the long-lived daemon or acts as a thin client
//! that sends a single control request to it over the Unix socket.

use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use eqtune::daemon::Daemon;
use eqtune::ipc::{self, PresetBackup, Request, Response, Tuning};

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
    /// Save the active tuning as a new preset and switch to it.
    #[command(name = "preset-save")]
    PresetSave { name: String },
    /// Clone an existing preset to a new name and switch to it.
    #[command(name = "preset-clone", visible_alias = "preset-copy")]
    PresetClone { source: String, dest: String },
    /// Delete one or more presets.
    #[command(name = "preset-rm", visible_alias = "preset-delete")]
    PresetRm {
        #[arg(required = true, num_args = 1..)]
        names: Vec<String>,
    },
    /// Rename a preset.
    #[command(name = "preset-rename")]
    PresetRename { from: String, to: String },
    /// Export a preset to a shareable TOML file.
    #[command(name = "preset-export")]
    PresetExport { name: String, path: Option<PathBuf> },
    /// Import a preset TOML file, optionally overriding its name.
    #[command(name = "preset-import")]
    PresetImport { path: PathBuf, name: Option<String> },
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
    /// Toggle auto-off while no media is active (on/off).
    Idle { state: Toggle },
    /// Reset all settings, or one shipped preset by name.
    Reset { name: Option<String> },
    /// Run the audio daemon in the foreground (used by the LaunchAgent).
    #[command(hide = true)]
    Daemon,
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
        client_cmd => {
            let req = match to_request(&client_cmd) {
                Ok(req) => req,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            };
            match ipc::send(&req) {
                Ok(resp) => {
                    if matches!(client_cmd, Command::Off) {
                        handle_off_response(&resp)?;
                    } else if matches!(client_cmd, Command::Reset { .. }) {
                        handle_reset_response(&client_cmd, &resp)?;
                    } else {
                        print_response(&client_cmd, &resp);
                    }
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

fn to_request(cmd: &Command) -> anyhow::Result<Request> {
    Ok(match cmd {
        Command::On => Request::Enable,
        Command::Off => Request::Disable,
        Command::Status => Request::Status,
        Command::Presets => Request::ListPresets,
        Command::Preset { name } => Request::SetPreset(name.clone()),
        Command::PresetSave { name } => Request::SavePreset { name: name.clone() },
        Command::PresetClone { source, dest } => Request::ClonePreset {
            source: source.clone(),
            dest: dest.clone(),
        },
        Command::PresetRm { names } => Request::DeletePresets {
            names: names.clone(),
        },
        Command::PresetRename { from, to } => Request::RenamePreset {
            from: from.clone(),
            to: to.clone(),
        },
        Command::PresetExport { name, path } => Request::ExportPreset {
            name: name.clone(),
            path: export_path(name, path.as_ref())?,
        },
        Command::PresetImport { path, name } => Request::ImportPreset {
            path: absolute_path(path)?,
            name: name.clone(),
        },
        Command::Band { freq, gain_db, q } => Request::SetBand {
            freq: *freq,
            gain_db: *gain_db,
            q: *q,
        },
        Command::BandRm { freq } => Request::RemoveBand { freq: *freq },
        Command::Preamp { db } => Request::SetPreamp(*db),
        Command::Lowpower { state } => Request::SetAutoOffLowPower(matches!(state, Toggle::On)),
        Command::Idle { state } => Request::SetAutoOffIdle(matches!(state, Toggle::On)),
        Command::Reset { name: Some(name) } => Request::ResetPreset { name: name.clone() },
        Command::Reset { name: None } => Request::Reset,
        Command::Daemon | Command::Install | Command::Uninstall => {
            unreachable!("handled above")
        }
    })
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
                Command::PresetSave { name } => {
                    println!("saved preset → {name}");
                    None
                }
                Command::PresetClone { source, dest } => {
                    println!("cloned preset {source} → {dest}");
                    None
                }
                Command::PresetRename { from, to } => {
                    println!("renamed preset {from} → {to}");
                    None
                }
                Command::PresetImport { path, name } => {
                    let path = absolute_path(path).unwrap_or_else(|_| path.clone());
                    if let Some(name) = name {
                        println!("imported preset {name} ← {}", path.display());
                    } else {
                        println!("imported preset ← {}", path.display());
                    }
                    None
                }
                Command::Band { freq, gain_db, q } => {
                    println!(
                        "band {} → {} (Q{})",
                        fmt_freq(*freq),
                        fmt_gain(*gain_db),
                        fmt_q(*q)
                    );
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
                Command::Reset { name: None } => {
                    println!("reset to shipped defaults");
                    None
                }
                Command::Reset { name: Some(name) } => {
                    println!("reset preset → {name}");
                    None
                }
                _ => None,
            };
            print_curve(t, changed);
        }
        Response::Ok => match cmd {
            Command::Off => println!("eqtune off — native Apple audio restored"),
            Command::Lowpower { state } => {
                println!(
                    "auto-off in Low Power Mode: {}",
                    if matches!(state, Toggle::On) {
                        "on"
                    } else {
                        "off"
                    }
                );
            }
            Command::Idle { state } => {
                println!(
                    "auto-off when idle: {}",
                    if matches!(state, Toggle::On) {
                        "on"
                    } else {
                        "off"
                    }
                );
            }
            Command::PresetExport { name, path } => {
                let path = export_path(name, path.as_ref()).unwrap_or_else(|_| {
                    path.clone()
                        .unwrap_or_else(|| PathBuf::from(format!("{name}.toml")))
                });
                println!("exported preset {name} → {}", path.display());
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
            println!(
                "auto-off LPM:  {}",
                if s.auto_off_low_power { "on" } else { "off" }
            );
            println!(
                "auto-off idle: {}",
                if s.auto_off_idle { "on" } else { "off" }
            );
            println!(
                "idle suspend:  {}",
                if s.idle_suspended { "yes" } else { "no" }
            );
        }
        Response::Presets { active, names } => {
            if let Command::PresetRm {
                names: deleted_names,
            } = cmd
            {
                println!("deleted presets: {}", deleted_names.join(", "));
            }
            for n in names {
                let marker = if n == active { "*" } else { " " };
                println!("{marker} {n}");
            }
        }
        Response::Error(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Response::UnsavedSession(t) => {
            println!("unsaved tuning changes");
            print_curve(t, None);
        }
        Response::ResetWouldOverwrite { names } => {
            println!(
                "reset would replace modified shipped presets: {}",
                names.join(", ")
            );
        }
    }
}

fn handle_off_response(resp: &Response) -> anyhow::Result<()> {
    match resp {
        Response::UnsavedSession(t) => {
            println!("eqtune off — native Apple audio restored");
            println!("unsaved tuning changes for preset '{}'", t.preset);
            print_curve(t, None);
            resolve_unsaved_session(&t.preset)
        }
        _ => {
            print_response(&Command::Off, resp);
            Ok(())
        }
    }
}

fn resolve_unsaved_session(active_preset: &str) -> anyhow::Result<()> {
    loop {
        print!("Preserve this tuning? [s]ave by name / [o]verwrite {active_preset} / [d]iscard: ");
        io::stdout().flush()?;
        let choice = read_line_trimmed()?;
        let req = match choice.as_str() {
            "s" | "save" => {
                print!("Preset name (new name, or bright/mellow/pro to overwrite that built-in): ");
                io::stdout().flush()?;
                let name = read_line_trimmed()?;
                if name.is_empty() {
                    eprintln!("preset name must not be empty");
                    continue;
                }
                Request::SaveSessionAs { name }
            }
            "o" | "overwrite" => Request::SaveSessionOverwrite,
            "d" | "discard" | "" => Request::DiscardSession,
            _ => {
                eprintln!("enter s, o, or d");
                continue;
            }
        };

        match ipc::send(&req)? {
            Response::Error(e) => {
                eprintln!("error: {e}");
                continue;
            }
            Response::Tuning(t) => {
                match req {
                    Request::SaveSessionAs { .. } => println!("saved tuning"),
                    Request::SaveSessionOverwrite => println!("overwrote preset {active_preset}"),
                    Request::DiscardSession => println!("discarded tuning changes"),
                    _ => {}
                }
                print_curve(&t, None);
                return Ok(());
            }
            other => {
                print_response(&Command::Off, &other);
                return Ok(());
            }
        }
    }
}

fn read_line_trimmed() -> anyhow::Result<String> {
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Ok(String::new());
    }
    Ok(line.trim().to_string())
}

fn handle_reset_response(cmd: &Command, resp: &Response) -> anyhow::Result<()> {
    match resp {
        Response::ResetWouldOverwrite { names } => resolve_reset_overwrite(cmd, names),
        _ => {
            print_response(cmd, resp);
            Ok(())
        }
    }
}

fn resolve_reset_overwrite(cmd: &Command, names: &[String]) -> anyhow::Result<()> {
    println!(
        "reset will restore shipped preset values for: {}",
        names.join(", ")
    );
    println!("The current local versions of those presets differ from the shipped originals.");
    loop {
        print!("Save copies before reset? [s]ave copies / [r]eset without saving / [c]ancel: ");
        io::stdout().flush()?;
        let choice = read_line_trimmed()?;
        match choice.as_str() {
            "s" | "save" => {
                let backups = prompt_reset_backups(names)?;
                send_confirm_reset(cmd, backups)?;
                return Ok(());
            }
            "r" | "reset" | "" => {
                send_confirm_reset(cmd, vec![])?;
                return Ok(());
            }
            "c" | "cancel" => {
                println!("reset canceled");
                return Ok(());
            }
            _ => eprintln!("enter s, r, or c"),
        }
    }
}

fn prompt_reset_backups(names: &[String]) -> anyhow::Result<Vec<PresetBackup>> {
    let mut backups = Vec::new();
    for source in names {
        let default = format!("{source}-custom");
        loop {
            print!("Save current {source} as [{default}]: ");
            io::stdout().flush()?;
            let entered = read_line_trimmed()?;
            let dest = if entered.is_empty() {
                default.clone()
            } else {
                entered
            };
            if backups.iter().any(|b: &PresetBackup| b.dest == dest) {
                eprintln!("backup name already used in this reset: {dest}");
                continue;
            }
            backups.push(PresetBackup {
                source: source.clone(),
                dest,
            });
            break;
        }
    }
    Ok(backups)
}

fn send_confirm_reset(cmd: &Command, backups: Vec<PresetBackup>) -> anyhow::Result<()> {
    let req = match cmd {
        Command::Reset { name: Some(name) } => Request::ConfirmResetPreset {
            name: name.clone(),
            backups,
        },
        Command::Reset { name: None } => Request::ConfirmReset { backups },
        _ => unreachable!("only reset commands need reset confirmation"),
    };
    match ipc::send(&req)? {
        Response::Error(e) => {
            eprintln!("error: {e}");
            Ok(())
        }
        resp => {
            print_response(cmd, &resp);
            Ok(())
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
        let mark = if Some(i) == marked {
            "   ← changed"
        } else {
            ""
        };
        println!(
            "  {:>8}  {:>8}  Q{}{mark}",
            fmt_freq(b.freq),
            fmt_gain(b.gain_db),
            trim(b.q)
        );
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

fn export_path(name: &str, path: Option<&PathBuf>) -> anyhow::Result<PathBuf> {
    match path {
        Some(path) => absolute_path(path),
        None => absolute_path(&PathBuf::from(format!("{name}.toml"))),
    }
}

fn absolute_path(path: &PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.clone())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
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

    #[test]
    fn export_path_defaults_to_current_directory() {
        let got = export_path("daily", None).unwrap();
        assert_eq!(got, std::env::current_dir().unwrap().join("daily.toml"));
    }

    #[test]
    fn relative_paths_are_resolved_against_current_directory() {
        let got = absolute_path(&PathBuf::from("presets/daily.toml")).unwrap();
        assert_eq!(
            got,
            std::env::current_dir().unwrap().join("presets/daily.toml")
        );
    }

    #[test]
    fn preset_rm_accepts_multiple_names() {
        let cli = Cli::try_parse_from(["eqtune", "preset-rm", "daily", "desk"]).unwrap();
        match cli.command {
            Command::PresetRm { names } => assert_eq!(names, ["daily", "desk"]),
            _ => panic!("expected preset-rm command"),
        }
    }

    #[test]
    fn preset_rm_requires_at_least_one_name() {
        assert!(Cli::try_parse_from(["eqtune", "preset-rm"]).is_err());
    }
}
