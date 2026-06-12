//! Chromium-family browser launcher (Chrome, Edge, Chromium, Brave).

use std::fs::File;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{
    allocate_free_port, background_after_launch, configure_session_detachment, wait_for_endpoint,
    LaunchOpts, LaunchedHandle,
};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating CDP port")?;

    if let Some(parent) = opts.profile_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::create_dir_all(&opts.profile_dir)
        .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;

    let log_path = opts.profile_dir.join("browser.log");
    let log_file =
        File::create(&log_path).with_context(|| format!("creating {}", log_path.display()))?;
    let log_clone = log_file
        .try_clone()
        .context("cloning log file handle for stderr")?;

    let mut cmd = Command::new(&installed.executable);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", opts.profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-component-update");
    if opts.headless {
        cmd.arg("--headless=new");
    } else {
        cmd.arg("--start-minimized");
    }
    cmd.arg("about:blank");

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_clone))
        .kill_on_drop(false);
    configure_session_detachment(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", installed.executable.display()))?;
    let pid = child.id().context("child has no pid")?;

    let endpoint = wait_for_endpoint(port, &mut child, &log_path).await?;
    if !opts.headless {
        background_after_launch(pid);
    }

    Ok(LaunchedHandle {
        pid,
        port,
        endpoint,
        engine: Engine::Cdp,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}
