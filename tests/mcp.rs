//! Integration tests for the MCP server skeleton.
//!
//! These exercise the public library API directly (no spawned subprocess) so
//! they don't depend on having a real browser available.

use browser_control::cli::env_resolver::{ResolvedBrowser, Source};
use browser_control::detect::Engine;
use browser_control::mcp::server::{run_with_streams, ServerState, ToolRegistry};
use browser_control::mcp::tools::register_all;
use browser_control::session::freshness;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

fn dummy_state() -> ServerState {
    ServerState::new(ResolvedBrowser {
        endpoint: "ws://x".into(),
        engine: Engine::Cdp,
        source: Source::External,
    })
}

#[tokio::test]
async fn initialize_round_trip() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize","params":{}
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["serverInfo"]["name"], "browser-control");
}

#[tokio::test]
async fn tools_list_empty() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["result"]["tools"], serde_json::json!([]));
}

#[tokio::test]
async fn unknown_method_yields_method_not_found() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"does/not/exist"});
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["error"]["code"], -32601);
}

#[tokio::test]
async fn registered_tools_list_contains_full_playwright_shaped_set() {
    let tools = ToolRegistry::new();
    register_all(&tools);
    let names: HashSet<String> = tools
        .list()
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    for expected in [
        "browser_navigate",
        "browser_get_html",
        "browser_take_screenshot",
        "browser_fetch",
        "browser_select_element",
        "browser_cookies",
        "browser_storage_get",
        "browser_storage_set",
        "browser_wait_for_cookie",
        "browser_tab_list",
        "browser_tab_new",
        "browser_tab_select",
        "browser_tab_close",
        "browser_select",
        "browser_list",
        "list_targets",
    ] {
        assert!(names.contains(expected), "missing {expected} in {names:?}");
    }
}

// ---------------------------------------------------------------------------
// Live-browser tool tests against a CDP mock.
// ---------------------------------------------------------------------------
//
// The mock tracks the live target set so create/close/list reflect reality.
// Probes (`Runtime.evaluate`) always succeed (the mock is responsive); for
// the "selected-tab hung" test we use a separate mock that hangs on
// `Runtime.evaluate`.

/// Behaviour knob for the CDP mock.
#[derive(Clone, Copy, Default)]
struct MockBehaviour {
    /// When true, the mock returns no reply to `Runtime.evaluate` (the
    /// request is dropped). The client's `tokio::time::timeout` fires.
    hang_runtime_evaluate: bool,
}

