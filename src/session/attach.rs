//! Attach to a page target and expose engine-agnostic high-level operations.
//!
//! [`PageSession`] hides the CDP/BiDi split behind a single async API
//! (`evaluate`, `navigate`, `screenshot`). The CLI subcommands instantiate
//! a fresh session per call; the MCP server may pre-build a session backed
//! by a long-lived BiDi client via [`PageSession::from_bidi_cache`].

use std::sync::Arc;

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::cdp::CdpClient;
use crate::detect::Engine;
use crate::session::targets::{open_bidi, open_cdp};

/// A bound page-level session. Variants are not constructed directly outside
/// this module; use [`PageSession::attach`].
pub enum PageSession {
    Cdp(CdpPage),
    /// A BiDi page session. The client is shared via `Arc` so the MCP server
    /// can keep a single persistent BiDi session across many tool calls
    /// (Firefox limits a browser to one BiDi session at a time).
    Bidi(BidiPage),
}

pub struct CdpPage {
    pub client: CdpClient,
    pub session_id: String,
    pub target_id: String,
}

pub struct BidiPage {
    pub client: Arc<BidiClient>,
    pub context: String,
}

impl PageSession {
    /// Attach to a fresh page session over `engine`.
    ///
    /// If `url_regex` is `Some`, the first page target whose URL matches is
    /// selected; otherwise the first page (or top-level browsing context) is
    /// used.
    pub async fn attach(
        endpoint: &str,
        engine: Engine,
        url_regex: Option<&str>,
    ) -> Result<Self> {
        let pattern = url_regex.map(Regex::new).transpose()?;
        match engine {
            Engine::Cdp => {
                let client = open_cdp(endpoint).await?;
                let target_id = pick_cdp_page(&client, pattern.as_ref()).await?;
                let session_id = client.attach_to_target(&target_id).await?;
                Ok(PageSession::Cdp(CdpPage {
                    client,
                    session_id,
                    target_id,
                }))
            }
            Engine::Bidi => {
                let client = Arc::new(open_bidi(endpoint).await?);
                client.session_new().await?;
                let context = pick_bidi_context(&client, pattern.as_ref()).await?;
                Ok(PageSession::Bidi(BidiPage { client, context }))
            }
        }
    }

    /// Build a BiDi session from a pre-opened, possibly cached client.
    ///
    /// The MCP server uses this to share one BiDi client across tool calls;
    /// `session.new` is invoked only when the client was freshly opened (the
    /// caller is expected to have done so).
    pub async fn from_bidi_cache(
        client: Arc<BidiClient>,
        url_regex: Option<&str>,
    ) -> Result<Self> {
        let pattern = url_regex.map(Regex::new).transpose()?;
        let context = pick_bidi_context(&client, pattern.as_ref()).await?;
        Ok(PageSession::Bidi(BidiPage { client, context }))
    }

    /// Evaluate `expression` in the page's main world.
    ///
    /// `await_promise = true` mirrors `Runtime.evaluate({awaitPromise:true})`
    /// and is appropriate for fetch / promise-returning code. The returned
    /// value is the raw `result.value` from CDP / BiDi after `returnByValue`.
    pub async fn evaluate(&self, expression: &str, await_promise: bool) -> Result<Value> {
        match self {
            PageSession::Cdp(p) => {
                let v = p
                    .client
                    .send_with_session(
                        "Runtime.evaluate",
                        json!({
                            "expression": expression,
                            "returnByValue": true,
                            "awaitPromise": await_promise,
                        }),
                        Some(&p.session_id),
                    )
                    .await?;
                Ok(v["result"]["value"].clone())
            }
            PageSession::Bidi(p) => {
                let _ = await_promise; // BiDi always awaits per script_evaluate
                let v = p.client.script_evaluate(&p.context, expression).await?;
                Ok(v["result"]["value"].clone())
            }
        }
    }

    /// Navigate the current page to `url`.
    pub async fn navigate(&self, url: &str) -> Result<()> {
        match self {
            PageSession::Cdp(p) => {
                p.client
                    .send_with_session(
                        "Page.navigate",
                        json!({"url": url}),
                        Some(&p.session_id),
                    )
                    .await?;
                Ok(())
            }
            PageSession::Bidi(p) => {
                p.client
                    .browsing_context_navigate(&p.context, url)
                    .await?;
                Ok(())
            }
        }
    }

