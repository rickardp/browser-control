#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 <version>" >&2
  echo "  <version>  semver without leading 'v' (e.g. 0.1.0)" >&2
  exit 2
}

if [ $# -ne 1 ]; then
  usage
fi

VERSION="$1"

if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  echo "error: version '$VERSION' is not valid semver (without leading v)" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree is not clean. Commit or stash changes first." >&2
  git status --short >&2
  exit 1
fi

echo "==> cargo fmt --check"
cargo fmt --check

echo "==> cargo clippy -- -D warnings"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test --all-targets"
cargo test --all-targets

echo "==> cargo build --release"
cargo build --release

echo ""
echo "Ready to tag v${VERSION}: run \`git tag v${VERSION} && git push origin v${VERSION}\`"
