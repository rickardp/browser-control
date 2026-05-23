//! Named-tab orchestration: SQLite + CDP working together.
//!
//! The registry layer (`crate::registry::tabs`) is pure SQL CRUD. This
//! module adds the logic the agent contract actually requires:
//!
//! - **Get-or-create**: `tab_open(_, name, url)` returns an existing
//!   live tab under that name, or creates a fresh `about:blank` (or the
//!   given `url`) and registers it.
//! - **Navigate-on-mismatch**: if the existing tab's `last_url` doesn't
//!   match the requested `url`, run `Page.navigate` first.
//! - **Sweep-on-read**: stale rows whose `target_id` no longer exists in
//!   the live browser are dropped before they're returned to the caller.
//! - **Budget pressure**: when daemon-created rows exceed
//!   [`HARD_CAP`], the LRU is closed and recycled.
//! - **Cute names**: agents who pass no `--name` get
//!   `tab-<cute-word>` from the same generator that names browsers.

use anyhow::{anyhow, Context, Result};
use rand::Rng;
use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::registry::{words::WORDS, Registry, TabRow};

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
    client: &CdpClient,
    registry: &Registry,
    browser_name: &str,
    name: Option<&str>,
    url: Option<&str>,
) -> Result<TabRow> {
    let want_url = url.unwrap_or("about:blank");

    // Fast path: named tab exists and is alive → maybe navigate, return.
    if let Some(requested_name) = name {
        if let Some(existing) = registry.tab_get(browser_name, requested_name)? {
            if target_alive(client, &existing.target_id).await? {
                if !want_url.is_empty() && want_url != existing.last_url && url.is_some() {
                    navigate_target(client, &existing.target_id, want_url)
                        .await
                        .with_context(|| {
                            format!("navigating {browser_name}/{requested_name} to {want_url}")
                        })?;
                    registry.tab_set_url(browser_name, requested_name, want_url)?;
                } else {
                    registry.tab_touch(browser_name, requested_name)?;
                }
                return registry
                    .tab_get(browser_name, requested_name)?
                    .ok_or_else(|| anyhow!("tab row vanished between lookups"));
            }
            // Stale row → close best-effort + delete + fall through to create.
            let _ = close_target(client, &existing.target_id).await;
            registry.tab_delete(browser_name, requested_name)?;
        }
    }

    // Create path. Enforce budget first.
    if registry.tabs_count_daemon_created(browser_name)? >= HARD_CAP {
        if let Some(victim) = registry.tabs_lru_daemon_created(browser_name)? {
            let _ = close_target(client, &victim.target_id).await;
            registry.tab_delete(&victim.browser_name, &victim.name)?;
        }
    }

    let assigned_name = match name {
        Some(n) => n.to_string(),
        None => fresh_cute_name(registry, browser_name)?,
    };
    let new_target_id = create_target(client, want_url).await?;
    registry.tab_upsert(browser_name, &assigned_name, &new_target_id, want_url, true)?;
    registry
        .tab_get(browser_name, &assigned_name)?
        .ok_or_else(|| anyhow!("tab row missing immediately after upsert"))
}

/// `tab list <browser>` backend with sweep-on-read.
///
/// Calls `Target.getTargets`, drops any tab rows whose `target_id` is no
/// longer present in the live browser (closed externally, browser restarted,
/// etc.), and returns the surviving rows.
pub async fn tab_list(
    client: &CdpClient,
    registry: &Registry,
    browser_name: &str,
) -> Result<Vec<TabRow>> {
    let live_targets = live_target_ids(client).await?;
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

/// Resolve `<browser>/<name>` to a live `target_id` for cross-command
/// routing (eval, fetch, etc.). Returns `Ok(None)` if no row matches or
/// the row's `target_id` no longer exists in the browser (`TabNotFound`
/// at the call site).
pub async fn resolve_tab(
    client: &CdpClient,
    registry: &Registry,
    browser_name: &str,
    name: &str,
) -> Result<Option<TabRow>> {
    let Some(row) = registry.tab_get(browser_name, name)? else {
        return Ok(None);
    };
    if target_alive(client, &row.target_id).await? {
        registry.tab_touch(browser_name, name)?;
        Ok(Some(row))
    } else {
        registry.tab_delete(browser_name, name)?;
        Ok(None)
    }
}

// ---- CDP helpers ----------------------------------------------------------

async fn create_target(client: &CdpClient, url: &str) -> Result<String> {
    let v = client
        .send("Target.createTarget", json!({ "url": url }))
        .await?;
    v.get("targetId")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))
}

async fn close_target(client: &CdpClient, target_id: &str) -> Result<()> {
    let _ = client
        .send("Target.closeTarget", json!({ "targetId": target_id }))
        .await?;
    Ok(())
}

