//! `start` subcommand: launch a browser and register it.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::path::PathBuf;

use crate::cli::output::print_json;
use crate::detect::{self, Engine, Installed, Kind};
use crate::launch::{self, LaunchOpts};
use crate::paths;
use crate::registry::{self, BrowserRow, Registry};

#[derive(Debug, Serialize)]
pub struct StartResult {
    pub name: String,
    pub kind: Kind,
    pub pid: u32,
    pub engine: Engine,
    pub endpoint: String,
    pub profile: PathBuf,
    pub headless: bool,
    pub started_at: String,
    pub reused: bool,
}

pub async fn run(
    browser: Option<String>,
    headless: bool,
    no_wait: bool,
    wait_timeout: u64,
    json: bool,
) -> Result<()> {
    let res = ensure_started(browser, headless, no_wait, wait_timeout).await?;
    emit(&res, json)?;
    Ok(())
}

/// Ensure a browser of the requested kind is running and registered.
///
/// This is the programmatic form of `browser-control start`: it reuses the
/// most recent live registry row for the selected kind, otherwise launches a
/// new browser with the same default profile semantics as the CLI command.
///
/// When no kind is requested, an installed browser the user already has
/// running (even one browser-control never launched or registered) is
/// preferred over the hardcoded detection order — so a manually-opened
/// browser wins over blindly starting whichever kind is first on the
/// platform's candidate list.
pub async fn ensure_started(
    browser: Option<String>,
    headless: bool,
    no_wait: bool,
    wait_timeout: u64,
) -> Result<StartResult> {
    let installed = detect::list_installed();
    if installed.is_empty() {
        anyhow::bail!("no supported browsers installed; run `browser-control list-installed`");
    }

    let resolved_kind: Kind = match browser.as_deref() {
        None => resolve_kind_when_unspecified(&installed)
            .ok_or_else(|| anyhow!("no chromium-based browser installed"))?,
        Some(s) => Kind::parse(s).ok_or_else(|| {
            anyhow!("unknown browser kind `{s}`; valid: chrome, edge, chromium, brave, firefox")
        })?,
    };

    let installed_match = installed
        .iter()
        .find(|i| i.kind == resolved_kind)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "browser `{}` is not installed on this machine",
                resolved_kind.as_str()
            )
        })?;

    let registry = Registry::open()?;
    if let Some(row) = registry.first_alive_by_kind(resolved_kind)? {
        if !no_wait {
            crate::cli::wait::wait_until_ready(
                &row.endpoint,
                row.engine,
                std::time::Duration::from_secs(wait_timeout),
            )
            .await?;
        }
        return Ok(to_result(&row, true));
    }

    let name = registry::naming::generate_default(resolved_kind, &registry)?;
    let profile_dir = paths::default_profile_dir(resolved_kind)?;
    std::fs::create_dir_all(&profile_dir).context("creating profile directory")?;
    let opts = LaunchOpts {
        headless,
        profile_dir: profile_dir.clone(),
    };
    let handle = launch::launch(&installed_match, opts)
        .await
        .with_context(|| format!("launching {}", installed_match.executable.display()))?;

    let row = BrowserRow {
        name: name.clone(),
        kind: resolved_kind,
        engine: handle.engine,
        pid: handle.pid,
        endpoint: handle.endpoint.clone(),
        port: handle.port,
        profile_dir: handle.profile_dir.clone(),
        executable: installed_match.executable.clone(),
        headless,
        started_at: registry::now_iso8601(),
    };
    registry.insert(&row)?;
    let _pid = handle.forget();

    if !no_wait {
        crate::cli::wait::wait_until_ready(
            &row.endpoint,
            row.engine,
            std::time::Duration::from_secs(wait_timeout),
        )
        .await?;
    }

    Ok(to_result(&row, false))
}

pub(crate) fn first_chromium_or_first(installed: &[Installed]) -> Option<Kind> {
    installed
        .iter()
        .find(|i| i.kind.is_chromium())
        .map(|i| i.kind)
        .or_else(|| installed.first().map(|i| i.kind))
}

/// Selection used by [`ensure_started`] when no `--browser` kind is given:
/// prefer an installed kind that already has a live process over the
/// hardcoded chromium-first fallback order.
pub(crate) fn resolve_kind_when_unspecified(installed: &[Installed]) -> Option<Kind> {
    let running = detect::list_running_kinds(installed);
    installed
        .iter()
        .find(|i| running.contains(&i.kind))
        .map(|i| i.kind)
        .or_else(|| first_chromium_or_first(installed))
}

fn to_result(row: &BrowserRow, reused: bool) -> StartResult {
    StartResult {
        name: row.name.clone(),
        kind: row.kind,
        pid: row.pid,
        engine: row.engine,
        endpoint: row.endpoint.clone(),
        profile: row.profile_dir.clone(),
        headless: row.headless,
        started_at: row.started_at.clone(),
        reused,
    }
}

fn emit(res: &StartResult, json: bool) -> Result<()> {
    if json {
        print_json(&mut std::io::stdout(), res)?;
    } else {
        let reused = if res.reused { " (reused)" } else { "" };
        println!("Started {}{}", res.name, reused);
        println!("  kind:     {}", res.kind.as_str());
        println!("  pid:      {}", res.pid);
        println!("  engine:   {:?}", res.engine);
        println!("  endpoint: {}", res.endpoint);
        println!("  profile:  {}", res.profile.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stands in for a "browser" that's actually running: we only care that a
    // live process's exe path matches `Installed.executable`, not that it's
    // a real browser. Mirrors the technique in detect::mod::tests.
    fn spawn_stand_in_process() -> (std::process::Child, PathBuf) {
        let exe = PathBuf::from(if cfg!(windows) {
            r"C:\Windows\System32\timeout.exe"
        } else {
            "/bin/sleep"
        });
        assert!(exe.exists(), "test requires {exe:?} to exist");
        let child = std::process::Command::new(&exe)
            .arg("5")
            .spawn()
            .expect("spawn stand-in process");
        (child, exe)
    }

    fn installed(kind: Kind, executable: PathBuf) -> Installed {
        Installed {
            kind,
            executable,
            version: "unknown".to_string(),
            engine: kind.engine(),
        }
    }

    #[test]
    fn resolve_kind_when_unspecified_prefers_running_kind_over_hardcoded_order() {
        let (mut child, exe) = spawn_stand_in_process();

        // Firefox is running (matches the live process's exe) but is listed
        // after Chrome, and `first_chromium_or_first` would otherwise pick
        // Chrome first. The running browser must win regardless of order.
        let candidates = vec![
            installed(Kind::Chrome, PathBuf::from("/definitely/not/a/real/chrome")),
            installed(Kind::Firefox, exe),
        ];

        let resolved = resolve_kind_when_unspecified(&candidates);

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(resolved, Some(Kind::Firefox));
    }

    #[test]
    fn resolve_kind_when_unspecified_falls_back_when_nothing_running() {
        let candidates = vec![
            installed(
                Kind::Firefox,
                PathBuf::from("/definitely/not/a/real/firefox"),
            ),
            installed(Kind::Chrome, PathBuf::from("/definitely/not/a/real/chrome")),
        ];
        assert_eq!(
            resolve_kind_when_unspecified(&candidates),
            first_chromium_or_first(&candidates)
        );
        assert_eq!(
            resolve_kind_when_unspecified(&candidates),
            Some(Kind::Chrome)
        );
    }

    #[test]
    fn resolve_kind_when_unspecified_none_when_nothing_installed() {
        assert_eq!(resolve_kind_when_unspecified(&[]), None);
    }
}
