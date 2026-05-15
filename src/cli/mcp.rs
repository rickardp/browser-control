//! `browser-control mcp` subcommand entry point.

use anyhow::{anyhow, Result};

use crate::cli::env_resolver::{self, ResolvedBrowser};
use crate::mcp::server::{run, ServerState, ToolRegistry};
use crate::registry::Registry;

/// Entry point for `browser-control mcp`.
pub async fn run_cli(browser_arg: Option<String>, playwright: bool) -> Result<()> {
    if playwright {
        let resolved = resolve_browser(browser_arg).await?;
        let code = crate::mcp::playwright::run(&resolved).await?;
        std::process::exit(code);
    }
    let resolved = resolve_browser(browser_arg).await?;
    let state = ServerState::new(resolved);
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
