//! Windows browser detection.

use super::{Installed, Kind, Probe};
use std::path::PathBuf;

fn expand(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("%LOCALAPPDATA%") {
        let base = std::env::var("LOCALAPPDATA").ok()?;
        Some(format!("{}{}", base, rest))
    } else {
        Some(path.to_string())
    }
}

fn candidates_for(kind: Kind) -> &'static [&'static str] {
    match kind {
        Kind::Chrome => &[
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe",
        ],
        Kind::Edge => &[
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        ],
        Kind::Chromium => &[r"%LOCALAPPDATA%\Chromium\Application\chrome.exe"],
        Kind::Brave => &[
            r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
            r"%LOCALAPPDATA%\BraveSoftware\Brave-Browser\Application\brave.exe",
        ],
        Kind::Firefox => &[
            r"C:\Program Files\Mozilla Firefox\firefox.exe",
            r"C:\Program Files (x86)\Mozilla Firefox\firefox.exe",
        ],
    }
}

pub(super) fn detect<P: Probe>(probe: &P) -> Vec<Installed> {
    let mut out = Vec::new();
    for kind in [
        Kind::Chrome,
        Kind::Edge,
        Kind::Chromium,
        Kind::Brave,
        Kind::Firefox,
    ] {
        for raw in candidates_for(kind) {
            let Some(expanded) = expand(raw) else {
                continue;
            };
            let p = PathBuf::from(expanded);
            if probe.exists(&p) {
                let version = probe
                    .run_version(&p)
                    .unwrap_or_else(|| "unknown".to_string());
                out.push(Installed {
                    kind,
                    executable: p,
                    version,
                    engine: kind.engine(),
                });
                break;
            }
        }
    }
    out
}
