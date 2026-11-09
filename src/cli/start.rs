//! `start` subcommand: launch a browser and register it.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::path::PathBuf;

use crate::cli::env_resolver::BrowserSelector;
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
/// When no kind is requested, selection follows the same order the MCP
/// server uses at startup (see [`crate::cli::mcp::resolve_browser`]):
/// the persisted default (`browser-control set default ...`) > the most
/// recent live registered browser > an installed kind that already has a
/// live process > the hardcoded chromium-first detection order.
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

    let registry = Registry::open()?;

    let resolved_kind: Kind = match browser.as_deref() {
        None => resolve_kind_when_unspecified(&installed, &registry)
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
    let download_dir = match paths::downloads_staging_dir(resolved_kind) {
        Ok(d) => Some(d),
        Err(e) => {
            tracing::warn!(error = %e, "no download staging dir; leaving profile download prefs");
            None
        }
    };
    let opts = LaunchOpts {
        headless,
        profile_dir: profile_dir.clone(),
        download_dir,
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

/// Selection used by [`ensure_started`] when no `--browser` kind is given.
///
/// Order, strongest first:
/// 1. the persisted default (`browser-control set default <kind|name>`),
/// 2. the kind of the most recent live registered browser,
/// 3. an installed kind that already has a live OS process (a browser the
///    user opened by hand, which browser-control never registered),
/// 4. the hardcoded chromium-first detection order.
///
/// Steps 1–3 only yield a kind that is actually installed, so a stale
/// default or registry row falls through instead of failing the start.
pub(crate) fn resolve_kind_when_unspecified(
    installed: &[Installed],
    registry: &Registry,
) -> Option<Kind> {
    if let Some(kind) = kind_from_default_setting(installed, registry) {
        return Some(kind);
    }
    if let Some(kind) = registry
        .most_recent_alive()
        .ok()
        .flatten()
        .map(|row| row.kind)
        .filter(|kind| is_installed(installed, *kind))
    {
        return Some(kind);
    }
    let running = detect::list_running_kinds(installed);
    installed
        .iter()
        .find(|i| running.contains(&i.kind))
        .map(|i| i.kind)
        .or_else(|| first_chromium_or_first(installed))
}

fn is_installed(installed: &[Installed], kind: Kind) -> bool {
    installed.iter().any(|i| i.kind == kind)
}

/// Kind named by the persisted `default` setting, if it points at an
/// installed browser. A name selector (`brave-jungle`) resolves through the
/// registry first, then through the generated-name prefix, so a default that
/// outlived its browser still selects the right kind.
fn kind_from_default_setting(installed: &[Installed], registry: &Registry) -> Option<Kind> {
    let value = crate::config::load().ok()?.default?;
    let kind = match crate::cli::env_resolver::parse(&value).ok()? {
        BrowserSelector::Kind(kind) => Some(kind),
        BrowserSelector::Name(name) => registry
            .get_by_name(&name)
            .ok()
            .flatten()
            .map(|row| row.kind)
            .or_else(|| crate::cli::mcp::kind_from_generated_name(&name)),
        // A URL or an executable path names an endpoint, not a startable
        // kind; those are handled by the MCP/CLI resolver, not here.
        BrowserSelector::Url(_) | BrowserSelector::ExecutablePath(_) => None,
    }?;
    is_installed(installed, kind).then_some(kind)
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
    use crate::config::{self, Config};

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
            std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        }
    }

    /// Point the config and registry at throwaway dirs: selection now reads
    /// the persisted default and the registry, and must not see the machine's.
    fn with_tmp_env<R>(f: impl FnOnce() -> R) -> R {
        let _lock = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data = tempfile::TempDir::new().unwrap();
        let cfg = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_DATA_DIR", data.path());
        std::env::set_var("BROWSER_CONTROL_CONFIG_DIR", cfg.path());
        let _guard = EnvGuard;
        f()
    }

    fn empty_registry() -> Registry {
        Registry::open_in_memory().unwrap()
    }

    /// A bound-but-unaccepted local port: `registry::liveness` only TCP
    /// connects, so this is enough to make a row read as alive.
    fn alive_listener() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn registered_row(name: &str, kind: Kind, port: u16, started_at: &str) -> BrowserRow {
        BrowserRow {
            name: name.to_string(),
            kind,
            engine: kind.engine(),
            // This process: alive, so the row counts as live.
            pid: std::process::id(),
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/{name}"),
            port,
            profile_dir: PathBuf::from("/tmp/profile"),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: started_at.to_string(),
        }
    }

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

        let resolved =
            with_tmp_env(|| resolve_kind_when_unspecified(&candidates, &empty_registry()));

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
        with_tmp_env(|| {
            let registry = empty_registry();
            assert_eq!(
                resolve_kind_when_unspecified(&candidates, &registry),
                first_chromium_or_first(&candidates)
            );
            assert_eq!(
                resolve_kind_when_unspecified(&candidates, &registry),
                Some(Kind::Chrome)
            );
        });
    }

    #[test]
    fn resolve_kind_when_unspecified_none_when_nothing_installed() {
        with_tmp_env(|| {
            assert_eq!(resolve_kind_when_unspecified(&[], &empty_registry()), None);
        });
    }

    #[test]
    fn persisted_default_kind_wins_over_another_running_browser() {
        let (mut child, exe) = spawn_stand_in_process();

        // Edge is "running" (its exe matches the live process) and is listed
        // first, but the user pinned `default = brave`. The pin wins: this is
        // the case where `browser_start` used to launch Edge next to an open
        // Brave.
        let candidates = vec![
            installed(Kind::Edge, exe),
            installed(Kind::Brave, PathBuf::from("/definitely/not/a/real/brave")),
        ];

        let resolved = with_tmp_env(|| {
            config::save(&Config {
                default: Some("brave".into()),
            })
            .unwrap();
            resolve_kind_when_unspecified(&candidates, &empty_registry())
        });

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(resolved, Some(Kind::Brave));
    }

    #[test]
    fn persisted_default_name_resolves_through_the_registry() {
        let candidates = vec![
            installed(Kind::Edge, PathBuf::from("/definitely/not/a/real/edge")),
            installed(Kind::Brave, PathBuf::from("/definitely/not/a/real/brave")),
        ];

        let resolved = with_tmp_env(|| {
            let registry = empty_registry();
            registry
                .insert(&registered_row(
                    "brave-jungle",
                    Kind::Brave,
                    1,
                    "2024-01-01T00:00:00Z",
                ))
                .unwrap();
            config::save(&Config {
                default: Some("brave-jungle".into()),
            })
            .unwrap();
            resolve_kind_when_unspecified(&candidates, &registry)
        });

        assert_eq!(resolved, Some(Kind::Brave));
    }

    #[test]
    fn default_naming_an_uninstalled_kind_falls_through() {
        let candidates = vec![installed(
            Kind::Edge,
            PathBuf::from("/definitely/not/a/real/edge"),
        )];

        let resolved = with_tmp_env(|| {
            config::save(&Config {
                default: Some("chrome".into()),
            })
            .unwrap();
            resolve_kind_when_unspecified(&candidates, &empty_registry())
        });

        assert_eq!(resolved, Some(Kind::Edge));
    }

    #[test]
    fn live_registered_browser_wins_over_another_running_kind() {
        let (mut child, exe) = spawn_stand_in_process();

        // No default pinned, Edge running unregistered, but browser-control
        // already has a live Brave it started: reuse that rather than
        // launching a second browser.
        let candidates = vec![
            installed(Kind::Edge, exe),
            installed(Kind::Brave, PathBuf::from("/definitely/not/a/real/brave")),
        ];

        let (_listener, port) = alive_listener();
        let resolved = with_tmp_env(|| {
            let registry = empty_registry();
            registry
                .insert(&registered_row(
                    "brave-jungle",
                    Kind::Brave,
                    port,
                    "2024-01-01T00:00:00Z",
                ))
                .unwrap();
            resolve_kind_when_unspecified(&candidates, &registry)
        });

        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(resolved, Some(Kind::Brave));
    }
}