async fn spawn_cdp_mock(behaviour: MockBehaviour) -> (String, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut next_target = 0u32;
        let mut next_session = 0u32;
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
                        if behaviour.hang_runtime_evaluate && method == "Runtime.evaluate" {
                            continue; // drop reply — client times out
                        }
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
                            "Runtime.evaluate" => json!({"result": {"value": 1}}),
                            "Target.getTargets" => {
                                let infos: Vec<Value> = live
                                    .iter()
                                    .map(|tid| json!({
                                        "targetId": tid,
                                        "type": "page",
                                        "url": format!("https://example.com/{tid}"),
                                        "title": format!("page-{tid}"),
                                    }))
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedEvaluate {
    target_id: String,
}

#[derive(Debug, Default)]
struct RecordingCdpState {
    created_targets: Vec<(String, String)>,
    evaluations: Vec<RecordedEvaluate>,
}

async fn spawn_recording_cdp_mock(
    initial_targets: Vec<(&str, &str)>,
) -> (String, oneshot::Sender<()>, Arc<Mutex<RecordingCdpState>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let recorded = Arc::new(Mutex::new(RecordingCdpState::default()));
    tokio::spawn({
        let recorded = recorded.clone();
        let initial_targets: Vec<(String, String)> = initial_targets
            .into_iter()
            .map(|(id, url)| (id.to_string(), url.to_string()))
            .collect();
        async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut next_target = 0u32;
            let mut next_session = 0u32;
            let mut live: HashMap<String, String> = initial_targets.into_iter().collect();
            let mut sessions: HashMap<String, String> = HashMap::new();
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
                                "Target.getTargets" => {
                                    let infos: Vec<Value> = live
                                        .iter()
                                        .map(|(tid, url)| json!({
                                            "targetId": tid,
                                            "type": "page",
                                            "url": url,
                                            "title": format!("page-{tid}"),
                                        }))
                                        .collect();
                                    json!({"targetInfos": infos})
                                }
                                "Target.createTarget" => {
                                    next_target += 1;
                                    let tid = format!("NEW{next_target}");
                                    let url = req
                                        .pointer("/params/url")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("about:blank")
                                        .to_string();
                                    live.insert(tid.clone(), url.clone());
                                    recorded
                                        .lock()
                                        .await
                                        .created_targets
                                        .push((tid.clone(), url));
                                    json!({"targetId": tid})
                                }
                                "Target.attachToTarget" => {
                                    next_session += 1;
                                    let sid = format!("S{next_session}");
                                    let tid = req
                                        .pointer("/params/targetId")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string();
                                    sessions.insert(sid.clone(), tid);
                                    json!({"sessionId": sid})
                                }
                                "Target.detachFromTarget" => {
                                    if let Some(sid) = req
                                        .pointer("/params/sessionId")
                                        .and_then(|v| v.as_str())
                                    {
                                        sessions.remove(sid);
                                    }
                                    json!({})
                                }
                                "Inspector.enable" => json!({}),
                                "Runtime.evaluate" => {
                                    let target_id = req
                                        .get("sessionId")
                                        .and_then(|v| v.as_str())
                                        .and_then(|sid| sessions.get(sid))
                                        .cloned()
                                        .unwrap_or_default();
                                    let expression = req
                                        .pointer("/params/expression")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    let value = if expression == freshness::PAGE_FRESHNESS_EXPR {
                                        json!({
                                            "href": live
                                                .get(&target_id)
                                                .cloned()
                                                .unwrap_or_else(|| "about:blank".to_string()),
                                            "ageMs": 0.0,
                                            "readyState": "complete"
                                        })
                                    } else if expression == freshness::READY_STATE_EXPR {
                                        json!("complete")
                                    } else {
                                        recorded
                                            .lock()
                                            .await
                                            .evaluations
                                            .push(RecordedEvaluate { target_id });
                                        json!(json!({
                                            "ok": true,
                                            "status": 200,
                                            "body": "ok"
                                        })
                                        .to_string())
                                    };
                                    json!({"result": {"value": value}})
                                }
                                _ => json!({}),
                            };
                            let resp = json!({"id": id, "result": result});
                            ws.send(Message::Text(resp.to_string())).await.unwrap();
                        }
                    }
                }
            }
        }
    });
    (format!("ws://{addr}"), stop_tx, recorded)
}

#[derive(Debug, Default)]
struct RecordingBidiState {
    session_new_calls: usize,
    methods: Vec<String>,
}

