//! Chromium-family browser launcher (Chrome, Edge, Chromium, Brave).

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{allocate_free_port, wait_for_endpoint, LaunchOpts, LaunchedHandle};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating CDP port")?;

    if let Some(parent) = opts.profile_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut cmd = Command::new(&installed.executable);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", opts.profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-component-update");
    if opts.headless {
        cmd.arg("--headless=new");
    }
    cmd.arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", installed.executable.display()))?;
    let pid = child.id().context("child has no pid")?;

    let endpoint = wait_for_endpoint(port, &mut child).await?;

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Cdp,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}
