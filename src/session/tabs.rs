//! Named-tab orchestration: SQLite + the engine-agnostic [`TabBackend`]
//! working together.
//!
//! The registry layer (`crate::registry::tabs`) is pure SQL CRUD. This
//! module adds the logic the agent contract actually requires:
//!
//! - **Get-or-create**: `tab_open(_, name, url)` returns an existing
//!   live tab under that name, or creates a fresh `about:blank` (or the
//!   given `url`) and registers it.
//! - **Navigate-on-mismatch**: if the existing tab's `last_url` doesn't
//!   match the requested `url`, run navigate first.
//! - **Sweep-on-read**: stale rows whose `target_id` no longer exists in
//!   the live browser are dropped before they're returned to the caller.
//! - **Budget pressure**: when daemon-created rows exceed
//!   [`HARD_CAP`], the LRU is closed and recycled.
//! - **Cute names**: agents who pass no `--name` get
//!   `tab-<cute-word>` from the same generator that names browsers.
//!
//! The same code path serves CDP (Chromium) and BiDi (Firefox) browsers
//! via [`TabBackend`] — CDP `targetId` and BiDi `context` are both opaque
//! ids stored in the `tabs.target_id` column. The registry doesn't care
//! which engine produced the id.

use anyhow::{anyhow, Context, Result};
use rand::Rng;

use crate::errors::SessionError;
use crate::registry::{words::WORDS, Registry, TabRow};
use crate::session::backend::TabBackend;

/// Hard cap on `daemon_created` rows per browser. Hitting this triggers
/// LRU close+recreate of the oldest daemon-created tab. Chromium itself
/// starts to struggle around a few hundred tabs depending on RAM; 50 is
/// comfortably under that ceiling and large enough that even busy
/// scraping agents won't routinely hit it.
pub const HARD_CAP: usize = 50;

/// Open a named tab (get-or-create), navigating if `url` differs from the
/// existing tab's `last_url`. Returns the up-to-date row.
///
/// Behaviour:
/// - `name = None`: always create a fresh tab; assign a cute name.
/// - `name = Some(n)`:
///   - if row `(browser, n)` exists and its `target_id` is alive →
///     navigate-on-mismatch and return.
///   - if row exists but `target_id` is stale → close (best-effort),
///     delete row, fall through to create.
///   - if no row → create.
/// - `url = None`: defaults to `about:blank`.
///
/// On create, if `daemon_created` rows ≥ [`HARD_CAP`], close the LRU
/// first (`Target.closeTarget` + `tab_delete`) to free a slot.
pub async fn tab_open(
    backend: &TabBackend,
    registry: &Registry,
    browser_name: &str,
    name: Option<&str>,
    url: Option<&str>,
) -> Result<TabRow> {
    let want_url = url.unwrap_or("about:blank");

    // Fast path: named tab exists and is alive → maybe navigate, return.
    if let Some(requested_name) = name {
        if let Some(existing) = registry.tab_get(browser_name, requested_name)? {
            let mut died_mid_navigate = false;
            if target_alive(backend, &existing.target_id).await? {
                if !want_url.is_empty() && want_url != existing.last_url && url.is_some() {
                    // Navigate-on-mismatch. The target was alive a moment
                    // ago, but a probe-then-act race means it may have died
                    // (crash/hang) between `target_alive` and here. A
                    // recoverable tab failure must NOT surface raw at the
                    // caller — fall through to the stale-row close+delete+
                    // create path below, mirroring the eval/fetch/storage
                    // contract enforced by `with_named_tab_recovery`.
                    match backend.navigate(&existing.target_id, want_url).await {
                        Ok(()) => {
                            registry.tab_set_url(browser_name, requested_name, want_url)?;
                        }
                        Err(e) if is_tab_failure(&e) => died_mid_navigate = true,
                        Err(e) => {
                            return Err(e).with_context(|| {
                                format!("navigating {browser_name}/{requested_name} to {want_url}")
                            });
                        }
                    }
                } else {
                    registry.tab_touch(browser_name, requested_name)?;
                }
                if !died_mid_navigate {
                    return registry
                        .tab_get(browser_name, requested_name)?
                        .ok_or_else(|| anyhow!("tab row vanished between lookups"));
                }
            }
            // Stale row (probe failed) or the tab died mid-navigate → close
            // best-effort + delete + fall through to create.
            let _ = backend.close_tab(&existing.target_id).await;
            registry.tab_delete(browser_name, requested_name)?;
        }
    }

    // Create path. Enforce budget first.
    if registry.tabs_count_daemon_created(browser_name)? >= HARD_CAP {
        if let Some(victim) = registry.tabs_lru_daemon_created(browser_name)? {
            let _ = backend.close_tab(&victim.target_id).await;
            registry.tab_delete(&victim.browser_name, &victim.name)?;
        }
    }

    let assigned_name = match name {
        Some(n) => n.to_string(),
        None => fresh_cute_name(registry, browser_name)?,
    };
    let new_target_id = backend.create_tab(want_url).await?;
    registry.tab_upsert(browser_name, &assigned_name, &new_target_id, want_url, true)?;
    registry
        .tab_get(browser_name, &assigned_name)?
        .ok_or_else(|| anyhow!("tab row missing immediately after upsert"))
}

