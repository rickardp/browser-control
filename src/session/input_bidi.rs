//! WebDriver BiDi (Firefox) counterpart of [`crate::session::input`].
//!
//! Element refs on BiDi are integer ids in a page-side registry written by
//! the snapshot walker (`crate::dom::scripts::SNAPSHOT_TREE_JS`). Geometry
//! is computed in the page (`REF_CENTER_JS`, `REF_CLIP_RECT_JS`) and input
//! is dispatched with `input.performActions` at viewport-origin CSS pixel
//! coordinates, so no `locateNodes` / `sharedId` bookkeeping is needed.
//! Typing goes through in-page `execCommand('insertText')` (with a
//! value-setter fallback) because BiDi has no `insertText`; per-character
//! key actions cover `press_sequentially`, and Enter is a key action.
//!
//! Every helper returns `SessionError::NodeGone` when the registry no longer
//! resolves the id, which the tool layer maps to `StaleRef`.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::dom::scripts::{
    DOC_SIZE_JS, DOC_TOKEN_JS, REF_CENTER_JS, REF_CLIP_RECT_JS, REF_TYPE_JS, SNAPSHOT_TREE_JS,
};
use crate::errors::SessionError;
use crate::session::input::Point;
use crate::session::keys::Chord;

/// Node budget handed to the walker.
const SNAPSHOT_MAX_NODES: u64 = 20_000;
/// WebDriver key code point for Enter.
const ENTER: &str = "\u{e007}";
/// Interpolated pointer moves during a drag.
const DRAG_STEPS: usize = 5;

/// The `value` of a string `RemoteValue`.
fn remote_string(v: &Value) -> Result<String> {
    v["value"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("script returned no string result: {v}"))
}

/// Decode a ref helper's JSON string: `{"gone":true}` → `NodeGone`,
/// `{"error":…}` → error, otherwise the payload.
fn decode_ref_result(id: u64, op: &'static str, s: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(s)
        .with_context(|| format!("{op} on node {id}: invalid helper result"))?;
    if v["gone"].as_bool().unwrap_or(false) {
        return Err(SessionError::NodeGone {
            backend_node_id: id,
            details: format!("{op}: ref id {id} is not in the page registry or was detached"),
        }
        .into());
    }
    if let Some(e) = v["error"].as_str() {
        return Err(anyhow!("{op} on node {id}: {e}"));
    }
    Ok(v)
}

async fn call_ref(
    c: &BidiClient,
    ctx: &str,
    id: u64,
    op: &'static str,
    script: &str,
    extra: Vec<Value>,
) -> Result<Value> {
    let mut args = vec![json!(id)];
    args.extend(extra);
    let v = c.script_call_function(ctx, script, args).await?;
    decode_ref_result(id, op, &remote_string(&v)?)
}

fn pointer_move(p: Point) -> Value {
    json!({
        "type": "pointerMove",
        "x": p.x.round() as i64,
        "y": p.y.round() as i64,
        "origin": "viewport",
    })
}

fn pointer_source(actions: Vec<Value>) -> Value {
    json!({
        "type": "pointer",
        "id": "mouse",
        "parameters": { "pointerType": "mouse" },
        "actions": actions,
    })
}

fn key_source(actions: Vec<Value>) -> Value {
    json!({ "type": "key", "id": "kb", "actions": actions })
}

fn key_press(value: &str) -> [Value; 2] {
    [
        json!({ "type": "keyDown", "value": value }),
        json!({ "type": "keyUp", "value": value }),
    ]
}

/// Run the walker and parse its JSON.
pub async fn accessibility_tree(c: &BidiClient, ctx: &str) -> Result<Value> {
    let v = c
        .script_call_function(ctx, SNAPSHOT_TREE_JS, vec![json!(SNAPSHOT_MAX_NODES)])
        .await?;
    let tree: Value = serde_json::from_str(&remote_string(&v)?)
        .context("accessibility walker returned invalid JSON")?;
    if tree["truncated"].as_bool().unwrap_or(false) {
        tracing::warn!(
            target = %ctx,
            "accessibility snapshot truncated at {SNAPSHOT_MAX_NODES} nodes"
        );
    }
    Ok(tree)
}

/// Per-document token (see `DOC_TOKEN_JS`).
pub async fn document_token(c: &BidiClient, ctx: &str) -> Result<u64> {
    let v = c.script_evaluate(ctx, DOC_TOKEN_JS).await?;
    let v = crate::bidi::unwrap_script_result(v)?;
    remote_string(&v)?
        .parse::<u64>()
        .context("document token is not an integer")
}

