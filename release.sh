#!/bin/bash
# Maintainer script: build the notch from the committed sources and publish it as the GitHub release v<version>.
# First installs download this binary instead of compiling for minutes (files/setup/fetch-prebuilt.sh).
# Needs a clean, pushed git tree, cargo, zstd and an authenticated `gh`.
# Usage: ./release.sh
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

[ -z "$(git status --porcelain)" ] || { echo "release: commit your changes first (the binary must match the pushed sources)"; exit 1; }
git fetch -q origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "release: push main first"; exit 1; }

VERSION=$(jq -r '.version' manifest.json)
TAG="v$VERSION"
HASH=$(bash files/setup/source-hash.sh .)
OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT

export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/waybar-notch/target"
cargo build --release --locked --manifest-path notch/Cargo.toml
BIN="$CARGO_TARGET_DIR/release/waybar-notch"
zstd -19 -q -o "$OUT/waybar-notch-x86_64.zst" "$BIN"
jq -n --arg h "$HASH" --arg s "$(sha256sum "$BIN" | cut -d' ' -f1)" --arg v "$VERSION" \
  '{version:$v, source_hash:$h, binary_sha256:$s, arch:"x86_64"}' > "$OUT/release-info.json"

if gh release view "$TAG" >/dev/null 2>&1; then
  gh release upload "$TAG" "$OUT/waybar-notch-x86_64.zst" "$OUT/release-info.json" --clobber
else
  gh release create "$TAG" "$OUT/waybar-notch-x86_64.zst" "$OUT/release-info.json" \
    --target "$(git rev-parse HEAD)" --title "Pill Bar $VERSION" \
    --notes "Prebuilt notch for x86_64 Arch/Omarchy. Installs download it automatically (and compile from source if it does not match)."
fi
echo "release: published $TAG (source $HASH)"
