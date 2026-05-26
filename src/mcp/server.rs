//! Minimal hand-rolled MCP JSON-RPC server over stdio.
//!
//! This is the wave-3 skeleton. A future task may replace this with a more
//! capable framework (e.g. `rmcp`). The protocol surface is small:
//! newline-delimited JSON-RPC 2.0 over stdin/stdout.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, RwLock};

use crate::cli::env_resolver::ResolvedBrowser;
use crate::session::backend::TabBackend;

/// Persistent BiDi client, opened lazily on first use. Reused across all
/// tool calls because Firefox limits concurrent BiDi sessions per browser
/// to one. The browsing-context id is resolved per call so multiple tabs
/// can be addressed once URL-regex selection is added.
pub type BidiCache = Arc<Mutex<Option<Arc<crate::bidi::BidiClient>>>>;

/// State carried by the server. Tools reach into this for the resolved
/// browser endpoint and any cached engine clients.
///
/// `browser` is `RwLock`-wrapped so `browser_select` can swap the active
/// browser at runtime; readers take a brief read lock and clone out the
/// value they need (the struct is cheap to clone).
///
/// `active_target_id` is the in-memory pointer to the MCP server's
/// "current tab" — replaces the SQLite `_mcp-<pid>` row pattern. The
/// pointer is lazy-initialised on first stateful tool call and updated
/// by `browser_tab_*` and `browser_select`.
#[derive(Clone)]
pub struct ServerState {
    pub browser: Arc<RwLock<ResolvedBrowser>>,
    pub bidi: BidiCache,
    /// Firefox BiDi single-session lock, acquired lazily on first tool
    /// call and held for the server's lifetime. `None` for CDP browsers
    /// and external endpoints (where `acquire_bidi_lock_if_needed`
    /// returns None) — the inner `Option<BidiLockGuard>` distinguishes
    /// "haven't tried yet" from "tried, not applicable" via the outer
    /// `Mutex` being unlocked vs returning None.
    pub bidi_lock: Arc<Mutex<BidiLockState>>,
    /// Cached [`TabBackend`] for the configured browser, opened lazily
    /// on first tool call and reused for the server's lifetime. Avoids
    /// repeatedly running the BiDi `session.new` handshake and lets us
    /// share one CDP WebSocket across all tool calls.
    pub backend: Arc<Mutex<Option<TabBackend>>>,
    /// In-memory "active tab" pointer. `None` until lazy-init by
    /// `current_tab()` or set explicitly by `browser_tab_select` /
    /// `browser_tab_new`. Cleared on `browser_tab_close` (when closing
    /// the active tab) and on `browser_select`.
    pub active_target_id: Arc<Mutex<Option<String>>>,
    /// Lazy-spawned Playwright sidecar for the Chromium-only interaction
    /// tools (`browser_click`, `browser_snapshot`, etc.). One sidecar
    /// per server-per-browser; `browser_select` disposes the old one
    /// and the next sidecar-using tool spawns a fresh one against the
    /// new endpoint. `None` for BiDi browsers (the sidecar tools error
    /// with `EngineUnsupported`) and on fresh servers until first use.
    pub sidecar: Arc<Mutex<Option<crate::sidecar::Sidecar>>>,
    /// Sidecar config (Playwright version override etc.) — set once at
    /// server startup from CLI args, read on each sidecar spawn.
    pub sidecar_config: crate::sidecar::SidecarConfig,
}

/// Three-state cache: `Pending` until first tool call attempts acquire;
/// `Acquired` holding the guard; `NotApplicable` for CDP / external
/// endpoints where no lock is needed.
#[derive(Default)]
pub enum BidiLockState {
    #[default]
    Pending,
    Acquired(crate::registry::BidiLockGuard),
    NotApplicable,
}

impl ServerState {
    pub fn new(browser: ResolvedBrowser) -> Self {
        Self::with_sidecar_config(browser, crate::sidecar::SidecarConfig::default())
    }

