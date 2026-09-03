//! `set` / `get` / `unset` subcommand handlers.
//!
//! Keys: `default` selects the browser to use when `BROWSER_CONTROL` is unset
//! and no positional argument is given; `foreground` (`always` | `off`) makes
//! the MCP server emulate a focused, visible foreground on every tab it
//! touches (see `browser_tab_foreground`).

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

use crate::cli::env_resolver::{self, BrowserSelector};
use crate::cli::output::print_json;
use crate::config::{self, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Key {
    /// Default browser to connect to when `BROWSER_CONTROL` and CLI args are absent.
    Default,
    /// `always` to emulate a focused, visible foreground on every tab the MCP
    /// server touches (requestAnimationFrame and timers keep running while
    /// the window is minimized or the display is locked); `off` to opt in per
    /// tab with `browser_tab_foreground` instead.
    Foreground,
}

impl Key {
    pub fn as_str(self) -> &'static str {
        match self {
            Key::Default => "default",
            Key::Foreground => "foreground",
        }
    }
}

#[derive(Debug, Serialize)]
struct KeyValue<'a> {
    key: &'a str,
    value: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ChangeResult<'a> {
    key: &'a str,
    value: Option<&'a str>,
    changed: bool,
}

/// `browser-control set <key> <value>`. With no value, returns an error
/// pointing at `unset`.
pub fn run_set(key: Key, value: Option<String>, json: bool) -> Result<()> {
    let value = value.ok_or_else(|| {
        anyhow!(
            "missing value for `{}`; pass a value, or use `browser-control unset {}` to clear it",
            key.as_str(),
            key.as_str()
        )
    })?;
    let canonical = canonicalize(key, &value)
        .with_context(|| format!("validating value for `{}`", key.as_str()))?;

    let mut cfg = config::load()?;
    let previous = take(key, &mut cfg);
    set(key, &mut cfg, Some(canonical.clone()));
    config::save(&cfg)?;
    let changed = previous.as_deref() != Some(canonical.as_str());

    emit_change(
        ChangeResult {
            key: key.as_str(),
            value: Some(canonical.as_str()),
            changed,
        },
        json,
        "set",
    )
}

/// `browser-control get <key> [--json]`.
pub fn run_get(key: Key, json: bool) -> Result<()> {
    let cfg = config::load()?;
    let value = get(key, &cfg);
    if json {
        print_json(
            &mut std::io::stdout(),
            &KeyValue {
                key: key.as_str(),
                value: value.as_deref(),
            },
        )?;
    } else {
        match value.as_deref() {
            Some(v) => println!("{} = {v}", key.as_str()),
            None => println!("{} = <unset>", key.as_str()),
        }
    }
    Ok(())
}

/// `browser-control unset <key>`.
pub fn run_unset(key: Key, json: bool) -> Result<()> {
    let mut cfg = config::load()?;
    let previous = take(key, &mut cfg);
    config::save(&cfg)?;

    emit_change(
        ChangeResult {
            key: key.as_str(),
            value: None,
            changed: previous.is_some(),
        },
        json,
        "unset",
    )
}

fn get(key: Key, cfg: &Config) -> Option<String> {
    match key {
        Key::Default => cfg.default.clone(),
        Key::Foreground => cfg.foreground.clone(),
    }
}

fn set(key: Key, cfg: &mut Config, value: Option<String>) {
    match key {
        Key::Default => cfg.default = value,
        Key::Foreground => cfg.foreground = value,
    }
}

fn take(key: Key, cfg: &mut Config) -> Option<String> {
    match key {
        Key::Default => cfg.default.take(),
        Key::Foreground => cfg.foreground.take(),
    }
}

/// Validate `value` against the rules for `key` and return its canonical form.
fn canonicalize(key: Key, value: &str) -> Result<String> {
    match key {
        Key::Default => {
            let sel = env_resolver::parse(value)?;
            Ok(match sel {
                BrowserSelector::Url(u) => u.to_string(),
                BrowserSelector::Kind(k) => k.as_str().to_string(),
                BrowserSelector::ExecutablePath(p) => p.display().to_string(),
                BrowserSelector::Name(n) => n,
            })
        }
        Key::Foreground => match value.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" | "1" => Ok("always".into()),
            "off" | "false" | "0" | "never" => Ok("off".into()),
            other => Err(anyhow!(
                "`foreground` must be `always` or `off`, got `{other}`"
            )),
        },
    }
}

fn emit_change(res: ChangeResult<'_>, json: bool, verb: &str) -> Result<()> {
    if json {
        print_json(&mut std::io::stdout(), &res)?;
    } else {
        let suffix = if res.changed { "" } else { " (no change)" };
        match res.value {
            Some(v) => println!("{verb} {} = {v}{suffix}", res.key),
            None => println!("{verb} {}{suffix}", res.key),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_config<R>(f: impl FnOnce() -> R) -> R {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", td.path());
        let r = f();
        std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        drop(td);
        r
    }

    #[test]
    fn set_default_kind_canonicalizes_to_lowercase() {
        with_tmp_config(|| {
            run_set(Key::Default, Some("FIREFOX".into()), true).unwrap();
            let cfg = config::load().unwrap();
            assert_eq!(cfg.default.as_deref(), Some("firefox"));
        });
    }

    #[test]
    fn set_default_url_stored_verbatim() {
        with_tmp_config(|| {
            let v = "ws://127.0.0.1:9222/devtools/browser/abc";
            run_set(Key::Default, Some(v.into()), true).unwrap();
            let cfg = config::load().unwrap();
            assert_eq!(cfg.default.as_deref(), Some(v));
        });
    }

    #[test]
    fn set_default_empty_rejected() {
        with_tmp_config(|| {
            let err = run_set(Key::Default, Some(String::new()), true).unwrap_err();
            assert!(format!("{err:#}").contains("validating value"));
        });
    }

    #[test]
    fn set_default_no_value_rejected() {
        with_tmp_config(|| {
            let err = run_set(Key::Default, None, true).unwrap_err();
            assert!(format!("{err:#}").contains("missing value"));
        });
    }

    #[test]
    fn unset_removes_default() {
        with_tmp_config(|| {
            run_set(Key::Default, Some("chrome".into()), true).unwrap();
            run_unset(Key::Default, true).unwrap();
            let cfg = config::load().unwrap();
            assert!(cfg.default.is_none());
        });
    }

    #[test]
    fn set_foreground_canonicalizes_and_rejects_garbage() {
        with_tmp_config(|| {
            run_set(Key::Foreground, Some("ON".into()), true).unwrap();
            let cfg = config::load().unwrap();
            assert_eq!(cfg.foreground.as_deref(), Some("always"));
            assert!(cfg.foreground_always());
            assert!(config::foreground_default());
            run_set(Key::Foreground, Some("off".into()), true).unwrap();
            assert!(!config::foreground_default());
            let err = run_set(Key::Foreground, Some("maybe".into()), true).unwrap_err();
            assert!(format!("{err:#}").contains("`always` or `off`"));
            std::env::set_var("BROWSER_CONTROL_FOREGROUND", "1");
            assert!(config::foreground_default());
            std::env::remove_var("BROWSER_CONTROL_FOREGROUND");
        });
    }

    #[test]
    fn get_returns_unset_when_absent() {
        with_tmp_config(|| {
            // Should not error even with no file on disk.
            run_get(Key::Default, true).unwrap();
        });
    }
}
