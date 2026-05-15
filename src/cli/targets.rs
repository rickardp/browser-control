//! `browser-control targets` — list page targets in the active browser.

use anyhow::Result;

use crate::cli::mcp::resolve_browser;
use crate::cli::output::{print_json, print_table};
use crate::session::targets::{self, TargetInfo};

pub async fn run(browser: Option<String>, url: Option<String>, json: bool) -> Result<()> {
    let resolved = resolve_browser(browser).await?;
    let targets = targets::list(&resolved.endpoint, resolved.engine, url.as_deref()).await?;
    let mut out = std::io::stdout();
    if json {
        print_json(&mut out, &targets)?;
    } else {
        let headers = ["KIND", "ID", "URL", "TITLE"];
        let rows = table_rows(&targets);
        print_table(&mut out, &headers, &rows)?;
    }
    Ok(())
}

fn table_rows(targets: &[TargetInfo]) -> Vec<Vec<String>> {
    targets
        .iter()
        .map(|t| {
            vec![
                t.kind.clone(),
                t.id.clone(),
                t.url.clone(),
                t.title.clone(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Engine;
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
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

    #[test]
    fn table_rows_have_kind_id_url_title_in_order() {
        let t = vec![TargetInfo {
            id: "abc".into(),
            url: "https://example.com/".into(),
            title: "Example".into(),
            kind: "page".into(),
        }];
        let rows = table_rows(&t);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec!["page", "abc", "https://example.com/", "Example"]);
    }

    #[tokio::test]
    async fn lists_targets_against_cdp_mock_json_and_table() {
        let endpoint = spawn_cdp_mock(vec![
            json!({"targetId":"a","type":"page","url":"https://example.com/","title":"Ex"}),
            json!({"targetId":"b","type":"page","url":"https://other.test/","title":"Other"}),
        ])
        .await;

        let targets = targets::list(&endpoint, Engine::Cdp, None).await.unwrap();
        assert_eq!(targets.len(), 2);

        let mut buf: Vec<u8> = Vec::new();
        print_json(&mut buf, &targets).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        for i in 0..2 {
            for key in ["id", "url", "title", "kind"] {
                assert!(v[i].get(key).is_some(), "missing {key} in {v:?}");
            }
        }
        assert_eq!(v[0]["url"], "https://example.com/");

        let headers = ["KIND", "ID", "URL", "TITLE"];
        let rows = table_rows(&targets);
        let mut tbuf: Vec<u8> = Vec::new();
        print_table(&mut tbuf, &headers, &rows).unwrap();
        let text = String::from_utf8(tbuf).unwrap();
        assert!(text.contains("https://example.com/"));
        assert!(text.contains("KIND"));
        assert!(text.contains("page"));
    }
}
