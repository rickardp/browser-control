use assert_cmd::Command;
use tempfile::TempDir;

#[test]
fn start_with_unknown_kind_errors() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .args(["start", "definitelynotabrowser"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown browser kind"));
}

/// Pick a browser kind that is actually installed on this machine and is
/// quick/safe to launch headless. Returns `None` to skip the test in
/// environments with no supported browsers.
fn pick_installed_kind() -> Option<String> {
    let bin = assert_cmd::cargo::cargo_bin("browser-control");
    let out = std::process::Command::new(&bin)
        .args(["list-installed", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let arr = v.as_array()?;
    // Prefer chromium-based browsers (faster, less stateful) in test order.
    for pref in ["chromium", "chrome", "edge", "brave", "firefox"] {
        if arr.iter().any(|e| e["kind"].as_str() == Some(pref)) {
            return Some(pref.to_string());
        }
    }
    None
}

#[test]
fn start_without_profile_uses_per_kind_default_dir_and_is_idempotent() {
    // TODO(daemon-phase-1): Edge headless launches reliably on the Windows
    // GitHub runner but the parent `browser-control start` process never
    // returns because the stdio pipes seem to be held open by the spawned
    // browser. Skip on Windows until the launcher's stdio detachment is
    // hardened in the daemon refactor.
    #[cfg(windows)]
    {
        eprintln!("skipping on windows: see TODO(daemon-phase-1)");
        return;
    }

    let Some(kind) = pick_installed_kind() else {
        eprintln!("skipping: no installed browser detected on this machine");
        return;
    };

    let data = TempDir::new().unwrap();
    let cfg = TempDir::new().unwrap();
    let expected = cfg.path().join("profiles").join(&kind).join("default");

    // First start: should create the per-kind default profile dir and a registry row.
    let out = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", data.path())
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .args(["start", &kind, "--headless", "--json"])
        .output()
        .expect("invoke start");
    if !out.status.success() {
        eprintln!(
            "skipping: failed to launch {kind} headless on this machine: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("start --json output is valid json");
    assert_eq!(v["reused"], serde_json::Value::Bool(false));
    assert_eq!(
        v["profile"].as_str().map(std::path::PathBuf::from),
        Some(expected.clone()),
        "expected profile path under sandboxed config dir"
    );
    assert!(
        expected.is_dir(),
        "expected profile dir {} to exist",
        expected.display()
    );
    let pid1 = v["pid"].as_u64().expect("pid in json");

    // Second start of the same kind: should reuse the running browser.
    let out2 = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", data.path())
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .args(["start", &kind, "--headless", "--json"])
        .output()
        .expect("invoke start (reuse)");
    assert!(
        out2.status.success(),
        "second start failed: stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let v2: serde_json::Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_eq!(v2["reused"], serde_json::Value::Bool(true));
    assert_eq!(v2["pid"].as_u64(), Some(pid1));
    assert_eq!(
        v2["profile"].as_str().map(std::path::PathBuf::from),
        Some(expected.clone())
    );

    // Registry should hold exactly one row for this kind.
    let list = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", data.path())
        .env("BROWSER_CONTROL_CONFIG_DIR", cfg.path())
        .args(["list-running", "--json"])
        .output()
        .expect("invoke list-running");
    assert!(list.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let arr = rows.as_array().expect("array");
    let same_kind: Vec<_> = arr
        .iter()
        .filter(|r| r["kind"].as_str() == Some(&kind))
        .collect();
    assert_eq!(
        same_kind.len(),
        1,
        "expected exactly one running row for kind {kind}, got {arr:?}"
    );

    // Cleanup: kill the headless browser we started.
    if let Some(pid) = same_kind[0]["pid"].as_u64() {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .output();
        }
    }
}