    /// Construct a `ServerState` with a non-default sidecar config (e.g.
    /// a custom Playwright version from `--playwright-version`).
    pub fn with_sidecar_config(
        browser: ResolvedBrowser,
        sidecar_config: crate::sidecar::SidecarConfig,
    ) -> Self {
        Self {
            browser: Arc::new(RwLock::new(browser)),
            bidi: Arc::new(Mutex::new(None)),
            bidi_lock: Arc::new(Mutex::new(BidiLockState::Pending)),
            backend: Arc::new(Mutex::new(None)),
            active_target_id: Arc::new(Mutex::new(None)),
            sidecar: Arc::new(Mutex::new(None)),
            sidecar_config,
        }
    }

    /// Lazy-spawn the Playwright sidecar against the current browser.
    /// Errors with `EngineUnsupported` when the active browser is BiDi
    /// (Playwright can't drive a user-launched Firefox over BiDi/CDP).
    ///
    /// Idempotent: subsequent calls return the cached handle. The handle
    /// is dropped (and the child killed) when `switch_browser` clears it.
    pub async fn ensure_sidecar(&self, tool_name: &str) -> Result<crate::sidecar::Sidecar> {
        let resolved = self.browser_snapshot().await;
        if resolved.engine != crate::detect::Engine::Cdp {
            return Err(crate::errors::SessionError::EngineUnsupported {
                tool: tool_name.to_string(),
                required_engine: "Chromium (CDP)".into(),
                current_engine: format!("{:?}", resolved.engine),
                hint: "use browser_evaluate or switch to a Chromium browser via browser_select",
            }
            .into());
        }
        let mut guard = self.sidecar.lock().await;
        if let Some(sc) = guard.as_ref() {
            return Ok(sc.clone());
        }
        let sc = crate::sidecar::Sidecar::start(self.sidecar_config.clone()).await?;
        sc.connect(&resolved.endpoint).await?;
        *guard = Some(sc.clone());
        Ok(sc)
    }

    /// Snapshot the current resolved browser (cheap clone of a small struct).
    pub async fn browser_snapshot(&self) -> ResolvedBrowser {
        self.browser.read().await.clone()
    }

    /// Ensure the BiDi single-session lock is held (if applicable).
    /// Lazy + idempotent: called by each tool handler before opening a
    /// BiDi session, returns immediately on second+ calls.
    pub async fn ensure_bidi_lock(&self) -> Result<()> {
        use crate::cli::mcp::acquire_bidi_lock_if_needed;
        use crate::registry::Registry;
        let mut guard = self.bidi_lock.lock().await;
        if matches!(*guard, BidiLockState::Pending) {
            let registry = Registry::open()?;
            let resolved = self.browser_snapshot().await;
            *guard = match acquire_bidi_lock_if_needed(&registry, &resolved)? {
                Some(lock) => BidiLockState::Acquired(lock),
                None => BidiLockState::NotApplicable,
            };
        }
        Ok(())
    }

    /// Lazy-open (or return cached) [`TabBackend`] for the server's
    /// browser. Acquires the BiDi lock first if applicable. The backend
    /// is cached for the server's lifetime so the BiDi `session.new`
    /// handshake runs once and the CDP WebSocket is reused across calls.
    pub async fn ensure_backend(&self) -> Result<TabBackend> {
        self.ensure_bidi_lock().await?;
        let mut guard = self.backend.lock().await;
        if let Some(b) = guard.as_ref() {
            return Ok(b.clone());
        }
        let resolved = self.browser_snapshot().await;
        let b = crate::session::backend::open_backend(&resolved.endpoint, resolved.engine).await?;
        *guard = Some(b.clone());
        Ok(b)
    }

