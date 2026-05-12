//! Firefox launcher (WebDriver BiDi via `--remote-debugging-port`, Firefox 129+).

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::detect::{Engine, Installed};

use super::{allocate_free_port, wait_for_endpoint, LaunchOpts, LaunchedHandle};

pub async fn launch(installed: &Installed, opts: LaunchOpts) -> Result<LaunchedHandle> {
    let port = allocate_free_port().context("allocating BiDi port")?;

    if !opts.profile_dir.exists() {
        std::fs::create_dir_all(&opts.profile_dir)
            .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;
    }

    let mut cmd = Command::new(&installed.executable);
    cmd.arg("-profile").arg(&opts.profile_dir).arg("-no-remote");
    if opts.headless {
        cmd.arg("-headless");
    }
    cmd.arg("--remote-debugging-port")
        .arg(port.to_string())
        .arg("about:blank");

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
        engine: Engine::Bidi,
        profile_dir: opts.profile_dir,
        child: Some(child),
    })
}
