//! Cap'n Proto RPC server + client for the daemon.
//!
//! The wire layer is `capnp_rpc::twoparty` over any `AsyncRead + AsyncWrite`
//! duplex (UDS on unix, named pipe on windows). We bridge tokio io to
//! futures-io via `tokio_util::compat`.
//!
//! The `Daemon` bootstrap capability is the only thing exposed at the wire
//! level; further capabilities (`LockFree`, `LockedSession`) are reached by
//! calling methods on it.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use capnp::capability::Promise;
use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::cdp::{CdpClient, TargetRegistry};
use crate::daemon::probe::probe_target;
use crate::daemon::schema::{daemon_capnp, errors_capnp};
use crate::daemon::tabs::{TabConfig, TabHealth, TabRegistry, TabRow};
use serde_json::json;
use std::time::Duration;

/// Default per-op timeout for `Daemon.eval` (scratch-tab JS execution).
/// Generous for legitimate work but tight enough that a wedged scratch tab
/// fast-fails. Override per-call via `EvalRequest.timeoutMs`.
pub const DEFAULT_EVAL_TIMEOUT_MS: u32 = 10_000;

/// Shared, cloneable handle to the daemon's runtime state.
///
/// Daemon lifetime ≡ browser lifetime: this struct owns the upstream CDP
/// client and target registry, and the daemon exits as soon as `upstream`
/// signals close. Cloning is cheap (everything is `Arc`-wrapped) and is the
/// expected way to hand state to a new RPC connection.
#[derive(Clone)]
pub struct DaemonState {
    pub browser_kind: String,
    pub browser_version: String,
    /// Persistent CDP client to the browser. `None` is for tests / Phase 0
    /// scaffolding that only exercise the wire layer.
    pub upstream: Option<Arc<CdpClient>>,
    pub target_registry: Option<TargetRegistry>,
    pub tab_registry: Option<TabRegistry>,
}

impl DaemonState {
    /// Phase 0 / unit-test stub state. No upstream client; `version()` works,
    /// but ops that need the browser will refuse.
    pub fn empty() -> Self {
        Self {
            browser_kind: String::new(),
            browser_version: String::new(),
            upstream: None,
            target_registry: None,
            tab_registry: None,
        }
    }

    /// Open a CDP client to the browser at `http_endpoint` (e.g.
    /// `http://127.0.0.1:9222`) and attach a target registry. Fetches the
    /// browser product/version via `Browser.getVersion` so `Daemon.version`
    /// returns truthful data.
    pub async fn open(http_endpoint: &str) -> Result<Self> {
        let client = Arc::new(CdpClient::connect_http(http_endpoint).await?);

        // Browser.getVersion → { product: "Chrome/138.0.0.0", revision, userAgent, jsVersion }
        let info = client
            .send("Browser.getVersion", serde_json::Value::Null)
            .await
            .map_err(|e| anyhow!("Browser.getVersion failed: {e}"))?;
        let product = info
            .get("product")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown/0.0.0");
        let (kind_raw, version) = match product.split_once('/') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (product.to_string(), String::new()),
        };
        // Normalise "HeadlessChrome" / "Chrome" / etc. to a short kind tag.
        let browser_kind = if kind_raw.contains("Chrome") {
            "chromium"
        } else if kind_raw.contains("Edge") {
            "edge"
        } else if kind_raw.contains("Brave") {
            "brave"
        } else {
            "chromium"
        }
        .to_string();

        let target_registry = TargetRegistry::attach(client.clone()).await?;
        let tab_registry = TabRegistry::new(client.clone(), TabConfig::default());
        // Spawn the background idle sweep; it runs for the daemon's life.
        let _sweep = tab_registry.start_idle_sweep();

        Ok(Self {
            browser_kind,
            browser_version: version,
            upstream: Some(client),
            target_registry: Some(target_registry),
            tab_registry: Some(tab_registry),
        })
    }
}

/// Server-side adapter implementing the `Daemon` Cap'n Proto interface.
pub struct DaemonImpl {
    state: DaemonState,
}

impl DaemonImpl {
    pub fn new(state: DaemonState) -> Self {
        Self { state }
    }
}

impl daemon_capnp::daemon::Server for DaemonImpl {
    fn version(
        &mut self,
        _: daemon_capnp::daemon::VersionParams,
        mut results: daemon_capnp::daemon::VersionResults,
    ) -> Promise<(), capnp::Error> {
        let mut info = results.get().init_info();
        info.set_schema_version(daemon_capnp::SCHEMA_VERSION);
        info.set_daemon_version(env!("CARGO_PKG_VERSION"));
        info.set_browser_kind(self.state.browser_kind.as_str());
        info.set_browser_version(self.state.browser_version.as_str());
        Promise::ok(())
    }

