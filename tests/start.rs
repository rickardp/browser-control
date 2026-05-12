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