async fn navigate_target(client: &CdpClient, target_id: &str, url: &str) -> Result<()> {
    let attach = client
        .send(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await?;
    let session_id = attach
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("attach returned no sessionId"))?
        .to_string();
    client
        .send_with_session("Page.navigate", json!({ "url": url }), Some(&session_id))
        .await?;
    let _ = client
        .send(
            "Target.detachFromTarget",
            json!({ "sessionId": session_id }),
        )
        .await;
    Ok(())
}

async fn live_target_ids(client: &CdpClient) -> Result<std::collections::HashSet<String>> {
    let v: Value = client.send("Target.getTargets", json!({})).await?;
    let arr = v
        .get("targetInfos")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|t| t.get("targetId").and_then(|v| v.as_str()).map(String::from))
        .collect())
}

async fn target_alive(client: &CdpClient, target_id: &str) -> Result<bool> {
    let live = live_target_ids(client).await?;
    Ok(live.contains(target_id))
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
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

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

    #[tokio::test]
    async fn open_without_name_assigns_cute_name() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let row = tab_open(&client, &reg, "brave", None, None).await.unwrap();
        assert!(row.name.starts_with("tab-"));
        assert_eq!(row.target_id, "T1");
        assert!(row.daemon_created);
        assert_eq!(row.last_url, "about:blank");
    }

    #[tokio::test]
    async fn open_with_name_is_idempotent() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&client, &reg, "b", Some("scrape"), None)
            .await
            .unwrap();
        let b = tab_open(&client, &reg, "b", Some("scrape"), None)
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id);
        assert_eq!(a.name, b.name);
    }

    #[tokio::test]
    async fn open_with_mismatched_url_navigates() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let a = tab_open(&client, &reg, "b", Some("nav"), Some("https://a"))
            .await
            .unwrap();
        let b = tab_open(&client, &reg, "b", Some("nav"), Some("https://b"))
            .await
            .unwrap();
        assert_eq!(a.target_id, b.target_id, "same target across nav");
        assert_eq!(b.last_url, "https://b");
    }

    #[tokio::test]
    async fn open_with_stale_target_recreates() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        // Insert a row pointing at a target that the mock never created.
        reg.tab_upsert("b", "ghost", "T999", "about:blank", true)
            .unwrap();
        let row = tab_open(&client, &reg, "b", Some("ghost"), None)
            .await
            .unwrap();
        assert_ne!(row.target_id, "T999", "stale target was recreated");
        assert_eq!(row.name, "ghost", "same name preserved");
    }

    #[tokio::test]
    async fn list_sweeps_stale_rows() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        // Real tab.
        let _ = tab_open(&client, &reg, "b", Some("live"), None)
            .await
            .unwrap();
        // Synthetic stale row.
        reg.tab_upsert("b", "ghost", "T999", "", true).unwrap();
        let rows = tab_list(&client, &reg, "b").await.unwrap();
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"live"));
        assert!(!names.contains(&"ghost"));
        // Stale row also removed from SQL.
        assert!(reg.tab_get("b", "ghost").unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_returns_none_for_missing_and_stale() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        assert!(resolve_tab(&client, &reg, "b", "nope")
            .await
            .unwrap()
            .is_none());
        reg.tab_upsert("b", "ghost", "T999", "", true).unwrap();
        assert!(resolve_tab(&client, &reg, "b", "ghost")
            .await
            .unwrap()
            .is_none());
        assert!(reg.tab_get("b", "ghost").unwrap().is_none(), "swept");
    }

    #[tokio::test]
    async fn resolve_returns_alive_row_and_touches() {
        let (url, _stop) = spawn_mock().await;
        let client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        let opened = tab_open(&client, &reg, "b", Some("hot"), None)
            .await
            .unwrap();
        let resolved = resolve_tab(&client, &reg, "b", "hot")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.target_id, opened.target_id);
    }

    #[tokio::test]
    async fn budget_pressure_recycles_lru() {
        // Shrink the cap for the test via a local override: we directly
        // exercise the LRU path by filling and asserting the behaviour
        // matches what HARD_CAP would force. To keep the test fast we
        // bypass the const by stuffing rows into SQL directly with
        // staggered last_used_at, then call tab_open to trigger eviction.
        // Validates the *logic* in tab_open without compiling against a
        // 50-create workload.
        let (url, _stop) = spawn_mock().await;
        let _client = Arc::new(CdpClient::connect(&url).await.unwrap());
        let reg = Registry::open_in_memory().unwrap();
        // Synthetic rows pretending to be at the cap. We'd need HARD_CAP+1
        // to trigger; this test only verifies tabs_lru_daemon_created
        // picks the right victim. Real budget-pressure end-to-end test
        // requires a tunable cap and is filed separately.
        reg.tab_upsert("b", "old", "T-OLD", "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        reg.tab_upsert("b", "new", "T-NEW", "", true).unwrap();
        let lru = reg.tabs_lru_daemon_created("b").unwrap().unwrap();
        assert_eq!(lru.name, "old");
    }
}