    /// Capture a PNG screenshot of the current page; returns base64 data.
    pub async fn screenshot(&self, full_page: bool) -> Result<String> {
        match self {
            PageSession::Cdp(p) => {
                let v = p
                    .client
                    .send_with_session(
                        "Page.captureScreenshot",
                        json!({
                            "format": "png",
                            "captureBeyondViewport": full_page,
                        }),
                        Some(&p.session_id),
                    )
                    .await?;
                v["data"]
                    .as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("no screenshot data"))
            }
            PageSession::Bidi(p) => {
                let _ = full_page; // BiDi captures the viewport by default
                p.client
                    .browsing_context_capture_screenshot(&p.context)
                    .await
            }
        }
    }

    /// Engine this session is bound to.
    pub fn engine(&self) -> Engine {
        match self {
            PageSession::Cdp(_) => Engine::Cdp,
            PageSession::Bidi(_) => Engine::Bidi,
        }
    }

    /// Release the underlying CDP connection (no-op for BiDi, whose client
    /// is shared via `Arc`).
    pub async fn close(self) {
        match self {
            PageSession::Cdp(p) => p.client.close().await,
            PageSession::Bidi(_) => {}
        }
    }
}

async fn pick_cdp_page(client: &CdpClient, pattern: Option<&Regex>) -> Result<String> {
    let targets = client.list_targets().await?;
    let mut pages = targets.iter().filter(|t| {
        t.get("type").and_then(|v| v.as_str()) == Some("page")
    });
    let pick = if let Some(re) = pattern {
        pages
            .find(|t| {
                t.get("url")
                    .and_then(|v| v.as_str())
                    .is_some_and(|u| re.is_match(u))
            })
            .ok_or_else(|| anyhow!("no CDP page target matched URL regex"))?
    } else {
        pages
            .next()
            .ok_or_else(|| anyhow!("no page target found"))?
    };
    pick.get("targetId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("targetId missing from page target"))
}

async fn pick_bidi_context(
    client: &BidiClient,
    pattern: Option<&Regex>,
) -> Result<String> {
    let tree = client.send("browsingContext.getTree", json!({})).await?;
    let contexts = tree
        .get("contexts")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no contexts in browsingContext.getTree"))?;
    if let Some(re) = pattern {
        for c in contexts {
            let url = c.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if re.is_match(url) {
                return c
                    .get("context")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| anyhow!("no context id"));
            }
        }
        Err(anyhow!("no BiDi context matched URL regex"))
    } else {
        contexts
            .first()
            .and_then(|c| c.get("context").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no top-level browsing context"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    async fn spawn_cdp_mock(targets: Vec<Value>) -> String {
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
                    "Runtime.evaluate" => json!({"result": {"value": "ok"}}),
                    "Page.navigate" => json!({}),
                    "Page.captureScreenshot" => json!({"data": "PNGDATA"}),
                    _ => json!({}),
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn attach_cdp_picks_first_page_when_no_regex() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
            json!({"targetId":"b","type":"page","url":"https://other.test/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        match s {
            PageSession::Cdp(p) => {
                assert_eq!(p.target_id, "a");
                assert_eq!(p.session_id, "S1");
            }
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_cdp_url_regex_selects_matching() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
            json!({"targetId":"b","type":"page","url":"https://other.test/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, Some(r"other"))
            .await
            .unwrap();
        match s {
            PageSession::Cdp(p) => assert_eq!(p.target_id, "b"),
            _ => panic!("expected CDP"),
        }
    }

    #[tokio::test]
    async fn attach_cdp_url_regex_no_match_errors() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let err = match PageSession::attach(&url, Engine::Cdp, Some("nomatch")).await {
            Ok(_) => panic!("expected error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no CDP page target matched"));
    }

    #[tokio::test]
    async fn evaluate_round_trip_cdp() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let v = s.evaluate("1+1", false).await.unwrap();
        assert_eq!(v, json!("ok"));
        s.close().await;
    }

    #[tokio::test]
    async fn screenshot_round_trip_cdp() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/"}),
        ])
        .await;
        let s = PageSession::attach(&url, Engine::Cdp, None).await.unwrap();
        let b64 = s.screenshot(false).await.unwrap();
        assert_eq!(b64, "PNGDATA");
        s.close().await;
    }
}
