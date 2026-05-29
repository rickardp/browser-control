//! Unified page-target listing across CDP and BiDi.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
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

/// Wire shape of a CDP `Target.getTargets` (`targetInfos`) entry. Only
/// `target_id` is required; other fields default to empty when absent.
/// Deserialized directly from the protocol JSON via [`serde_json`].
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CdpTarget {
    #[serde(rename = "targetId")]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

impl CdpTarget {
    /// Parse the `type == "page"` entries from a `Target.getTargets` /
    /// `list_targets` result, skipping any entry that fails to deserialize
    /// (e.g. missing `targetId`).
    pub(crate) fn pages(targets: &[Value]) -> impl Iterator<Item = CdpTarget> + '_ {
        targets
            .iter()
            .filter_map(|t| CdpTarget::deserialize(t).ok())
            .filter(|t| t.kind == "page")
    }
}

/// Wire shape of a BiDi `browsingContext.getTree` context node. Only
/// `context` is required; `url`/`title` default to empty when absent.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BidiContext {
    pub context: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

impl BidiContext {
    /// Parse the top-level context nodes from a `browsingContext.getTree`
    /// result, skipping any node that fails to deserialize. Children are not
    /// recursed (callers only care about top-level contexts).
    pub(crate) fn from_tree(tree: &Value) -> Vec<BidiContext> {
        tree.get("contexts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| BidiContext::deserialize(c).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
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
    Ok(CdpTarget::pages(&targets)
        .map(|t| TargetInfo {
            id: t.id,
            url: t.url,
            title: t.title,
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
    Ok(BidiContext::from_tree(&tree)
        .into_iter()
        .map(|c| TargetInfo {
            id: c.context,
            url: c.url,
            title: c.title,
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
