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

impl Default for Config {
    fn default() -> Self {
        let mut presets = BTreeMap::new();
        presets.insert(
            "default".to_string(),
            Preset { bands: dsp::default_bands(), preamp_db: dsp::DEFAULT_PREAMP_DB },
        );
        presets.insert("flat".to_string(), Preset { bands: Vec::new(), preamp_db: 0.0 });
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