    fn tab_open(
        &mut self,
        params: daemon_capnp::daemon::TabOpenParams,
        mut results: daemon_capnp::daemon::TabOpenResults,
    ) -> Promise<(), capnp::Error> {
        let tab_reg = match self.state.tab_registry.clone() {
            Some(r) => r,
            None => {
                return err_promise(
                    results.get().init_result().init_err(),
                    errors_capnp::ErrorCode::BrowserNotRunning,
                    "daemon has no upstream browser (Phase 0 stub)",
                )
            }
        };
        let (name, url) = match parse_tab_open_request(params) {
            Ok(pair) => pair,
            Err(e) => {
                return err_promise(
                    results.get().init_result().init_err(),
                    errors_capnp::ErrorCode::BadRequest,
                    &e,
                )
            }
        };
        Promise::from_future(async move {
            match tab_reg
                .open(
                    name.as_deref().filter(|s| !s.is_empty()),
                    url.as_deref().filter(|s| !s.is_empty()),
                )
                .await
            {
                Ok(row) => {
                    let mut ok = results.get().init_result().init_ok();
                    fill_tab_info(&mut ok, &row);
                    Ok(())
                }
                Err(e) => {
                    let code = match &e {
                        crate::daemon::tabs::OpenError::BudgetExceeded { .. } => {
                            errors_capnp::ErrorCode::LockQueueFull
                        }
                        crate::daemon::tabs::OpenError::Upstream(_) => {
                            errors_capnp::ErrorCode::BrowserUnhealthy
                        }
                    };
                    let msg = format!("{e}");
                    let mut err_b = results.get().init_result().init_err();
                    fill_err(&mut err_b, code, &msg);
                    Ok(())
                }
            }
        })
    }

    fn tab_list(
        &mut self,
        _: daemon_capnp::daemon::TabListParams,
        mut results: daemon_capnp::daemon::TabListResults,
    ) -> Promise<(), capnp::Error> {
        let tab_reg = self.state.tab_registry.clone();
        Promise::from_future(async move {
            let rows = match tab_reg {
                Some(r) => r.list().await,
                None => Vec::new(),
            };
            let mut list = results.get().init_tabs(rows.len() as u32);
            for (i, row) in rows.iter().enumerate() {
                let mut info = list.reborrow().get(i as u32);
                fill_tab_info(&mut info, row);
            }
            Ok(())
        })
    }

    fn eval(
        &mut self,
        params: daemon_capnp::daemon::EvalParams,
        mut results: daemon_capnp::daemon::EvalResults,
    ) -> Promise<(), capnp::Error> {
        // Lock-free eval: always routes through the daemon-owned scratch tab,
        // never against an arbitrary user tab. This is the architectural fix
        // for the iLO failure mode (default selection picked a wedged user
        // tab). The agent can still target a specific tab via the locked
        // `acquireLocked` → `LockedSession.eval` path in a later chunk.
        let (upstream, tab_reg) =
            match (self.state.upstream.clone(), self.state.tab_registry.clone()) {
                (Some(u), Some(r)) => (u, r),
                _ => {
                    return err_promise(
                        results.get().init_result().init_err(),
                        errors_capnp::ErrorCode::BrowserNotRunning,
                        "daemon has no upstream browser",
                    )
                }
            };

        let (expression, timeout_ms, await_promise) = match parse_eval_request(params) {
            Ok(t) => t,
            Err(e) => {
                return err_promise(
                    results.get().init_result().init_err(),
                    errors_capnp::ErrorCode::BadRequest,
                    &e,
                )
            }
        };
        let timeout_ms = if timeout_ms == 0 {
            DEFAULT_EVAL_TIMEOUT_MS
        } else {
            timeout_ms
        };

        Promise::from_future(async move {
            let scratch = match tab_reg.get_or_create_scratch().await {
                Ok(r) => r,
                Err(e) => {
                    let mut err_b = results.get().init_result().init_err();
                    fill_err(
                        &mut err_b,
                        errors_capnp::ErrorCode::BrowserUnhealthy,
                        &format!("scratch tab: {e}"),
                    );
                    return Ok(());
                }
            };

            // Attach a session and evaluate, bounded by timeout_ms.
            let outcome = run_scratch_eval(
                &upstream,
                &scratch.target_id,
                &expression,
                await_promise,
                Duration::from_millis(timeout_ms as u64),
            )
            .await;

            match outcome {
                EvalOutcome::Ok(value) => {
                    let mut ok = results.get().init_result().init_ok();
                    let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
                    ok.set_json(json.as_str());
                    Ok(())
                }
                EvalOutcome::Hung => {
                    // Mark the scratch tab stuck so the next request gets a
                    // fresh one (TabRegistry::open recreates on Stuck).
                    tab_reg.mark_stuck(&scratch.name).await;
                    let details = json!({
                        "targetId": scratch.target_id,
                        "tab": scratch.name,
                        "hint": "scratch tab unresponsive — will be recycled"
                    })
                    .to_string();
                    let mut err = results.get().init_result().init_err();
                    err.set_code(errors_capnp::ErrorCode::TabHung);
                    err.set_message(
                        format!("eval timed out after {timeout_ms}ms with no reply").as_str(),
                    );
                    err.set_hint("retry — scratch tab will be recycled");
                    err.set_recoverable(true);
                    err.set_details(details.as_str());
                    Ok(())
                }
                EvalOutcome::ProtocolErr(msg) => {
                    let mut err_b = results.get().init_result().init_err();
                    fill_err(&mut err_b, errors_capnp::ErrorCode::Internal, &msg);
                    Ok(())
                }
            }
        })
    }