    /// Resolve the MCP server's "active tab" — backed by an in-memory
    /// `active_target_id` pointer rather than a SQLite row.
    ///
    /// The returned `(backend, target_id)` is the routing pair stateful
    /// MCP tools (`browser_navigate`, `browser_get_html`, …) use when no
    /// explicit `tab` / `target` arg is given.
    ///
    /// Behaviour:
    /// - **None** → create an `about:blank` and store it.
    /// - **Set but dead** (no longer in `live_target_ids`) → recreate
    ///   `about:blank` and re-point the pointer. This is the scratch-style
    ///   implicit recovery for the **server-owned** active tab; explicit
    ///   tabs created via `browser_tab_new` / `browser_tab_select` also
    ///   travel through here once they become the active tab, but
    ///   recovery there means the agent-named tab is gone — see
    ///   `browser_tab_select`'s dead-tab handling for the explicit-select
    ///   contract.
    /// - **Set and alive** → return as-is.
    pub async fn current_tab(&self) -> Result<(TabBackend, String)> {
        let backend = self.ensure_backend().await?;
        let mut pointer = self.active_target_id.lock().await;
        if let Some(tid) = pointer.as_ref() {
            let live = backend.live_target_ids().await?;
            if live.contains(tid) {
                return Ok((backend, tid.clone()));
            }
            // Dead — fall through to recreate.
        }
        let new_tid = backend.create_tab("about:blank").await?;
        *pointer = Some(new_tid.clone());
        Ok((backend, new_tid))
    }

    /// Route a stateful tool call to a backend + target id based on the
    /// optional `tab` (named) and `target` (URL regex) args. `tab` and
    /// `target` are mutually exclusive. Falls through to `current_tab()`
    /// when neither is provided.
    ///
    /// For the named-tab path: the registered tab row is resolved (with
    /// sweep-on-read for stale rows) and returned. Tools that want
    /// recover-on-failure semantics should structure their op around the
    /// returned `(backend, target_id)` — full `with_named_tab_recovery`
    /// can't run from a `Send` MCP future because `Registry` is `!Send`.
    ///
    /// For the URL-regex path, probe-and-iterate via the live targets
    /// snapshot. Surfaces `SessionError::TabHung` if every match is
    /// unresponsive within a 500ms probe.
    pub async fn resolve_target_for_args(
        &self,
        args: &Value,
    ) -> Result<(TabBackend, String)> {
        let tab = args.get("tab").and_then(|v| v.as_str()).map(String::from);
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from);
        match (tab, target) {
            (Some(_), Some(_)) => Err(anyhow::anyhow!(
                "`tab` and `target` are mutually exclusive"
            )),
            (Some(name), None) => {
                let backend = self.ensure_backend().await?;
                let browser_name = self.registered_browser_name().await?;
                // Mimic `session::tabs::resolve_tab` here so we never hold
                // a `!Send` `Registry` across `.await`: sync registry-read,
                // async liveness probe, sync registry-mutate.
                let row = sync_registry_op(|reg| reg.tab_get(&browser_name, &name))?
                    .ok_or_else(|| crate::errors::SessionError::TabNotFound {
                        browser: browser_name.clone(),
                        name: name.clone(),
                    })?;
                let live = backend.live_target_ids().await?;
                if !live.contains(&row.target_id) {
                    // Stale — sweep, then error.
                    let bn = browser_name.clone();
                    let n = name.clone();
                    sync_registry_op(move |reg| reg.tab_delete(&bn, &n))?;
                    return Err(crate::errors::SessionError::TabNotFound {
                        browser: browser_name,
                        name,
                    }
                    .into());
                }
                let bn = browser_name.clone();
                let n = name.clone();
                sync_registry_op(move |reg| reg.tab_touch(&bn, &n))?;
                Ok((backend, row.target_id))
            }
            (None, Some(regex)) => {
                let backend = self.ensure_backend().await?;
                let target_id = resolve_target_by_regex(&backend, &regex).await?;
                Ok((backend, target_id))
            }
            (None, None) => self.current_tab().await,
        }
    }

    /// The registered browser's name. Errors if the active browser is an
    /// external URL endpoint (no stable identity for named tabs).
    pub async fn registered_browser_name(&self) -> Result<String> {
        use crate::cli::env_resolver::Source;
        let resolved = self.browser_snapshot().await;
        match resolved.source {
            Source::Registered { name } => Ok(name),
            Source::External => Err(anyhow::anyhow!(
                "operation requires a registered browser; external URL endpoints \
                 don't have a stable identity"
            )),
        }
    }

    /// Swap the active browser. Drops the cached backend and BiDi session,
    /// releases the BiDi lock (if held), clears the active tab pointer,
    /// then installs the new browser and re-acquires the BiDi lock if
    /// the new one needs it. The next stateful tool call lazy-opens the
    /// new backend.
    pub async fn switch_browser(&self, new_browser: ResolvedBrowser) -> Result<()> {
        // Close the cached BiDi session if any (best-effort).
        {
            let mut bidi = self.bidi.lock().await;
            if let Some(client) = bidi.take() {
                let _ = client.session_end().await;
            }
        }
        // Drop the cached backend so the next call rebuilds against
        // the new browser.
        {
            let mut backend = self.backend.lock().await;
            *backend = None;
        }
        // Release the BiDi lock guard (Drop releases it) and reset to Pending.
        {
            let mut lock = self.bidi_lock.lock().await;
            *lock = BidiLockState::Pending;
        }
        // Clear the active tab pointer.
        {
            let mut pointer = self.active_target_id.lock().await;
            *pointer = None;
        }
        // Dispose the Playwright sidecar — different browser means
        // different CDP endpoint; the next sidecar tool spawns a fresh
        // child connected to the new endpoint.
        {
            let mut sidecar = self.sidecar.lock().await;
            if let Some(sc) = sidecar.take() {
                let _ = sc.call("dispose", serde_json::json!({})).await;
                // Drop releases the child via SidecarInner::drop.
                drop(sc);
            }
        }
        // Install the new browser.
        {
            let mut br = self.browser.write().await;
            *br = new_browser;
        }
        // Eagerly re-acquire the BiDi lock if applicable, so any error
        // surfaces here rather than at the next tool call.
        self.ensure_bidi_lock().await?;
        Ok(())
    }
}

