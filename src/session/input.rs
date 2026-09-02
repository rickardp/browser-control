//! Native CDP input and geometry for ref-based interaction.
//!
//! Every function here works on an already-attached flat session (see
//! [`crate::session::cdp_session::with_page_session`]) and a CDP
//! `backendDOMNodeId`, which is what an element ref resolves to. Clicks go
//! through `Input.dispatchMouseEvent` at the element's centre in viewport
//! CSS pixels — the coordinate space `DOM.getContentQuads` already reports
//! in, so no device-pixel-ratio maths is involved. Text goes through
//! `Input.insertText`, which fires the same `beforeinput`/`input` events a
//! paste does (Playwright's `fill` does the same).
//!
//! Deliberately not done in v1: occlusion checks (a click on a covered
//! element lands on the overlay), auto-waiting, and `Page.bringToFront`
//! (input works on background tabs, and the project keeps automated tabs
//! in the background).

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::cdp::CdpClient;
use crate::dom::scripts::SELECT_ALL_JS;
use crate::errors::{is_cdp_node_gone, SessionError};

/// A point in viewport CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// Map a `DOM.*` failure for `backend_node_id` onto the typed
/// [`SessionError::NodeGone`] when the message says the node left the
/// document; otherwise attach context and pass through.
fn node_err(backend_node_id: u64, op: &'static str) -> impl FnOnce(anyhow::Error) -> anyhow::Error {
    move |e| {
        let msg = format!("{e:#}");
        if is_cdp_node_gone(&msg) {
            SessionError::NodeGone {
                backend_node_id,
                details: msg,
            }
            .into()
        } else {
            e.context(format!("{op} on node {backend_node_id}"))
        }
    }
}