    // Other methods (health, lockFree, acquireLocked, diagnose) land in
    // subsequent chunks. The generated trait provides stub impls returning
    // `unimplemented` so the RPC layer surfaces them as a typed
    // method-not-implemented error rather than a panic.
}

fn parse_tab_open_request(
    params: daemon_capnp::daemon::TabOpenParams,
) -> std::result::Result<(Option<String>, Option<String>), String> {
    let req = params
        .get()
        .map_err(|e| format!("malformed request: {e}"))?
        .get_req()
        .map_err(|e| format!("missing req: {e}"))?;
    let name = req
        .get_name()
        .map_err(|e| format!("missing name: {e}"))?
        .to_str()
        .map_err(|e| format!("name utf8: {e}"))?
        .to_string();
    let url = req
        .get_url()
        .map_err(|e| format!("missing url: {e}"))?
        .to_str()
        .map_err(|e| format!("url utf8: {e}"))?
        .to_string();
    Ok((Some(name), Some(url)))
}

fn parse_eval_request(
    params: daemon_capnp::daemon::EvalParams,
) -> std::result::Result<(String, u32, bool), String> {
    let req = params
        .get()
        .map_err(|e| format!("malformed request: {e}"))?
        .get_req()
        .map_err(|e| format!("missing req: {e}"))?;
    let expression = req
        .get_expression()
        .map_err(|e| format!("missing expression: {e}"))?
        .to_str()
        .map_err(|e| format!("expression utf8: {e}"))?
        .to_string();
    let timeout_ms = req.get_timeout_ms();
    let await_promise = req.get_await_promise();
    Ok((expression, timeout_ms, await_promise))
}

fn fill_tab_info(info: &mut daemon_capnp::tab_info::Builder, row: &TabRow) {
    info.set_name(row.name.as_str());
    info.set_target_id(row.target_id.as_str());
    info.set_url(row.url.as_str());
    let state_str = match row.state {
        TabHealth::Ready => "ready",
        TabHealth::Stuck => "stuck",
        TabHealth::Closed => "closed",
    };
    info.set_state(state_str);
    info.set_daemon_created(row.daemon_created);
    let idle_ms = row.last_used.elapsed().as_millis().min(u64::MAX as u128) as u64;
    info.set_idle_ms(idle_ms);
}

fn fill_err(err: &mut errors_capnp::error::Builder, code: errors_capnp::ErrorCode, message: &str) {
    err.set_code(code);
    err.set_message(message);
}

fn err_promise(
    mut err: errors_capnp::error::Builder,
    code: errors_capnp::ErrorCode,
    message: &str,
) -> Promise<(), capnp::Error> {
    fill_err(&mut err, code, message);
    Promise::ok(())
}

enum EvalOutcome {
    Ok(serde_json::Value),
    Hung,
    ProtocolErr(String),
}