/// Helper that opens a fresh `Registry` and runs a closure against it,
/// returning the result. Used by the MCP layer to keep `!Send`
/// `rusqlite::Connection` references off of `.await`-crossing scopes.
/// The closure runs synchronously inside the helper; the registry is
/// dropped before the function returns.
pub(crate) fn sync_registry_op<T, F>(f: F) -> Result<T>
where
    F: FnOnce(&crate::registry::Registry) -> Result<T>,
{
    let reg = crate::registry::Registry::open()?;
    f(&reg)
}

/// Resolve a `BrowserSelector` to a `ResolvedBrowser` from a `Send`
/// async context. The URL branch awaits an HTTP roundtrip (no registry
/// needed); the registered/kind/path branches run synchronously via
/// `tokio::task::spawn_blocking` so the `!Send` `Registry` never sits
/// across `.await`.
pub(crate) async fn resolve_browser_send(
    selector: crate::cli::env_resolver::BrowserSelector,
) -> Result<ResolvedBrowser> {
    use crate::cli::env_resolver::{BrowserSelector, DefaultResolver, Resolver};
    match selector {
        BrowserSelector::Url(u) => match u.scheme() {
            "ws" | "wss" => Ok(ResolvedBrowser {
                engine: if u.path().contains("/session") {
                    crate::detect::Engine::Bidi
                } else {
                    crate::detect::Engine::Cdp
                },
                endpoint: u.to_string(),
                source: crate::cli::env_resolver::Source::External,
            }),
            "http" | "https" => {
                let base = u.as_str().trim_end_matches('/').to_string();
                let ws = DefaultResolver.fetch_version(&base).await?;
                let ws_url = url::Url::parse(&ws)?;
                Ok(ResolvedBrowser {
                    engine: if ws_url.path().contains("/session") {
                        crate::detect::Engine::Bidi
                    } else {
                        crate::detect::Engine::Cdp
                    },
                    endpoint: ws,
                    source: crate::cli::env_resolver::Source::External,
                })
            }
            other => anyhow::bail!("unsupported URL scheme: {other}"),
        },
        other => tokio::task::spawn_blocking(move || {
            let reg = crate::registry::Registry::open()?;
            // Non-URL branches of `resolve_with` are pure SQL with no
            // awaits — the future polls to completion in one step.
            // We need to drive a tiny async, but `block_on` is fine
            // here on a blocking thread.
            let rt = tokio::runtime::Builder::new_current_thread().build()?;
            rt.block_on(crate::cli::env_resolver::resolve_with(
                other,
                &reg,
                &DefaultResolver,
            ))
        })
        .await?,
    }
}