/// Viewport size from `Page.getLayoutMetrics` (`cssLayoutViewport`, with
/// the pre-CSS field as fallback for older Chromium).
fn viewport_size(metrics: &Value) -> (f64, f64) {
    let vp = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"));
    let w = vp
        .and_then(|v| v.get("clientWidth"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX);
    let h = vp
        .and_then(|v| v.get("clientHeight"))
        .and_then(Value::as_f64)
        .unwrap_or(f64::MAX);
    (w, h)
}

/// Scroll position from `Page.getLayoutMetrics`, in CSS pixels.
fn viewport_offset(metrics: &Value) -> (f64, f64) {
    let vp = metrics
        .get("cssLayoutViewport")
        .or_else(|| metrics.get("layoutViewport"));
    let x = vp
        .and_then(|v| v.get("pageX"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let y = vp
        .and_then(|v| v.get("pageY"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    (x, y)
}

/// Pick the centre of the first quad with a visible area after clipping
/// to the viewport. Mirrors Puppeteer's `clickablePoint`.
pub fn pick_point(quads: &Value, vw: f64, vh: f64) -> Option<Point> {
    let quads = quads.as_array()?;
    for q in quads {
        let nums: Vec<f64> = q.as_array()?.iter().filter_map(Value::as_f64).collect();
        if nums.len() != 8 {
            continue;
        }
        let pts: Vec<(f64, f64)> = (0..4)
            .map(|i| (nums[i * 2].clamp(0.0, vw), nums[i * 2 + 1].clamp(0.0, vh)))
            .collect();
        // Shoelace area of the clipped quad.
        let mut area = 0.0;
        for i in 0..4 {
            let (x1, y1) = pts[i];
            let (x2, y2) = pts[(i + 1) % 4];
            area += x1 * y2 - x2 * y1;
        }
        if area.abs() / 2.0 <= 1.0 {
            continue;
        }
        let x = pts.iter().map(|p| p.0).sum::<f64>() / 4.0;
        let y = pts.iter().map(|p| p.1).sum::<f64>() / 4.0;
        return Some(Point { x, y });
    }
    None
}

/// Scroll the node into view and return its clickable centre.
pub async fn node_center(c: &CdpClient, sid: &str, backend_node_id: u64) -> Result<Point> {
    let _ = c
        .send_with_session("DOM.enable", json!({}), Some(sid))
        .await;
    c.send_with_session(
        "DOM.scrollIntoViewIfNeeded",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await
    .map_err(node_err(backend_node_id, "scrollIntoViewIfNeeded"))?;
    let quads = c
        .send_with_session(
            "DOM.getContentQuads",
            json!({ "backendNodeId": backend_node_id }),
            Some(sid),
        )
        .await
        .map_err(node_err(backend_node_id, "getContentQuads"))?;
    let metrics = c
        .send_with_session("Page.getLayoutMetrics", json!({}), Some(sid))
        .await?;
    let (vw, vh) = viewport_size(&metrics);
    pick_point(&quads["quads"], vw, vh)
        .ok_or_else(|| anyhow!("element has no visible box (hidden, zero-size, or outside the viewport after scrolling)"))
}

async fn mouse(c: &CdpClient, sid: &str, kind: &str, p: Point, pressed: bool) -> Result<()> {
    let mut params = json!({ "type": kind, "x": p.x, "y": p.y });
    if pressed {
        params["button"] = json!("left");
        params["clickCount"] = json!(1);
    }
    c.send_with_session("Input.dispatchMouseEvent", params, Some(sid))
        .await
        .context("Input.dispatchMouseEvent")?;
    Ok(())
}

/// Left-click the node's centre.
pub async fn click(c: &CdpClient, sid: &str, backend_node_id: u64) -> Result<Point> {
    let p = node_center(c, sid, backend_node_id).await?;
    mouse(c, sid, "mouseMoved", p, false).await?;
    mouse(c, sid, "mousePressed", p, true).await?;
    mouse(c, sid, "mouseReleased", p, true).await?;
    Ok(p)
}

/// Move the pointer over the node's centre.
pub async fn hover(c: &CdpClient, sid: &str, backend_node_id: u64) -> Result<Point> {
    let p = node_center(c, sid, backend_node_id).await?;
    mouse(c, sid, "mouseMoved", p, false).await?;
    Ok(p)
}

/// Focus the node, replace its current content with `text`, and
/// optionally press Enter. `press_sequentially` inserts one character
/// per event for inputs that react to each keystroke.
pub async fn type_text(
    c: &CdpClient,
    sid: &str,
    backend_node_id: u64,
    text: &str,
    press_sequentially: bool,
    submit: bool,
) -> Result<()> {
    // Background tabs do not deliver focus events unless emulated.
    let _ = c
        .send_with_session(
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": true }),
            Some(sid),
        )
        .await;
    c.send_with_session(
        "DOM.focus",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await
    .map_err(node_err(backend_node_id, "focus"))?;
    let resolved = c
        .send_with_session(
            "DOM.resolveNode",
            json!({ "backendNodeId": backend_node_id }),
            Some(sid),
        )
        .await
        .map_err(node_err(backend_node_id, "resolveNode"))?;
    let object_id = resolved["object"]["objectId"]
        .as_str()
        .ok_or_else(|| anyhow!("DOM.resolveNode returned no objectId"))?
        .to_string();
    let select = c
        .send_with_session(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": SELECT_ALL_JS,
                "arguments": [{ "value": text.is_empty() }],
                "returnByValue": true,
            }),
            Some(sid),
        )
        .await;
    let _ = c
        .send_with_session(
            "Runtime.releaseObject",
            json!({ "objectId": object_id }),
            Some(sid),
        )
        .await;
    select.context("selecting existing content")?;
    if !text.is_empty() {
        if press_sequentially {
            for ch in text.chars() {
                insert_text(c, sid, &ch.to_string()).await?;
            }
        } else {
            insert_text(c, sid, text).await?;
        }
    }
    if submit {
        press_enter(c, sid).await?;
    }
    Ok(())
}

async fn insert_text(c: &CdpClient, sid: &str, text: &str) -> Result<()> {
    c.send_with_session("Input.insertText", json!({ "text": text }), Some(sid))
        .await
        .context("Input.insertText")?;
    Ok(())
}

/// Press and release Enter on the focused element. `text: "\r"` on the
/// keyDown is what makes Chromium synthesise the `keypress` and submit
/// forms, matching Puppeteer.
pub async fn press_enter(c: &CdpClient, sid: &str) -> Result<()> {
    c.send_with_session(
        "Input.dispatchKeyEvent",
        json!({
            "type": "keyDown",
            "key": "Enter",
            "code": "Enter",
            "windowsVirtualKeyCode": 13,
            "nativeVirtualKeyCode": 13,
            "text": "\r",
            "unmodifiedText": "\r",
        }),
        Some(sid),
    )
    .await
    .context("Input.dispatchKeyEvent keyDown")?;
    c.send_with_session(
        "Input.dispatchKeyEvent",
        json!({
            "type": "keyUp",
            "key": "Enter",
            "code": "Enter",
            "windowsVirtualKeyCode": 13,
            "nativeVirtualKeyCode": 13,
        }),
        Some(sid),
    )
    .await
    .context("Input.dispatchKeyEvent keyUp")?;
    Ok(())
}

/// Pointer-event drag from one node's centre to another's. HTML5 native
/// drag-and-drop (`draggable`) needs `Input.setInterceptDrags`; not in v1.
pub async fn drag(c: &CdpClient, sid: &str, from: u64, to: u64) -> Result<()> {
    let a = node_center(c, sid, from).await?;
    let b = node_center(c, sid, to).await?;
    mouse(c, sid, "mouseMoved", a, false).await?;
    mouse(c, sid, "mousePressed", a, true).await?;
    const STEPS: usize = 5;
    for i in 1..=STEPS {
        let t = i as f64 / STEPS as f64;
        let p = Point {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
        };
        mouse(c, sid, "mouseMoved", p, true).await?;
    }
    mouse(c, sid, "mouseReleased", b, true).await?;
    Ok(())
}

/// The Document node's `backendNodeId`: changes on every navigation, so
/// it identifies "the document the refs were taken from".
pub async fn document_token(c: &CdpClient, sid: &str) -> Result<u64> {
    let doc = c
        .send_with_session("DOM.getDocument", json!({ "depth": 0 }), Some(sid))
        .await
        .context("DOM.getDocument")?;
    doc["root"]["backendNodeId"]
        .as_u64()
        .ok_or_else(|| anyhow!("DOM.getDocument returned no root backendNodeId"))
}

/// Border box of the node in *document* coordinates, the same contract as
/// [`crate::dom::scripts::GET_CLIP_RECT_JS`], for element screenshots.
pub async fn node_clip_rect(c: &CdpClient, sid: &str, backend_node_id: u64) -> Result<Value> {
    let _ = c
        .send_with_session("DOM.enable", json!({}), Some(sid))
        .await;
    c.send_with_session(
        "DOM.scrollIntoViewIfNeeded",
        json!({ "backendNodeId": backend_node_id }),
        Some(sid),
    )
    .await
    .map_err(node_err(backend_node_id, "scrollIntoViewIfNeeded"))?;
    let model = c
        .send_with_session(
            "DOM.getBoxModel",
            json!({ "backendNodeId": backend_node_id }),
            Some(sid),
        )
        .await
        .map_err(node_err(backend_node_id, "getBoxModel"))?;
    let border: Vec<f64> = model["model"]["border"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    if border.len() != 8 {
        return Err(anyhow!("element has no box model (hidden or detached)"));
    }
    let xs = [border[0], border[2], border[4], border[6]];
    let ys = [border[1], border[3], border[5], border[7]];
    let min_x = xs.iter().cloned().fold(f64::MAX, f64::min);
    let max_x = xs.iter().cloned().fold(f64::MIN, f64::max);
    let min_y = ys.iter().cloned().fold(f64::MAX, f64::min);
    let max_y = ys.iter().cloned().fold(f64::MIN, f64::max);
    if max_x - min_x <= 0.0 || max_y - min_y <= 0.0 {
        return Err(anyhow!("element has zero area"));
    }
    let metrics = c
        .send_with_session("Page.getLayoutMetrics", json!({}), Some(sid))
        .await?;
    let (sx, sy) = viewport_offset(&metrics);
    Ok(json!({
        "x": min_x + sx,
        "y": min_y + sy,
        "width": max_x - min_x,
        "height": max_y - min_y,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;

    /// Records every request and answers geometry methods with canned
    /// values; everything else gets `{}`.
    async fn spawn_mock(node_gone: bool) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        tokio::spawn({
            let seen = seen.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(t))) = ws.next().await {
                    let req: Value = serde_json::from_str(&t).unwrap();
                    seen.lock().await.push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    if node_gone && method.starts_with("DOM.") && method != "DOM.enable" {
                        let resp = json!({"id": id, "error": {"code": -32000, "message": "No node with given id found"}});
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                        continue;
                    }
                    let result = match method {
                        "DOM.getContentQuads" => json!({"quads": [
                            // Off-screen quad (negative), then a real 100x20 box at (10,30).
                            [-50, -50, -10, -50, -10, -40, -50, -40],
                            [10, 30, 110, 30, 110, 50, 10, 50],
                        ]}),
                        "Page.getLayoutMetrics" => json!({"cssLayoutViewport": {
                            "pageX": 0, "pageY": 400, "clientWidth": 800, "clientHeight": 600
                        }}),
                        "DOM.resolveNode" => json!({"object": {"objectId": "obj-1"}}),
                        "DOM.getDocument" => json!({"root": {"backendNodeId": 4242}}),
                        "DOM.getBoxModel" => {
                            json!({"model": {"border": [10, 30, 110, 30, 110, 50, 10, 50]}})
                        }
                        _ => json!({}),
                    };
                    let resp = json!({"id": id, "result": result});
                    ws.send(Message::Text(resp.to_string())).await.unwrap();
                }
            }
        });
        (format!("ws://{addr}"), seen)
    }

    fn methods(seen: &[Value]) -> Vec<String> {
        seen.iter()
            .filter_map(|v| v["method"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn pick_point_skips_offscreen_and_clips() {
        let quads = json!([
            [-50, -50, -10, -50, -10, -40, -50, -40],
            [700, 10, 900, 10, 900, 30, 700, 30],
        ]);
        let p = pick_point(&quads, 800.0, 600.0).unwrap();
        assert_eq!(p, Point { x: 750.0, y: 20.0 });
        assert!(pick_point(&json!([[0, 0, 0, 0, 0, 0, 0, 0]]), 800.0, 600.0).is_none());
        assert!(pick_point(&json!(null), 800.0, 600.0).is_none());
    }

    #[tokio::test]
    async fn click_dispatches_move_press_release_at_centre() {
        let (url, seen) = spawn_mock(false).await;
        let c = CdpClient::connect(&url).await.unwrap();
        let p = click(&c, "S1", 77).await.unwrap();
        assert_eq!(p, Point { x: 60.0, y: 40.0 });
        let calls = seen.lock().await;
        assert_eq!(
            methods(&calls),
            vec![
                "DOM.enable",
                "DOM.scrollIntoViewIfNeeded",
                "DOM.getContentQuads",
                "Page.getLayoutMetrics",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
                "Input.dispatchMouseEvent",
            ]
        );
        let press = &calls[5];
        assert_eq!(press["sessionId"], "S1");
        assert_eq!(press["params"]["type"], "mousePressed");
        assert_eq!(press["params"]["button"], "left");
        assert_eq!(press["params"]["x"], 60.0);
        assert_eq!(press["params"]["y"], 40.0);
        assert_eq!(calls[1]["params"]["backendNodeId"], 77);
    }

    #[tokio::test]
    async fn type_text_focuses_selects_inserts_and_submits() {
        let (url, seen) = spawn_mock(false).await;
        let c = CdpClient::connect(&url).await.unwrap();
        type_text(&c, "S1", 5, "hi", false, true).await.unwrap();
        let calls = seen.lock().await;
        assert_eq!(
            methods(&calls),
            vec![
                "Emulation.setFocusEmulationEnabled",
                "DOM.focus",
                "DOM.resolveNode",
                "Runtime.callFunctionOn",
                "Runtime.releaseObject",
                "Input.insertText",
                "Input.dispatchKeyEvent",
                "Input.dispatchKeyEvent",
            ]
        );
        assert_eq!(calls[3]["params"]["arguments"][0]["value"], false);
        assert_eq!(calls[5]["params"]["text"], "hi");
        assert_eq!(calls[6]["params"]["text"], "\r");
        assert_eq!(calls[7]["params"]["type"], "keyUp");
    }

    #[tokio::test]
    async fn type_text_sequential_and_clear() {
        let (url, seen) = spawn_mock(false).await;
        let c = CdpClient::connect(&url).await.unwrap();
        type_text(&c, "S1", 5, "ab", true, false).await.unwrap();
        type_text(&c, "S1", 5, "", false, false).await.unwrap();
        let calls = seen.lock().await;
        let inserts: Vec<&Value> = calls
            .iter()
            .filter(|v| v["method"] == "Input.insertText")
            .collect();
        assert_eq!(inserts.len(), 2);
        assert_eq!(inserts[0]["params"]["text"], "a");
        assert_eq!(inserts[1]["params"]["text"], "b");
        let clears: Vec<&Value> = calls
            .iter()
            .filter(|v| v["method"] == "Runtime.callFunctionOn")
            .collect();
        assert_eq!(clears[1]["params"]["arguments"][0]["value"], true);
    }

    #[tokio::test]
    async fn drag_presses_moves_and_releases() {
        let (url, seen) = spawn_mock(false).await;
        let c = CdpClient::connect(&url).await.unwrap();
        drag(&c, "S1", 1, 2).await.unwrap();
        let calls = seen.lock().await;
        let types: Vec<&str> = calls
            .iter()
            .filter(|v| v["method"] == "Input.dispatchMouseEvent")
            .map(|v| v["params"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(types[0], "mouseMoved");
        assert_eq!(types[1], "mousePressed");
        assert_eq!(*types.last().unwrap(), "mouseReleased");
        assert_eq!(types.len(), 2 + 5 + 1);
    }

    #[tokio::test]
    async fn node_gone_maps_to_typed_error() {
        let (url, _seen) = spawn_mock(true).await;
        let c = CdpClient::connect(&url).await.unwrap();
        let err = click(&c, "S1", 9).await.unwrap_err();
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::NodeGone {
                backend_node_id, ..
            }) => assert_eq!(*backend_node_id, 9),
            other => panic!("expected NodeGone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn document_token_and_clip_rect() {
        let (url, _seen) = spawn_mock(false).await;
        let c = CdpClient::connect(&url).await.unwrap();
        assert_eq!(document_token(&c, "S1").await.unwrap(), 4242);
        let rect = node_clip_rect(&c, "S1", 3).await.unwrap();
        // Viewport is scrolled 400px down, so document y = 30 + 400.
        assert_eq!(
            rect,
            json!({"x": 10.0, "y": 430.0, "width": 100.0, "height": 20.0})
        );
    }
}
