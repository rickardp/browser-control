//! `browser-control targets` — list page targets in the active browser.

use anyhow::{bail, Result};

use crate::cli::env_resolver;
use crate::cli::mcp::resolve_browser;
use crate::cli::output::{print_json, print_table, terminal_width};
use crate::session::targets::{self, TargetInfo};

pub async fn run(browser: Option<String>, url: Option<String>, json: bool) -> Result<()> {
    if let Some(ref raw) = browser {
        let parsed = env_resolver::parse_target(raw)?;
        if parsed.tab.is_some() {
            bail!(
                "`targets` operates browser-wide; tab suffixes are not supported \
                 (got `{raw}`). Use a bare browser selector instead."
            );
        }
    }
    let resolved = resolve_browser(browser).await?;
    let targets = targets::list(&resolved.endpoint, resolved.engine, url.as_deref()).await?;
    let mut out = std::io::stdout();
    if json {
        print_json(&mut out, &targets)?;
    } else {
        let headers = ["KIND", "ID", "URL", "TITLE"];
        let rows = table_rows(&targets, terminal_width());
        print_table(&mut out, &headers, &rows)?;
    }
    Ok(())
}

fn table_rows(targets: &[TargetInfo], terminal_width: usize) -> Vec<Vec<String>> {
    let limits = target_table_limits(targets, terminal_width);
    targets
        .iter()
        .map(|t| {
            vec![
                truncate_middle(&t.kind, limits.kind),
                truncate_middle(&t.id, limits.id),
                truncate_middle(&t.url, limits.url),
                truncate_middle(&t.title, limits.title),
            ]
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetTableLimits {
    kind: usize,
    id: usize,
    url: usize,
    title: usize,
}

fn target_table_limits(targets: &[TargetInfo], terminal_width: usize) -> TargetTableLimits {
    const GUTTER_WIDTH: usize = 2 * 3;
    const MAX_ID_WIDTH: usize = 32;
    const MIN_URL_WIDTH: usize = 24;
    const MIN_TITLE_WIDTH: usize = 12;

    let kind = max_col_len("KIND", targets.iter().map(|t| t.kind.as_str()));
    let id_available = terminal_width
        .saturating_sub(kind + GUTTER_WIDTH + MIN_URL_WIDTH + MIN_TITLE_WIDTH)
        .max("ID".len());
    let id = max_col_len("ID", targets.iter().map(|t| t.id.as_str()))
        .min(MAX_ID_WIDTH)
        .min(id_available);
    let raw_url = max_col_len("URL", targets.iter().map(|t| t.url.as_str()));
    let raw_title = max_col_len("TITLE", targets.iter().map(|t| t.title.as_str()));
    let remaining = terminal_width.saturating_sub(kind + id + GUTTER_WIDTH);

    if raw_url + raw_title <= remaining {
        return TargetTableLimits {
            kind,
            id,
            url: raw_url,
            title: raw_title,
        };
    }

    let mut title = raw_title
        .min(remaining / 3)
        .max(MIN_TITLE_WIDTH.min(remaining));
    let mut url = raw_url.min(remaining.saturating_sub(title));

    if url < MIN_URL_WIDTH.min(remaining) {
        url = MIN_URL_WIDTH.min(remaining);
        title = remaining.saturating_sub(url).max("TITLE".len());
    }

    TargetTableLimits {
        kind,
        id,
        url: url.max("URL".len()),
        title: title.max("TITLE".len()),
    }
}

fn max_col_len<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells.map(str::len).max().unwrap_or(0).max(header.len())
}

fn truncate_middle(s: &str, max_len: usize) -> String {
    let char_len = s.chars().count();
    if char_len <= max_len {
        return s.to_string();
    }
    if max_len <= 3 {
        return ".".repeat(max_len);
    }

    let keep = max_len - 3;
    let head = keep / 2 + keep % 2;
    let tail = keep / 2;
    let head_part: String = s.chars().take(head).collect();
    let tail_part: String = s
        .chars()
        .rev()
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head_part}...{tail_part}")
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
        let rows = table_rows(&t, 120);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            vec!["page", "abc", "https://example.com/", "Example"]
        );
    }

    #[test]
    fn table_rows_truncate_long_urls_for_terminal_width() {
        let long_url = format!("https://example.com/path?{}", "x".repeat(120));
        let t = vec![TargetInfo {
            id: "0123456789abcdef0123456789abcdef".into(),
            url: long_url.clone(),
            title: "A long page title".into(),
            kind: "page".into(),
        }];
        let rows = table_rows(&t, 80);
        assert_eq!(rows.len(), 1);
        assert!(rows[0][2].len() < long_url.len(), "row: {:?}", rows[0]);
        assert!(rows[0][2].contains("..."), "row: {:?}", rows[0]);

        let headers = ["KIND", "ID", "URL", "TITLE"];
        let mut tbuf: Vec<u8> = Vec::new();
        print_table(&mut tbuf, &headers, &rows).unwrap();
        let text = String::from_utf8(tbuf).unwrap();
        assert!(
            text.lines().all(|line| line.len() <= 80),
            "table exceeded width:\n{text}"
        );
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
        let rows = table_rows(&targets, 120);
        let mut tbuf: Vec<u8> = Vec::new();
        print_table(&mut tbuf, &headers, &rows).unwrap();
        let text = String::from_utf8(tbuf).unwrap();
        assert!(text.contains("https://example.com/"));
        assert!(text.contains("KIND"));
        assert!(text.contains("page"));
    }
}
