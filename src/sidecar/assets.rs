//! Cache-dir management for the embedded sidecar assets.
//!
//! The sidecar JS + `package.json` are bundled into the Rust binary via
//! `include_str!` at compile time. At runtime we materialize them into a
//! cache directory under the user's data dir (e.g.
//! `~/.cache/browser-control/playwright-sidecar-<version>/`) and run
//! `bun install` / `npm install` there once. Subsequent spawns reuse the
//! cache.
//!
//! The cache dir name encodes the `playwright-core` version so different
//! versions don't share `node_modules`.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Bundled sidecar JS source.
const SIDECAR_JS: &str = include_str!("../../assets/playwright-sidecar/sidecar.mjs");

/// Bundled `package.json` template. The `{version}` placeholder is
/// replaced at runtime with the requested `playwright-core` version.
const PACKAGE_JSON: &str = include_str!("../../assets/playwright-sidecar/package.json");

/// Compute the per-version cache directory and ensure it exists with the
/// current sidecar assets written into it.
pub async fn ensure_sidecar_dir(playwright_version: &str) -> Result<PathBuf> {
    let dir = cache_dir(playwright_version)?;
    tokio::fs::create_dir_all(&dir).await.with_context(|| {
        format!("creating sidecar cache directory at {dir:?}")
    })?;

    // Write the JS verbatim each time so a `cargo install` upgrade picks
    // up changes to the bundled script automatically.
    let js_path = dir.join("sidecar.mjs");
    tokio::fs::write(&js_path, SIDECAR_JS)
        .await
        .with_context(|| format!("writing {js_path:?}"))?;

    // Substitute the requested Playwright version into the package.json
    // template before writing. The template ships with the default
    // version baked in.
    let pkg_json = patch_version(PACKAGE_JSON, playwright_version);
    let pkg_path = dir.join("package.json");
    tokio::fs::write(&pkg_path, pkg_json)
        .await
        .with_context(|| format!("writing {pkg_path:?}"))?;

    Ok(dir)
}

fn cache_dir(playwright_version: &str) -> Result<PathBuf> {
    let project = directories::ProjectDirs::from("dev", "browser-control", "browser-control")
        .ok_or_else(|| anyhow::anyhow!("could not determine user cache directory"))?;
    let base = project.cache_dir().to_path_buf();
    Ok(base.join(format!("playwright-sidecar-{playwright_version}")))
}

/// Replace `"playwright-core": "<X.Y.Z>"` in the template with the
/// requested version. The template ships with a known default; this is a
/// targeted substitution so an unrelated version-shaped string elsewhere
/// in the file isn't accidentally rewritten.
fn patch_version(template: &str, version: &str) -> String {
    // Find the `"playwright-core":` key and rewrite the value string.
    let key = "\"playwright-core\":";
    let Some(idx) = template.find(key) else {
        return template.to_string();
    };
    // Locate the opening quote of the value after the key.
    let after_key = &template[idx + key.len()..];
    let Some(open_off) = after_key.find('"') else {
        return template.to_string();
    };
    let after_open = &after_key[open_off + 1..];
    let Some(close_off) = after_open.find('"') else {
        return template.to_string();
    };
    let mut out = String::with_capacity(template.len() + version.len());
    out.push_str(&template[..idx + key.len() + open_off + 1]);
    out.push_str(version);
    out.push_str(&after_open[close_off..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_version_rewrites_known_template() {
        let t = r#"{
  "name": "x",
  "dependencies": {
    "playwright-core": "1.49.1"
  }
}"#;
        let out = patch_version(t, "1.55.0");
        assert!(out.contains("\"playwright-core\": \"1.55.0\""));
        assert!(!out.contains("1.49.1"));
    }

    #[test]
    fn patch_version_keeps_input_when_key_missing() {
        let t = r#"{ "name": "x" }"#;
        let out = patch_version(t, "1.55.0");
        assert_eq!(out, t);
    }
}
