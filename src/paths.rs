//! OS app-data directory resolution.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

use crate::detect::Kind;

const ENV_OVERRIDE: &str = "BROWSER_CONTROL_DATA_DIR";
const CONFIG_ENV_OVERRIDE: &str = "BROWSER_CONTROL_CONFIG_DIR";

/// Root data directory for browser-control. Used for the registry DB and profile dirs.
///
/// macOS:   `~/Library/Application Support/browser-control`
/// Linux:   `$XDG_DATA_HOME/browser-control` (or `~/.local/share/browser-control`)
/// Windows: `%APPDATA%/browser-control`
///
/// Override with env var `BROWSER_CONTROL_DATA_DIR` (absolute path).
pub fn data_dir() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(ENV_OVERRIDE) {
        let p = PathBuf::from(v);
        if p.as_os_str().is_empty() {
            return Err(anyhow!("{} is set but empty", ENV_OVERRIDE));
        }
        return Ok(p);
    }

    let pd = directories::ProjectDirs::from("", "", "browser-control")
        .ok_or_else(|| anyhow!("could not determine user data directory (no home dir found)"))?;
    Ok(pd.data_dir().to_path_buf())
}

/// Returns `<data_dir>/registry.db`. Ensures the parent directory exists.
pub fn registry_db_path() -> Result<PathBuf> {
    let dir = data_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating data dir {}", dir.display()))?;
    Ok(dir.join("registry.db"))
}

/// Root config directory for browser-control.
///
/// macOS:   `~/Library/Application Support/browser-control` (same as `data_dir`)
/// Linux:   `$XDG_CONFIG_HOME/browser-control` (or `~/.config/browser-control`)
/// Windows: `%APPDATA%/browser-control` (same as `data_dir`)
///
/// Override with env var `BROWSER_CONTROL_CONFIG_DIR` (absolute path).
pub fn config_dir() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(CONFIG_ENV_OVERRIDE) {
        let p = PathBuf::from(v);
        if p.as_os_str().is_empty() {
            return Err(anyhow!("{} is set but empty", CONFIG_ENV_OVERRIDE));
        }
        return Ok(p);
    }

    let pd = directories::ProjectDirs::from("", "", "browser-control")
        .ok_or_else(|| anyhow!("could not determine user config directory (no home dir found)"))?;
    Ok(pd.config_dir().to_path_buf())
}

/// Returns `<config_dir>/config.toml`. Ensures the parent directory exists.
pub fn config_file_path() -> Result<PathBuf> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating config dir {}", dir.display()))?;
    Ok(dir.join("config.toml"))
}

/// Returns `<data_dir>/profiles`. Ensures the directory exists.
pub fn profiles_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("profiles");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating profiles dir {}", dir.display()))?;
    Ok(dir)
}

/// Returns the stable per-kind default profile directory used when
/// `browser-control start` is invoked without `--profile`.
///
/// The directory is rooted under [`config_dir`] (so it tracks
/// `BROWSER_CONTROL_CONFIG_DIR` for tests and user overrides):
///
/// * macOS:   `~/Library/Application Support/browser-control/profiles/<kind>/default/`
/// * Linux:   `~/.config/browser-control/profiles/<kind>/default/`
/// * Windows: `%APPDATA%/browser-control/profiles/<kind>/default/`
///
/// `<kind>` is the lowercase [`Kind`] variant name (`chrome`, `edge`,
/// `chromium`, `brave`, `firefox`). The split-by-kind is intentional:
/// Chromium and Firefox profile layouts are not interchangeable.
///
/// The directory is created lazily on first call.
pub fn default_profile_dir(kind: Kind) -> Result<PathBuf> {
    let dir = config_dir()?
        .join("profiles")
        .join(kind.as_str())
        .join("default");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating default profile dir {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_work() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Test 1: override via env var.
        let tmp = tempfile::TempDir::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        // Safety: tests in this module share process env; we run them sequentially
        // within a single #[test] to avoid races with other tests.
        std::env::set_var(ENV_OVERRIDE, &tmp_path);

        assert_eq!(data_dir().unwrap(), tmp_path);

        let db = registry_db_path().unwrap();
        assert_eq!(db, tmp_path.join("registry.db"));
        assert!(db.parent().unwrap().exists());
        assert_eq!(db.file_name().unwrap(), "registry.db");

        let pdir = profiles_dir().unwrap();
        assert_eq!(pdir, tmp_path.join("profiles"));
        assert!(pdir.is_dir());
        assert_eq!(pdir.file_name().unwrap(), "profiles");

        // Test 2: default path has the expected suffix.
        std::env::remove_var(ENV_OVERRIDE);
        let d = data_dir().unwrap();
        assert!(
            d.ends_with("browser-control"),
            "expected default data_dir to end with 'browser-control', got {}",
            d.display()
        );
    }

    #[test]
    fn config_paths_work() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        std::env::set_var(CONFIG_ENV_OVERRIDE, &tmp_path);

        assert_eq!(config_dir().unwrap(), tmp_path);

        let cfg = config_file_path().unwrap();
        assert_eq!(cfg, tmp_path.join("config.toml"));
        assert!(cfg.parent().unwrap().exists());
        assert_eq!(cfg.file_name().unwrap(), "config.toml");

        std::env::remove_var(CONFIG_ENV_OVERRIDE);
        let d = config_dir().unwrap();
        assert!(
            d.ends_with("browser-control"),
            "expected default config_dir to end with 'browser-control', got {}",
            d.display()
        );
    }

    #[test]
    fn default_profile_dir_honours_config_env_override() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        let tmp_path = tmp.path().to_path_buf();
        std::env::set_var(CONFIG_ENV_OVERRIDE, &tmp_path);

        for k in [
            Kind::Chrome,
            Kind::Edge,
            Kind::Chromium,
            Kind::Brave,
            Kind::Firefox,
        ] {
            let p = default_profile_dir(k).unwrap();
            assert_eq!(
                p,
                tmp_path.join("profiles").join(k.as_str()).join("default")
            );
            assert!(p.is_dir(), "expected {} to be created", p.display());
        }

        std::env::remove_var(CONFIG_ENV_OVERRIDE);
    }
}