async fn spawn_recording_bidi_mock() -> (String, oneshot::Sender<()>, Arc<Mutex<RecordingBidiState>>)
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let recorded = Arc::new(Mutex::new(RecordingBidiState::default()));
    tokio::spawn({
        let recorded = recorded.clone();
        async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let recorded = recorded.clone();
                        tokio::spawn(async move {
                            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                            while let Some(Ok(Message::Text(t))) = ws.next().await {
                                let req: Value = serde_json::from_str(&t).unwrap();
                                let id = req["id"].as_u64().unwrap();
                                let method = req["method"].as_str().unwrap_or("").to_string();
                                {
                                    let mut rec = recorded.lock().await;
                                    rec.methods.push(method.clone());
                                }
                                if method == "session.new" {
                                    let mut rec = recorded.lock().await;
                                    rec.session_new_calls += 1;
                                    if rec.session_new_calls > 1 {
                                        let resp = json!({
                                            "type": "error",
                                            "id": id,
                                            "error": "session not created",
                                            "message": "unexpected second session.new",
                                        });
                                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                                        continue;
                                    }
                                    let resp = json!({
                                        "type": "success",
                                        "id": id,
                                        "result": {"sessionId": "SESSION1"},
                                    });
                                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                                    continue;
                                }
                                let result = match method.as_str() {
                                    "browsingContext.getTree" => json!({
                                        "contexts": [{
                                            "context": "CTX1",
                                            "url": "https://example.com/",
                                        }]
                                    }),
                                    "storage.getCookies" => json!({
                                        "cookies": [{
                                            "domain": "example.com",
                                            "name": "session",
                                            "value": {"type": "string", "value": "abc"},
                                            "path": "/",
                                            "secure": false,
                                            "httpOnly": true,
                                            "sameSite": "lax",
                                            "expiry": 1893456000,
                                        }]
                                    }),
                                    "session.end" => json!({}),
                                    _ => json!({}),
                                };
                                let resp = json!({"type": "success", "id": id, "result": result});
                                ws.send(Message::Text(resp.to_string())).await.unwrap();
                            }
                        });
                    }
                }
            }
        }
    });
    (format!("ws://{addr}"), stop_tx, recorded)
}

/// Wrap a `ServerState` around the given mock URL.
fn state_for_mock(url: &str) -> ServerState {
    ServerState::new(ResolvedBrowser {
        endpoint: url.into(),
        engine: Engine::Cdp,
        source: Source::External,
    })
}

fn bidi_state_for_mock(url: &str) -> ServerState {
    ServerState::new(ResolvedBrowser {
        endpoint: url.into(),
        engine: Engine::Bidi,
        source: Source::External,
    })
}

/// Run a single tool through the registered handler. Returns the tool's
/// raw result `Value` (the `{"content": [...]}` wrapper).
async fn call_tool(state: ServerState, name: &str, args: Value) -> anyhow::Result<Value> {
    let tools = ToolRegistry::new();
    register_all(&tools);
    let handler = tools
        .handler(name)
        .unwrap_or_else(|| panic!("no handler for {name}"));
    handler(state, args).await
}

/// Pull the text payload out of a tool result envelope.
fn text_payload(v: &Value) -> String {
    v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn browser_tab_new_makes_tab_active() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    let out = call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    let parsed: Value = serde_json::from_str(&text_payload(&out)).unwrap();
    assert_eq!(parsed["target_id"], "T1");
    assert_eq!(parsed["active"], true);
    // The state's pointer now references the new tab.
    let ptr = state.active_target_id.lock().await.clone();
    assert_eq!(ptr.as_deref(), Some("T1"));
}

#[tokio::test]
async fn browser_tab_list_marks_active_tab() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    // Open two tabs; second is active.
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    let out = call_tool(state.clone(), "browser_tab_list", json!({}))
        .await
        .unwrap();
    let arr: Vec<Value> = serde_json::from_str(&text_payload(&out)).unwrap();
    assert_eq!(arr.len(), 2);
    let active: Vec<&str> = arr
        .iter()
        .filter(|t| t["active"].as_bool() == Some(true))
        .filter_map(|t| t["target_id"].as_str())
        .collect();
    assert_eq!(active, vec!["T2"], "second tab is the active one");
}

#[tokio::test]
async fn browser_tab_select_live_tab_updates_active_pointer() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    // Open three tabs to have something to choose from.
    for _ in 0..3 {
        call_tool(state.clone(), "browser_tab_new", json!({}))
            .await
            .unwrap();
    }
    // Active currently T3; flip to T1.
    let _ = call_tool(
        state.clone(),
        "browser_tab_select",
        json!({"target_id": "T1"}),
    )
    .await
    .unwrap();
    let ptr = state.active_target_id.lock().await.clone();
    assert_eq!(ptr.as_deref(), Some("T1"));
}