/// Resolve a `target` URL-regex arg to a live `target_id` on `backend`,
/// using the same probe-and-iterate semantics as `pick_cdp_page` /
/// `pick_bidi_context` in `session::attach`. Walks `live_targets()` and
/// returns the first matching responsive target; if all matches are
/// unresponsive, returns `SessionError::TabHung`. Errors `anyhow` if
/// the regex matches nothing.
async fn resolve_target_by_regex(backend: &TabBackend, regex: &str) -> Result<String> {
    use crate::errors::SessionError;
    use regex::Regex;
    use std::time::Duration;
    const PROBE: Duration = Duration::from_millis(500);

    let re = Regex::new(regex)?;
    let targets = backend.live_targets().await?;
    let matches: Vec<_> = targets.iter().filter(|t| re.is_match(&t.url)).collect();
    if matches.is_empty() {
        return Err(anyhow::anyhow!(
            "no target matched URL regex `{regex}`"
        ));
    }
    let mut last_id: Option<String> = None;
    let mut last_url: Option<String> = None;
    for t in &matches {
        last_id = Some(t.id.clone());
        last_url = Some(t.url.clone());
        let ok = matches!(
            tokio::time::timeout(PROBE, backend.evaluate(&t.id, "1", false, PROBE)).await,
            Ok(Ok(_))
        );
        if ok {
            return Ok(t.id.clone());
        }
    }
    Err(SessionError::TabHung {
        target_id: last_id,
        url: last_url,
        timeout_ms: PROBE.as_millis() as u64,
        hint: "all-matches-hung",
    }
    .into())
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerState").finish()
    }
}

/// Handler signature: takes `(state, params)` and returns a tool result.
pub type ToolHandler = std::sync::Arc<
    dyn Fn(ServerState, Value) -> futures_util::future::BoxFuture<'static, Result<Value>>
        + Send
        + Sync,
>;

pub struct RegisteredTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: ToolHandler,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    inner: std::sync::Arc<std::sync::Mutex<Vec<RegisteredTool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, t: RegisteredTool) {
        self.inner.lock().unwrap().push(t);
    }

    pub fn list(&self) -> Vec<Value> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                })
            })
            .collect()
    }

    pub fn handler(&self, name: &str) -> Option<ToolHandler> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.handler.clone())
    }
}

/// Run the server using the real stdin/stdout.
pub async fn run(state: ServerState, tools: ToolRegistry) -> Result<()> {
    run_with_streams(state, tools, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Run the server with injected I/O streams (used by tests).
pub async fn run_with_streams<I, O>(
    state: ServerState,
    tools: ToolRegistry,
    stdin: I,
    mut stdout: O,
) -> Result<()>
where
    I: tokio::io::AsyncRead + Unpin,
    O: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_error(
                    &mut stdout,
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                )
                .await?;
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        // Notifications: no id, no response.
        if id.is_null() && method == "notifications/initialized" {
            continue;
        }

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "browser-control",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools.list()})),
            "tools/call" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                match tools.handler(name) {
                    Some(h) => h(state.clone(), args).await,
                    None => Err(anyhow::anyhow!("tool not found: {name}")),
                }
            }
            _ => {
                write_error(
                    &mut stdout,
                    id,
                    -32601,
                    &format!("method not found: {method}"),
                )
                .await?;
                continue;
            }
        };

        match result {
            Ok(v) => write_result(&mut stdout, id, v).await?,
            Err(e) => write_error(&mut stdout, id, -32000, &e.to_string()).await?,
        }
    }
    Ok(())
}

async fn write_result<O: tokio::io::AsyncWrite + Unpin>(
    out: &mut O,
    id: Value,
    result: Value,
) -> Result<()> {
    let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let mut s = serde_json::to_vec(&resp)?;
    s.push(b'\n');
    out.write_all(&s).await?;
    out.flush().await?;
    Ok(())
}

