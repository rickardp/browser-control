//! Launch-time download preferences for browser-control-owned profiles
//! (ADR-005).
//!
//! These are the safety net for every path that holds no long-lived
//! connection (CLI invocations, the gap between an MCP server's tool calls):
//! the browser itself is configured to save downloads silently into the
//! staging directory instead of opening a native Save dialog. The MCP
//! server's runtime override (`session::downloads`) layers on top.
//!
//! Written strictly before the browser is spawned; a running Chromium
//! rewrites `Preferences` from memory and would discard edits.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

/// Chromium `Preferences` keys (dotted paths) set for owned profiles.
///
/// - `download.prompt_for_download=false`: no "Ask where to save" dialog.
/// - `download.directory_upgrade=true`: keep the directory even if Chrome
///   would consider it "dangerous" and reset it once.
/// - `savefile.default_directory`: Save Page As default.
/// - `automatic_downloads=1` (ALLOW): no "download multiple files" prompt.
/// - `download_bubble.partial_view_enabled=false`: no completion bubble.
fn chromium_pref_values(dir: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("download.prompt_for_download", Value::Bool(false)),
        ("download.default_directory", Value::String(dir.to_string())),
        ("download.directory_upgrade", Value::Bool(true)),
        ("savefile.default_directory", Value::String(dir.to_string())),
        (
            "profile.default_content_setting_values.automatic_downloads",
            Value::from(1),
        ),
        ("download_bubble.partial_view_enabled", Value::Bool(false)),
    ]
}

/// Merge the download prefs into `<profile>/Default/Preferences`.
///
/// A missing file is created. A file that cannot be read or parsed is left
/// alone (error): rewriting it would drop every other setting in it. The
/// write is atomic (temp file + rename) so a crash never leaves half a file.
pub fn apply_chromium(profile_dir: &Path, download_dir: &Path) -> Result<()> {
    let default_dir = profile_dir.join("Default");
    std::fs::create_dir_all(&default_dir)
        .with_context(|| format!("creating {}", default_dir.display()))?;
    let path = default_dir.join("Preferences");
    let mut root = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice::<Value>(&b)
            .ok()
            .filter(Value::is_object)
            .with_context(|| {
                format!(
                    "{} is not a JSON object; leaving it unchanged",
                    path.display()
                )
            })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let dir = download_dir.to_string_lossy();
    for (dotted, value) in chromium_pref_values(&dir) {
        set_dotted(&mut root, dotted, value);
    }
    let bytes = serde_json::to_vec(&root).context("serializing Preferences")?;
    write_atomic(&path, &bytes)
}

fn set_dotted(root: &mut Value, dotted: &str, value: Value) {
    let mut cur = root;
    let mut parts = dotted.split('.').peekable();
    while let Some(part) = parts.next() {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let obj = cur.as_object_mut().expect("just ensured object");
        if parts.peek().is_none() {
            obj.insert(part.to_string(), value);
            return;
        }
        cur = obj
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

/// Firefox prefs set for owned profiles through `user.js` (re-applied by
/// Firefox on every start).
///
/// `always_ask_before_handling_new_types` already defaults to false; it is
/// pinned so a profile that flipped it still saves silently. Types the
/// profile's `handlers.json` marks "always ask" can still prompt on Firefox
/// older than 149, where BiDi `browser.setDownloadBehavior` is unavailable.
fn firefox_pref_values(dir: &str) -> Vec<(&'static str, String)> {
    vec![
        ("browser.download.folderList", "2".into()),
        ("browser.download.dir", js_string(dir)),
        ("browser.download.useDownloadDir", "true".into()),
        (
            "browser.download.always_ask_before_handling_new_types",
            "false".into(),
        ),
        ("browser.download.alwaysOpenPanel", "false".into()),
        ("browser.download.manager.showWhenStarting", "false".into()),
    ]
}

fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Merge the download prefs into `<profile>/user.js`, replacing any earlier
/// `user_pref` line for the same key and keeping unrelated lines.
pub fn apply_firefox(profile_dir: &Path, download_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(profile_dir)
        .with_context(|| format!("creating {}", profile_dir.display()))?;
    let path = profile_dir.join("user.js");
    // Only a missing file counts as empty: on any other read error (bad
    // UTF-8, permissions) rewriting would delete the user's other prefs.
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let prefs = firefox_pref_values(&download_dir.to_string_lossy());
    let mut out: Vec<String> = existing
        .lines()
        .filter(|line| {
            !prefs
                .iter()
                .any(|(k, _)| line.contains(&format!("user_pref(\"{k}\"")))
        })
        .map(String::from)
        .collect();
    for (k, v) in &prefs {
        out.push(format!("user_pref(\"{k}\", {v});"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    write_atomic(&path, text.as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    // Per-process temp name: two concurrent `start`s must not interleave.
    let tmp = path.with_extension(format!("browser-control-{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::Error::new(e).context(format!("replacing {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_prefs_merge_into_existing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prof = tmp.path();
        std::fs::create_dir_all(prof.join("Default")).unwrap();
        std::fs::write(
            prof.join("Default/Preferences"),
            r#"{"download":{"prompt_for_download":true,"other":1},"keep":"me"}"#,
        )
        .unwrap();
        let dl = tmp.path().join("dl dir/ü");
        apply_chromium(prof, &dl).unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(prof.join("Default/Preferences")).unwrap())
                .unwrap();
        assert_eq!(v["download"]["prompt_for_download"], false);
        assert_eq!(v["download"]["other"], 1);
        assert_eq!(v["download"]["directory_upgrade"], true);
        assert_eq!(
            v["download"]["default_directory"],
            dl.to_string_lossy().as_ref()
        );
        assert_eq!(
            v["profile"]["default_content_setting_values"]["automatic_downloads"],
            1
        );
        assert_eq!(v["keep"], "me");
    }

    #[test]
    fn chromium_prefs_create_missing_and_keep_corrupt_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        apply_chromium(tmp.path(), Path::new("/x")).unwrap();
        let prefs = tmp.path().join("Default/Preferences");
        let v: Value = serde_json::from_slice(&std::fs::read(&prefs).unwrap()).unwrap();
        assert_eq!(v["download"]["default_directory"], "/x");
        std::fs::write(&prefs, b"{not json").unwrap();
        assert!(apply_chromium(tmp.path(), Path::new("/y")).is_err());
        assert_eq!(std::fs::read(&prefs).unwrap(), b"{not json");
    }

    #[test]
    fn firefox_user_js_with_invalid_utf8_is_left_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("user.js");
        let original: &[u8] = b"// caf\xe9\nuser_pref(\"x\", 1);\n";
        std::fs::write(&path, original).unwrap();
        assert!(apply_firefox(tmp.path(), Path::new("/d")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn firefox_user_js_replaces_managed_lines_and_keeps_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("user.js"),
            "user_pref(\"browser.download.dir\", \"/old\");\nuser_pref(\"other.pref\", 1);\n",
        )
        .unwrap();
        apply_firefox(tmp.path(), Path::new("/new \"dir\"")).unwrap();
        apply_firefox(tmp.path(), Path::new("/new \"dir\"")).unwrap();
        let s = std::fs::read_to_string(tmp.path().join("user.js")).unwrap();
        assert!(s.contains("user_pref(\"other.pref\", 1);"));
        assert!(!s.contains("/old"));
        assert_eq!(s.matches("browser.download.dir\"").count(), 1);
        assert!(s.contains(r#"user_pref("browser.download.dir", "/new \"dir\"");"#));
        assert!(s.contains("user_pref(\"browser.download.folderList\", 2);"));
    }
}
