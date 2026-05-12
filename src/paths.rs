//! OS app-data directory resolution.

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

const ENV_OVERRIDE: &str = "BROWSER_CONTROL_DATA_DIR";

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

/// Returns `<data_dir>/profiles`. Ensures the directory exists.
pub fn profiles_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("profiles");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating profiles dir {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_work() {
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
}
