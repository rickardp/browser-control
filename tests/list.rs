use assert_cmd::Command;
use tempfile::TempDir;

#[test]
fn list_installed_table_runs() {
    let tmp = TempDir::new().unwrap();
    let assert = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .args(["list-installed"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("KIND"));
}

#[test]
fn list_installed_json_is_valid_json() {
    let tmp = TempDir::new().unwrap();
    let assert = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .args(["list-installed", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let _: serde_json::Value = serde_json::from_str(&stdout).unwrap();
}

#[test]
fn list_running_empty_registry_succeeds() {
    let tmp = TempDir::new().unwrap();
    let assert = Command::cargo_bin("browser-control")
        .unwrap()
        .env("BROWSER_CONTROL_DATA_DIR", tmp.path())
        .args(["list-running"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("NAME"));
}