/// `tab list <browser>` backend with sweep-on-read.
///
/// Asks the [`TabBackend`] for the live id set and drops any tab rows
/// whose `target_id` is no longer present (closed externally, browser
/// restarted, etc.).
pub async fn tab_list(
    backend: &TabBackend,
    registry: &Registry,
    browser_name: &str,
) -> Result<Vec<TabRow>> {
    let live_targets = backend.live_target_ids().await?;
    let mut rows = registry.tabs_list_for(browser_name)?;
    let mut keep = Vec::with_capacity(rows.len());
    rows.retain(|r| {
        let alive = live_targets.contains(&r.target_id);
        if !alive {
            let _ = registry.tab_delete(&r.browser_name, &r.name);
        }
        alive
    });
    keep.append(&mut rows);
    Ok(keep)
}

/// Resolve `<browser>/<name>` to a live tab row for cross-command routing
/// (eval, fetch, etc.). Returns `Ok(None)` if no row matches or the row's
/// `target_id` no longer exists in the browser (`TabNotFound` at the call
/// site).
pub async fn resolve_tab(
    backend: &TabBackend,
    registry: &Registry,
    browser_name: &str,
    name: &str,
) -> Result<Option<TabRow>> {
    let Some(row) = registry.tab_get(browser_name, name)? else {
        return Ok(None);
    };
    if target_alive(backend, &row.target_id).await? {
        registry.tab_touch(browser_name, name)?;
        Ok(Some(row))
    } else {
        registry.tab_delete(browser_name, name)?;
        Ok(None)
    }
}

async fn target_alive(backend: &TabBackend, target_id: &str) -> Result<bool> {
    let live = backend.live_target_ids().await?;
    Ok(live.contains(target_id))
}