/// Scroll the node into view and return its clickable centre.
pub async fn node_center(c: &BidiClient, ctx: &str, id: u64) -> Result<Point> {
    let v = call_ref(c, ctx, id, "center", REF_CENTER_JS, vec![]).await?;
    Ok(Point {
        x: v["x"].as_f64().unwrap_or(0.0),
        y: v["y"].as_f64().unwrap_or(0.0),
    })
}

/// Left-click the node's centre.
pub async fn click(c: &BidiClient, ctx: &str, id: u64) -> Result<Point> {
    let p = node_center(c, ctx, id).await?;
    c.input_perform_actions(
        ctx,
        json!([pointer_source(vec![
            pointer_move(p),
            json!({ "type": "pointerDown", "button": 0 }),
            json!({ "type": "pointerUp", "button": 0 }),
        ])]),
    )
    .await?;
    let _ = c.input_release_actions(ctx).await;
    Ok(p)
}

/// Move the pointer over the node's centre.
pub async fn hover(c: &BidiClient, ctx: &str, id: u64) -> Result<Point> {
    let p = node_center(c, ctx, id).await?;
    c.input_perform_actions(ctx, json!([pointer_source(vec![pointer_move(p)])]))
        .await?;
    Ok(p)
}

/// Focus the node, replace its content with `text`, optionally one key at a
/// time, and optionally press Enter.
pub async fn type_text(
    c: &BidiClient,
    ctx: &str,
    id: u64,
    text: &str,
    press_sequentially: bool,
    submit: bool,
) -> Result<()> {
    let mode = if text.is_empty() {
        "clear"
    } else if press_sequentially {
        "select"
    } else {
        "fill"
    };
    call_ref(
        c,
        ctx,
        id,
        "type",
        REF_TYPE_JS,
        vec![json!(text), json!(mode)],
    )
    .await?;
    if press_sequentially && !text.is_empty() {
        let mut keys = Vec::new();
        for ch in text.chars() {
            let v = match ch {
                '\n' | '\r' => ENTER.to_string(),
                other => other.to_string(),
            };
            keys.extend(key_press(&v));
        }
        c.input_perform_actions(ctx, json!([key_source(keys)]))
            .await?;
    }
    if submit {
        c.input_perform_actions(ctx, json!([key_source(key_press(ENTER).to_vec())]))
            .await?;
    }
    let _ = c.input_release_actions(ctx).await;
    Ok(())
}

/// Press and release a key, with any modifiers held around it.
///
/// BiDi tracks modifier state from the keyDown/keyUp pairs itself, so unlike
/// CDP there is no bitmask — the nesting order *is* the state. Modifiers are
/// released in reverse for the same reason as CDP: a key left down leaks into
/// whatever the page does next.
///
/// `input.releaseActions` runs afterwards regardless, so a failed action
/// sequence cannot strand the browser with a modifier held.
pub async fn press_key(c: &BidiClient, ctx: &str, chord: &Chord) -> Result<()> {
    let mut actions = Vec::new();
    for m in &chord.modifiers {
        actions.push(json!({ "type": "keyDown", "value": m.def().bidi }));
    }
    actions.extend(key_press(chord.key.bidi));
    for m in chord.modifiers.iter().rev() {
        actions.push(json!({ "type": "keyUp", "value": m.def().bidi }));
    }
    let result = c
        .input_perform_actions(ctx, json!([key_source(actions)]))
        .await;
    let _ = c.input_release_actions(ctx).await;
    result
}

/// Type into whatever currently has focus, with no node id.
///
/// BiDi has no "insert text at the caret" primitive, so this sends the value
/// as key actions — which is what typing into focus means on this engine.
/// Unlike the CDP path there is no select-all first: without a node handle
/// there is nothing to select, so the caller should clear the field before
/// piping into it if replacement is wanted.
pub async fn type_focused(c: &BidiClient, ctx: &str, text: &str, submit: bool) -> Result<()> {
    let mut keys = Vec::new();
    for ch in text.chars() {
        let v = match ch {
            '\n' | '\r' => ENTER.to_string(),
            other => other.to_string(),
        };
        keys.extend(key_press(&v));
    }
    if !keys.is_empty() {
        c.input_perform_actions(ctx, json!([key_source(keys)]))
            .await?;
    }
    if submit {
        c.input_perform_actions(ctx, json!([key_source(key_press(ENTER).to_vec())]))
            .await?;
    }
    let _ = c.input_release_actions(ctx).await;
    Ok(())
}

