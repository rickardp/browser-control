//! Cross-cutting end-to-end integration tests.
//!
//! These spawn the real `browser-control` binary and exercise flows that
//! span CLI parsing, env resolution, the MCP server skeleton, and table
//! rendering.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use tempfile::TempDir;

#[test]
fn mcp_tools_list_returns_ten_tools() {
    let tmp = TempDir::new().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("browser-control");
    let mut child = Command::new(bin)
        .args(["mcp"])
        .env("BROWSER_CONTROL", "ws://127.0.0.1:9999/fake")
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("init json");
    assert_eq!(v["id"], 1);
    assert!(v["result"]["protocolVersion"].is_string());

    line.clear();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).expect("tools json");
    let tools = v["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 10, "expected 10 tools, got {tools:?}");
    let names: HashSet<String> = tools
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    for expected in [
        "navigate",
        "get_dom",
        "screenshot",
        "fetch",
        "select_element",
        "list_targets",
        "cookies",
        "storage_get",
        "storage_set",
        "wait_for_cookie",
    ] {
        assert!(
            names.contains(expected),
            "missing tool {expected}; got {names:?}"
        );
    }

    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(status.success(), "child exited with {status:?}");
}

#[test]
fn mcp_with_no_browser_errors_helpfully() {
    let tmp = TempDir::new().unwrap();
    let cfg = TempDir::new().unwrap();
    let out = Command::new(assert_cmd::cargo::cargo_bin("browser-control"))
        .args(["mcp"])
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .env_remove("BROWSER_CONTROL")
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no browser selected")
            || stderr.contains("BROWSER_CONTROL")
            || stderr.contains("browser-control start"),
        "stderr should hint at how to select a browser, got: {stderr}"
    );
}

#[test]
fn list_commands_have_stable_headers() {
    let tmp = TempDir::new().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("browser-control");

    let out = Command::new(&bin)
        .args(["list-installed"])
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for h in ["KIND", "VERSION", "ENGINE", "EXECUTABLE"] {
        assert!(s.contains(h), "expected header {h} in:\n{s}");
    }

    let out = Command::new(&bin)
        .args(["list-running"])
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for h in ["NAME", "KIND", "PID", "ENGINE", "ENDPOINT"] {
        assert!(s.contains(h), "expected header {h} in:\n{s}");
    }
}

#[test]
fn help_lists_all_subcommands() {
    let out = Command::new(assert_cmd::cargo::cargo_bin("browser-control"))
        .args(["--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for cmd in [
        "list-installed",
        "list-running",
        "start",
        "mcp",
        "set",
        "get",
        "unset",
    ] {
        assert!(s.contains(cmd), "missing subcommand {cmd} in:\n{s}");
    }
}

#[test]
fn mcp_help_mentions_browser_control_env() {
    let out = Command::new(assert_cmd::cargo::cargo_bin("browser-control"))
        .args(["mcp", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("BROWSER_CONTROL"),
        "mcp --help should mention BROWSER_CONTROL, got:\n{s}"
    );
}

#[test]
fn set_get_unset_default_round_trip() {
    let bin = assert_cmd::cargo::cargo_bin("browser-control");
    let cfg = TempDir::new().unwrap();

    let out = Command::new(&bin)
        .args(["set", "default", "firefox"])
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "set failed: {:?}", out);

    let out = Command::new(&bin)
        .args(["get", "default", "--json"])
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["key"], "default");
    assert_eq!(v["value"], "firefox");

    let out = Command::new(&bin)
        .args(["unset", "default"])
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = Command::new(&bin)
        .args(["get", "default", "--json"])
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["value"].is_null(), "expected null, got {v}");
}

#[test]
fn browser_control_executable_path_is_accepted_as_selector() {
    let tmp = TempDir::new().unwrap();
    let exe = assert_cmd::cargo::cargo_bin("browser-control");
    let out = Command::new(&exe)
        .args(["mcp"])
        .env("BROWSER_CONTROL", exe.to_str().unwrap())
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "should not panic: {stderr}");
}
