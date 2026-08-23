//! User configuration loaded from
//! `$XDG_CONFIG_HOME/osnip/config.toml` (or `~/.config/...` if
//! `XDG_CONFIG_HOME` is unset).
//!
//! Missing file or missing fields fall back to documented defaults so a
//! fresh install Just Works without a config file. Parse errors are
//! surfaced at daemon startup — we do **not** silently ignore a
//! malformed config.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Resolved daemon configuration. Constructed via [`Config::load`] or
/// [`Config::with_defaults`] — `Default` returns the same values as
/// `with_defaults` modulo `~` resolution.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory where `Save` writes PNGs. Created on demand.
    pub save_dir: PathBuf,
    /// Filename template. The literal token `{timestamp}` is replaced
    /// with `YYYYMMDD-HHMMSS` (local time) at save time.
    pub filename_template: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    save_dir: Option<String>,
    filename_template: Option<String>,
}

impl Config {
    /// Default save directory: `$HOME/Pictures/osnip/`. Falls back
    /// to `./osnip/` only if `dirs::picture_dir` and `dirs::home_dir`
    /// both fail (vanishingly rare; logged at warn).
    fn default_save_dir() -> PathBuf {
        if let Some(p) = dirs::picture_dir() {
            return p.join("osnip");
        }
        if let Some(home) = dirs::home_dir() {
            return home.join("Pictures").join("osnip");
        }
        tracing::warn!("could not resolve picture_dir or home_dir; using ./osnip");
        PathBuf::from("./osnip")
    }

    /// Defaults only — no file read.
    pub fn with_defaults() -> Self {
        Self {
            save_dir: Self::default_save_dir(),
            filename_template: "osnip-{timestamp}.png".to_string(),
        }
    }

    /// Load from `$XDG_CONFIG_HOME/osnip/config.toml`, applying
    /// defaults for any missing field. Returns defaults if the file
    /// does not exist. Returns an error only if the file exists but is
    /// unreadable or malformed.
    pub fn load() -> anyhow::Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            tracing::info!(
                path = %path.display(),
                "no config file; using defaults",
            );
            return Ok(Self::with_defaults());
        }
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        let defaults = Self::with_defaults();
        Ok(Self {
            save_dir: raw.save_dir.map(expand_tilde).unwrap_or(defaults.save_dir),
            filename_template: raw.filename_template.unwrap_or(defaults.filename_template),
        })
    }

    fn config_path() -> PathBuf {
        if let Some(dir) = dirs::config_dir() {
            return dir.join("osnip").join("config.toml");
        }
        PathBuf::from("./osnip-config.toml")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::with_defaults()
    }
}

fn expand_tilde(s: String) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_have_template_and_dir() {
        let cfg = Config::with_defaults();
        assert!(cfg.filename_template.contains("{timestamp}"));
        assert!(!cfg.save_dir.as_os_str().is_empty());
    }

    #[test]
    fn parses_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "save_dir = \"/tmp/snips\"\nfilename_template = \"x-{{timestamp}}.png\""
        )
        .unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.save_dir, PathBuf::from("/tmp/snips"));
        assert_eq!(cfg.filename_template, "x-{timestamp}.png");
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "save_dir = \"/tmp/only-save\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.save_dir, PathBuf::from("/tmp/only-save"));
        assert_eq!(cfg.filename_template, "osnip-{timestamp}.png");
    }

    #[test]
    fn malformed_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this = is = not = toml").unwrap();
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn tilde_in_save_dir_expands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "save_dir = \"~/snips\"\n").unwrap();
        let cfg = Config::load_from(&path).unwrap();
        let home = dirs::home_dir().expect("home dir for test");
        assert_eq!(cfg.save_dir, home.join("snips"));
    }
}
