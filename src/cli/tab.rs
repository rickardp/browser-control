//! `browser-control tab` subcommand: named-tab lifecycle backed by the
//! SQLite `tabs` table. The agent surface is intentionally minimal —
//! `tab open` and `tab list`. There is no `tab close` or `tab navigate`
//! per ADR-002 follow-up scope:
//!
//! - Close is implicit (sweep-on-read evicts stale rows; LRU recycles
//!   under budget pressure). Agents don't know when they're done.
//! - Navigate is folded into `tab open <browser>/<name> <url>`, which
//!   navigates the existing tab if `url` differs from `last_url`.

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::cli::env_resolver;
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::registry::{Registry, TabRow};
use crate::session::backend::open_backend;
use crate::session::tabs as session_tabs;

#[derive(Subcommand, Debug)]
pub enum TabCmd {
    /// Get-or-create a named tab. Idempotent: re-running with the same
    /// `<browser>/<name>` returns the existing tab (navigating if `url`
    /// differs from `last_url`).
    Open {
        /// `<browser>` or `<browser>/<name>`. With no name, the daemon
        /// assigns a cute name (`tab-<word>`) and creates fresh.
        browser: String,
        /// Optional initial URL. Defaults to `about:blank`.
        #[arg(default_value = "")]
        url: String,
        /// Stable name override (when the positional doesn't include `/<name>`).
        /// If both forms are supplied, the positional wins.
        #[arg(long)]
        name: Option<String>,
        /// Emit JSON instead of the one-line text summary.
        #[arg(long)]
        json: bool,
    },
    /// List every named tab the registry knows for `<browser>`. Sweeps
    /// rows whose `target_id` is no longer in the live browser.
    List {
        browser: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: TabCmd) -> Result<()> {
    match cmd {
        TabCmd::Open {
            browser,
            url,
            name,
            json,
        } => open(&browser, &url, name.as_deref(), json).await,
        TabCmd::List { browser, json } => list(&browser, json).await,
    }
}

async fn open(positional: &str, url: &str, fallback_name: Option<&str>, json: bool) -> Result<()> {
    let target = env_resolver::parse_target(positional)
        .with_context(|| format!("parsing `{positional}` as <browser>[/<tab>]"))?;
    let name = match (target.tab.as_deref(), fallback_name) {
        (Some(p), _) => Some(p), // positional wins
        (None, Some(f)) => {
            // Validate even when supplied via flag, so error shape matches
            // path-style usage.
            env_resolver::validate_tab_name(f)?;
            Some(f)
        }
        (None, None) => None,
    };
    let url_opt = if url.is_empty() { None } else { Some(url) };

    let registry = Registry::open()?;
    let resolved = resolve_browser(Some(reassemble_browser_only(positional)?)).await?;
    let browser_name = match &resolved.source {
        crate::cli::env_resolver::Source::Registered { name } => name.clone(),
        _ => {
            return Err(anyhow!(
                "named tabs require a registered browser; `{positional}` resolved to an external endpoint"
            ));
        }
    };

    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let row = session_tabs::tab_open(&backend, &registry, &browser_name, name, url_opt).await?;
    print_summary(&row, json);
    Ok(())
}

async fn list(positional: &str, json: bool) -> Result<()> {
    // `tab list` only accepts a bare browser; tabs in the positional don't
    // make sense here.
    let target = env_resolver::parse_target(positional)?;
    if target.tab.is_some() {
        return Err(anyhow!(
            "`tab list` takes a bare `<browser>`, not `<browser>/<tab>`"
        ));
    }
    let registry = Registry::open()?;
    let resolved = resolve_browser(Some(positional.to_string())).await?;
    let browser_name = match &resolved.source {
        crate::cli::env_resolver::Source::Registered { name } => name.clone(),
        _ => {
            return Err(anyhow!(
                "tab list requires a registered browser; `{positional}` resolved to an external endpoint"
            ));
        }
    };
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let rows = session_tabs::tab_list(&backend, &registry, &browser_name).await?;

    if json {
        let arr: Vec<_> = rows.iter().map(tab_to_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!("(no tabs)");
    } else {
        println!("NAME\tSTATE\tIDLE_S\tURL");
        let now = crate::registry::now_epoch_s();
        for r in &rows {
            let idle = (now - r.last_used_at_epoch_s).max(0);
            let provenance = if r.daemon_created { "agent" } else { "user" };
            println!("{}\t{}\t{}\t{}", r.name, provenance, idle, r.last_url);
        }
    }
    Ok(())
}

fn tab_to_json(r: &TabRow) -> serde_json::Value {
    json!({
        "name": r.name,
        "target_id": r.target_id,
        "url": r.last_url,
        "last_used_at_epoch_s": r.last_used_at_epoch_s,
        "daemon_created": r.daemon_created,
    })
}

fn print_summary(row: &TabRow, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&tab_to_json(row)).unwrap()
        );
    } else {
        // One-line: NAME<TAB>URL — the minimum the agent needs to capture.
        println!("{}\t{}", row.name, row.last_url);
    }
}

/// Strip the optional `/<tab>` suffix so the result is parseable as a
/// bare browser positional — i.e. `brave-twilight/scrape` → `brave-twilight`.
/// Returns the input verbatim if there's no slash (after position 0).
fn reassemble_browser_only(positional: &str) -> Result<String> {
    if let Some(idx) = positional.find('/') {
        if idx > 0 {
            return Ok(positional[..idx].to_string());
        }
    }
    Ok(positional.to_string())
}