async fn write_error<O: tokio::io::AsyncWrite + Unpin>(
    out: &mut O,
    id: Value,
    code: i64,
    message: &str,
) -> Result<()> {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    });
    let mut s = serde_json::to_vec(&resp)?;
    s.push(b'\n');
    out.write_all(&s).await?;
    out.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::env_resolver::Source;
    use crate::detect::Engine;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn dummy_resolved() -> ResolvedBrowser {
        ResolvedBrowser {
            endpoint: "ws://localhost:9999".into(),
            engine: Engine::Cdp,
            source: Source::External,
        }
    }

    fn dummy_state() -> ServerState {
        ServerState::new(dummy_resolved())
    }

    async fn send_recv(tools: ToolRegistry, requests: &[Value]) -> Vec<Value> {
        let (mut client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);
        let state = dummy_state();
        let join = tokio::spawn(async move {
            let _ = run_with_streams(state, tools, server_r, server_w).await;
        });

        for req in requests {
            let mut s = serde_json::to_vec(req).unwrap();
            s.push(b'\n');
            client_w.write_all(&s).await.unwrap();
        }
        // Closing the writer ends the server loop after it drains.
        drop(client_w);

        let mut reader = BufReader::new(client_r);
        let mut responses = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            if n == 0 {
                break;
            }
            responses.push(serde_json::from_str(line.trim()).unwrap());
        }
        let _ = join.await;
        responses
    }

    fn echo_tool() -> RegisteredTool {
        RegisteredTool {
            name: "echo".to_string(),
            description: "Echo arguments back".to_string(),
            input_schema: json!({"type": "object"}),
            handler: std::sync::Arc::new(|_state, args| {
                Box::pin(async move { Ok(json!({"echoed": args})) })
            }),
        }
    }

    #[tokio::test]
    async fn initialize_round_trip() {
        let resp = send_recv(
            ToolRegistry::new(),
            &[json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})],
        )
        .await;
        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0]["id"], 1);
        assert_eq!(resp[0]["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp[0]["result"]["serverInfo"]["name"], "browser-control");
    }

    #[tokio::test]
    async fn tools_list_empty() {
        let resp = send_recv(
            ToolRegistry::new(),
            &[json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})],
        )
        .await;
        assert_eq!(resp[0]["result"]["tools"], json!([]));
    }

    #[tokio::test]
    async fn tools_list_after_register() {
        let tools = ToolRegistry::new();
        tools.register(echo_tool());
        let resp = send_recv(
            tools,
            &[json!({"jsonrpc":"2.0","id":3,"method":"tools/list"})],
        )
        .await;
        let list = resp[0]["result"]["tools"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "echo");
    }

    #[tokio::test]
    async fn tools_call_unknown_errors() {
        let resp = send_recv(
            ToolRegistry::new(),
            &[json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"nope","arguments":{}}
            })],
        )
        .await;
        assert!(resp[0]["error"].is_object());
        assert!(resp[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nope"));
    }

    #[tokio::test]
    async fn tools_call_registered_returns_result() {
        let tools = ToolRegistry::new();
        tools.register(echo_tool());
        let resp = send_recv(
            tools,
            &[json!({
                "jsonrpc":"2.0","id":5,"method":"tools/call",
                "params":{"name":"echo","arguments":{"hello":"world"}}
            })],
        )
        .await;
        assert_eq!(resp[0]["result"]["echoed"], json!({"hello":"world"}));
    }

    #[tokio::test]
    async fn unknown_method_returns_minus_32601() {
        let resp = send_recv(
            ToolRegistry::new(),
            &[json!({"jsonrpc":"2.0","id":6,"method":"bogus"})],
        )
        .await;
        assert_eq!(resp[0]["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn ping_returns_empty_object() {
        let resp = send_recv(
            ToolRegistry::new(),
            &[json!({"jsonrpc":"2.0","id":7,"method":"ping"})],
        )
        .await;
        assert_eq!(resp[0]["result"], json!({}));
    }

    #[tokio::test]
    async fn initialized_notification_is_silently_ignored() {
        // Send notification, then a real request; we should only see the
        // response to the real request.
        let resp = send_recv(
            ToolRegistry::new(),
            &[
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                json!({"jsonrpc":"2.0","id":8,"method":"ping"}),
            ],
        )
        .await;
        assert_eq!(resp.len(), 1);
        assert_eq!(resp[0]["id"], 8);
    }
}
