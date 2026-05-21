//! `browser-control daemon` subcommand: developer surface for the daemon.
//!
//! In production the daemon is auto-spawned by clients; this subcommand exists
//! so contributors and CI can drive lifecycle explicitly.

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::daemon::bringup;

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Print daemon status for a browser registry name.
    Status {
        browser: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove the daemon state file (does not signal a running daemon).
    Clear { browser: String },
    /// Run the daemon in the foreground (Phase 0: scaffolding only; exits
    /// immediately after writing a Ready state record).
    Run {
        browser: String,
        /// Override the endpoint path.
        #[arg(long)]
        endpoint: Option<std::path::PathBuf>,
    },
}

pub async fn run(cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Status { browser, json } => status(&browser, json),
        DaemonCmd::Clear { browser } => bringup::clear_state(&browser),
        DaemonCmd::Run { browser, endpoint } => run_foreground(&browser, endpoint).await,
    }
}

fn status(browser: &str, json: bool) -> Result<()> {
    let record = bringup::read_state(browser)?;
    if json {
        let value = match &record {
            Some(r) => serde_json::to_value(r)?,
            None => serde_json::Value::Null,
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    match record {
        Some(r) => {
            println!("daemon: {} ({:?})", r.browser_name, r.state);
            println!("  pid:      {}", r.pid);
            println!("  endpoint: {}", r.endpoint.display());
            println!("  version:  {}", r.daemon_version);
            println!("  schema:   v{}", r.schema_version);
            println!("  alive:    {}", bringup::pid_alive(r.pid));
        }
        None => println!("daemon for {browser}: not running"),
    }
    Ok(())
}

async fn run_foreground(
    browser: &str,
    endpoint_override: Option<std::path::PathBuf>,
) -> Result<()> {
    let _lock = bringup::acquire_bringup_lock(browser).context("acquire bringup lock")?;

    let endpoint = match endpoint_override {
        Some(p) => p,
        None => bringup::endpoint_path(browser)?,
    };
    let record = bringup::DaemonRecord {
        browser_name: browser.to_string(),
        pid: std::process::id(),
        state: bringup::DaemonState::Starting,
        endpoint: endpoint.clone(),
        started_at_epoch_s: bringup::now_epoch_s(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        schema_version: 1,
    };
    bringup::write_state(browser, &record)?;

    // Bind the transport (proves we can listen on this OS).
    let endpoint_obj = crate::daemon::Endpoint::new(&endpoint);
    let _listener = crate::daemon::listen(&endpoint_obj)
        .await
        .with_context(|| format!("bind {}", endpoint.display()))?;

    let ready = bringup::DaemonRecord {
        state: bringup::DaemonState::Ready,
        ..record
    };
    bringup::write_state(browser, &ready)?;

    eprintln!(
        "daemon scaffolding ready for {browser} at {}",
        endpoint.display()
    );
    eprintln!("(Phase 0: RPC handlers not implemented; exiting)");

    // Graceful: mark stopping + clear state on exit.
    let stopping = bringup::DaemonRecord {
        state: bringup::DaemonState::Stopping,
        ..ready
    };
    bringup::write_state(browser, &stopping)?;
    bringup::clear_state(browser)?;
    #[cfg(unix)]
    crate::daemon::transport::unix::unlink(&endpoint);
    Ok(())
}