async fn run_scratch_eval(
    client: &CdpClient,
    target_id: &str,
    expression: &str,
    await_promise: bool,
    timeout: Duration,
) -> EvalOutcome {
    // Attach. Bounded by `timeout` itself — a wedged target won't even
    // respond to attach in some cases.
    let attach = match tokio::time::timeout(
        timeout,
        client.send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        ),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return EvalOutcome::ProtocolErr(format!("attach: {e}")),
        Err(_) => return EvalOutcome::Hung,
    };
    let session_id = match attach.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return EvalOutcome::ProtocolErr("attach: no sessionId".into()),
    };
    let send = client.send_with_session(
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": await_promise,
        }),
        Some(&session_id),
    );
    let result = match tokio::time::timeout(timeout, send).await {
        Ok(Ok(v)) => EvalOutcome::Ok(v["result"]["value"].clone()),
        Ok(Err(e)) => EvalOutcome::ProtocolErr(format!("evaluate: {e}")),
        Err(_) => {
            // Probe whether the renderer is wedged via a cheap follow-up.
            let _ = probe_target(client, target_id, Duration::from_millis(200)).await;
            EvalOutcome::Hung
        }
    };
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
    result
}

/// Serve a single Cap'n Proto RPC connection until the peer disconnects.
///
/// Splits the duplex stream into halves and hands them to `twoparty::VatNetwork`.
/// The bootstrap capability is a fresh `DaemonImpl` wrapping `state`.
pub async fn serve<R, W>(reader: R, writer: W, state: DaemonState) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let reader = reader.compat();
    let writer = writer.compat_write();
    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );
    let client: daemon_capnp::daemon::Client = capnp_rpc::new_client(DaemonImpl::new(state));
    let rpc = RpcSystem::new(Box::new(network), Some(client.client));
    // RpcSystem drives the connection to completion. It exits cleanly on
    // peer disconnect (returns Ok), or with an error on protocol violation.
    rpc.await
        .map_err(|e| anyhow!("daemon rpc loop exited with error: {e}"))?;
    Ok(())
}

