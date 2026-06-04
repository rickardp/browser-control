//! Cross-platform browser detection.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Chrome,
    Edge,
    Chromium,
    Brave,
    Firefox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Cdp,
    Bidi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub kind: Kind,
    pub executable: PathBuf,
    pub version: String,
    pub engine: Engine,
}

impl Kind {
    pub fn engine(self) -> Engine {
        match self {
            Kind::Firefox => Engine::Bidi,
            _ => Engine::Cdp,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Chrome => "chrome",
            Kind::Edge => "edge",
            Kind::Chromium => "chromium",
            Kind::Brave => "brave",
            Kind::Firefox => "firefox",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "chrome" => Some(Kind::Chrome),
            "edge" => Some(Kind::Edge),
            "chromium" => Some(Kind::Chromium),
            "brave" => Some(Kind::Brave),
            "firefox" => Some(Kind::Firefox),
            _ => None,
        }
    }

    pub fn is_chromium(self) -> bool {
        !matches!(self, Kind::Firefox)
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Kind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Kind::parse(s).ok_or(())
    }
}

/// Filesystem/process injection trait for testability.
pub trait Probe {
    fn exists(&self, p: &Path) -> bool;
    fn run_version(&self, exe: &Path) -> Option<String>;
    fn which(&self, name: &str) -> Option<PathBuf>;
}

pub struct RealProbe;

impl Probe for RealProbe {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn run_version(&self, exe: &Path) -> Option<String> {
        if !exe.exists() {
            return None;
        }
        use std::process::Stdio;
        use std::time::Duration;
        use wait_timeout::ChildExt;

        let mut child = std::process::Command::new(exe)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        // Some browsers (notably GUI builds on Windows) never exit when invoked
        // with --version. Cap the wait so detection cannot hang the caller.
        match child.wait_timeout(Duration::from_secs(5)).ok()? {
            Some(status) if status.success() => {}
            Some(_) => {
                let _ = child.wait();
                return None;
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }

        let output = child.wait_with_output().ok()?;
        let s = String::from_utf8_lossy(&output.stdout);
        parse_version(&s)
    }

