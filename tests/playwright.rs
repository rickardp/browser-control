use browser_control::cli::env_resolver::{ResolvedBrowser, Source};
use browser_control::detect::Engine;
use browser_control::mcp::playwright::run_with_streams;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn dummy_resolved() -> ResolvedBrowser {
    ResolvedBrowser {
        endpoint: "ws://127.0.0.1:9999/fake".into(),
        engine: Engine::Cdp,
        source: Source::External,
    }
}

#[tokio::test]
async fn stdio_passthrough_round_trips() {
    if which::which("node").is_err() {
        eprintln!("node not installed, skipping");
        return;
    }
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake_playwright_mcp.js");

    let (mut c_in_w, c_in_r) = tokio::io::duplex(8192);
    let (c_out_w, mut c_out_r) = tokio::io::duplex(8192);
    let (c_err_w, mut c_err_r) = tokio::io::duplex(8192);

    let browser = dummy_resolved();
    let cmd = (
        "node".to_string(),
        vec![
            script.to_string_lossy().to_string(),
            "--cdp-endpoint".into(),
            browser.endpoint.clone(),
        ],
    );

    let handle = tokio::spawn(async move {
        run_with_streams(&browser, c_in_r, c_out_w, c_err_w, Some(cmd))
            .await
            .unwrap()
    });

    let req = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    c_in_w.write_all(req).await.unwrap();
    drop(c_in_w);

    let mut buf = Vec::new();
    c_out_r.read_to_end(&mut buf).await.unwrap();
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("\"id\":1"), "expected echo of id=1, got: {s}");
    assert!(s.contains("\"echo\""));

    let mut ebuf = Vec::new();
    c_err_r.read_to_end(&mut ebuf).await.unwrap();
    let e = String::from_utf8(ebuf).unwrap();
    assert!(
        e.contains("fake-playwright-mcp: started"),
        "stderr was: {e}"
    );

    let code = handle.await.unwrap();
    assert_eq!(code, 0);
}