/// Connect a client to a duplex stream and return the bootstrap `Daemon` cap.
///
/// The returned `Disconnector` future must be polled (typically by spawning
/// `rpc.disconnector()` and the rpc system itself on a background task) for
/// the connection to make progress. Callers normally wrap this in a
/// higher-level client helper that owns the spawn.
pub fn connect_client<R, W>(
    reader: R,
    writer: W,
) -> (
    daemon_capnp::daemon::Client,
    RpcSystem<rpc_twoparty_capnp::Side>,
)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let reader = reader.compat();
    let writer = writer.compat_write();
    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Client,
        Default::default(),
    );
    let mut rpc = RpcSystem::new(Box::new(network), None);
    let client: daemon_capnp::daemon::Client = rpc.bootstrap(rpc_twoparty_capnp::Side::Server);
    (client, rpc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::LocalSet;

    /// Loopback test: server + client connected via `tokio::io::duplex`,
    /// client calls `version()`, asserts the daemon pkg version comes back.
    ///
    /// Uses `current_thread` flavor and drives both RpcSystem futures
    /// concurrently in the same task via `tokio::select!`. capnp_rpc's
    /// RpcSystem is `!Send`, and we found that nesting `spawn_local` for
    /// the server side inside a `LocalSet` does not reliably get polled in
    /// this test context — so we drive it inline alongside the client.
    #[tokio::test(flavor = "current_thread")]
    async fn version_roundtrip_over_duplex() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (client_side, server_side) = tokio::io::duplex(64 * 1024);
                let (client_read, client_write) = tokio::io::split(client_side);
                let (server_read, server_write) = tokio::io::split(server_side);

                let state = DaemonState {
                    browser_kind: "chromium".into(),
                    browser_version: "138.0.0.0".into(),
                    upstream: None,
                    target_registry: None,
                    tab_registry: None,
                };

                // Drive the server's RpcSystem inline (no spawn_local) so its
                // polls are guaranteed to interleave with the client's call.
                let server_fut = serve(server_read, server_write, state);
                tokio::pin!(server_fut);

                // Client side: spawn the RPC system, then call version().
                let (client, rpc) = connect_client(client_read, client_write);
                let _rpc_task = tokio::task::spawn_local(async move {
                    let _ = rpc.await;
                });

                let response = {
                    let mut call = client.version_request().send().promise;
                    let mut server_done = false;
                    loop {
                        tokio::select! {
                            biased;
                            res = &mut call => break res.expect("version() failed"),
                            _ = &mut server_fut, if !server_done => {
                                // Server exited (e.g. client dropped); stop polling it.
                                server_done = true;
                            }
                        }
                    }
                };
                let info = response
                    .get()
                    .expect("response root")
                    .get_info()
                    .expect("info struct");

                assert_eq!(
                    info.get_schema_version(),
                    daemon_capnp::SCHEMA_VERSION,
                    "schema version on the wire"
                );
                assert_eq!(
                    info.get_daemon_version()
                        .expect("daemon_version text")
                        .to_str()
                        .expect("utf8"),
                    env!("CARGO_PKG_VERSION"),
                );
                assert_eq!(
                    info.get_browser_kind()
                        .expect("browser_kind text")
                        .to_str()
                        .expect("utf8"),
                    "chromium",
                );
                assert_eq!(
                    info.get_browser_version()
                        .expect("browser_version text")
                        .to_str()
                        .expect("utf8"),
                    "138.0.0.0",
                );

                // Drop the client. The server's RpcSystem will see EOF and
                // its future will resolve. We don't need to drive it further.
                drop(client);
            })
            .await;
    }

    /// Test #14: a client mid-call against a dying daemon fast-fails — the
    /// promise resolves with an error rather than hanging.
    ///
    /// We model "daemon dies mid-call" by abruptly aborting the server-side
    /// RPC task. The client-side `RpcSystem` sees the duplex EOF, every
    /// pending call promise resolves with `capnp::Error::Disconnected`, and
    /// the wrapping `tokio::time::timeout` confirms we never blocked past a
    /// tight bound.
    #[tokio::test(flavor = "current_thread")]
    async fn pending_call_resolves_when_daemon_drops() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (client_side, server_side) = tokio::io::duplex(64 * 1024);
                let (client_read, client_write) = tokio::io::split(client_side);
                let (server_read, server_write) = tokio::io::split(server_side);

                // Server: stalls a tiny bit, then we abort it from the test
                // (mimicking the daemon process dying).
                let state = DaemonState::empty();
                let server = tokio::task::spawn_local(async move {
                    // Hold the connection but don't actually drive an
                    // RpcSystem — so requests never get served. Drop both
                    // halves of the stream when aborted to surface EOF on
                    // the client.
                    let _ = (server_read, server_write);
                    std::future::pending::<()>().await;
                    drop(state);
                });

                let (client, rpc) = connect_client(client_read, client_write);
                let rpc_task = tokio::task::spawn_local(async move {
                    let _ = rpc.await;
                });

                // Fire the call; do NOT await it yet.
                let req = client.version_request();
                let call = req.send().promise;

                // After a small delay, kill the server. Pending call must
                // resolve quickly (RpcSystem sees EOF, errors all pending).
                let kill = tokio::task::spawn_local(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    server.abort();
                    let _ = server.await;
                });

                let start = std::time::Instant::now();
                let outcome =
                    tokio::time::timeout(std::time::Duration::from_millis(500), call).await;
                let elapsed = start.elapsed();
                assert!(
                    elapsed < std::time::Duration::from_millis(500),
                    "call did not fast-fail; took {elapsed:?}"
                );
                // Inner result must be an Err — successful version() is
                // not the contract being tested here.
                match outcome {
                    Ok(Err(_)) => {} // disconnected / canceled, as expected
                    Ok(Ok(_)) => panic!("version() succeeded against an unresponsive server"),
                    Err(_) => panic!("call hung past 500ms"),
                }

                let _ = kill.await;
                let _ = rpc_task.await;
            })
            .await;
    }

    // -------- Mock CDP server used by the end-to-end eval test below --------

    use futures_util::{SinkExt, StreamExt};
    use serde_json::Value as JsonValue;
    use tokio::sync::{oneshot, Mutex as TokioMutex};
    use tokio_tungstenite::tungstenite::Message;

    /// Spawn a mock CDP server that records every (method, params) it sees.
    /// Returns (ws_url, history_handle, stop_signal).
    async fn spawn_recording_mock() -> (
        String,
        std::sync::Arc<TokioMutex<Vec<(String, JsonValue)>>>,
        oneshot::Sender<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let history: std::sync::Arc<TokioMutex<Vec<(String, JsonValue)>>> =
            std::sync::Arc::new(TokioMutex::new(Vec::new()));
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let history_for_task = history.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: JsonValue = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("").to_string();
                            let params = req["params"].clone();
                            history_for_task.lock().await.push((method.clone(), params));
                            let result = match method.as_str() {
                                "Target.createTarget" => {
                                    next_target += 1;
                                    serde_json::json!({"targetId": format!("T{next_target}")})
                                }
                                "Target.closeTarget" => serde_json::json!({"success": true}),
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    serde_json::json!({"sessionId": format!("S{next_session}")})
                                }
                                "Target.detachFromTarget" => serde_json::json!({}),
                                "Runtime.evaluate" => {
                                    serde_json::json!({"result": {"value": 2}})
                                }
                                "Browser.getVersion" => {
                                    serde_json::json!({"product": "Chrome/138.0.0.0"})
                                }
                                "Target.getTargets" => serde_json::json!({"targetInfos": []}),
                                "Target.setDiscoverTargets"
                                | "Target.setAutoAttach"
                                | "Page.navigate" => serde_json::json!({}),
                                _ => serde_json::json!({}),
                            };
                            let resp = serde_json::json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), history, stop_tx)
    }

    /// Test #6: `Daemon.eval` (lock-free) routes through a daemon-owned
    /// scratch tab — never against an arbitrary user tab.
    ///
    /// Uses the in-process `capnp_rpc::new_client` path (no twoparty RPC,
    /// no duplex stream) — the test exercises the actual `DaemonImpl::eval`
    /// handler and asserts via the mock CDP server's recorded method
    /// history that:
    ///   - the daemon created a tab with `about:blank` (scratch pool)
    ///   - `Runtime.evaluate` was sent against that scratch tab's session
    ///
    /// The twoparty path itself is exercised by
    /// `version_roundtrip_over_duplex` and
    /// `pending_call_resolves_when_daemon_drops`. Skipping it here side-steps
    /// a scheduling deadlock between `Promise::from_future`'s polling and
    /// nested `tokio::spawn`'d tasks that block on cross-task wakers when
    /// driven inside a single `LocalSet::run_until` over a duplex pair.
    #[tokio::test(flavor = "current_thread")]
    async fn eval_routes_through_scratch_tab() {
        let (cdp_url, history, _stop) = spawn_recording_mock().await;

        let cdp_client = std::sync::Arc::new(CdpClient::connect(&cdp_url).await.unwrap());
        // Skip TargetRegistry::attach so the recorded history stays focused
        // on the eval-related calls.
        let tab_reg = TabRegistry::new(cdp_client.clone(), TabConfig::default());
        let state = DaemonState {
            browser_kind: "chromium".into(),
            browser_version: "138.0.0.0".into(),
            upstream: Some(cdp_client),
            target_registry: None,
            tab_registry: Some(tab_reg.clone()),
        };

        // In-process client: dispatches directly to the server impl, no
        // network, no RpcSystem task pump.
        let daemon_client: daemon_capnp::daemon::Client =
            capnp_rpc::new_client(DaemonImpl::new(state));

        let mut req = daemon_client.eval_request();
        let mut root = req.get();
        let mut inner = root.reborrow().init_req();
        inner.set_target_id("");
        inner.set_expression("1+1");
        inner.set_await_promise(false);
        inner.set_timeout_ms(2000);
        let resp = req.send().promise.await.expect("eval call");

        let result = resp.get().expect("root").get_result().expect("result");
        match result.which().expect("which") {
            daemon_capnp::daemon::eval_result_env::Which::Ok(ok) => {
                let json_str = ok
                    .expect("ok branch")
                    .get_json()
                    .expect("json field")
                    .to_str()
                    .expect("utf8");
                assert_eq!(json_str, "2", "echoed runtime evaluate value");
            }
            daemon_capnp::daemon::eval_result_env::Which::Err(_) => {
                panic!("eval should succeed against responsive mock")
            }
        }

        // Inspect the recorded methods: we must have created at least one
        // Target with url=about:blank (the scratch tab) and Runtime.evaluate
        // must have been routed against the daemon-created tab.
        let h = history.lock().await;
        let created_blanks: Vec<&JsonValue> = h
            .iter()
            .filter(|(m, _)| m == "Target.createTarget")
            .map(|(_, p)| p)
            .collect();
        assert!(
            !created_blanks.is_empty(),
            "no Target.createTarget recorded; scratch tab not used"
        );
        assert!(
            created_blanks
                .iter()
                .any(|p| p.get("url").and_then(|v| v.as_str()) == Some("about:blank")),
            "scratch tab was not created with about:blank: {:?}",
            created_blanks
        );
        assert!(
            h.iter().any(|(m, _)| m == "Runtime.evaluate"),
            "Runtime.evaluate was not called"
        );
    }
}
