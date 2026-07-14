#!/usr/bin/env bash
# Thin wrapper around `cargo release`. Drives the workflow defined in
# release.toml: bump, verify (cargo publish --dry-run), commit, tag, push.
# CI takes over from the tag push — see .github/workflows/release.yml.
#
# Usage:
#   scripts/release.sh patch
#   scripts/release.sh minor
#   scripts/release.sh major
#   scripts/release.sh 1.2.3
#
# Add --dry-run to inspect what cargo-release would do without touching git
# or the network:
#   scripts/release.sh patch --dry-run
#
# Prereqs:
#   cargo install cargo-release
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "Usage: $0 <level|version> [--dry-run]" >&2
  exit 2
fi

if ! command -v cargo-release >/dev/null 2>&1; then
  echo "error: cargo-release not installed. Run: cargo install cargo-release" >&2
  exit 1
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

LEVEL="$1"
shift || true

DRY_RUN=false
for arg in "$@"; do
  if [ "$arg" = "--dry-run" ]; then
    DRY_RUN=true
  else
    echo "Usage: $0 <level|version> [--dry-run]" >&2
    exit 2
  fi
done

if [ "$DRY_RUN" = true ]; then
  exec cargo release "$LEVEL"
fi

exec cargo release "$LEVEL" --execute
