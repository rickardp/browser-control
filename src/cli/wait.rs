//! `browser-control wait` — block until the browser endpoint is ready.

use anyhow::{anyhow, bail, Result};
use std::time::{Duration, Instant};

use crate::detect::Engine;

pub async fn run(browser: Option<String>, ready: bool, timeout: u64) -> Result<()> {
    if !ready {
        bail!("`wait` requires --ready in this version");
    }
    let resolved = crate::cli::mcp::resolve_browser(browser).await?;
    wait_until_ready(
        &resolved.endpoint,
        resolved.engine,
        Duration::from_secs(timeout),
    )
    .await?;
    println!("ready");
    Ok(())
}

/// Convert a raw browser endpoint (which may be a `ws://host:port/...` URL or
/// already an `http://host:port` base) into the HTTP base used for probing.
pub(crate) fn http_base_from_endpoint(endpoint: &str) -> String {
    if let Ok(u) = url::Url::parse(endpoint) {
        let scheme = match u.scheme() {
            "ws" | "http" => "http",
            "wss" | "https" => "https",
            other => other,
        };
        if let (Some(host), Some(port)) = (u.host_str(), u.port_or_known_default()) {
            return format!("{scheme}://{host}:{port}");
        }
    }
    endpoint.trim_end_matches('/').to_string()
}

async fn probe_once(client: &reqwest::Client, endpoint: &str, engine: Engine) -> bool {
    let base = http_base_from_endpoint(endpoint);
    let url = format!("{base}/json/version");
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !resp.status().is_success() {
        return false;
    }
    match engine {
        Engine::Cdp => {
            let v: serde_json::Value = match resp.json().await {
                Ok(v) => v,
                Err(_) => return false,
            };
            v.get("webSocketDebuggerUrl")
                .and_then(|x| x.as_str())
                .is_some()
        }
        Engine::Bidi => true,
    }
}

pub(crate) async fn wait_until_ready(
    endpoint: &str,
    engine: Engine,
    timeout: Duration,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| anyhow!("failed to build http client: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if probe_once(&client, endpoint, engine).await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "timeout after {}s waiting for browser ready",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_base_from_ws_endpoint_with_path() {
        let got = http_base_from_endpoint(
            "ws://127.0.0.1:52679/devtools/browser/992c1917-9f77-4eee-9bf7-90f400d826b5",
        );
        assert_eq!(got, "http://127.0.0.1:52679");
    }

    #[test]
    fn http_base_from_wss_endpoint() {
        let got = http_base_from_endpoint("wss://host.example:9222/session/abc");
        assert_eq!(got, "https://host.example:9222");
    }

    #[test]
    fn http_base_passes_through_http_base() {
        let got = http_base_from_endpoint("http://127.0.0.1:9222");
        assert_eq!(got, "http://127.0.0.1:9222");
    }

    #[tokio::test]
    async fn wait_ready_succeeds_when_server_immediately_responds() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/json/version")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"webSocketDebuggerUrl":"ws://x"}"#)
            .expect_at_least(1)
            .create_async()
            .await;

        let url = server.url();
        let res = tokio::time::timeout(
            Duration::from_secs(1),
            wait_until_ready(&url, Engine::Cdp, Duration::from_secs(5)),
        )
        .await
        .expect("did not complete within 1s");
        assert!(res.is_ok(), "expected Ok, got {res:?}");
    }

    #[tokio::test]
    async fn wait_ready_times_out_when_server_500s() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/json/version")
            .with_status(500)
            .expect_at_least(1)
            .create_async()
            .await;

        let url = server.url();
        let res = wait_until_ready(&url, Engine::Cdp, Duration::from_secs(1)).await;
        let err = res.expect_err("expected timeout error");
        assert!(
            err.to_string().contains("timeout"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn wait_ready_succeeds_after_initial_failure() {
        let mut server = mockito::Server::new_async().await;
        let m_fail = server
            .mock("GET", "/json/version")
            .with_status(500)
            .expect(2)
            .create_async()
            .await;
        let m_ok = server
            .mock("GET", "/json/version")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"webSocketDebuggerUrl":"ws://x"}"#)
            .expect_at_least(1)
            .create_async()
            .await;

        let url = server.url();
        let res = wait_until_ready(&url, Engine::Cdp, Duration::from_secs(5)).await;
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        m_fail.assert_async().await;
        m_ok.assert_async().await;
    }
}
