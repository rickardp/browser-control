//! Linux browser detection.

use super::{Installed, Kind, Probe};
use std::path::PathBuf;

enum Cand<'a> {
    Which(&'a str),
    Abs(&'a str),
}

fn candidates_for(kind: Kind) -> &'static [Cand<'static>] {
    match kind {
        Kind::Chrome => &[
            Cand::Which("google-chrome"),
            Cand::Which("google-chrome-stable"),
            Cand::Abs("/usr/bin/google-chrome"),
            Cand::Abs("/opt/google/chrome/chrome"),
        ],
        Kind::Edge => &[
            Cand::Which("microsoft-edge"),
            Cand::Which("microsoft-edge-stable"),
            Cand::Abs("/usr/bin/microsoft-edge"),
        ],
        Kind::Chromium => &[
            Cand::Which("chromium"),
            Cand::Which("chromium-browser"),
            Cand::Abs("/usr/bin/chromium"),
            Cand::Abs("/snap/bin/chromium"),
        ],
        Kind::Brave => &[
            Cand::Which("brave-browser"),
            Cand::Which("brave"),
            Cand::Abs("/usr/bin/brave-browser"),
        ],
        Kind::Firefox => &[
            Cand::Which("firefox"),
            Cand::Abs("/usr/bin/firefox"),
            Cand::Abs("/snap/bin/firefox"),
        ],
    }
}

fn find_one<P: Probe>(probe: &P, kind: Kind) -> Option<PathBuf> {
    for c in candidates_for(kind) {
        match c {
            Cand::Which(name) => {
                if let Some(p) = probe.which(name) {
                    return Some(p);
                }
            }
            Cand::Abs(path) => {
                let p = PathBuf::from(path);
                if probe.exists(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
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
        if let Some(exe) = find_one(probe, kind) {
            let version = probe
                .run_version(&exe)
                .unwrap_or_else(|| "unknown".to_string());
            out.push(Installed {
                kind,
                executable: exe,
                version,
                engine: kind.engine(),
            });
        }
    }
    out
}
