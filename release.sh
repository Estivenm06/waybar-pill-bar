#!/bin/bash
# Maintainer script for the prebuilt notch that first installs download (files/setup/fetch-prebuilt.sh).
#
#   ./release.sh --verify    check the currently listed snapshot (release v<version>): its source hash must equal the
#                            sources in this checkout and its binary must match its checksum. Changes nothing.
#   ./release.sh [--publish] verify and publish: test and build the committed sources, then create or update
#                            release v<version> so the snapshot matches the newer commit.
#
# --publish needs a clean, pushed git tree, cargo, zstd and an authenticated `gh` (GH_TOKEN works).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
MODE=publish
case "${1:-}" in --verify) MODE=verify ;; --publish|"") ;; *) echo "usage: ./release.sh [--verify|--publish]"; exit 2 ;; esac

if [ "$MODE" = verify ]; then
  VERSION=$(jq -r '.version' manifest.json)
  HASH=$(bash files/setup/source-hash.sh .)
  BASE="${PILL_BAR_RELEASE_URL:-https://github.com/Estivenm06/waybar-pill-bar/releases/download/v$VERSION}"
  T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
  curl -fsSL --max-time 60 -o "$T/info.json" "$BASE/release-info.json" || { echo "verify: no snapshot is listed for v$VERSION (run ./release.sh --publish)"; exit 1; }
  LISTED=$(jq -r '.source_hash' "$T/info.json")
  [ "$LISTED" = "$HASH" ] || { echo "verify: STALE. v$VERSION lists sources $LISTED, this checkout has $HASH (run ./release.sh --publish)"; exit 1; }
  curl -fsSL --max-time 120 -o "$T/bin.zst" "$BASE/waybar-notch-x86_64.zst" && zstd -d -q -o "$T/bin" "$T/bin.zst" \
    || { echo "verify: the listed binary cannot be downloaded or unpacked"; exit 1; }
  [ "$(sha256sum "$T/bin" | cut -d' ' -f1)" = "$(jq -r '.binary_sha256' "$T/info.json")" ] || { echo "verify: the listed binary does not match its checksum"; exit 1; }
  echo "verify: ok, v$VERSION is the snapshot of the current sources ($HASH)"
  exit 0
fi

[ -z "$(git status --porcelain)" ] || { echo "release: commit your changes first (the binary must match the pushed sources)"; exit 1; }
git fetch -q origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "release: push main first"; exit 1; }

VERSION=$(jq -r '.version' manifest.json)
TAG="v$VERSION"
HASH=$(bash files/setup/source-hash.sh .)
OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT

export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/waybar-notch/target"
cargo test --locked --manifest-path notch/Cargo.toml
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
