//! Minimal hand-rolled MCP JSON-RPC server over stdio.
//!
//! This is the wave-3 skeleton. A future task may replace this with a more
//! capable framework (e.g. `rmcp`). The protocol surface is small:
//! newline-delimited JSON-RPC 2.0 over stdin/stdout.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
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
    /// MCP-owned origin tabs created by bare `browser_fetch`, keyed by
    /// requested origin root (`https://example.com/`). This supplements
    /// URL-based live-target matching so a tab that redirects to a login
    /// origin after token expiry is still reused on later fetches instead
    /// of creating one new tab per retry.
    pub origin_target_ids: Arc<Mutex<HashMap<String, String>>>,
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
    /// Operation barrier. Non-exclusive tool calls acquire a **read**
    /// guard so they can run concurrently; `switch_browser` acquires a
    /// **write** guard which waits for all in-flight tool operations to
    /// finish, preventing the old backend / BiDi session from being
    /// torn down while another tool is still using it.
    ///
    /// `browser_select` is the only tool that needs exclusive access
    /// (via `switch_browser`); `handle_tools_call` skips the read
    /// guard for it to avoid deadlocking with its own write guard.
    pub op_barrier: Arc<RwLock<()>>,
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
            origin_target_ids: Arc::new(Mutex::new(HashMap::new())),
            sidecar: Arc::new(Mutex::new(None)),
            sidecar_config,
            op_barrier: Arc::new(RwLock::new(())),
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
                hint: "use engine-agnostic tools such as browser_get_html, browser_fetch, browser_take_screenshot, or switch to a Chromium browser via browser_select",
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
            let resolved = self.browser_snapshot().await;
            // `bidi_lock_acquire` polls with a blocking `std::thread::sleep`
            // for up to 30s under contention. Run it on a blocking thread so
            // we never park a tokio worker (mirrors `resolve_browser_send`).
            // `Registry` is opened fresh inside and `resolved` is an owned
            // clone, so both move into the closure.
            let acquired = tokio::task::spawn_blocking(move || {
                let registry = Registry::open()?;
                acquire_bidi_lock_if_needed(&registry, &resolved)
            })
            .await??;
            *guard = match acquired {
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

    /// Resolve or create an MCP-owned tab for a fetch URL's origin.
    ///
    /// `TabBackend::resolve_or_create_for_origin` can only reuse targets
    /// whose current browser URL still has the requested origin. During auth
    /// expiry, an origin tab may redirect to an identity provider or login
    /// route; if we only inspect current URLs, each retry can create another
    /// tab. This cache records the target originally allocated for each
    /// requested origin and reuses it while it is still live.
    pub async fn resolve_or_create_for_origin(&self, url: &str) -> Result<(TabBackend, String)> {
        let want =
            url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid fetch URL `{url}`: {e}"))?;
        let origin_root = crate::session::attach::origin_root_url(&want);
        let backend = self.ensure_backend().await?;
        let mut origin_targets = self.origin_target_ids.lock().await;
        let live_targets = backend.live_targets().await?;
        let live_ids: std::collections::HashSet<&str> =
            live_targets.iter().map(|t| t.id.as_str()).collect();

        if let Some(cached) = origin_targets.get(&origin_root) {
            if live_ids.contains(cached.as_str()) {
                return Ok((backend, cached.clone()));
            }
            origin_targets.remove(&origin_root);
        }

        if let Some(existing) = live_targets.iter().find(|t| {
            url::Url::parse(&t.url)
                .map(|parsed| crate::session::attach::same_origin(&parsed, &want))
                .unwrap_or(false)
        }) {
            origin_targets.insert(origin_root, existing.id.clone());
            return Ok((backend, existing.id.clone()));
        }

        let new_tid = backend.create_tab(&origin_root).await?;
        origin_targets.insert(origin_root, new_tid.clone());
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
    pub async fn resolve_target_for_args(&self, args: &Value) -> Result<(TabBackend, String)> {
        let tab = args.get("tab").and_then(|v| v.as_str()).map(String::from);
        let target = args
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from);
        match (tab, target) {
            (Some(_), Some(_)) => Err(anyhow::anyhow!("`tab` and `target` are mutually exclusive")),
            (Some(name), None) => {
                let backend = self.ensure_backend().await?;
                let browser_name = self.registered_browser_name().await?;
                // Mimic `session::tabs::resolve_tab` here so we never hold
                // a `!Send` `Registry` across `.await`: sync registry-read,
                // async liveness probe, sync registry-mutate.
                let bn = browser_name.clone();
                let n = name.clone();
                let row = sync_registry_op(move |reg| reg.tab_get(&bn, &n))
                    .await?
                    .ok_or_else(|| crate::errors::SessionError::TabNotFound {
                        browser: browser_name.clone(),
                        name: name.clone(),
                    })?;
                let live = backend.live_target_ids().await?;
                if !live.contains(&row.target_id) {
                    // Stale — sweep, then error.
                    let bn = browser_name.clone();
                    let n = name.clone();
                    sync_registry_op(move |reg| reg.tab_delete(&bn, &n)).await?;
                    return Err(crate::errors::SessionError::TabNotFound {
                        browser: browser_name,
                        name,
                    }
                    .into());
                }
                let bn = browser_name.clone();
                let n = name.clone();
                sync_registry_op(move |reg| reg.tab_touch(&bn, &n)).await?;
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
    ///
    /// # Concurrency
    /// The caller must hold a **write** guard on [`Self::op_barrier`] to
    /// ensure no concurrent tool call is still using the old backend.
    /// [`handle_tools_call`] acquires the write guard for `browser_select`
    /// before invoking this method.
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
        // Clear origin-bound fetch target cache.
        {
            let mut origins = self.origin_target_ids.lock().await;
            origins.clear();
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
///
/// The closure runs on a blocking thread via `tokio::task::spawn_blocking`
/// (mirroring `resolve_browser_send`) so the synchronous `rusqlite` work
/// and the blocking exclusive `flock` taken across schema migration in
/// `Registry::open` never park a tokio worker. The `!Send` `Registry` is
/// created and dropped entirely inside the closure, so it never crosses an
/// `.await`.
pub(crate) async fn sync_registry_op<T, F>(f: F) -> Result<T>
where
    F: FnOnce(&crate::registry::Registry) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let reg = crate::registry::Registry::open()?;
        f(&reg)
    })
    .await?
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
        other => {
            tokio::task::spawn_blocking(move || {
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
            .await?
        }
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
        return Err(anyhow::anyhow!("no target matched URL regex `{regex}`"));
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
///
/// Requests are dispatched concurrently: `tools/call` handlers are spawned
/// on their own tasks so a slow op (30s BiDi lock, sidecar `npm install`,
/// 30s CDP timeout) never blocks `ping`, `tools/list`, or other calls on
/// the connection. The read loop keeps pulling lines while handlers run.
///
/// Stdout is owned by a single writer task fed over an mpsc channel, so
/// all response frames are serialized to the wire one at a time even though
/// they are produced concurrently — no interleaved partial writes. Per
/// JSON-RPC, response ordering is correlated by `id`, so out-of-order
/// completion is sound. Correctness of shared backend access is unchanged:
/// it is still serialized by the `ServerState` locks.
pub async fn run_with_streams<I, O>(
    state: ServerState,
    tools: ToolRegistry,
    stdin: I,
    mut stdout: O,
) -> Result<()>
where
    I: tokio::io::AsyncRead + Unpin,
    O: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Single writer task owns stdout; every response frame (already
    // serialized to bytes incl. trailing newline) flows through here so
    // concurrent handlers can never interleave their writes.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if stdout.write_all(&frame).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(stdin).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(error_frame(
                    Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ));
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

        // Dispatch. `initialize` / `ping` / `tools/list` are cheap synchronous
        // frame builders; `tools/call` spawns its handler so the read loop
        // stays responsive while a slow op runs. Each arm sends through the
        // single writer task, preserving serialized stdout writes.
        match method {
            "initialize" => {
                let _ = tx.send(handle_initialize(id));
            }
            "ping" => {
                let _ = tx.send(handle_ping(id));
            }
            "tools/list" => {
                let _ = tx.send(handle_tools_list(id, &tools));
            }
            "tools/call" => {
                handle_tools_call(id, &params, &state, &tools, &tx);
            }
            _ => {
                let _ = tx.send(error_frame(
                    id,
                    -32601,
                    &format!("method not found: {method}"),
                ));
            }
        }
    }
    // stdin closed: drop our sender so the writer drains and exits, then
    // wait for it so all buffered responses reach the wire before returning.
    drop(tx);
    let _ = writer.await;
    Ok(())
}

/// Build the `initialize` response frame: advertise the protocol version,
/// the (tools-only) capability set, and server identity.
fn handle_initialize(id: Value) -> Vec<u8> {
    result_frame(
        id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "browser-control",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

/// Build the `ping` response frame (an empty result object).
fn handle_ping(id: Value) -> Vec<u8> {
    result_frame(id, json!({}))
}

/// Build the `tools/list` response frame from the registry.
fn handle_tools_list(id: Value, tools: &ToolRegistry) -> Vec<u8> {
    result_frame(id, json!({"tools": tools.list()}))
}

/// Dispatch a `tools/call` request. The handler is spawned on its own task
/// so the read loop stays responsive while a slow op (30s BiDi lock,
/// sidecar `npm install`, 30s CDP timeout) runs; the completed frame is
/// sent to the single writer task, preserving serialized stdout writes.
///
/// Concurrency guard: `browser_select` (which calls `switch_browser`)
/// acquires a **write** guard on `state.op_barrier`, blocking until all
/// concurrent tool calls finish. Every other tool acquires a **read**
/// guard, ensuring they cannot overlap with the destructive browser
/// switch.
fn handle_tools_call(
    id: Value,
    params: &Value,
    state: &ServerState,
    tools: &ToolRegistry,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let handler = tools.handler(&name);
    let state = state.clone();
    let tx = tx.clone();
    let barrier = state.op_barrier.clone();
    let is_exclusive = name == "browser_select";
    tokio::spawn(async move {
        let frame = match handler {
            // An unknown tool name is bad params, not a tool
            // failure: keep it a genuine `-32602` protocol error.
            None => error_frame(id, -32602, &format!("tool not found: {name}")),
            Some(h) => {
                if is_exclusive {
                    // Wait for every in-flight non-exclusive tool call
                    // to finish, then hold the write guard for the
                    // entire handler so no new tool call can start
                    // until the switch is complete.
                    let _guard = barrier.write().await;
                    match h(state, args).await {
                        Ok(v) => result_frame(id, v),
                        Err(e) => tool_error_frame(id, &e),
                    }
                } else {
                    let _guard = barrier.read().await;
                    match h(state, args).await {
                        Ok(v) => result_frame(id, v),
                        Err(e) => tool_error_frame(id, &e),
                    }
                }
            }
        };
        let _ = tx.send(frame);
    });
}

/// Serialize a JSON-RPC success response to a newline-terminated frame.
fn result_frame(id: Value, result: Value) -> Vec<u8> {
    let resp = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let mut s = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
    s.push(b'\n');
    s
}

/// Serialize a *tool-execution* failure as a successful JSON-RPC result with
/// `isError: true`, per the MCP spec (tool failures are not protocol faults —
/// the agent reads the content and recovers). Typed errors are downcast so the
/// content message carries their structure (e.g. the recover-once hint, or the
/// BiDi lock holder PID) instead of an opaque flattened string.
fn tool_error_frame(id: Value, err: &anyhow::Error) -> Vec<u8> {
    let message = tool_error_message(err);
    result_frame(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    )
}

/// Build the human/agent-readable message for a failed tool call. Downcasts
/// the typed error variants the agent can act on so their machine-relevant
/// fields survive (rather than relying on `Display` alone), falling back to the
/// full `anyhow` chain otherwise.
fn tool_error_message(err: &anyhow::Error) -> String {
    use crate::errors::SessionError;
    use crate::registry::bidi_lock::BidiLockBusy;

    if let Some(se) = err.downcast_ref::<SessionError>() {
        // SessionError's Display already encodes target/url/hint and the
        // EngineUnsupported recovery hint, so reuse it verbatim.
        return se.to_string();
    }
    if let Some(busy) = err.downcast_ref::<BidiLockBusy>() {
        return busy.to_string();
    }
    // Unknown failure: surface the whole context chain ("{:#}") so causes
    // aren't lost.
    format!("{err:#}")
}

/// Serialize a JSON-RPC error response to a newline-terminated frame.
fn error_frame(id: Value, code: i64, message: &str) -> Vec<u8> {
    let resp = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    });
    let mut s = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{}".to_vec());
    s.push(b'\n');
    s
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

    fn failing_tool() -> RegisteredTool {
        RegisteredTool {
            name: "boom".to_string(),
            description: "Always fails with a typed SessionError".to_string(),
            input_schema: json!({"type": "object"}),
            handler: std::sync::Arc::new(|_state, _args| {
                Box::pin(async move {
                    Err(anyhow::Error::new(crate::errors::SessionError::TabHung {
                        target_id: Some("T1".into()),
                        url: Some("https://example.test".into()),
                        timeout_ms: 20_000,
                        hint: "renderer wedged",
                    }))
                })
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
        // Unknown tool name is bad params — a genuine protocol fault.
        assert_eq!(resp[0]["error"]["code"], -32602);
        assert!(resp[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nope"));
    }

    #[tokio::test]
    async fn tool_failure_returns_iserror_result_not_protocol_error() {
        let tools = ToolRegistry::new();
        tools.register(failing_tool());
        let resp = send_recv(
            tools,
            &[json!({
                "jsonrpc":"2.0","id":7,"method":"tools/call",
                "params":{"name":"boom","arguments":{}}
            })],
        )
        .await;
        // Spec-compliant: a tool that executes and fails is a *successful*
        // JSON-RPC result carrying `isError: true`, not a `-32xxx` fault.
        assert!(resp[0]["error"].is_null());
        assert_eq!(resp[0]["result"]["isError"], true);
        let text = resp[0]["result"]["content"][0]["text"].as_str().unwrap();
        // Typed SessionError structure survives (target id + hint).
        assert!(text.contains("tab hung"), "got: {text}");
        assert!(text.contains("renderer wedged"), "got: {text}");
        assert!(text.contains("T1"), "got: {text}");
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

    // -- resolve_target_for_args + concurrent ensure_backend ----------------
    //
    // These drive the real `ServerState` against an in-process mock CDP
    // WebSocket server (no browser) and a temp-dir registry (via
    // `BROWSER_CONTROL_DATA_DIR`). The mock counts accepted connections so we
    // can assert `ensure_backend`'s double-checked locking opens exactly one
    // backend under concurrency.

    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::Message;

    /// A CDP mock that reports a fixed set of live targets via
    /// `Target.getTargets` and counts how many WebSocket connections it
    /// accepts. Returns `(ws_url, connections_accepted, stop_tx)`.
    async fn spawn_counting_cdp_mock(
        live: Vec<String>,
    ) -> (String, Arc<AtomicUsize>, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns_srv = conns.clone();
        tokio::spawn(async move {
            loop {
                let accept = tokio::select! {
                    _ = &mut stop_rx => break,
                    a = listener.accept() => a,
                };
                let (stream, _) = match accept {
                    Ok(s) => s,
                    Err(_) => break,
                };
                conns_srv.fetch_add(1, Ordering::SeqCst);
                let live = live.clone();
                tokio::spawn(async move {
                    let mut ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(w) => w,
                        Err(_) => return,
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        if let Message::Text(t) = msg {
                            let req: Value = match serde_json::from_str(&t) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let id = req["id"].as_u64().unwrap_or(0);
                            let method = req["method"].as_str().unwrap_or("");
                            let result = match method {
                                "Target.getTargets" => {
                                    let infos: Vec<Value> = live
                                        .iter()
                                        .map(|tid| {
                                            json!({"targetId": tid, "type": "page", "url": ""})
                                        })
                                        .collect();
                                    json!({"targetInfos": infos})
                                }
                                "Target.attachToTarget" => json!({"sessionId": "S1"}),
                                "Runtime.evaluate" => json!({"result": {"value": 1}}),
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            if ws.send(Message::Text(resp.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                });
            }
        });
        (format!("ws://{addr}"), conns, stop_tx)
    }

    /// Build a `ServerState` for a *registered* browser (named-tab routing
    /// requires a stable identity) pointing at `endpoint`.
    fn registered_state(name: &str, endpoint: &str) -> ServerState {
        ServerState::new(ResolvedBrowser {
            endpoint: endpoint.to_string(),
            engine: Engine::Cdp,
            source: Source::Registered { name: name.into() },
        })
    }

    // Holds the synchronous ENV_LOCK across awaits on purpose: it serializes
    // the whole env-mutating test against the rest of the suite.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_named_tab_live_resolves_and_touches() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_DATA_DIR", tmp.path());

        // Register a named tab whose target is live in the mock.
        {
            let reg = crate::registry::Registry::open().unwrap();
            reg.tab_upsert("bx", "work", "T1", "about:blank", true)
                .unwrap();
        }

        let (url, _conns, _stop) = spawn_counting_cdp_mock(vec!["T1".into()]).await;
        let state = registered_state("bx", &url);

        let target_id = match state.resolve_target_for_args(&json!({"tab": "work"})).await {
            Ok((_backend, tid)) => tid,
            Err(e) => panic!("named tab should resolve: {e:#}"),
        };
        assert_eq!(target_id, "T1");

        std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
    }

    // See note above: ENV_LOCK is intentionally held across awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_named_tab_stale_sweeps_and_errors() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_DATA_DIR", tmp.path());

        // Register a named tab whose target is NOT among the mock's live
        // targets — the resolve must sweep the stale row and return
        // TabNotFound.
        {
            let reg = crate::registry::Registry::open().unwrap();
            reg.tab_upsert("bx", "gone", "T_DEAD", "about:blank", true)
                .unwrap();
        }

        let (url, _conns, _stop) = spawn_counting_cdp_mock(vec!["T1".into()]).await;
        let state = registered_state("bx", &url);

        let err = match state.resolve_target_for_args(&json!({"tab": "gone"})).await {
            Ok(_) => panic!("stale named tab must error"),
            Err(e) => e,
        };
        let typed = err
            .downcast_ref::<crate::errors::SessionError>()
            .expect("typed SessionError");
        assert!(
            matches!(typed, crate::errors::SessionError::TabNotFound { .. }),
            "expected TabNotFound, got {typed:?}"
        );

        // The stale row must have been swept.
        let reg = crate::registry::Registry::open().unwrap();
        assert!(
            reg.tab_get("bx", "gone").unwrap().is_none(),
            "row not swept"
        );

        std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
    }

    // See note above: ENV_LOCK is intentionally held across awaits.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn resolve_named_tab_missing_row_errors() {
        let _g = crate::test_support::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("BROWSER_CONTROL_DATA_DIR", tmp.path());

        let (url, _conns, _stop) = spawn_counting_cdp_mock(vec!["T1".into()]).await;
        let state = registered_state("bx", &url);

        let err = match state.resolve_target_for_args(&json!({"tab": "nope"})).await {
            Ok(_) => panic!("unknown named tab must error"),
            Err(e) => e,
        };
        let typed = err
            .downcast_ref::<crate::errors::SessionError>()
            .expect("typed SessionError");
        assert!(
            matches!(typed, crate::errors::SessionError::TabNotFound { .. }),
            "expected TabNotFound, got {typed:?}"
        );

        std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
    }

    #[tokio::test]
    async fn resolve_tab_and_target_mutually_exclusive() {
        // No registry / backend needed: the guard fires first.
        let state = dummy_state();
        let err = match state
            .resolve_target_for_args(&json!({"tab": "a", "target": "b"}))
            .await
        {
            Ok(_) => panic!("tab+target must error"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("mutually exclusive"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn concurrent_ensure_backend_opens_one_backend() {
        // Fire many concurrent `ensure_backend` calls against a state whose
        // double-checked lock must open exactly one backend (one WS
        // connection to the mock). Guards future refactors of the lock.
        let (url, conns, _stop) = spawn_counting_cdp_mock(vec!["T1".into()]).await;
        let state = ServerState::new(ResolvedBrowser {
            endpoint: url,
            engine: Engine::Cdp,
            source: Source::External,
        });

        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = state.clone();
            handles.push(tokio::spawn(async move { s.ensure_backend().await }));
        }
        for h in handles {
            h.await.unwrap().expect("ensure_backend should succeed");
        }
        assert_eq!(
            conns.load(Ordering::SeqCst),
            1,
            "expected exactly one backend (one WS connection) under concurrency"
        );
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
