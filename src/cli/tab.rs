//! `browser-control tab` subcommand: named-tab lifecycle through the daemon.
//!
//! The agent contract is two verbs:
//! - `tab open <browser> [url] [--name <name>]` — get-or-create a named tab.
//! - `tab list <browser>` — snapshot of every tab the daemon knows about.
//!
//! There is no `tab close` or `tab release` — the daemon GCs daemon-created
//! tabs via idle sweep and budget pressure. Agents carry only the name
//! across CLI invocations; rerunning `tab open --name X` is safe and
//! idempotent.

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::daemon::{connect_browser, TabSummary};

#[derive(Subcommand, Debug)]
pub enum TabCmd {
    /// Get-or-create a named tab. Returns one-line "name<TAB>state<TAB>url"
    /// on stdout (machine-friendly with `--json`).
    Open {
        /// Browser registry name (e.g. `brave-twilight`).
        browser: String,
        /// Optional initial URL. Defaults to `about:blank`.
        #[arg(default_value = "")]
        url: String,
        /// Stable name for the tab. If omitted, the daemon assigns one.
        #[arg(long)]
        name: Option<String>,
        /// Emit JSON instead of one-line text.
        #[arg(long)]
        json: bool,
    },
    /// List every tab the daemon currently tracks.
    List {
        browser: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: TabCmd) -> Result<()> {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            match cmd {
                TabCmd::Open {
                    browser,
                    url,
                    name,
                    json,
                } => open(&browser, &url, name.as_deref(), json).await,
                TabCmd::List { browser, json } => list(&browser, json).await,
            }
        })
        .await
}

async fn open(browser: &str, url: &str, name: Option<&str>, json: bool) -> Result<()> {
    let conn = connect_browser(browser)
        .await
        .with_context(|| format!("connecting to daemon for {browser}"))?;
    let url_opt = if url.is_empty() { None } else { Some(url) };
    let row = conn.tab_open(name, url_opt).await?;
    print_summary(&row, json);
    Ok(())
}

async fn list(browser: &str, json: bool) -> Result<()> {
    let conn = connect_browser(browser).await?;
    let rows = conn.tab_list().await?;
    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "target_id": r.target_id,
                    "url": r.url,
                    "state": r.state,
                    "daemon_created": r.daemon_created,
                    "idle_ms": r.idle_ms,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if rows.is_empty() {
        println!("(no tabs)");
    } else {
        println!("NAME\tSTATE\tIDLE_MS\tURL");
        for r in &rows {
            println!("{}\t{}\t{}\t{}", r.name, r.state, r.idle_ms, r.url);
        }
    }
    Ok(())
}

fn print_summary(row: &TabSummary, json: bool) {
    if json {
        let v = serde_json::json!({
            "name": row.name,
            "target_id": row.target_id,
            "url": row.url,
            "state": row.state,
            "daemon_created": row.daemon_created,
            "idle_ms": row.idle_ms,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("{}\t{}\t{}", row.name, row.state, row.url);
    }
}
