//! `browser-control tab` subcommand: named-tab lifecycle backed by the
//! SQLite `tabs` table. Commands: `tab open`, `tab list`, `tab adopt`.
//!
//! - Close is implicit (sweep-on-read evicts stale rows; LRU recycles
//!   under budget pressure). Agents don't know when they're done.
//! - Navigate is folded into `tab open <browser>/<name> <url>`, which
//!   navigates the existing tab if `url` differs from `last_url`.
//! - `tab adopt` binds an unnamed live tab (discovered via `tab list --all`)
//!   to a name so it becomes addressable in page-context commands.

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use serde_json::json;

use crate::cli::env_resolver;
use crate::cli::mcp::{acquire_bidi_lock_if_needed, resolve_browser};
use crate::cli::trace::CommandTrace;
use crate::registry::{Registry, TabRow};
use crate::session::backend::open_backend;
use crate::session::tabs as session_tabs;

#[derive(Subcommand, Debug)]
pub enum TabCmd {
    /// Get-or-create a named tab. Idempotent: re-running with the same
    /// `<browser>/<name>` returns the existing tab (navigating if `url`
    /// differs from `last_url`).
    Open {
        /// `<browser>` or `<browser>/<name>`. With no `/<name>`, the
        /// daemon assigns a cute name (`tab-<word>`) and creates fresh.
        browser: String,
        /// URL to open or navigate to. Passing a url for an existing named
        /// tab navigates it (when the url differs from its `last_url`); this
        /// is the way to navigate — do not eval `location.href`. Omit to
        /// create a fresh tab at `about:blank`.
        #[arg(default_value = "")]
        url: String,
        /// Emit JSON instead of the one-line text summary.
        #[arg(long)]
        json: bool,
    },
    /// List tabs for `<browser>`.
    ///
    /// By default returns rows in the named-tab registry (those agents
    /// opened explicitly via `tab open`). With `--all`, returns every
    /// live top-level tab in the browser — registered rows merged with
    /// unnamed user tabs (name column empty for the unnamed ones).
    List {
        browser: String,
        /// Include every live tab in the browser, not just registered names.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Adopt an existing live tab by target ID, binding it to a name.
    ///
    /// Use `tab list --all` to discover unnamed tabs and their target IDs,
    /// then `tab adopt <browser>/<name> <target-id>` to make them
    /// addressable via `--browser <browser>/<name>` in page-context commands.
    Adopt {
        /// `<browser>/<name>` — the browser and name to assign.
        browser: String,
        /// The target ID from `tab list --all`.
        target_id: String,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(cmd: TabCmd) -> Result<()> {
    match cmd {
        TabCmd::Open { browser, url, json } => {
            let mut trace = CommandTrace::new("tab-open");
            let result = open(&browser, &url, json, &mut trace).await;
            trace.finish(result)
        }
        TabCmd::List { browser, all, json } => {
            let mut trace = CommandTrace::new("tab-list");
            let result = list(&browser, all, json, &mut trace).await;
            trace.finish(result)
        }
        TabCmd::Adopt {
            browser,
            target_id,
            json,
        } => {
            let mut trace = CommandTrace::new("tab-adopt");
            let result = adopt(&browser, &target_id, json, &mut trace).await;
            trace.finish(result)
        }
    }
}

async fn open(positional: &str, url: &str, json: bool, trace: &mut CommandTrace) -> Result<()> {
    let target = env_resolver::parse_target(positional)
        .with_context(|| format!("parsing `{positional}` as <browser>[/<tab>]"))?;
    let name = target.tab.as_deref();
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

    trace.browser(&browser_name).engine(resolved.engine);
    if let Some(n) = name {
        trace.tab_name(n);
    }
    trace.route("tab-open");

    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let row = session_tabs::tab_open(&backend, &registry, &browser_name, name, url_opt).await;
    backend.shutdown().await;
    let row = row?;
    trace.target_id(&row.target_id);
    if name.is_none() {
        // Capture the daemon-assigned name in the trace too.
        trace.tab_name(&row.name);
    }
    print_summary(&row, json);
    Ok(())
}

async fn list(positional: &str, all: bool, json: bool, trace: &mut CommandTrace) -> Result<()> {
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
    trace.browser(&browser_name).engine(resolved.engine);
    trace.route(if all { "tab-list-all" } else { "tab-list" });
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let merged = async {
        let rows = session_tabs::tab_list(&backend, &registry, &browser_name).await?;
        // With `--all`, fold in every live tab the browser knows about with
        // an empty `name` column. Registered ids stay as-is; unregistered
        // ones get synthesized rows. Sorted: named rows first (alpha), then
        // unnamed (by url).
        let merged: Vec<DisplayRow> = if all {
            let live = backend.live_targets().await?;
            let known_ids: std::collections::HashSet<&str> =
                rows.iter().map(|r| r.target_id.as_str()).collect();
            let mut out: Vec<DisplayRow> = rows.iter().map(DisplayRow::from_row).collect();
            for t in &live {
                if !known_ids.contains(t.id.as_str()) {
                    out.push(DisplayRow::from_live(t));
                }
            }
            out
        } else {
            rows.iter().map(DisplayRow::from_row).collect()
        };
        Ok::<_, anyhow::Error>(merged)
    }
    .await;
    backend.shutdown().await;
    let merged = merged?;

    if json {
        let arr: Vec<serde_json::Value> = merged.iter().map(DisplayRow::to_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if merged.is_empty() {
        println!("(no tabs)");
    } else {
        println!("NAME\tOWNER\tIDLE_S\tURL");
        let now = crate::registry::now_epoch_s();
        for r in &merged {
            let idle = r
                .last_used_at_epoch_s
                .map(|t| (now - t).max(0).to_string())
                .unwrap_or_else(|| "-".to_string());
            println!("{}\t{}\t{}\t{}", r.name, r.owner, idle, r.url);
        }
    }
    Ok(())
}

async fn adopt(
    positional: &str,
    target_id: &str,
    json: bool,
    trace: &mut CommandTrace,
) -> Result<()> {
    let target = env_resolver::parse_target(positional)
        .with_context(|| format!("parsing `{positional}` as <browser>/<tab>"))?;
    let name = target
        .tab
        .as_deref()
        .ok_or_else(|| anyhow!("`tab adopt` requires `<browser>/<name>`, got `{positional}`"))?;

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

    trace
        .browser(&browser_name)
        .engine(resolved.engine)
        .tab_name(name)
        .route("tab-adopt");

    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let looked_up = async {
        // Verify the target ID actually exists in the browser.
        let live_ids = backend.live_target_ids().await?;
        if !live_ids.contains(target_id) {
            return Err(anyhow!(
                "target ID `{target_id}` not found among live tabs. \
                 Use `tab list --all` to see available target IDs."
            ));
        }
        // Get the URL of the live tab for the registry row.
        let live_targets = backend.live_targets().await?;
        Ok::<_, anyhow::Error>(
            live_targets
                .iter()
                .find(|t| t.id == target_id)
                .map(|t| t.url.clone())
                .unwrap_or_else(|| "about:blank".to_string()),
        )
    }
    .await;
    backend.shutdown().await;
    let url = looked_up?;
    let url = url.as_str();

    // daemon_created = false because this is a user-adopted tab.
    registry.tab_upsert(&browser_name, name, target_id, url, false)?;
    let row = registry
        .tab_get(&browser_name, name)?
        .ok_or_else(|| anyhow!("tab row missing immediately after upsert"))?;
    trace.target_id(&row.target_id);
    print_summary(&row, json);
    Ok(())
}

/// Unified row used by `tab list` output. Comes either from a registered
/// `TabRow` (name + last_used_at populated, owner = "agent"/"user") or a
/// live `LiveTarget` that has no row yet (name empty, idle "-",
/// owner = "unnamed").
struct DisplayRow {
    name: String,
    owner: &'static str,
    url: String,
    last_used_at_epoch_s: Option<i64>,
    target_id: String,
    daemon_created: bool,
}

impl DisplayRow {
    fn from_row(r: &TabRow) -> Self {
        Self {
            name: r.name.clone(),
            owner: if r.daemon_created { "agent" } else { "user" },
            url: r.last_url.clone(),
            last_used_at_epoch_s: Some(r.last_used_at_epoch_s),
            target_id: r.target_id.clone(),
            daemon_created: r.daemon_created,
        }
    }
    fn from_live(t: &crate::session::backend::LiveTarget) -> Self {
        Self {
            name: String::new(),
            owner: "unnamed",
            url: t.url.clone(),
            last_used_at_epoch_s: None,
            target_id: t.id.clone(),
            daemon_created: false,
        }
    }
    fn to_json(&self) -> serde_json::Value {
        json!({
            "name": self.name,
            "owner": self.owner,
            "target_id": self.target_id,
            "url": self.url,
            "last_used_at_epoch_s": self.last_used_at_epoch_s,
            "daemon_created": self.daemon_created,
        })
    }
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