#[tokio::test]
async fn browser_tab_select_missing_target_returns_tab_not_found() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    let err = call_tool(
        state.clone(),
        "browser_tab_select",
        json!({"target_id": "DOES_NOT_EXIST"}),
    )
    .await
    .expect_err("must error");
    let typed = err
        .downcast_ref::<browser_control::errors::SessionError>()
        .expect("SessionError");
    assert!(matches!(
        typed,
        browser_control::errors::SessionError::TabNotFound { .. }
    ));
}

#[tokio::test]
async fn browser_tab_select_hung_tab_returns_tab_hung() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour {
        hang_runtime_evaluate: true,
    })
    .await;
    let state = state_for_mock(&url);
    // Create a tab (Target.createTarget still works), then try to select
    // it — the probe hangs.
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    let err = call_tool(
        state.clone(),
        "browser_tab_select",
        json!({"target_id": "T1"}),
    )
    .await
    .expect_err("probe should time out");
    let typed = err
        .downcast_ref::<browser_control::errors::SessionError>()
        .expect("SessionError");
    match typed {
        browser_control::errors::SessionError::TabHung { hint, .. } => {
            assert_eq!(*hint, "selected-tab-hung");
        }
        other => panic!("expected TabHung, got {other:?}"),
    }
}

#[tokio::test]
async fn browser_tab_close_active_clears_pointer() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    assert_eq!(
        state.active_target_id.lock().await.clone().as_deref(),
        Some("T1")
    );
    // Default arg = close active tab.
    call_tool(state.clone(), "browser_tab_close", json!({}))
        .await
        .unwrap();
    assert!(state.active_target_id.lock().await.is_none());
    // And T1 is gone from the live set.
    let out = call_tool(state.clone(), "browser_tab_list", json!({}))
        .await
        .unwrap();
    let arr: Vec<Value> = serde_json::from_str(&text_payload(&out)).unwrap();
    assert!(arr.iter().all(|t| t["target_id"] != "T1"));
}

#[tokio::test]
async fn browser_navigate_uses_active_tab_when_no_args() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    // Lazy-creates active tab.
    call_tool(
        state.clone(),
        "browser_navigate",
        json!({"url": "https://example.com/"}),
    )
    .await
    .unwrap();
    // Active pointer now set.
    assert!(state.active_target_id.lock().await.is_some());
}

#[tokio::test]
async fn browser_navigate_with_target_regex_routes_to_match() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    // Open two tabs; the second becomes active.
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    call_tool(state.clone(), "browser_tab_new", json!({}))
        .await
        .unwrap();
    let active_before = state.active_target_id.lock().await.clone();
    assert_eq!(active_before.as_deref(), Some("T2"));
    // The mock gives each tab a unique URL like https://example.com/T1.
    // Target the T1 one explicitly via regex.
    let res = call_tool(
        state.clone(),
        "browser_navigate",
        json!({"url": "https://example.com/T1", "target": "T1"}),
    )
    .await
    .unwrap();
    assert!(text_payload(&res).contains("Navigated"));
    // Active pointer is untouched by the regex route: T2 stays active.
    let active_after = state.active_target_id.lock().await.clone();
    assert_eq!(active_after, active_before);
}

#[tokio::test]
async fn browser_navigate_rejects_both_tab_and_target() {
    let (url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&url);
    let err = call_tool(
        state.clone(),
        "browser_navigate",
        json!({"url": "https://x", "tab": "n", "target": "x"}),
    )
    .await
    .expect_err("must reject");
    assert!(err
        .to_string()
        .to_lowercase()
        .contains("mutually exclusive"));
}