/// Run `op` against the named tab `<browser>/<name>` with one round of
/// recover-and-retry on tab failures. Mirrors [`crate::session::with_scratch_recovery`]
/// but for the agent-owned named-tab path.
///
/// Semantics, in order:
///
/// 1. Resolve the row via [`resolve_tab`]. If the row is missing or its
///    `target_id` is stale, surface a typed `SessionError::TabNotFound` —
///    we do NOT auto-create a tab the agent never asked for.
/// 2. Run `op` against the live `target_id`.
/// 3. If `op` returns a recoverable failure (`TabHung`, `TabCrashed`, or a
///    CDP/BiDi "no target / no context" protocol error), the tab died
///    between resolve and op. **Leave the dead tab in the browser** — the
///    agent named it, the corpse is human-inspectable. Create a fresh
///    tab, navigate it to the row's `last_url` (best-effort; falls back
///    to `about:blank` if the rehydration navigation itself fails), and
///    re-point the registry row at it under the **same name**, then
///    retry `op` once. After recovery, `<browser>/<name>` resolves to
///    the new live tab pointing at the same URL the agent last saw it
///    at; the dead tab is no longer addressable by name (and `tab list`
///    won't show it, since sweep-on-read checks against `live_target_ids`).
/// 4. If the retry also fails, escalate the typed error to the caller.
pub async fn with_named_tab_recovery<F, T, Fut>(
    backend: &TabBackend,
    registry: &Registry,
    browser_name: &str,
    tab_name: &str,
    mut op: F,
) -> Result<T>
where
    F: FnMut(TabBackend, String) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    // Step 1: resolve. `resolve_tab` already sweeps stale rows internally
    // (deletes the row if its target_id no longer exists in the browser).
    let row = match resolve_tab(backend, registry, browser_name, tab_name).await? {
        Some(r) => r,
        None => {
            return Err(SessionError::TabNotFound {
                browser: browser_name.to_string(),
                name: tab_name.to_string(),
            }
            .into());
        }
    };

    // Step 2: first attempt.
    // The `Ok(value)` branch returns early; the failure branch falls
    // through to attempt 2. Clippy reads the early return as "needless"
    // because the failure branch also has `return Err(e)` — but
    // restructuring (e.g. via `if let`) makes the recover-once
    // contract less obvious.
    #[allow(clippy::needless_return)]
    match op(backend.clone(), row.target_id.clone()).await {
        Ok(value) => return Ok(value),
        Err(e) if is_tab_failure(&e) => {
            // Step 3: recover. Tab died between resolve and op.
            //
            // Do NOT close the dead tab. The agent named it; the corpse
            // stays in the browser so the human can inspect it. The
            // registry row gets re-pointed at a fresh tab under the
            // same name — addressing-by-name now resolves to the new
            // live tab, the dead tab is no longer reachable via
            // `<browser>/<name>` and falls off `tab list` (sweep-on-read
            // checks the row's target_id against live_target_ids).
            //
            // Rehydrate `last_url`: if the dead tab was at a real URL,
            // navigate the new tab there before retrying so the agent
            // sees its addressable state preserved. Best-effort — if
            // navigation itself fails (origin gone, network down) we
            // fall back to blank rather than block recovery.
            let rehydrate_url = if row.last_url.is_empty() || row.last_url == "about:blank" {
                "about:blank".to_string()
            } else {
                row.last_url.clone()
            };
            let new_target_id = backend.create_tab("about:blank").await?;
            let (stored_url, ready_target) = if rehydrate_url != "about:blank" {
                match backend.navigate(&new_target_id, &rehydrate_url).await {
                    Ok(()) => (rehydrate_url, new_target_id),
                    Err(nav_err) => {
                        tracing::warn!(
                            target = "session::tabs",
                            "rehydrating {browser_name}/{tab_name} to {rehydrate_url} failed: {nav_err:#}; falling back to about:blank"
                        );
                        ("about:blank".to_string(), new_target_id)
                    }
                }
            } else {
                ("about:blank".to_string(), new_target_id)
            };
            registry.tab_upsert(browser_name, tab_name, &ready_target, &stored_url, true)?;
            // Step 4: retry once.
            op(backend.clone(), ready_target).await
        }
        Err(e) => Err(e),
    }
}

/// Does this error suggest the named tab is dead and we should retry on
/// a fresh one? Delegates to the shared classifier in `errors` so the
/// scratch / named-tab / origin-bound recovery wrappers can never drift.
fn is_tab_failure(err: &anyhow::Error) -> bool {
    crate::errors::is_recoverable_tab_failure(err)
}

