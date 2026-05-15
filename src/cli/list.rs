//! `list-installed` and `list-running` subcommand handlers.

use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

use crate::cli::output::{print_json, print_table};
use crate::detect::{self, Engine};
use crate::registry::{BrowserRow, Registry};

/// JSON-only enriched view of a registry row. Adds `cdp_port`, `cdp_ws_url`,
/// `bidi_ws_url` where applicable. All added fields are optional.
#[derive(Debug, Serialize)]
struct EnrichedBrowserRow {
    #[serde(flatten)]
    row: BrowserRow,
    #[serde(skip_serializing_if = "Option::is_none")]
    cdp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cdp_ws_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bidi_ws_url: Option<String>,
}

/// Fetch `webSocketDebuggerUrl` from `<endpoint>/json/version` with a short timeout.
/// Returns `None` if anything fails — callers should treat the probe as optional.
async fn probe_ws_url(client: &reqwest::Client, endpoint: &str) -> Option<String> {
    let base = endpoint.trim_end_matches('/');
    let url = format!("{base}/json/version");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("webSocketDebuggerUrl")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// Enrich rows in parallel by probing each endpoint's `/json/version`.
async fn enrich_rows(rows: Vec<BrowserRow>) -> Vec<EnrichedBrowserRow> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return rows
                .into_iter()
                .map(|row| {
                    let cdp_port = match row.engine {
                        Engine::Cdp => Some(row.port),
                        _ => None,
                    };
                    EnrichedBrowserRow {
                        row,
                        cdp_port,
                        cdp_ws_url: None,
                        bidi_ws_url: None,
                    }
                })
                .collect();
        }
    };

    let probes = rows.into_iter().map(|row| {
        let client = client.clone();
        async move {
            let ws = probe_ws_url(&client, &row.endpoint).await;
            let (cdp_port, cdp_ws_url, bidi_ws_url) = match row.engine {
                Engine::Cdp => (Some(row.port), ws, None),
                Engine::Bidi => (None, None, ws),
            };
            EnrichedBrowserRow {
                row,
                cdp_port,
                cdp_ws_url,
                bidi_ws_url,
            }
        }
    });
    futures_util::future::join_all(probes).await
}

/// Implements `browser-control list-installed [--json]`.
pub fn run_list_installed(json: bool) -> Result<()> {
    let installed = detect::list_installed();
    if json {
        print_json(&mut std::io::stdout(), &installed)?;
    } else {
        let headers = ["KIND", "VERSION", "ENGINE", "EXECUTABLE"];
        let rows = installed
            .iter()
            .map(|i| {
                vec![
                    i.kind.as_str().to_string(),
                    i.version.clone(),
                    format!("{:?}", i.engine).to_lowercase(),
                    i.executable.display().to_string(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&mut std::io::stdout(), &headers, &rows)?;
    }
    Ok(())
}

/// Implements `browser-control list-running [--json]`.
pub async fn run_list_running(json: bool) -> Result<()> {
    let reg = Registry::open()?;
    let alive = reg.list_alive()?;
    if json {
        let enriched = enrich_rows(alive).await;
        print_json(&mut std::io::stdout(), &enriched)?;
    } else {
        let headers = [
            "NAME", "KIND", "PID", "ENGINE", "ENDPOINT", "PROFILE", "STARTED",
        ];
        let rows = alive
            .iter()
            .map(|r| {
                vec![
                    r.name.clone(),
                    r.kind.as_str().to_string(),
                    r.pid.to_string(),
                    format!("{:?}", r.engine).to_lowercase(),
                    r.endpoint.clone(),
                    r.profile_dir.display().to_string(),
                    r.started_at.clone(),
                ]
            })
            .collect::<Vec<_>>();
        print_table(&mut std::io::stdout(), &headers, &rows)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Kind;
    use std::path::PathBuf;

    fn row_for(endpoint: &str, port: u16, engine: Engine) -> BrowserRow {
        BrowserRow {
            name: "test-name".to_string(),
            kind: Kind::Chrome,
            engine,
            pid: 1,
            endpoint: endpoint.to_string(),
            port,
            profile_dir: PathBuf::from("/tmp/profile"),
            executable: PathBuf::from("/usr/bin/chrome"),
            headless: false,
            started_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn enriches_cdp_row_with_port_and_ws_url() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("GET", "/json/version")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:1234/devtools/browser/abc"}"#)
            .create_async()
            .await;

        let url = server.url();
        let port: u16 = url::Url::parse(&url).unwrap().port().unwrap();
        let row = row_for(&url, port, Engine::Cdp);

        let enriched = enrich_rows(vec![row]).await;
        m.assert_async().await;
        assert_eq!(enriched.len(), 1);
        let e = &enriched[0];
        assert_eq!(e.cdp_port, Some(port));
        assert_eq!(
            e.cdp_ws_url.as_deref(),
            Some("ws://127.0.0.1:1234/devtools/browser/abc")
        );
        assert!(e.bidi_ws_url.is_none());

        let mut buf: Vec<u8> = Vec::new();
        print_json(&mut buf, &enriched).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v[0]["cdp_port"], port);
        assert_eq!(
            v[0]["cdp_ws_url"],
            "ws://127.0.0.1:1234/devtools/browser/abc"
        );
        assert_eq!(v[0]["name"], "test-name");
        assert_eq!(v[0]["engine"], "cdp");
        assert_eq!(v[0]["endpoint"], url);
        assert!(v[0].get("bidi_ws_url").is_none());
    }

    #[tokio::test]
    async fn cdp_port_present_even_when_probe_fails() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/json/version")
            .with_status(500)
            .create_async()
            .await;

        let url = server.url();
        let port: u16 = url::Url::parse(&url).unwrap().port().unwrap();
        let row = row_for(&url, port, Engine::Cdp);

        let enriched = enrich_rows(vec![row]).await;
        assert_eq!(enriched.len(), 1);
        let e = &enriched[0];
        assert_eq!(e.cdp_port, Some(port));
        assert!(e.cdp_ws_url.is_none());

        let mut buf: Vec<u8> = Vec::new();
        print_json(&mut buf, &enriched).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v[0]["cdp_port"], port);
        assert!(v[0].get("cdp_ws_url").is_none());
    }

    #[tokio::test]
    async fn enriches_bidi_row_with_ws_url_only() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/json/version")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:5555/session"}"#)
            .create_async()
            .await;

        let url = server.url();
        let port: u16 = url::Url::parse(&url).unwrap().port().unwrap();
        let row = row_for(&url, port, Engine::Bidi);

        let enriched = enrich_rows(vec![row]).await;
        let e = &enriched[0];
        assert!(e.cdp_port.is_none());
        assert!(e.cdp_ws_url.is_none());
        assert_eq!(
            e.bidi_ws_url.as_deref(),
            Some("ws://127.0.0.1:5555/session")
        );

        let mut buf: Vec<u8> = Vec::new();
        print_json(&mut buf, &enriched).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert!(v[0].get("cdp_port").is_none());
        assert_eq!(v[0]["bidi_ws_url"], "ws://127.0.0.1:5555/session");
    }
}
