//! `start` subcommand: launch a browser and register it.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::path::PathBuf;

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
    profile: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let installed = detect::list_installed();
    if installed.is_empty() {
        anyhow::bail!("no supported browsers installed; run `browser-control list-installed`");
    }

    let resolved_kind: Kind = match browser.as_deref() {
        None => first_chromium_or_first(&installed)
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

    let registry = Registry::open()?;
    if let Some(row) = registry.first_alive_by_kind(resolved_kind)? {
        let res = to_result(&row, true);
        emit(&res, json)?;
        return Ok(());
    }

    let name = registry::naming::generate_default(resolved_kind, &registry)?;
    let profile_dir = match profile {
        Some(p) => p,
        None => paths::default_profile_dir(resolved_kind)?,
    };
    std::fs::create_dir_all(&profile_dir).context("creating profile directory")?;
    let opts = LaunchOpts {
        headless,
        profile_dir: profile_dir.clone(),
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

    let res = to_result(&row, false);
    emit(&res, json)?;
    Ok(())
}

fn first_chromium_or_first(installed: &[Installed]) -> Option<Kind> {
    installed
        .iter()
        .find(|i| i.kind.is_chromium())
        .map(|i| i.kind)
        .or_else(|| installed.first().map(|i| i.kind))
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