/// Pointer drag from one node's centre to another's.
pub async fn drag(c: &BidiClient, ctx: &str, from: u64, to: u64) -> Result<()> {
    let a = node_center(c, ctx, from).await?;
    let b = node_center(c, ctx, to).await?;
    let mut actions = vec![
        pointer_move(a),
        json!({ "type": "pointerDown", "button": 0 }),
    ];
    for i in 1..=DRAG_STEPS {
        let t = i as f64 / DRAG_STEPS as f64;
        actions.push(pointer_move(Point {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
        }));
    }
    actions.push(pointer_move(b));
    actions.push(json!({ "type": "pointerUp", "button": 0 }));
    c.input_perform_actions(ctx, json!([pointer_source(actions)]))
        .await?;
    let _ = c.input_release_actions(ctx).await;
    Ok(())
}

/// Border box of the node in document coordinates.
pub async fn node_clip_rect(c: &BidiClient, ctx: &str, id: u64) -> Result<Value> {
    call_ref(c, ctx, id, "clip", REF_CLIP_RECT_JS, vec![]).await
}

/// Document scroll size, for full-page screenshots.
pub async fn document_size(c: &BidiClient, ctx: &str) -> Result<(f64, f64)> {
    let v = c.script_evaluate(ctx, DOC_SIZE_JS).await?;
    let v = crate::bidi::unwrap_script_result(v)?;
    let parsed: Value = serde_json::from_str(&remote_string(&v)?)
        .context("document size helper returned invalid JSON")?;
    Ok((
        parsed["width"].as_f64().unwrap_or(0.0),
        parsed["height"].as_f64().unwrap_or(0.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio_tungstenite::tungstenite::Message;

    /// BiDi-framed recording mock that dispatches `script.callFunction` on
    /// the helper markers and records input commands.
    async fn spawn_mock(gone: bool) -> (String, Arc<Mutex<Vec<Value>>>) {
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
                    seen.lock().unwrap().push(req.clone());
                    let id = req["id"].as_u64().unwrap();
                    let method = req["method"].as_str().unwrap_or("");
                    let decl = req["params"]["functionDeclaration"].as_str().unwrap_or("");
                    let expr = req["params"]["expression"].as_str().unwrap_or("");
                    let string = |s: String| json!({"type": "success", "result": {"type": "string", "value": s}, "realm": "R1"});
                    let result = match method {
                        "script.callFunction" if gone => string("{\"gone\":true}".into()),
                        "script.callFunction" if decl.contains("bc:center") => {
                            string("{\"x\":30.4,\"y\":20}".into())
                        }
                        "script.callFunction" if decl.contains("bc:clip") => {
                            string("{\"x\":10,\"y\":430,\"width\":100,\"height\":20}".into())
                        }
                        "script.callFunction" if decl.contains("bc:type") => {
                            string("{\"kind\":\"field\",\"method\":\"execCommand\"}".into())
                        }
                        "script.callFunction" if decl.contains("bc:snapshot") => string(
                            json!({"nodes": [
                                {"nodeId": "root", "backendDOMNodeId": 4294967296u64,
                                 "role": {"value": "RootWebArea"}, "name": {"value": "T"}, "childIds": ["n1"]},
                                {"nodeId": "n1", "parentId": "root", "backendDOMNodeId": 1,
                                 "role": {"value": "button"}, "name": {"value": "Go"}, "childIds": [],
                                 "properties": [{"name": "focusable", "value": {"value": true}}]}
                            ], "truncated": false})
                            .to_string(),
                        ),
                        "script.evaluate" if expr.contains("__bcDocToken") => {
                            string("4294967296".into())
                        }
                        "script.evaluate" if expr.contains("scrollWidth") => {
                            string("{\"width\":1000,\"height\":3000}".into())
                        }
                        _ => json!({}),
                    };
                    ws.send(Message::Text(
                        json!({"type": "success", "id": id, "result": result}).to_string(),
                    ))
                    .await
                    .unwrap();
                }
            }
        });
        (format!("ws://{addr}"), seen)
    }

    fn input_calls(seen: &[Value]) -> Vec<Value> {
        seen.iter()
            .filter(|r| r["method"] == "input.performActions")
            .map(|r| r["params"]["actions"].clone())
            .collect()
    }

    #[tokio::test]
    async fn click_moves_presses_releases_then_releases_actions() {
        let (url, seen) = spawn_mock(false).await;
        let c = BidiClient::connect(&url).await.unwrap();
        let p = click(&c, "C1", 7).await.unwrap();
        assert_eq!(p, Point { x: 30.4, y: 20.0 });
        let seen = seen.lock().unwrap();
        let first = &seen[0];
        assert_eq!(first["method"], "script.callFunction");
        assert_eq!(
            first["params"]["arguments"][0],
            json!({"type": "number", "value": 7})
        );
        let acts = input_calls(&seen);
        assert_eq!(acts.len(), 1);
        let pointer = &acts[0][0];
        assert_eq!(pointer["type"], "pointer");
        assert_eq!(pointer["parameters"]["pointerType"], "mouse");
        assert_eq!(
            pointer["actions"],
            json!([
                {"type": "pointerMove", "x": 30, "y": 20, "origin": "viewport"},
                {"type": "pointerDown", "button": 0},
                {"type": "pointerUp", "button": 0},
            ])
        );
        assert_eq!(seen.last().unwrap()["method"], "input.releaseActions");
    }

    #[tokio::test]
    async fn type_fill_then_enter() {
        let (url, seen) = spawn_mock(false).await;
        let c = BidiClient::connect(&url).await.unwrap();
        type_text(&c, "C1", 3, "hi", false, true).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0]["params"]["arguments"],
            json!([
                {"type": "number", "value": 3},
                {"type": "string", "value": "hi"},
                {"type": "string", "value": "fill"}
            ])
        );
        let acts = input_calls(&seen);
        assert_eq!(acts.len(), 1, "fill sends no key actions; only Enter");
        assert_eq!(acts[0][0]["type"], "key");
        assert_eq!(acts[0][0]["actions"][0]["value"], ENTER);
        assert_eq!(acts[0][0]["actions"][1]["type"], "keyUp");
    }

    #[tokio::test]
    async fn type_sequentially_emits_per_char_keys_and_clear_sends_none() {
        let (url, seen) = spawn_mock(false).await;
        let c = BidiClient::connect(&url).await.unwrap();
        type_text(&c, "C1", 3, "a\n", true, false).await.unwrap();
        type_text(&c, "C1", 3, "", false, false).await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0]["params"]["arguments"][2]["value"], "select");
        let acts = input_calls(&seen);
        assert_eq!(acts.len(), 1);
        let keys = &acts[0][0]["actions"];
        assert_eq!(keys.as_array().unwrap().len(), 4);
        assert_eq!(keys[0]["value"], "a");
        assert_eq!(keys[2]["value"], ENTER);
        let clear = seen
            .iter()
            .filter(|r| r["method"] == "script.callFunction")
            .nth(1)
            .unwrap();
        assert_eq!(clear["params"]["arguments"][2]["value"], "clear");
    }

    #[tokio::test]
    async fn drag_sequence() {
        let (url, seen) = spawn_mock(false).await;
        let c = BidiClient::connect(&url).await.unwrap();
        drag(&c, "C1", 1, 2).await.unwrap();
        let seen = seen.lock().unwrap();
        let acts = input_calls(&seen);
        let types: Vec<&str> = acts[0][0]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["type"].as_str().unwrap())
            .collect();
        assert_eq!(types[0], "pointerMove");
        assert_eq!(types[1], "pointerDown");
        assert_eq!(*types.last().unwrap(), "pointerUp");
        assert_eq!(types.len(), 2 + DRAG_STEPS + 1 + 1);
    }

    #[tokio::test]
    async fn gone_maps_to_node_gone() {
        let (url, _seen) = spawn_mock(true).await;
        let c = BidiClient::connect(&url).await.unwrap();
        let err = hover(&c, "C1", 9).await.unwrap_err();
        match err.downcast_ref::<SessionError>() {
            Some(SessionError::NodeGone {
                backend_node_id, ..
            }) => {
                assert_eq!(*backend_node_id, 9)
            }
            other => panic!("expected NodeGone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tree_token_clip_and_document_size() {
        let (url, _seen) = spawn_mock(false).await;
        let c = BidiClient::connect(&url).await.unwrap();
        let tree = accessibility_tree(&c, "C1").await.unwrap();
        let parsed = crate::a11y::parse_full_ax_tree(&tree).unwrap();
        assert_eq!(crate::a11y::document_token(&parsed), Some(4294967296));
        assert_eq!(document_token(&c, "C1").await.unwrap(), 4294967296);
        assert_eq!(
            node_clip_rect(&c, "C1", 1).await.unwrap(),
            json!({"x": 10, "y": 430, "width": 100, "height": 20})
        );
        assert_eq!(document_size(&c, "C1").await.unwrap(), (1000.0, 3000.0));
    }
}
