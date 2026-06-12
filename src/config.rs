//! Persistent configuration: named presets (each an ordered set of EQ bands plus a
//! preamp) and global audio toggles. Serialized as TOML at
//! `~/Library/Application Support/eqtune/config.toml`.
//!
//! Ships a working default (the built-in curve) so a first run needs no config file.

use std::collections::BTreeMap;
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
    pub presets: BTreeMap<String, Preset>,
}

/// Serde default for boolean fields that should be enabled when absent from an older
/// config file.
fn default_true() -> bool {
    true
}

/// Build a graphic-EQ-style preset (peaking filters at ~octave Q) from (freq_hz,
/// gain_db) points.
fn graphic(points: &[(f32, f32)]) -> Vec<Band> {
    const Q: f32 = 1.41;
    points
        .iter()
        .map(|&(freq, gain_db)| Band { kind: dsp::BandKind::Peaking, freq, gain_db, q: Q })
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
                    (32.0, 7.5), (64.0, 9.0), (125.0, 11.0), (250.0, 7.5), (500.0, 4.0),
                    (1000.0, 4.5), (2000.0, 7.5), (4000.0, 7.5), (8000.0, 9.5), (16000.0, 7.0),
                ]),
                preamp_db: -8.0,
            },
        );
        presets.insert(
            "mellow".to_string(),
            Preset {
                bands: graphic(&[
                    (32.0, 3.0), (64.0, 2.0), (125.0, 1.0), (250.0, -2.0), (500.0, -3.0),
                    (1000.0, -4.0), (2000.0, -7.0), (4000.0, -1.0), (8000.0, 2.0), (16000.0, 2.0),
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
                    (20.0, 0.0), (25.0, 1.0), (31.5, 2.0), (40.0, 1.5), (50.0, 1.5), (63.0, 1.5),
                    (80.0, -2.0), (100.0, -6.0), (125.0, -15.0), (160.0, -7.0), (200.0, -3.0),
                    (250.0, -2.0), (315.0, -1.0), (400.0, -1.0), (500.0, 0.0), (630.0, 0.0),
                    (800.0, 0.5), (1000.0, 0.75), (1250.0, 1.0), (1600.0, 0.75), (2000.0, 0.0),
                    (2500.0, 0.75), (3150.0, 1.0), (4000.0, 1.0), (5000.0, 0.0), (6300.0, -1.0),
                    (8000.0, 0.5), (10000.0, 0.5),
                ]),
                preamp_db: 0.0,
            },
        );
        Self {
            active_preset: "bright".to_string(),
            limiter: true,
            auto_follow_new_devices: true,
            auto_off_low_power: true,
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
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(toml::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Persist to [`Config::path`], creating the parent directory if needed.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
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
        assert!(!c.presets.contains_key("original"), "original should be removed");
        assert!(c.limiter);
        assert!(c.auto_follow_new_devices);
        assert!(c.auto_off_low_power);
    }

    #[test]
    fn library_has_expected_presets() {
        let c = Config::default();
        for name in ["bright", "mellow", "pro"] {
            assert!(c.presets.contains_key(name), "missing preset {name}");
        }
        for gone in ["flat", "macbook-pro", "original", "air-desk", "air-lap", "engineer"] {
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
        // A config written before `auto_off_low_power` existed must still load.
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
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = std::env::temp_dir().join(format!("eqtune-cfg-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut c = Config::default();
        c.active_preset = "flat".to_string();
        c.save_to(&path).unwrap();
        let back = Config::load_from(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(c, back);
    }
}
