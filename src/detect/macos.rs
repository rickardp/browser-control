//! macOS browser detection.

use super::{Installed, Kind, Probe};
use std::path::PathBuf;

pub(super) fn detect<P: Probe>(probe: &P) -> Vec<Installed> {
    let candidates: &[(Kind, &str)] = &[
        (
            Kind::Chrome,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ),
        (
            Kind::Edge,
            "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        ),
        (
            Kind::Chromium,
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ),
        (
            Kind::Brave,
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        ),
        (
            Kind::Firefox,
            "/Applications/Firefox.app/Contents/MacOS/firefox",
        ),
    ];
    let mut out = Vec::new();
    for (kind, path) in candidates {
        let p = PathBuf::from(path);
        if probe.exists(&p) {
            let version = probe
                .run_version(&p)
                .unwrap_or_else(|| "unknown".to_string());
            out.push(Installed {
                kind: *kind,
                executable: p,
                version,
                engine: kind.engine(),
            });
        }
    }
    out
}
