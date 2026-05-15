//! `browser-control eval` — evaluate a JS expression in the active page.

use anyhow::Result;
use serde_json::Value;

use crate::cli::mcp::resolve_browser;
use crate::session::PageSession;

pub async fn run(
    browser: Option<String>,
    expression: String,
    target: Option<String>,
    json: bool,
    await_promise: bool,
) -> Result<()> {
    let resolved = resolve_browser(browser).await?;
    let session = PageSession::attach(&resolved.endpoint, resolved.engine, target.as_deref()).await?;
    let value = session.evaluate(&expression, await_promise).await;
    session.close().await;
    let value = value?;
    println!("{}", format_output(&value, json));
    Ok(())
}

fn format_output(v: &Value, json: bool) -> String {
    if json {
        serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
    } else if let Some(s) = v.as_str() {
        s.to_string()
    } else {
        serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn eval_returns_string_unquoted_in_text_mode() {
        let v = json!("hello");
        assert_eq!(format_output(&v, false), "hello");
    }

    #[test]
    fn eval_returns_number_as_json() {
        let v = json!(42);
        assert_eq!(format_output(&v, false), "42");
    }

    #[test]
    fn eval_returns_json_envelope_when_json_flag() {
        let v = json!({"a": 1});
        let out = format_output(&v, true);
        let parsed: Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(parsed, v);
        assert!(out.contains("\"a\""));
    }

    #[test]
    fn eval_null_text_mode() {
        let v = json!(null);
        assert_eq!(format_output(&v, false), "null");
    }

    #[test]
    fn eval_bool_text_mode() {
        assert_eq!(format_output(&json!(true), false), "true");
    }

    // Mock CDP round-trip test mirroring src/session/attach.rs tests.
    use crate::detect::Engine;
    use crate::session::PageSession;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    async fn spawn_cdp_mock(targets: Vec<Value>, eval_value: Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let req: Value = serde_json::from_str(&t).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap_or("");
                let result = match method {
                    "Target.getTargets" => json!({"targetInfos": targets.clone()}),
                    "Target.attachToTarget" => json!({"sessionId": "S1"}),
                    "Runtime.evaluate" => json!({"result": {"value": eval_value.clone()}}),
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn eval_mock_returns_string() {
        let url = spawn_cdp_mock(
            vec![json!({"targetId":"a","type":"page","url":"https://example.com/"})],
            json!("hello"),
        )
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("'hello'", false).await.unwrap();
        s.close().await;
        assert_eq!(format_output(&v, false), "hello");
    }

    #[tokio::test]
    async fn eval_mock_returns_object_json() {
        let url = spawn_cdp_mock(
            vec![json!({"targetId":"a","type":"page","url":"https://example.com/"})],
            json!({"a": 1}),
        )
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("({a:1})", false).await.unwrap();
        s.close().await;
        let out = format_output(&v, true);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"a": 1}));
    }
}