fn fresh_cute_name(registry: &Registry, browser_name: &str) -> Result<String> {
    let mut rng = rand::thread_rng();
    for _ in 0..20 {
        let word = WORDS[rng.gen_range(0..WORDS.len())];
        let base = format!("tab-{word}");
        if registry.tab_get(browser_name, &base)?.is_none() {
            return Ok(base);
        }
        for n in 2..=1000 {
            let candidate = format!("tab-{word}-{n}");
            if registry.tab_get(browser_name, &candidate)?.is_none() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "failed to generate a unique tab name after 20 attempts"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::CdpClient;
    use crate::detect::Engine;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    /// Build a CDP TabBackend backed by `spawn_mock`. Each test holds the
    /// returned `_stop` to keep the mock alive.
    async fn cdp_backend() -> (TabBackend, oneshot::Sender<()>) {
        let (url, stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        (TabBackend::Cdp(client), stop)
    }

    /// Build a BiDi TabBackend backed by `spawn_bidi_mock`. Mirror of
    /// `cdp_backend` so the same test logic exercises both engines.
    async fn bidi_backend() -> (TabBackend, oneshot::Sender<()>) {
        let (url, stop) = spawn_bidi_mock().await;
        let backend = crate::session::backend::open_backend(&url, Engine::Bidi)
            .await
            .unwrap();
        (backend, stop)
    }

    /// Mock CDP server backing tabs tests. Tracks created targets,
    /// supports closing, attach/navigate/detach.
    async fn spawn_mock() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            // We track which target ids are currently alive so
            // Target.getTargets responses are correct.
            let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            let result = match method {
                                "Target.createTarget" => {
                                    next_target += 1;
                                    let tid = format!("T{next_target}");
                                    live.insert(tid.clone());
                                    json!({"targetId": tid})
                                }
                                "Target.closeTarget" => {
                                    if let Some(tid) = req
                                        .pointer("/params/targetId")
                                        .and_then(|v| v.as_str())
                                    {
                                        live.remove(tid);
                                    }
                                    json!({"success": true})
                                }
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    json!({"sessionId": format!("S{next_session}")})
                                }
                                "Target.detachFromTarget" => json!({}),
                                "Page.navigate" => json!({}),
                                "Target.getTargets" => {
                                    let infos: Vec<Value> = live
                                        .iter()
                                        .map(|tid| {
                                            json!({"targetId": tid, "type": "page", "url": ""})
                                        })
                                        .collect();
                                    json!({"targetInfos": infos})
                                }
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    /// Mock BiDi server for parallel-engine tests. Mirrors the shape of
    /// `spawn_mock` (CDP), tracking live contexts so getTree responses
    /// stay accurate after create/close.
    async fn spawn_bidi_mock() -> (String, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_ctx = 0u32;
            let mut live = std::collections::HashSet::<String>::new();
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = ws.next() => {
                        let msg = match msg {
                            Some(Ok(m)) => m,
                            _ => break,
                        };
                        if let Message::Text(t) = msg {
                            let req: Value = serde_json::from_str(&t).unwrap();
                            let id = req["id"].as_u64().unwrap();
                            let method = req["method"].as_str().unwrap_or("");
                            let result = match method {
                                "session.new" => json!({"sessionId": "S1", "capabilities": {}}),
                                "browsingContext.create" => {
                                    next_ctx += 1;
                                    let c = format!("C{next_ctx}");
                                    live.insert(c.clone());
                                    json!({"context": c})
                                }
                                "browsingContext.close" => {
                                    if let Some(c) = req
                                        .pointer("/params/context")
                                        .and_then(|v| v.as_str())
                                    {
                                        live.remove(c);
                                    }
                                    json!({})
                                }
                                "browsingContext.navigate" => json!({"navigation": "N1"}),
                                "browsingContext.getTree" => {
                                    let contexts: Vec<Value> = live
                                        .iter()
                                        .map(|c| json!({"context": c, "url": "", "children": []}))
                                        .collect();
                                    json!({"contexts": contexts})
                                }
                                _ => json!({}),
                            };
                            let resp = json!({"type": "success", "id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        });
        (format!("ws://{addr}"), stop_tx)
    }

    // ---- CDP-engine tests ------------------------------------------------

    #[tokio::test]
    async fn open_without_name_assigns_cute_name_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let row = tab_open(&backend, &reg, "brave", None, None).await.unwrap();
        assert!(row.name.starts_with("tab-"));
        assert_eq!(row.target_id, "T1");
        assert!(row.daemon_created);
        assert_eq!(row.last_url, "about:blank");
    }

    #[tokio::test]
    async fn open_with_name_is_idempotent_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&backend, &reg, "b", Some("scrape"), None)
            .await
            .unwrap();
        let b = tab_open(&backend, &reg, "b", Some("scrape"), None)
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id);
        assert_eq!(a.name, b.name);
    }

    #[tokio::test]
    async fn open_with_mismatched_url_navigates_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&backend, &reg, "b", Some("nav"), Some("https://a"))
            .await
            .unwrap();
        let b = tab_open(&backend, &reg, "b", Some("nav"), Some("https://b"))
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id, "same target across nav");
        assert_eq!(b.last_url, "https://b");
    }

    #[tokio::test]
    async fn open_with_stale_target_recreates_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        reg.tab_upsert("b", "ghost", "T999", "about:blank", true)
            .unwrap();
        let row = tab_open(&backend, &reg, "b", Some("ghost"), None)
            .await
            .unwrap();
        assert_ne!(row.target_id, "T999", "stale target was recreated");
        assert_eq!(row.name, "ghost", "same name preserved");
    }

    #[tokio::test]
    async fn list_sweeps_stale_rows_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let _ = tab_open(&backend, &reg, "b", Some("live"), None)
            .await
            .unwrap();
        reg.tab_upsert("b", "ghost", "T999", "", true).unwrap();
        let rows = tab_list(&backend, &reg, "b").await.unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"live"));
        assert!(!names.contains(&"ghost"));
        assert!(reg.tab_get("b", "ghost").unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_returns_none_for_missing_and_stale_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        assert!(resolve_tab(&backend, &reg, "b", "nope")
            .await
            .unwrap()
            .is_none());
        reg.tab_upsert("b", "ghost", "T999", "", true).unwrap();
        assert!(resolve_tab(&backend, &reg, "b", "ghost")
            .await
            .unwrap()
            .is_none());
        assert!(reg.tab_get("b", "ghost").unwrap().is_none(), "swept");
    }

    #[tokio::test]
    async fn resolve_returns_alive_row_and_touches_cdp() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let opened = tab_open(&backend, &reg, "b", Some("hot"), None)
            .await
            .unwrap();
        let resolved = resolve_tab(&backend, &reg, "b", "hot")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.target_id, opened.target_id);
    }

    #[tokio::test]
    async fn budget_pressure_picks_lru_daemon_row() {
        // Verifies the SQL helper that the create path uses. End-to-end
        // budget-pressure exercise would need HARD_CAP+1 tabs and is
        // covered by registry::tabs::tests instead.
        let reg = Registry::open_in_memory().unwrap();
        reg.tab_upsert("b", "old", "T-OLD", "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        reg.tab_upsert("b", "new", "T-NEW", "", true).unwrap();
        let lru = reg.tabs_lru_daemon_created("b").unwrap().unwrap();
        assert_eq!(lru.name, "old");
    }

    // ---- BiDi-engine tests (same behaviour, different protocol) ---------

    #[tokio::test]
    async fn open_without_name_assigns_cute_name_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let row = tab_open(&backend, &reg, "ff", None, None).await.unwrap();
        assert!(row.name.starts_with("tab-"));
        assert_eq!(row.target_id, "C1");
        assert!(row.daemon_created);
    }

    #[tokio::test]
    async fn open_with_name_is_idempotent_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&backend, &reg, "ff", Some("scrape"), None)
            .await
            .unwrap();
        let b = tab_open(&backend, &reg, "ff", Some("scrape"), None)
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id);
    }

    #[tokio::test]
    async fn open_with_mismatched_url_navigates_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&backend, &reg, "ff", Some("nav"), Some("https://a"))
            .await
            .unwrap();
        let b = tab_open(&backend, &reg, "ff", Some("nav"), Some("https://b"))
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id);
        assert_eq!(b.last_url, "https://b");
    }

    #[tokio::test]
    async fn open_with_stale_target_recreates_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        reg.tab_upsert("ff", "ghost", "C999", "", true).unwrap();
        let row = tab_open(&backend, &reg, "ff", Some("ghost"), None)
            .await
            .unwrap();
        assert_ne!(row.target_id, "C999");
        assert_eq!(row.name, "ghost");
    }

    #[tokio::test]
    async fn list_sweeps_stale_rows_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let _ = tab_open(&backend, &reg, "ff", Some("live"), None)
            .await
            .unwrap();
        reg.tab_upsert("ff", "ghost", "C999", "", true).unwrap();
        let rows = tab_list(&backend, &reg, "ff").await.unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"live"));
        assert!(!names.contains(&"ghost"));
    }

    #[tokio::test]
    async fn resolve_returns_alive_row_and_touches_bidi() {
        let (backend, _stop) = bidi_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let opened = tab_open(&backend, &reg, "ff", Some("hot"), None)
            .await
            .unwrap();
        let resolved = resolve_tab(&backend, &reg, "ff", "hot")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.target_id, opened.target_id);
    }

    // ---- with_named_tab_recovery -----------------------------------------

    use crate::errors::SessionError;

    /// Missing row → typed `TabNotFound`.
    #[tokio::test]
    async fn recover_missing_row_returns_tab_not_found() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let err = with_named_tab_recovery(&backend, &reg, "b", "nope", |_, _| async {
            Ok::<_, anyhow::Error>(serde_json::json!(null))
        })
        .await
        .expect_err("must error");
        let typed = err
            .downcast_ref::<SessionError>()
            .expect("typed SessionError");
        match typed {
            SessionError::TabNotFound { browser, name } => {
                assert_eq!(browser, "b");
                assert_eq!(name, "nope");
            }
            other => panic!("expected TabNotFound, got {other:?}"),
        }
    }

    /// Stale row whose target_id is gone → resolve_tab sweeps it →
    /// also `TabNotFound` (not silent recreate — the agent never asked
    /// for the recreate at resolve time).
    #[tokio::test]
    async fn recover_stale_row_returns_tab_not_found_after_sweep() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        reg.tab_upsert("b", "ghost", "T999", "", true).unwrap();
        let err = with_named_tab_recovery(&backend, &reg, "b", "ghost", |_, _| async {
            Ok::<_, anyhow::Error>(serde_json::json!(null))
        })
        .await
        .expect_err("must error after sweep");
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::TabNotFound { .. })
        ));
        assert!(reg.tab_get("b", "ghost").unwrap().is_none(), "swept");
    }

    /// First op call wedges (returns `TabHung`); wrapper leaves the dead
    /// tab alone, recreates a fresh blank under the same name, retries;
    /// second attempt succeeds. Caller sees a value, dead tab persists
    /// in the browser for inspection.
    #[tokio::test]
    async fn recover_after_op_returns_tab_hung() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let opened = tab_open(&backend, &reg, "b", Some("flaky"), None)
            .await
            .unwrap();
        let original_target = opened.target_id.clone();

        // Op that returns TabHung the first call, ok the second.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let result = with_named_tab_recovery(&backend, &reg, "b", "flaky", move |_, target_id| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Err(SessionError::TabHung {
                        target_id: Some(target_id.clone()),
                        url: None,
                        timeout_ms: 100,
                        hint: "test",
                    }
                    .into())
                } else {
                    Ok::<_, anyhow::Error>(serde_json::json!(format!("ok:{target_id}")))
                }
            }
        })
        .await
        .expect("recover succeeded");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        // The row now points at the fresh target, not the dead one.
        let row = reg.tab_get("b", "flaky").unwrap().unwrap();
        assert_ne!(
            row.target_id, original_target,
            "row updated to fresh target after recovery"
        );
        assert_eq!(row.last_url, "about:blank", "recovered tab is blank");
        // The op was called with the fresh target on the second attempt.
        assert_eq!(result, serde_json::json!(format!("ok:{}", row.target_id)));
    }

    /// Recovery must NOT close the dead tab: the corpse stays in the
    /// browser so the human can inspect it. The mock tracks live targets
    /// via `Target.createTarget` / `Target.closeTarget`; if recovery
    /// closes the dead one, `live_target_ids` will drop it.
    #[tokio::test]
    async fn recovery_leaves_dead_named_tab_in_browser() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        let opened = tab_open(&backend, &reg, "b", Some("doomed"), None)
            .await
            .unwrap();
        let original_target = opened.target_id.clone();

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let _ = with_named_tab_recovery(&backend, &reg, "b", "doomed", move |_, target_id| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Err(SessionError::TabHung {
                        target_id: Some(target_id),
                        url: None,
                        timeout_ms: 100,
                        hint: "test",
                    }
                    .into())
                } else {
                    Ok::<_, anyhow::Error>(serde_json::json!("ok"))
                }
            }
        })
        .await
        .expect("recover succeeded");

        // Two live targets after recovery: the dead one + the fresh blank.
        // (Recovery left the dead one alone instead of closing it.)
        let live = backend.live_target_ids().await.unwrap();
        assert!(
            live.contains(&original_target),
            "dead tab must persist; live = {live:?}, original = {original_target}"
        );
        assert_eq!(live.len(), 2, "expected dead + fresh; got {live:?}");

        // The registry row points at the fresh tab, not the dead one.
        let row = reg.tab_get("b", "doomed").unwrap().unwrap();
        assert_ne!(row.target_id, original_target);
    }

    /// `is_tab_failure` matches the typed `TargetGone` variant first
    /// (primary path) and falls back to substring matching for
    /// un-classified raw errors.
    #[test]
    fn is_tab_failure_recognizes_typed_target_gone() {
        use crate::errors::TargetKind;
        let typed: anyhow::Error = SessionError::TargetGone {
            kind: TargetKind::Cdp,
            details: "CDP error -32000: target closed".into(),
        }
        .into();
        assert!(is_tab_failure(&typed));

        let typed_bidi: anyhow::Error = SessionError::TargetGone {
            kind: TargetKind::Bidi,
            details: "BiDi error no such frame: C1".into(),
        }
        .into();
        assert!(is_tab_failure(&typed_bidi));

        let hung: anyhow::Error = SessionError::TabHung {
            target_id: None,
            url: None,
            timeout_ms: 100,
            hint: "t",
        }
        .into();
        assert!(is_tab_failure(&hung));

        let raw: anyhow::Error = anyhow::anyhow!("Target closed");
        assert!(is_tab_failure(&raw));

        let unrelated: anyhow::Error = anyhow::anyhow!("dns failure");
        assert!(!is_tab_failure(&unrelated));
    }

    /// Recovery rehydrates the dead tab's `last_url` onto the fresh tab
    /// instead of dropping the agent back to `about:blank`. The agent
    /// addresses by name and expects the name to point at the same URL
    /// after a transient renderer failure.
    #[tokio::test]
    async fn recover_rehydrates_last_url_onto_fresh_tab() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        // Seed a row whose last_url is a real URL (not about:blank).
        let opened = tab_open(
            &backend,
            &reg,
            "b",
            Some("pinned"),
            Some("https://example.com/app"),
        )
        .await
        .unwrap();
        let original_target = opened.target_id.clone();
        assert_eq!(opened.last_url, "https://example.com/app");

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let _ = with_named_tab_recovery(&backend, &reg, "b", "pinned", move |_, target_id| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Err(SessionError::TabHung {
                        target_id: Some(target_id),
                        url: None,
                        timeout_ms: 100,
                        hint: "test",
                    }
                    .into())
                } else {
                    Ok::<_, anyhow::Error>(serde_json::json!("ok"))
                }
            }
        })
        .await
        .expect("recover succeeded");

        let row = reg.tab_get("b", "pinned").unwrap().unwrap();
        assert_ne!(row.target_id, original_target, "row points at fresh tab");
        assert_eq!(
            row.last_url, "https://example.com/app",
            "last_url rehydrated on recovery instead of falling back to about:blank"
        );
    }

    /// Both attempts return `TabHung` → escalate to the caller.
    #[tokio::test]
    async fn recover_escalates_when_retry_also_fails() {
        let (backend, _stop) = cdp_backend().await;
        let reg = Registry::open_in_memory().unwrap();
        tab_open(&backend, &reg, "b", Some("doomed"), None)
            .await
            .unwrap();
        let err =
            with_named_tab_recovery(&backend, &reg, "b", "doomed", |_, target_id| async move {
                Err::<serde_json::Value, _>(
                    SessionError::TabHung {
                        target_id: Some(target_id),
                        url: None,
                        timeout_ms: 100,
                        hint: "test",
                    }
                    .into(),
                )
            })
            .await
            .expect_err("must escalate");
        assert!(matches!(
            err.downcast_ref::<SessionError>(),
            Some(SessionError::TabHung { .. })
        ));
    }
}
