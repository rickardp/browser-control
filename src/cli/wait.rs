//! `browser-control wait` — block until the browser endpoint is ready.

use anyhow::{anyhow, bail, Result};
use std::time::{Duration, Instant};

use crate::detect::Engine;

pub async fn run(browser: Option<String>, ready: bool, timeout: u64) -> Result<()> {
    if !ready {
        bail!("`wait` requires --ready in this version");
    }
    let resolved = crate::cli::mcp::resolve_browser(browser).await?;
    wait_until_ready(&resolved.endpoint, resolved.engine, Duration::from_secs(timeout)).await?;
    println!("ready");
    Ok(())
}

async fn probe_once(client: &reqwest::Client, endpoint: &str, engine: Engine) -> bool {
    let base = endpoint.trim_end_matches('/');
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

async fn wait_until_ready(endpoint: &str, engine: Engine, timeout: Duration) -> Result<()> {
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
