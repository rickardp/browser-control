//! Integration tests for the MCP server skeleton.
//!
//! These exercise the public library API directly (no spawned subprocess) so
//! they don't depend on having a real browser available.

use browser_control::cli::env_resolver::{ResolvedBrowser, Source};
use browser_control::detect::Engine;
use browser_control::mcp::server::{run_with_streams, ServerState, ToolRegistry};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn dummy_state() -> ServerState {
    ServerState {
        browser: ResolvedBrowser {
            endpoint: "ws://x".into(),
            engine: Engine::Cdp,
            source: Source::External,
        },
    }
}

#[tokio::test]
async fn initialize_round_trip() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize","params":{}
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["serverInfo"]["name"], "browser-control");
}

#[tokio::test]
async fn tools_list_empty() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["result"]["tools"], serde_json::json!([]));
}

#[tokio::test]
async fn unknown_method_yields_method_not_found() {
    let (mut c_to_s, s_in) = tokio::io::duplex(8192);
    let (s_out, c_from_s) = tokio::io::duplex(8192);
    tokio::spawn(async move {
        let _ = run_with_streams(dummy_state(), ToolRegistry::new(), s_in, s_out).await;
    });

    let req = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"does/not/exist"});
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    c_to_s.write_all(&bytes).await.unwrap();

    let mut reader = BufReader::new(c_from_s);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["error"]["code"], -32601);
}
