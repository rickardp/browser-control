//! Persistent user configuration (TOML at `<config_dir>/config.toml`).
//!
//! Edited via the `browser-control set|get|unset` subcommands.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

use crate::paths;

const HEADER: &str =
    "# Managed by browser-control. Edit with `browser-control set <key> <value>`.\n";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// `"always"` to emulate a focused, visible foreground on every tab the
    /// MCP server touches (see `browser_tab_foreground`); `"off"` or absent
    /// for per-tab opt-in only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
}

impl Config {
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.foreground.is_none()
    }

    /// Whether the persisted `foreground` key asks for always-on emulation.
    pub fn foreground_always(&self) -> bool {
        matches!(self.foreground.as_deref(), Some("always"))
    }
}

/// Resolve the server-wide foreground default: `BROWSER_CONTROL_FOREGROUND`
/// (`1` / `always` / `true` on, `0` / `off` / `false` off) wins over the
/// persisted config key; both absent means off.
pub fn foreground_default() -> bool {
    if let Ok(v) = std::env::var("BROWSER_CONTROL_FOREGROUND") {
        let v = v.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "1" | "always" | "true" | "on") {
            return true;
        }
        if matches!(v.as_str(), "0" | "off" | "false" | "never") {
            return false;
        }
    }
    load().map(|c| c.foreground_always()).unwrap_or(false)
}

/// Load the config from disk. A missing file yields `Config::default()`.
pub fn load() -> Result<Config> {
    load_from(&paths::config_file_path()?)
}

/// Save `cfg` to disk atomically.
pub fn save(cfg: &Config) -> Result<()> {
    save_to(&paths::config_file_path()?, cfg)
}

pub(crate) fn load_from(path: &PathBuf) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str::<Config>(&s)
            .with_context(|| format!("parsing config file {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("reading config file {}", path.display())),
    }
}

pub(crate) fn save_to(path: &PathBuf, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir {}", parent.display()))?;
    }
    let body = toml::to_string_pretty(cfg).context("serializing config to TOML")?;
    let mut contents = String::with_capacity(HEADER.len() + body.len());
    contents.push_str(HEADER);
    contents.push_str(&body);

    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("creating tmp config file {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("writing tmp config file {}", tmp.display()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_cfg() -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::TempDir::new().unwrap();
        let p = td.path().join("config.toml");
        (td, p)
    }

    #[test]
    fn missing_file_yields_default() {
        let (_td, p) = tmp_cfg();
        let cfg = load_from(&p).unwrap();
        assert_eq!(cfg, Config::default());
        assert!(cfg.is_empty());
    }

    #[test]
    fn round_trip_default() {
        let (_td, p) = tmp_cfg();
        let cfg = Config {
            default: Some("firefox".into()),
            foreground: Some("always".into()),
        };
        save_to(&p, &cfg).unwrap();
        assert!(cfg.foreground_always());
        let read = load_from(&p).unwrap();
        assert_eq!(read, cfg);

        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Managed by browser-control"));
        assert!(text.contains("default = \"firefox\""));
    }

    #[test]
    fn save_clears_when_default_is_none() {
        let (_td, p) = tmp_cfg();
        save_to(
            &p,
            &Config {
                default: Some("chrome".into()),
                foreground: None,
            },
        )
        .unwrap();
        save_to(&p, &Config::default()).unwrap();
        let read = load_from(&p).unwrap();
        assert!(read.is_empty());
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            !text.contains("default ="),
            "expected key to be absent, got: {text}"
        );
    }

    #[test]
    fn malformed_file_is_an_error() {
        let (_td, p) = tmp_cfg();
        std::fs::write(&p, "this is = not [valid toml").unwrap();
        let err = load_from(&p).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(msg.contains("parsing config file"), "got: {msg}");
    }
}
