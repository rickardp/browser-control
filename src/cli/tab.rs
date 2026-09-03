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
    /// Make a tab behave as the focused, visible foreground tab even while
    /// the window is minimized or the display is locked (games, canvas apps,
    /// anything that pauses in the background). A small holder process keeps
    /// it on until `off`, the timeout elapses, the tab closes, or the browser
    /// exits. Chromium only.
    ///
    ///   browser-control tab foreground brave/game on --timeout 2h
    ///   browser-control tab foreground brave/game off
    ///   browser-control tab foreground brave off        # every tab
    Foreground {
        /// `<browser>/<name>`, or a bare `<browser>` with `off` to stop all.
        browser: String,
        /// `on` (default) or `off`.
        #[arg(default_value = "on")]
        state: String,
        /// How long to hold it (`30m`, `2h`, `90s`). Default 1h.
        #[arg(long, default_value = "1h")]
        timeout: String,
        #[arg(long)]
        json: bool,
    },
    /// Internal: the holder process spawned by `tab foreground`.
    #[command(hide = true)]
    ForegroundHold {
        browser_name: String,
        target_id: String,
        #[arg(long, default_value_t = 3600)]
        timeout_s: u64,
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
        TabCmd::Foreground {
            browser,
            state,
            timeout,
            json,
        } => {
            let mut trace = CommandTrace::new("tab-foreground");
            let result = foreground(&browser, &state, &timeout, json, &mut trace).await;
            trace.finish(result)
        }
        TabCmd::ForegroundHold {
            browser_name,
            target_id,
            timeout_s,
        } => {
            crate::session::foreground::hold(
                &browser_name,
                &target_id,
                std::time::Duration::from_secs(timeout_s),
            )
            .await
        }
    }
}

async fn foreground(
    positional: &str,
    state: &str,
    timeout: &str,
    json: bool,
    trace: &mut CommandTrace,
) -> Result<()> {
    use crate::session::foreground as fg;
    let enabled = match state.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => return Err(anyhow!("state must be `on` or `off`, got `{other}`")),
    };
    let timeout = crate::session::freshness::parse_max_age(timeout)
        .with_context(|| format!("parsing --timeout `{timeout}`"))?;
    let target = env_resolver::parse_target(positional)
        .with_context(|| format!("parsing `{positional}` as <browser>[/<tab>]"))?;
    let registry = Registry::open()?;
    let resolved = resolve_browser(Some(reassemble_browser_only(positional)?)).await?;
    let browser_name = match &resolved.source {
        crate::cli::env_resolver::Source::Registered { name } => name.clone(),
        _ => {
            return Err(anyhow!(
                "foreground emulation requires a registered browser; `{positional}` resolved to an external endpoint"
            ));
        }
    };
    trace.browser(&browser_name).engine(resolved.engine);
    trace.route("tab-foreground");
    if resolved.engine != crate::detect::Engine::Cdp {
        return Err(anyhow!(
            "foreground emulation is Chromium-only: Firefox has no WebDriver BiDi equivalent"
        ));
    }
    let Some(name) = target.tab.as_deref() else {
        if enabled {
            return Err(anyhow!(
                "`tab foreground <browser> on` needs a tab: use `<browser>/<tab>`; a bare browser is only valid with `off` (stop all)"
            ));
        }
        let n = fg::stop_all(&registry, &browser_name)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"browser": browser_name, "foreground": false, "stopped": n})
                )?
            );
        } else {
            println!("foreground off for {n} tab(s) on {browser_name}");
        }
        return Ok(());
    };
    trace.tab_name(name);
    let _bidi_lock = acquire_bidi_lock_if_needed(&registry, &resolved)?;
    let backend = open_backend(&resolved.endpoint, resolved.engine).await?;
    let row = crate::session::resolve_tab(&backend, &registry, &browser_name, name).await;
    backend.shutdown().await;
    let row = row?.ok_or_else(|| crate::errors::SessionError::TabNotFound {
        browser: browser_name.clone(),
        name: name.to_string(),
    })?;
    trace.target_id(&row.target_id);
    let (pid, changed) = if enabled {
        let (pid, created) = fg::spawn_holder(&registry, &browser_name, &row.target_id, timeout)?;
        (Some(pid), created)
    } else {
        (
            None,
            fg::stop_holder(&registry, &browser_name, &row.target_id)?,
        )
    };
    // The holder that is actually running may predate this call with its
    // own expiry; report that rather than the requested timeout.
    let expires_in = fg::status(&registry, &browser_name, &row.target_id)?
        .map(|r| (r.expires_at_epoch_s - crate::registry::now_epoch_s()).max(0) as u64)
        .map(std::time::Duration::from_secs);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "browser": browser_name,
                "name": name,
                "target_id": row.target_id,
                "foreground": enabled,
                "changed": changed,
                "holder_pid": pid,
                "expires_in_s": expires_in.map(|d| d.as_secs()),
            }))?
        );
    } else if enabled {
        println!(
            "foreground {} for {browser_name}/{name} (holder pid {}, expires in {})",
            if changed { "on" } else { "already on" },
            pid.unwrap_or(0),
            crate::session::freshness::format_duration(expires_in.unwrap_or(timeout))
        );
    } else {
        println!(
            "foreground {} for {browser_name}/{name}",
            if changed { "off" } else { "already off" }
        );
    }
    Ok(())
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
    let foreground = crate::session::foreground::active_targets(&registry, &browser_name)?;
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
    let mut merged = merged?;
    for r in &mut merged {
        r.foreground = foreground.contains(&r.target_id);
    }

    if json {
        let arr: Vec<serde_json::Value> = merged.iter().map(DisplayRow::to_json).collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else if merged.is_empty() {
        println!("(no tabs)");
    } else {
        println!("NAME\tOWNER\tIDLE_S\tFOREGROUND\tURL");
        let now = crate::registry::now_epoch_s();
        for r in &merged {
            let idle = r
                .last_used_at_epoch_s
                .map(|t| (now - t).max(0).to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{}\t{}\t{}\t{}\t{}",
                r.name,
                r.owner,
                idle,
                if r.foreground { "on" } else { "-" },
                r.url
            );
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
    foreground: bool,
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
            foreground: false,
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
            foreground: false,
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
            "foreground": self.foreground,
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
