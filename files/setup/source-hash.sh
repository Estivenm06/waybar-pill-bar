#!/bin/bash
# Print the content hash of the Rust sources (what decides whether a built notch matches this checkout).
# Used by install.sh (skip builds, match prebuilt releases) and release.sh (name the release's source).
# Usage: source-hash.sh [PLUGIN_ROOT]
set -euo pipefail
ROOT="${1:-$(dirname "$(readlink -f "$0")")/../..}"
cd "$ROOT/notch" 2>/dev/null || cd "$ROOT"
{ cat Cargo.toml Cargo.lock 2>/dev/null || true; { find src -type f 2>/dev/null || true; } | LC_ALL=C sort | xargs -r cat; } | sha256sum | cut -d' ' -f1