    fn which(&self, name: &str) -> Option<PathBuf> {
        which::which(name).ok()
    }
}

/// Take the last whitespace-separated token from version output as the version.
fn parse_version(s: &str) -> Option<String> {
    let line = s.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let token = line.split_whitespace().last()?;
    Some(token.to_string())
}

pub fn list_installed() -> Vec<Installed> {
    list_installed_with(&RealProbe)
}

pub fn list_installed_with<P: Probe>(probe: &P) -> Vec<Installed> {
    #[cfg(target_os = "macos")]
    {
        crate::detect::macos::detect(probe)
    }
    #[cfg(target_os = "linux")]
    {
        crate::detect::linux::detect(probe)
    }
    #[cfg(target_os = "windows")]
    {
        crate::detect::windows::detect(probe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = probe;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeProbe {
        existing: HashSet<PathBuf>,
        path_map: HashMap<String, PathBuf>,
        versions: HashMap<PathBuf, String>,
    }

    impl Probe for FakeProbe {
        fn exists(&self, p: &Path) -> bool {
            self.existing.contains(p)
        }
        fn run_version(&self, exe: &Path) -> Option<String> {
            self.versions.get(exe).cloned()
        }
        fn which(&self, name: &str) -> Option<PathBuf> {
            self.path_map.get(name).cloned()
        }
    }

    #[test]
    fn kind_parse_roundtrip() {
        for k in [
            Kind::Chrome,
            Kind::Edge,
            Kind::Chromium,
            Kind::Brave,
            Kind::Firefox,
        ] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert_eq!(k.to_string(), k.as_str());
            assert_eq!(k.as_str().parse::<Kind>().unwrap(), k);
        }
        assert_eq!(Kind::parse("CHROME"), Some(Kind::Chrome));
        assert_eq!(Kind::parse("Firefox"), Some(Kind::Firefox));
        assert_eq!(Kind::parse("safari"), None);
    }

    #[test]
    fn kind_engine_mapping() {
        assert_eq!(Kind::Firefox.engine(), Engine::Bidi);
        assert_eq!(Kind::Chrome.engine(), Engine::Cdp);
        assert_eq!(Kind::Edge.engine(), Engine::Cdp);
        assert_eq!(Kind::Chromium.engine(), Engine::Cdp);
        assert_eq!(Kind::Brave.engine(), Engine::Cdp);
        assert!(Kind::Chrome.is_chromium());
        assert!(!Kind::Firefox.is_chromium());
    }

    #[test]
    fn parse_version_takes_last_token() {
        assert_eq!(
            parse_version("Google Chrome 130.0.6723.91\n").as_deref(),
            Some("130.0.6723.91")
        );
        assert_eq!(
            parse_version("Mozilla Firefox 131.0").as_deref(),
            Some("131.0")
        );
        assert_eq!(parse_version(""), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_finds_chrome_and_firefox() {
        let chrome = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        let firefox = PathBuf::from("/Applications/Firefox.app/Contents/MacOS/firefox");
        let mut probe = FakeProbe::default();
        probe.existing.insert(chrome.clone());
        probe.existing.insert(firefox.clone());
        probe
            .versions
            .insert(chrome.clone(), "130.0.6723.91".to_string());
        probe.versions.insert(firefox.clone(), "131.0".to_string());

        let found = super::macos::detect(&probe);
        assert_eq!(found.len(), 2);
        let chrome_entry = found.iter().find(|i| i.kind == Kind::Chrome).unwrap();
        assert_eq!(chrome_entry.executable, chrome);
        assert_eq!(chrome_entry.version, "130.0.6723.91");
        assert_eq!(chrome_entry.engine, Engine::Cdp);
        let ff_entry = found.iter().find(|i| i.kind == Kind::Firefox).unwrap();
        assert_eq!(ff_entry.executable, firefox);
        assert_eq!(ff_entry.engine, Engine::Bidi);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unknown_version_when_run_fails() {
        let chrome = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        let mut probe = FakeProbe::default();
        probe.existing.insert(chrome.clone());
        let found = super::macos::detect(&probe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].version, "unknown");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_finds_chrome_via_which_and_firefox_absolute() {
        let chrome = PathBuf::from("/usr/local/bin/google-chrome");
        let firefox = PathBuf::from("/usr/bin/firefox");
        let mut probe = FakeProbe::default();
        probe
            .path_map
            .insert("google-chrome".to_string(), chrome.clone());
        probe.existing.insert(firefox.clone());
        probe
            .versions
            .insert(chrome.clone(), "130.0.6723.91".to_string());
        probe.versions.insert(firefox.clone(), "131.0".to_string());

        let found = super::linux::detect(&probe);
        let chrome_entry = found.iter().find(|i| i.kind == Kind::Chrome).unwrap();
        assert_eq!(chrome_entry.executable, chrome);
        assert_eq!(chrome_entry.engine, Engine::Cdp);
        let ff_entry = found.iter().find(|i| i.kind == Kind::Firefox).unwrap();
        assert_eq!(ff_entry.executable, firefox);
        assert_eq!(ff_entry.engine, Engine::Bidi);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_takes_first_match_per_kind() {
        let chrome_a = PathBuf::from("/usr/local/bin/google-chrome");
        let chrome_b = PathBuf::from("/usr/bin/google-chrome");
        let mut probe = FakeProbe::default();
        probe
            .path_map
            .insert("google-chrome".to_string(), chrome_a.clone());
        probe.existing.insert(chrome_b.clone());
        let found = super::linux::detect(&probe);
        let chrome_entries: Vec<_> = found.iter().filter(|i| i.kind == Kind::Chrome).collect();
        assert_eq!(chrome_entries.len(), 1);
        assert_eq!(chrome_entries[0].executable, chrome_a);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_finds_chrome_and_firefox() {
        let chrome = PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe");
        let firefox = PathBuf::from(r"C:\Program Files\Mozilla Firefox\firefox.exe");
        let mut probe = FakeProbe::default();
        probe.existing.insert(chrome.clone());
        probe.existing.insert(firefox.clone());
        probe
            .versions
            .insert(chrome.clone(), "130.0.6723.91".to_string());
        probe.versions.insert(firefox.clone(), "131.0".to_string());

        let found = super::windows::detect(&probe);
        assert!(found
            .iter()
            .any(|i| i.kind == Kind::Chrome && i.executable == chrome));
        assert!(found
            .iter()
            .any(|i| i.kind == Kind::Firefox && i.executable == firefox));
    }
}
