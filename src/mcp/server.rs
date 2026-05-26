//! Minimal hand-rolled MCP JSON-RPC server over stdio.
//!
//! This is the wave-3 skeleton. A future task may replace this with a more
//! capable framework (e.g. `rmcp`). The protocol surface is small:
//! newline-delimited JSON-RPC 2.0 over stdin/stdout.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::cli::env_resolver::ResolvedBrowser;
use crate::session::backend::TabBackend;

/// Persistent BiDi client, opened lazily on first use. Reused across all
/// tool calls because Firefox limits concurrent BiDi sessions per browser
/// to one. The browsing-context id is resolved per call so multiple tabs
/// can be addressed once URL-regex selection is added.
pub type BidiCache =
    std::sync::Arc<tokio::sync::Mutex<Option<std::sync::Arc<crate::bidi::BidiClient>>>>;

/// State carried by the server. Tools reach into this for the resolved
/// browser endpoint and any cached engine clients.
#[derive(Clone)]
pub struct ServerState {
    pub browser: ResolvedBrowser,
    pub bidi: BidiCache,
    /// Firefox BiDi single-session lock, acquired lazily on first tool
    /// call and held for the server's lifetime. `None` for CDP browsers
    /// and external endpoints (where `acquire_bidi_lock_if_needed`
    /// returns None) — the inner `Option<BidiLockGuard>` distinguishes
    /// "haven't tried yet" from "tried, not applicable" via the outer
    /// `Mutex` being unlocked vs returning None.
    pub bidi_lock: std::sync::Arc<tokio::sync::Mutex<BidiLockState>>,
    /// Cached [`TabBackend`] for the configured browser, opened lazily
    /// on first tool call and reused for the server's lifetime. Avoids
    /// repeatedly running the BiDi `session.new` handshake and lets us
    /// share one CDP WebSocket across all tool calls.
    pub backend: std::sync::Arc<tokio::sync::Mutex<Option<TabBackend>>>,
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
        Self {
            browser,
            bidi: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            bidi_lock: std::sync::Arc::new(tokio::sync::Mutex::new(BidiLockState::Pending)),
            backend: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
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
            *guard = match acquire_bidi_lock_if_needed(&registry, &self.browser)? {
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
        let b = crate::session::backend::open_backend(&self.browser.endpoint, self.browser.engine)
            .await?;
        *guard = Some(b.clone());
        Ok(b)
    }

    /// Lazy-open the **MCP server's** "active tab" — a daemon-created
    /// row in the `tabs` table keyed by the server's pid (so concurrent
    /// MCP servers against the same browser get distinct active tabs)
    /// and the registered browser name.
    ///
    /// The returned `(backend, target_id)` is the routing pair every
    /// stateful MCP tool (`navigate`, `get_dom`, `screenshot`,
    /// `select_element`, `fetch`) operates against. **Closes the iLO
    /// failure mode for MCP**: tools no longer call
    /// `PageSession::attach(..., None)` which would pick the first page
    /// in `Target.getTargets` order (an iLO admin UI etc.).
    ///
    /// On a tab that's died since last use (`target_id` no longer in
    /// the live browser), recreates fresh `about:blank` under the same
    /// name. Agents that need a specific URL navigate via the
    /// `navigate` tool.
    ///
    /// External URL endpoints (no registered browser name) error: MCP
    /// is meaningful only against a registered browser whose state can
    /// outlive a single CLI invocation.
    ///
    /// This implementation does not call `session::tabs::tab_open`
    /// because `Registry` is `!Send` (rusqlite `Connection` holds a
    /// `RefCell`), and MCP tool handlers are `Send` futures. Instead we
    /// open + close the Registry around each sync SQL op, never holding
    /// it across an `.await` of a CDP/BiDi call.
    pub async fn ensure_active_tab(&self) -> Result<(TabBackend, String)> {
        use crate::cli::env_resolver::Source;
        use crate::registry::Registry;

        let backend = self.ensure_backend().await?;
        let browser_name = match &self.browser.source {
            Source::Registered { name } => name.clone(),
            Source::External => {
                return Err(anyhow::anyhow!(
                    "MCP server requires a registered browser; external URL endpoints \
                     don't have a stable identity for a server-owned active tab"
                ));
            }
        };
        // Name scoped per MCP server PID so two MCPs against the same
        // browser don't fight over the same row. Leading `_` keeps it
        // out of the agent-visible namespace (validate_tab_name rejects
        // it; the registry helpers accept it).
        let name = format!("_mcp-{}", std::process::id());

        // Sync: look up the existing row.
        let existing = {
            let registry = Registry::open()?;
            registry.tab_get(&browser_name, &name)?
        };

        // Async: validate the cached target id is still alive.
        if let Some(row) = existing {
            let live = backend.live_target_ids().await?;
            if live.contains(&row.target_id) {
                // Sync: bump last_used_at.
                let registry = Registry::open()?;
                registry.tab_touch(&browser_name, &name)?;
                return Ok((backend, row.target_id));
            }
            // Stale — close best-effort + delete row. CDP/BiDi roundtrip
            // is async; SQL delete is sync.
            let _ = backend.close_tab(&row.target_id).await;
            let registry = Registry::open()?;
            registry.tab_delete(&browser_name, &name)?;
        }

        // Async: create a fresh `about:blank` tab.
        let new_target_id = backend.create_tab("about:blank").await?;

        // Sync: insert the row.
        {
            let registry = Registry::open()?;
            registry.tab_upsert(&browser_name, &name, &new_target_id, "about:blank", true)?;
        }
        Ok((backend, new_target_id))
    }
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerState")
            .field("browser", &self.browser)
            .finish()
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
