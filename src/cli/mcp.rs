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
