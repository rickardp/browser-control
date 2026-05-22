//! Client-side helper for connecting to a running daemon and making RPC
//! calls from a short-lived CLI process.
//!
//! Each CLI invocation:
//! 1. Builds a `tokio::task::LocalSet` (capnp_rpc requires single-threaded
//!    spawning).
//! 2. Connects to the daemon's UDS / named pipe.
//! 3. Spawns the `RpcSystem` task on the local set.
//! 4. Calls the desired method via the bootstrap `Daemon` capability.
//! 5. Returns the result; the local set winds down on drop.
//!
//! Auto-spawn (start the daemon if it's not running) is intentionally not
//! implemented here yet — Phase 1 keeps the developer surface explicit
//! (`browser-control daemon run <browser>` in a background terminal). The
//! CLI subcommands surface a clear "daemon not running" hint if no socket
//! is found.

use anyhow::{anyhow, Context, Result};
use std::time::Duration;

use crate::daemon::bringup;
use crate::daemon::schema::daemon_capnp;
use crate::daemon::Endpoint;

/// How long to wait for the IPC connect step. Mirrors the upstream CDP
/// connect timeout so a wedged daemon doesn't hang the CLI.
const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the daemon endpoint path for `browser_name`, then connect.
/// Returns `Err` with a helpful hint if no daemon is running.
pub async fn connect_browser(browser_name: &str) -> Result<DaemonClient> {
    let endpoint_path = bringup::endpoint_path(browser_name)?;
    if !endpoint_path.exists() {
        return Err(anyhow!(
            "no daemon for {browser_name}: run `browser-control daemon run {browser_name}` first \
             (expected endpoint at {})",
            endpoint_path.display()
        ));
    }
    let endpoint = Endpoint::new(endpoint_path);
    let stream = tokio::time::timeout(IPC_CONNECT_TIMEOUT, crate::daemon::connect(&endpoint))
        .await
        .map_err(|_| anyhow!("IPC connect to daemon timed out after {IPC_CONNECT_TIMEOUT:?}"))?
        .context("connect to daemon UDS")?;

    let (client, rpc) = crate::daemon::connect_client(stream.reader, stream.writer);
    let rpc_task = tokio::task::spawn_local(async move {
        let _ = rpc.await;
    });

    Ok(DaemonClient {
        client,
        _rpc_task: rpc_task,
    })
}

/// Owning handle to a live daemon RPC connection. Keeps the RpcSystem task
/// alive for as long as the handle is held; dropping closes the connection.
pub struct DaemonClient {
    pub client: daemon_capnp::daemon::Client,
    _rpc_task: tokio::task::JoinHandle<()>,
}

impl DaemonClient {
    /// Convenience: call `Daemon.tabOpen` and return a printable summary.
    pub async fn tab_open(&self, name: Option<&str>, url: Option<&str>) -> Result<TabSummary> {
        let mut req = self.client.tab_open_request();
        let mut root = req.get();
        let mut inner = root.reborrow().init_req();
        inner.set_name(name.unwrap_or(""));
        inner.set_url(url.unwrap_or(""));
        let resp = req.send().promise.await?;
        let result = resp.get()?.get_result()?;
        match result.which()? {
            daemon_capnp::daemon::tab_open_result::Which::Ok(info) => {
                let info = info?;
                Ok(TabSummary::from_reader(info)?)
            }
            daemon_capnp::daemon::tab_open_result::Which::Err(err) => {
                let err = err?;
                Err(anyhow!(
                    "tab open failed: {} (code={:?})",
                    err.get_message()?.to_str().unwrap_or(""),
                    err.get_code()?
                ))
            }
        }
    }

    pub async fn tab_list(&self) -> Result<Vec<TabSummary>> {
        let req = self.client.tab_list_request();
        let resp = req.send().promise.await?;
        let tabs = resp.get()?.get_tabs()?;
        let mut out = Vec::with_capacity(tabs.len() as usize);
        for t in tabs.iter() {
            out.push(TabSummary::from_reader(t)?);
        }
        Ok(out)
    }

    pub async fn eval(
        &self,
        expression: &str,
        timeout_ms: u32,
        await_promise: bool,
    ) -> Result<serde_json::Value> {
        let mut req = self.client.eval_request();
        let mut root = req.get();
        let mut inner = root.reborrow().init_req();
        inner.set_target_id(""); // ignored by daemon for lock-free eval
        inner.set_expression(expression);
        inner.set_await_promise(await_promise);
        inner.set_timeout_ms(timeout_ms);
        let resp = req.send().promise.await?;
        let result = resp.get()?.get_result()?;
        match result.which()? {
            daemon_capnp::daemon::eval_result_env::Which::Ok(ok) => {
                let ok = ok?;
                let json_str = ok.get_json()?.to_str().unwrap_or("null");
                Ok(serde_json::from_str(json_str).unwrap_or(serde_json::Value::Null))
            }
            daemon_capnp::daemon::eval_result_env::Which::Err(err) => {
                let err = err?;
                Err(anyhow!(
                    "{} (code={:?}; hint={})",
                    err.get_message()?.to_str().unwrap_or(""),
                    err.get_code()?,
                    err.get_hint()?.to_str().unwrap_or("")
                ))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TabSummary {
    pub name: String,
    pub target_id: String,
    pub url: String,
    pub state: String,
    pub daemon_created: bool,
    pub idle_ms: u64,
}

impl TabSummary {
    fn from_reader(r: daemon_capnp::tab_info::Reader) -> Result<Self> {
        Ok(Self {
            name: r.get_name()?.to_str().unwrap_or("").to_string(),
            target_id: r.get_target_id()?.to_str().unwrap_or("").to_string(),
            url: r.get_url()?.to_str().unwrap_or("").to_string(),
            state: r.get_state()?.to_str().unwrap_or("").to_string(),
            daemon_created: r.get_daemon_created(),
            idle_ms: r.get_idle_ms(),
        })
    }
}
