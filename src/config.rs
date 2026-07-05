//! Persistent configuration: named presets (each an ordered set of EQ bands plus a
//! preamp) and global audio toggles. Serialized as TOML at
//! `~/Library/Application Support/eqtune/config.toml`.
//!
//! Ships a working default (the built-in curve) so a first run needs no config file.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::dsp::{self, Band};

/// A named tuning: the EQ bands plus the preamp make-up gain (dB).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub bands: Vec<Band>,
    pub preamp_db: f32,
}

/// Top-level config: the active preset name, global audio toggles, and all presets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub active_preset: String,
    pub limiter: bool,
    pub auto_follow_new_devices: bool,
    /// Automatically disable the EQ engine while macOS Low Power Mode is active (an
    /// explicit `eqtune on` still overrides it). Defaults on; absent in older config
    /// files, hence the serde default.
    #[serde(default = "default_true")]
    pub auto_off_low_power: bool,
    /// Automatically disable the EQ engine after sustained silence and resume when the
    /// default output device becomes active again.
    #[serde(default = "default_true")]
    pub auto_off_idle: bool,
    pub presets: BTreeMap<String, Preset>,
}

/// Serde default for boolean fields that should be enabled when absent from an older
/// config file.
fn default_true() -> bool {
    true
}

/// A sibling path with `suffix` appended to the file name (e.g. `config.toml` +
/// `.tmp` -> `config.toml.tmp`), so it lives in the same directory — and thus the same
/// filesystem — as `path`, which is required for an atomic `rename`.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("config"))
        .to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Build a graphic-EQ-style preset (peaking filters at ~octave Q) from (freq_hz,
/// gain_db) points.
fn graphic(points: &[(f32, f32)]) -> Vec<Band> {
    const Q: f32 = 1.41;
    points
        .iter()
        .map(|&(freq, gain_db)| Band {
            kind: dsp::BandKind::Peaking,
            freq,
            gain_db,
            q: Q,
        })
        .collect()
}

impl Default for Config {
    fn default() -> Self {
        let mut presets = BTreeMap::new();
        // Candidate tunings (peaking filters at ~octave Q).
        // NOTE: "bright" is all boosts; it ships with -8 dB preamp to tame the
        // loudness. Nudge the preamp toward 0 with `eqtune preamp` if you want it louder.
        presets.insert(
            "bright".to_string(),
            Preset {
                bands: graphic(&[
                    (32.0, 7.5),
                    (64.0, 9.0),
                    (125.0, 11.0),
                    (250.0, 7.5),
                    (500.0, 4.0),
                    (1000.0, 4.5),
                    (2000.0, 7.5),
                    (4000.0, 7.5),
                    (8000.0, 9.5),
                    (16000.0, 7.0),
                ]),
                preamp_db: -8.0,
            },
        );
        presets.insert(
            "mellow".to_string(),
            Preset {
                bands: graphic(&[
                    (32.0, 3.0),
                    (64.0, 2.0),
                    (125.0, 1.0),
                    (250.0, -2.0),
                    (500.0, -3.0),
                    (1000.0, -4.0),
                    (2000.0, -7.0),
                    (4000.0, -1.0),
                    (8000.0, 2.0),
                    (16000.0, 2.0),
                ]),
                preamp_db: 0.0,
            },
        );
        // "pro" — sound-engineer 31-band 1/3-octave curve (sub-bass lift + deep 125 Hz notch).
        // Best-effort parse of a hand-supplied spec; tweak by ear with `eqtune band`.
        presets.insert(
            "pro".to_string(),
            Preset {
                bands: graphic(&[
                    (20.0, 0.0),
                    (25.0, 1.0),
                    (31.5, 2.0),
                    (40.0, 1.5),
                    (50.0, 1.5),
                    (63.0, 1.5),
                    (80.0, -2.0),
                    (100.0, -6.0),
                    (125.0, -15.0),
                    (160.0, -7.0),
                    (200.0, -3.0),
                    (250.0, -2.0),
                    (315.0, -1.0),
                    (400.0, -1.0),
                    (500.0, 0.0),
                    (630.0, 0.0),
                    (800.0, 0.5),
                    (1000.0, 0.75),
                    (1250.0, 1.0),
                    (1600.0, 0.75),
                    (2000.0, 0.0),
                    (2500.0, 0.75),
                    (3150.0, 1.0),
                    (4000.0, 1.0),
                    (5000.0, 0.0),
                    (6300.0, -1.0),
                    (8000.0, 0.5),
                    (10000.0, 0.5),
                ]),
                preamp_db: 0.0,
            },
        );
        Self {
            active_preset: "bright".to_string(),
            limiter: true,
            auto_follow_new_devices: true,
            auto_off_low_power: true,
            auto_off_idle: true,
            presets,
        }
    }
}

impl Config {
    /// Standard config-file location.
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/eqtune/config.toml")
    }

    /// Load from [`Config::path`], or return defaults if the file does not exist.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        let contents = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        // A config that is unusable — malformed TOML, or parseable but with a preset the
        // real-time engine could not run without allocating (over dsp::MAX_BANDS) — is
        // quarantined rather than propagated: under launchd KeepAlive a hard error would
        // restart the daemon into the same failure forever and lose every preset. Back
        // the bad file up for manual recovery and continue with defaults.
        let reason = match toml::from_str::<Config>(&contents) {
            Ok(config) => match config.max_preset_bands() {
                over if over > dsp::MAX_BANDS => {
                    format!(
                        "a preset has {over} bands, over the maximum of {}",
                        dsp::MAX_BANDS
                    )
                }
                _ => return Ok(config),
            },
            Err(e) => format!("invalid TOML ({e})"),
        };
        let backup = with_suffix(path, ".corrupt");
        let _ = std::fs::rename(path, &backup);
        eprintln!(
            "eqtune: config at {} is unusable ({reason}); backed up to {} and continuing with defaults",
            path.display(),
            backup.display()
        );
        Ok(Self::default())
    }

    /// Persist to [`Config::path`], creating the parent directory if needed.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        // Write to a sibling temp file, then atomically rename it into place. A crash or
        // power loss mid-write can then only truncate the throwaway temp file, never the
        // live config — the rename either fully replaces it or leaves the old one intact.
        let tmp = with_suffix(path, ".tmp");
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The largest band count across all presets (0 if there are none). Used at load
    /// time to reject a config the real-time engine could not run without allocating on
    /// the audio thread (see [`crate::dsp::MAX_BANDS`]).
    fn max_preset_bands(&self) -> usize {
        self.presets
            .values()
            .map(|p| p.bands.len())
            .max()
            .unwrap_or(0)
    }

    /// The currently selected preset, falling back to "default" then any preset.
    pub fn active(&self) -> Option<&Preset> {
        self.presets
            .get(&self.active_preset)
            .or_else(|| self.presets.get("default"))
            .or_else(|| self.presets.values().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_active_is_bright() {
        let c = Config::default();
        assert_eq!(c.active_preset, "bright");
        assert_eq!(c.active().unwrap().bands.len(), 10);
        assert!(
            !c.presets.contains_key("original"),
            "original should be removed"
        );
        assert!(c.limiter);
        assert!(c.auto_follow_new_devices);
        assert!(c.auto_off_low_power);
        assert!(c.auto_off_idle);
    }

    #[test]
    fn library_has_expected_presets() {
        let c = Config::default();
        for name in ["bright", "mellow", "pro"] {
            assert!(c.presets.contains_key(name), "missing preset {name}");
        }
        for gone in [
            "flat",
            "macbook-pro",
            "original",
            "air-desk",
            "air-lap",
            "engineer",
        ] {
            assert!(!c.presets.contains_key(gone), "{gone} should be gone");
        }
        assert_eq!(c.presets["bright"].bands.len(), 10);
        assert_eq!(c.presets["bright"].preamp_db, -8.0);
        assert_eq!(c.presets["pro"].bands.len(), 28);
    }

    #[test]
    fn toml_round_trip() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn load_missing_returns_default() {
        let p = Path::new("/nonexistent/eqtune-xyz/config.toml");
        assert_eq!(Config::load_from(p).unwrap(), Config::default());
    }

    #[test]
    fn missing_auto_off_low_power_defaults_true() {
        // A config written before `auto_off_low_power`/`auto_off_idle` existed must still load.
        let toml = r#"
active_preset = "bright"
limiter = true
auto_follow_new_devices = true

[presets.bright]
preamp_db = -8.0
bands = []
"#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.auto_off_low_power, "absent field should default to true");
        assert!(c.auto_off_idle, "absent field should default to true");
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("eqtune-cfg-{}-{unique}-{tag}", std::process::id()))
    }

    #[test]
    fn save_to_is_atomic_and_leaves_no_temp_file() {
        let dir = unique_dir("atomic");
        let path = dir.join("config.toml");
        let c = Config::default();
        c.save_to(&path).unwrap();

        assert!(path.exists(), "config must be written");
        assert!(
            !dir.join("config.toml.tmp").exists(),
            "the temp file must be renamed away, not left behind"
        );
        assert_eq!(Config::load_from(&path).unwrap(), c);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_recovers_from_corrupt_config() {
        let dir = unique_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "not valid toml {{{").unwrap();

        // Must fall back to defaults instead of erroring (which would crash-loop the
        // daemon under launchd KeepAlive).
        let recovered = Config::load_from(&path).unwrap();
        assert_eq!(recovered, Config::default());
        // The bad file is preserved for manual recovery, and moved out of the way so the
        // next save writes a fresh, valid config.
        assert!(dir.join("config.toml.corrupt").exists());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wide_bands(n: usize) -> Vec<Band> {
        (0..n)
            .map(|i| Band {
                kind: dsp::BandKind::Peaking,
                freq: 20.0 + i as f32,
                gain_db: 1.0,
                q: 1.0,
            })
            .collect()
    }

    #[test]
    fn load_from_recovers_from_over_cap_preset() {
        let dir = unique_dir("overcap");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // Valid TOML, but a preset exceeds the band cap — the RT engine could not run it
        // without allocating, so load must quarantine it and fall back to defaults.
        let mut c = Config::default();
        c.presets.insert(
            "big".into(),
            Preset {
                bands: wide_bands(dsp::MAX_BANDS + 1),
                preamp_db: 0.0,
            },
        );
        std::fs::write(&path, toml::to_string_pretty(&c).unwrap()).unwrap();

        let recovered = Config::load_from(&path).unwrap();
        assert_eq!(recovered, Config::default());
        assert!(dir.join("config.toml.corrupt").exists());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_accepts_preset_at_the_band_cap() {
        let dir = unique_dir("atcap");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.presets.insert(
            "full".into(),
            Preset {
                bands: wide_bands(dsp::MAX_BANDS),
                preamp_db: 0.0,
            },
        );
        std::fs::write(&path, toml::to_string_pretty(&c).unwrap()).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.presets["full"].bands.len(), dsp::MAX_BANDS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("eqtune-cfg-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let c = Config {
            active_preset: "flat".to_string(),
            ..Config::default()
        };
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(c, back);
    }
}
