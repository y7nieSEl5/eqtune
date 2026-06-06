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
    pub presets: BTreeMap<String, Preset>,
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
        presets.insert(
            "default".to_string(),
            Preset { bands: dsp::default_bands(), preamp_db: dsp::DEFAULT_PREAMP_DB },
        );
        presets.insert("flat".to_string(), Preset { bands: Vec::new(), preamp_db: 0.0 });
        // Candidate device tunings supplied by users (provisional names).
        // NOTE: "air-desk" is all boosts; it ships with -8 dB preamp to tame the
        // loudness. Nudge the preamp toward 0 with `eqtune preamp` if you want it louder.
        presets.insert(
            "air-desk".to_string(),
            Preset {
                bands: graphic(&[
                    (32.0, 7.5), (64.0, 9.0), (125.0, 11.0), (250.0, 7.5), (500.0, 4.0),
                    (1000.0, 4.5), (2000.0, 7.5), (4000.0, 7.5), (8000.0, 9.5), (16000.0, 7.0),
                ]),
                preamp_db: -8.0,
            },
        );
        presets.insert(
            "air-lap".to_string(),
            Preset {
                bands: graphic(&[
                    (32.0, 3.0), (64.0, 2.0), (125.0, 1.0), (250.0, -2.0), (500.0, -3.0),
                    (1000.0, -4.0), (2000.0, -7.0), (4000.0, -1.0), (8000.0, 2.0), (16000.0, 2.0),
                ]),
                preamp_db: 0.0,
            },
        );
        // Sound-engineer 31-band 1/3-octave curve (sub-bass lift + deep 125 Hz notch).
        // Best-effort parse of a hand-supplied spec; tweak by ear with `eqtune band`.
        presets.insert(
            "engineer".to_string(),
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
            active_preset: "default".to_string(),
            limiter: true,
            auto_follow_new_devices: true,
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
    fn default_ships_user_curve() {
        let c = Config::default();
        let p = c.active().unwrap();
        assert_eq!(p.bands.len(), dsp::default_bands().len());
        assert_eq!(p.preamp_db, dsp::DEFAULT_PREAMP_DB);
        assert!(c.limiter);
        assert!(c.auto_follow_new_devices);
    }

    #[test]
    fn library_has_device_presets() {
        let c = Config::default();
        for name in ["default", "flat", "air-desk", "air-lap", "engineer"] {
            assert!(c.presets.contains_key(name), "missing preset {name}");
        }
        assert!(!c.presets.contains_key("macbook-pro"), "macbook-pro should be removed");
        assert_eq!(c.presets["air-desk"].bands.len(), 10);
        assert_eq!(c.presets["air-desk"].preamp_db, -8.0);
        assert_eq!(c.presets["engineer"].bands.len(), 28);
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
