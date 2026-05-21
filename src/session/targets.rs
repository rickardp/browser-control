//! Unified page-target listing across CDP and BiDi.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::bidi::BidiClient;
use crate::cdp::CdpClient;
use crate::detect::Engine;

/// Normalised view of a page-like target across engines.
///
/// For CDP this maps to a `targetInfo` entry of `type == "page"`. For BiDi
/// this maps to a top-level browsing context.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TargetInfo {
    /// Engine-specific id (CDP `targetId` or BiDi `context`).
    pub id: String,
    /// Page URL, possibly empty for new tabs.
    pub url: String,
    /// Page title, possibly empty.
    pub title: String,
    /// Always `"page"` for CDP; `"context"` for BiDi.
    pub kind: String,
}

/// Connect to `endpoint` (per `engine`) and return the list of page targets,
/// filtered by `url_regex` if given. The regex is unanchored; use `^…$` if
/// strict matching is desired.
pub async fn list(
    endpoint: &str,
    engine: Engine,
    url_regex: Option<&str>,
) -> Result<Vec<TargetInfo>> {
    let pattern = url_regex.map(Regex::new).transpose()?;
    let raw = match engine {
        Engine::Cdp => list_cdp(endpoint).await?,
        Engine::Bidi => list_bidi(endpoint).await?,
    };
    Ok(raw
        .into_iter()
        .filter(|t| pattern.as_ref().map_or(true, |re| re.is_match(&t.url)))
        .collect())
}

async fn list_cdp(endpoint: &str) -> Result<Vec<TargetInfo>> {
    let client = open_cdp(endpoint).await?;
    let targets = client.list_targets().await?;
    client.close().await;
    Ok(targets
        .into_iter()
        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .map(|t| TargetInfo {
            id: t
                .get("targetId")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            url: t
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            title: t
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            kind: "page".to_string(),
        })
        .collect())
}

async fn list_bidi(endpoint: &str) -> Result<Vec<TargetInfo>> {
    let client = open_bidi(endpoint).await?;
    client.session_new().await?;
    let tree = client.send("browsingContext.getTree", json!({})).await;
    let _ = client.session_end().await;
    let tree = tree?;
    let contexts = tree
        .get("contexts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(contexts
        .into_iter()
        .map(|c| TargetInfo {
            id: c
                .get("context")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            url: c
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            title: c
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            kind: "context".to_string(),
        })
        .collect())
}

pub(crate) async fn open_cdp(endpoint: &str) -> Result<CdpClient> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        CdpClient::connect(endpoint).await
    } else {
        CdpClient::connect_http(endpoint).await
    }
}

pub(crate) async fn open_bidi(endpoint: &str) -> Result<BidiClient> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        BidiClient::connect(endpoint).await
    } else {
        let client = reqwest::Client::new();
        let v: Value = client
            .get(format!("{}/json/version", endpoint.trim_end_matches('/')))
            .send()
            .await?
            .json()
            .await?;
        let ws = v
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("no webSocketDebuggerUrl"))?
            .to_string();
        BidiClient::connect(&ws).await
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
                let result = if method == "Target.getTargets" {
                    json!({"targetInfos": targets.clone()})
                } else {
                    json!({})
                };
                let resp = json!({"id": id, "result": result});
                ws.send(Message::Text(resp.to_string())).await.unwrap();
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn list_cdp_filters_pages_only() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/","title":"Ex"}),
            json!({"targetId":"b","type":"iframe","url":"https://example.com/","title":""}),
            json!({"targetId":"c","type":"page","url":"https://other.test/","title":"Other"}),
        ])
        .await;
        let out = list(&url, Engine::Cdp, None).await.unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].id, "c");
    }

    #[tokio::test]
    async fn list_cdp_applies_url_regex() {
        let url = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/","title":"Ex"}),
            json!({"targetId":"c","type":"page","url":"https://other.test/","title":"Other"}),
        ])
        .await;
        let out = list(&url, Engine::Cdp, Some(r"example\.com"))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://example.com/");
    }

    #[tokio::test]
    async fn list_propagates_invalid_regex() {
        let err = list("ws://127.0.0.1:1", Engine::Cdp, Some("(invalid"))
            .await
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("regex"));
    }
}