#[tokio::test]
async fn browser_fetch_without_route_creates_url_origin_tab_without_setting_active() {
    let (url, _stop, recorded) = spawn_recording_cdp_mock(vec![]).await;
    let state = state_for_mock(&url);

    let out = call_tool(
        state.clone(),
        "browser_fetch",
        json!({"url": "https://example.com/api/data"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&text_payload(&out)).unwrap();
    assert_eq!(parsed["ok"], true);

    let rec = recorded.lock().await;
    assert_eq!(
        rec.created_targets,
        vec![("NEW1".to_string(), "https://example.com/".to_string())]
    );
    assert_eq!(
        rec.evaluations,
        vec![RecordedEvaluate {
            target_id: "NEW1".to_string()
        }]
    );
    drop(rec);
    assert!(state.active_target_id.lock().await.is_none());
}

#[tokio::test]
async fn browser_fetch_without_route_reuses_matching_origin_tab_without_setting_active() {
    let (url, _stop, recorded) =
        spawn_recording_cdp_mock(vec![("EXISTING", "https://example.com/app")]).await;
    let state = state_for_mock(&url);

    let out = call_tool(
        state.clone(),
        "browser_fetch",
        json!({"url": "https://example.com/api/data"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&text_payload(&out)).unwrap();
    assert_eq!(parsed["ok"], true);

    let rec = recorded.lock().await;
    assert!(
        rec.created_targets.is_empty(),
        "same-origin tab should be reused, got {:?}",
        rec.created_targets
    );
    assert_eq!(
        rec.evaluations,
        vec![RecordedEvaluate {
            target_id: "EXISTING".to_string()
        }]
    );
    drop(rec);
    assert!(state.active_target_id.lock().await.is_none());
}

#[tokio::test]
async fn bidi_browser_wide_tools_reuse_server_owned_backend() {
    let (url, _stop, recorded) = spawn_recording_bidi_mock().await;
    let state = bidi_state_for_mock(&url);

    state.ensure_backend().await.unwrap();
    assert_eq!(recorded.lock().await.session_new_calls, 1);

    let targets_out = call_tool(state.clone(), "list_targets", json!({}))
        .await
        .unwrap();
    let targets: Vec<Value> = serde_json::from_str(&text_payload(&targets_out)).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0]["id"], "CTX1");
    assert_eq!(targets[0]["kind"], "context");

    let cookies_out = call_tool(state.clone(), "browser_cookies", json!({}))
        .await
        .unwrap();
    let cookies: Vec<Value> = serde_json::from_str(&text_payload(&cookies_out)).unwrap();
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0]["name"], "session");

    let waited = call_tool(
        state.clone(),
        "browser_wait_for_cookie",
        json!({
            "domain": "^example\\.com$",
            "name": "^session$",
            "timeout_seconds": 0.05,
            "poll_interval_seconds": 0.001,
        }),
    )
    .await
    .unwrap();
    assert_eq!(text_payload(&waited), "session");

    let rec = recorded.lock().await;
    assert_eq!(
        rec.session_new_calls, 1,
        "browser-wide BiDi tools must reuse the established backend"
    );
    assert_eq!(
        rec.methods
            .iter()
            .filter(|method| method.as_str() == "browsingContext.getTree")
            .count(),
        1
    );
    assert_eq!(
        rec.methods
            .iter()
            .filter(|method| method.as_str() == "storage.getCookies")
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------
// browser_select / browser_list.
//
// These tests touch the global registry which is keyed by the
// `BROWSER_CONTROL_DATA_DIR` env var (process-wide). They serialize
// against each other via `REGISTRY_TEST_LOCK` to avoid races with other
// tests that also read the registry.
// ---------------------------------------------------------------------------

/// Process-wide lock so registry-touching MCP tests don't race on the
/// shared `BROWSER_CONTROL_DATA_DIR` env var. Async-aware so the guard
/// is safe to hold across `.await`.
static REGISTRY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Listener-port info for a mock; we keep the listener around to keep
/// the port alive for liveness checks against the registry row.
struct AliveListener {
    _listener: std::net::TcpListener,
    port: u16,
}

fn alive_listener() -> AliveListener {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    AliveListener {
        _listener: listener,
        port,
    }
}

#[tokio::test]
async fn browser_list_enumerates_registered_browsers() {
    use browser_control::detect::Kind;
    use browser_control::registry::{BrowserRow, Registry};
    use std::path::PathBuf;
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("BROWSER_CONTROL_DATA_DIR", tmp.path());
    let reg = Registry::open().unwrap();
    // Use the current pid + an alive listener so `is_alive` returns true.
    let live = alive_listener();
    reg.insert(&BrowserRow {
        name: "chrome-fixture".into(),
        kind: Kind::Chrome,
        engine: Engine::Cdp,
        pid: std::process::id(),
        endpoint: format!("ws://127.0.0.1:{}/devtools/browser/x", live.port),
        port: live.port,
        profile_dir: PathBuf::from("/tmp/profiles/x"),
        executable: PathBuf::from("/usr/bin/example"),
        headless: false,
        started_at: "2024-01-01T00:00:00Z".into(),
    })
    .unwrap();
    // Drop the registry handle so the MCP path can re-open it.
    drop(reg);

    let (_url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    let state = state_for_mock(&_url);
    let out = call_tool(state, "browser_list", json!({})).await.unwrap();
    let arr: Vec<Value> = serde_json::from_str(&text_payload(&out)).unwrap();
    let names: Vec<&str> = arr.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"chrome-fixture"), "got names {names:?}");
    std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
}

#[tokio::test]
async fn browser_select_switches_to_registered_browser() {
    use browser_control::detect::Kind;
    use browser_control::registry::{BrowserRow, Registry};
    use std::path::PathBuf;
    let _guard = REGISTRY_TEST_LOCK.lock().await;
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("BROWSER_CONTROL_DATA_DIR", tmp.path());
    // Stand up a CDP mock and register it as a named browser.
    let (mock_url, _stop) = spawn_cdp_mock(MockBehaviour::default()).await;
    // We need an alive listener whose port we *reuse* in the registry,
    // so `is_alive` passes against the row. Trick: registry liveness
    // checks pid+port — pid is `std::process::id()` (ourselves, alive),
    // and the port is whatever bound the mock's WS listener.
    let mock_port: u16 = mock_url
        .strip_prefix("ws://127.0.0.1:")
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.parse().ok())
        .expect("parse mock port");
    {
        let reg = Registry::open().unwrap();
        reg.insert(&BrowserRow {
            name: "switch-target".into(),
            kind: Kind::Chrome,
            engine: Engine::Cdp,
            pid: std::process::id(),
            endpoint: mock_url.clone(),
            port: mock_port,
            profile_dir: PathBuf::from("/tmp/profiles/y"),
            executable: PathBuf::from("/usr/bin/example"),
            headless: false,
            started_at: "2024-01-01T00:00:00Z".into(),
        })
        .unwrap();
    }
    // Start from an "external" placeholder browser.
    let state = ServerState::new(ResolvedBrowser {
        endpoint: "ws://127.0.0.1:1/unused".into(),
        engine: Engine::Cdp,
        source: Source::External,
    });
    let out = call_tool(
        state.clone(),
        "browser_select",
        json!({"name": "switch-target"}),
    )
    .await
    .unwrap();
    let parsed: Value = serde_json::from_str(&text_payload(&out)).unwrap();
    assert_eq!(parsed["name"], "switch-target");
    assert_eq!(parsed["endpoint"], mock_url);
    // Active browser swapped.
    let snap = state.browser_snapshot().await;
    assert_eq!(snap.endpoint, mock_url);
    // Active tab pointer cleared on swap.
    assert!(state.active_target_id.lock().await.is_none());
    std::env::remove_var("BROWSER_CONTROL_DATA_DIR");
}
