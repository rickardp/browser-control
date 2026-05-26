//! `browser-control mcp` subcommand entry point.

use anyhow::{anyhow, Result};

use crate::cli::env_resolver::{self, ResolvedBrowser};
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
        let sel = env_resolver::parse(&value)?;
        return env_resolver::resolve(sel, &registry).await;
    }
    Err(anyhow!(
        "no browser selected: pass a browser argument, set BROWSER_CONTROL, or run `browser-control set default <value>`"
    ))
}

/// Resolved positional with optional tab name.
///
/// Returned by [`resolve_target`] so every command that takes a
/// `<browser>` positional can opt into the unified `<browser>/<tab>` path
/// syntax. The `tab` field is `Some` if the user wrote `<browser>/<tab>`,
/// `None` if they wrote a bare `<browser>`.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub resolved: ResolvedBrowser,
    pub tab: Option<String>,
}

impl ResolvedTarget {
    /// The registered browser name suitable for keying SQLite tables
    /// (`scratches`, `tabs`, `bidi_locks`). `None` for external URL targets
    /// where there is no registry row.
    pub fn registered_name(&self) -> Option<&str> {
        match &self.resolved.source {
            crate::cli::env_resolver::Source::Registered { name } => Some(name.as_str()),
            crate::cli::env_resolver::Source::External => None,
        }
    }
}

/// Resolve a `<browser>[/<tab>]` positional. Same defaulting rules as
/// [`resolve_browser`] for the browser part.
///
/// When a tab is present, the browser part MUST resolve to a registered
/// browser (so the tab row has a stable key); otherwise we error early
/// with a clear message — external URL endpoints can't carry named tabs.
pub async fn resolve_target(browser_arg: Option<String>) -> Result<ResolvedTarget> {
    let registry = Registry::open()?;
    let raw = match browser_arg.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => match crate::config::load()?.default {
            Some(v) => v,
            None => {
                return Err(anyhow!(
                    "no browser selected: pass a browser argument, set BROWSER_CONTROL, or run `browser-control set default <value>`"
                ));
            }
        },
    };
    let target = env_resolver::parse_target(&raw)?;
    let resolved = env_resolver::resolve(target.browser, &registry).await?;
    if target.tab.is_some() {
        match resolved.source {
            crate::cli::env_resolver::Source::Registered { .. } => {}
            _ => {
                return Err(anyhow!(
                    "named tabs (`<browser>/<name>`) require a registered browser; \
                     external URL endpoints cannot carry tab names"
                ));
            }
        }
    }
    Ok(ResolvedTarget {
        resolved,
        tab: target.tab,
    })
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
