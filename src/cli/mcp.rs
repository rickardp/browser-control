//! `browser-control mcp` subcommand entry point.

use anyhow::{anyhow, Result};

use crate::cli::env_resolver::{self, ResolvedBrowser, Source};
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

/// Resolution order: `BROWSER_CONTROL` env > positional arg > most-recent-alive > error.
pub async fn resolve_browser(browser_arg: Option<String>) -> Result<ResolvedBrowser> {
    let registry = Registry::open()?;
    if let Ok(val) = std::env::var("BROWSER_CONTROL") {
        if !val.is_empty() {
            let sel = env_resolver::parse(&val)?;
            return env_resolver::resolve(sel, &registry).await;
        }
    }
    if let Some(arg) = browser_arg.as_deref() {
        let sel = env_resolver::parse(arg)?;
        return env_resolver::resolve(sel, &registry).await;
    }
    if let Some(row) = registry.most_recent_alive()? {
        return Ok(ResolvedBrowser {
            endpoint: row.endpoint,
            engine: row.engine,
            source: Source::Registered { name: row.name },
        });
    }
    Err(anyhow!(
        "no browser selected: set BROWSER_CONTROL, pass a browser argument, or run `browser-control start`"
    ))
}
