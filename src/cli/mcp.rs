//! `browser-control mcp` subcommand entry point.

use anyhow::{anyhow, Context, Result};

use crate::cli::env_resolver::{self, BrowserSelector, ResolvedBrowser};
use crate::detect::Kind;
use crate::mcp::server::{run, ServerState, ToolRegistry};
use crate::registry::Registry;

/// Entry point for `browser-control mcp`.
pub async fn run_cli(
    browser_arg: Option<String>,
    playwright_version: Option<String>,
) -> Result<()> {
    let resolved = resolve_browser(browser_arg).await?;
    let sidecar_config = crate::sidecar::SidecarConfig {
        version: playwright_version,
    };
    let state = ServerState::with_sidecar_config(resolved, sidecar_config);
    let tools = ToolRegistry::new();
    crate::mcp::tools::register_all(&tools);
    run(state, tools).await
}

/// Resolution order: positional arg / `BROWSER_CONTROL` env (arg wins, env is
/// the fallback — both are merged by clap into `browser_arg`) > persisted
/// default (`browser-control set default ...`) > error.
///
/// We deliberately do NOT fall back to a "most recently alive" registry row:
/// that hides which browser is being controlled and depends on global state
/// that other processes can mutate, producing surprising results for agents
/// that share a host.
pub async fn resolve_browser(browser_arg: Option<String>) -> Result<ResolvedBrowser> {
    let registry = Registry::open()?;
    if let Some(arg) = browser_arg.as_deref().filter(|s| !s.is_empty()) {
        let sel = env_resolver::parse(arg)?;
        return env_resolver::resolve(sel, &registry).await;
    }
    if let Some(value) = crate::config::load()?.default {
        return resolve_default_browser(&value, &registry).await;
    }
    Err(anyhow!(
        "no browser selected: pass a browser argument, set BROWSER_CONTROL, or run `browser-control set default <value>`"
    ))
}

async fn resolve_default_browser(value: &str, registry: &Registry) -> Result<ResolvedBrowser> {
    let sel = env_resolver::parse(value)?;
    if let BrowserSelector::Name(name) = &sel {
        if let Some(kind) = stale_default_kind(name, registry)? {
            rewrite_default_if_unchanged(value, kind)?;
            return env_resolver::resolve(BrowserSelector::Kind(kind), registry)
                .await
                .with_context(|| {
                    format!(
                        "default browser `{name}` is stale; rewrote default to `{}` but failed to resolve a live {} browser",
                        kind.as_str(),
                        kind.as_str()
                    )
                });
        }
    }
    env_resolver::resolve(sel, registry).await
}

fn stale_default_kind(name: &str, registry: &Registry) -> Result<Option<Kind>> {
    if let Some(row) = registry
        .get_by_name(name)
        .with_context(|| format!("looking up default browser {name}"))?
    {
        return match crate::registry::liveness(&row) {
            crate::registry::BrowserLiveness::DeadPid => {
                registry
                    .delete(&row.name)
                    .with_context(|| format!("pruning stale default browser {}", row.name))?;
                Ok(Some(row.kind))
            }
            crate::registry::BrowserLiveness::Alive
            | crate::registry::BrowserLiveness::EndpointUnreachable => Ok(None),
        };
    }

    Ok(kind_from_generated_name(name))
}

fn kind_from_generated_name(name: &str) -> Option<Kind> {
    let (prefix, _) = name.split_once('-')?;
    Kind::parse(prefix)
}

fn rewrite_default_if_unchanged(previous: &str, kind: Kind) -> Result<()> {
    let mut cfg = crate::config::load()?;
    if cfg.default.as_deref() == Some(previous) {
        cfg.default = Some(kind.as_str().to_string());
        crate::config::save(&cfg)
            .with_context(|| format!("rewriting stale default browser `{previous}`"))?;
    }
    Ok(())
}

/// Acquire the Firefox BiDi single-session lock if `resolved` is a
/// registered browser on the BiDi engine. Returns `None` for external
/// URL endpoints (the lock is per-registered-name) and for CDP engines.
/// Wait bound: 30 s; on timeout, returns the typed `BidiLockBusy` error.
pub fn acquire_bidi_lock_if_needed(
    registry: &crate::registry::Registry,
    resolved: &ResolvedBrowser,
) -> Result<Option<crate::registry::BidiLockGuard>> {
    use crate::detect::Engine;
    if resolved.engine != Engine::Bidi {
        return Ok(None);
    }
    let name = match &resolved.source {
        crate::cli::env_resolver::Source::Registered { name } => name.clone(),
        crate::cli::env_resolver::Source::External => return Ok(None),
    };
    let guard = registry.bidi_lock_acquire(&name, std::time::Duration::from_secs(30))?;
    Ok(Some(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{self, Config};
    use crate::detect::{Engine, Kind};
    use crate::registry::{BrowserRow, Registry};
    use std::path::PathBuf;

    struct EnvGuard;

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
            std::env::remove_var("BROWSER_CONTROL_CONFIG_DIR");
        }
    }

    struct AliveListener {
        _listener: std::net::TcpListener,
        port: u16,
    }

    fn alive_listener() -> AliveListener {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        AliveListener {
            _listener: listener,
            port,
        }
    }

    fn row(name: &str, kind: Kind, port: u16, pid: u32, started_at: &str) -> BrowserRow {
        BrowserRow {
            name: name.to_string(),
            kind,
            engine: kind.engine(),
            pid,
            endpoint: format!("ws://127.0.0.1:{port}/devtools/browser/{name}"),
            port,
            profile_dir: PathBuf::from(format!("/tmp/profiles/{name}")),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: started_at.to_string(),
        }
    }

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

    #[test]
    fn stale_named_default_rewrites_to_kind_and_resolves_live_same_kind() {
        with_tmp_env(|| {
            let live = alive_listener();
            let reg = Registry::open().unwrap();
            reg.insert(&row(
                "brave-cosmos",
                Kind::Brave,
                9,
                99_999_999,
                "2024-01-01T00:00:00Z",
            ))
            .unwrap();
            let live_row = row(
                "brave-spruce",
                Kind::Brave,
                live.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            );
            reg.insert(&live_row).unwrap();
            config::save(&Config {
                default: Some("brave-cosmos".into()),
            })
            .unwrap();
            drop(reg);

            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_browser(None)).unwrap();
            assert_eq!(got.endpoint, live_row.endpoint);
            assert_eq!(got.engine, Engine::Cdp);
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "brave-spruce".into()
                }
            );
            assert_eq!(config::load().unwrap().default.as_deref(), Some("brave"));
            let reg = Registry::open().unwrap();
            assert!(reg.get_by_name("brave-cosmos").unwrap().is_none());
        });
    }

    #[test]
    fn missing_generated_default_rewrites_to_kind_and_resolves_live_same_kind() {
        with_tmp_env(|| {
            let live = alive_listener();
            let reg = Registry::open().unwrap();
            let live_row = row(
                "brave-spruce",
                Kind::Brave,
                live.port,
                std::process::id(),
                "2024-01-02T00:00:00Z",
            );
            reg.insert(&live_row).unwrap();
            config::save(&Config {
                default: Some("brave-cosmos".into()),
            })
            .unwrap();
            drop(reg);

            let rt = tokio::runtime::Runtime::new().unwrap();
            let got = rt.block_on(resolve_browser(None)).unwrap();
            assert_eq!(got.endpoint, live_row.endpoint);
            assert_eq!(
                got.source,
                env_resolver::Source::Registered {
                    name: "brave-spruce".into()
                }
            );
            assert_eq!(config::load().unwrap().default.as_deref(), Some("brave"));
        });
    }
}
