#!/bin/bash
# Maintainer script for the prebuilt notch that first installs download (files/setup/fetch-prebuilt.sh).
#
# Installers trust only the pin committed in files/setup/prebuilt.json, never anything the release host serves.
#
#   ./release.sh --verify    check the committed pin: its source hash must equal the sources in this checkout, and the
#                            asset published under its tag must match its checksum. Changes nothing.
#   ./release.sh [--publish] test and build the committed sources and publish them as a NEW candidate release
#                            notch-<commit>. Existing releases are never overwritten. The candidate pin is printed;
#                            review it and commit it as files/setup/prebuilt.json to approve the binary.
#
# --publish needs a clean, pushed git tree, cargo, zstd and an authenticated `gh` (GH_TOKEN works).
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"
MODE=publish
case "${1:-}" in --verify) MODE=verify ;; --publish|"") ;; *) echo "usage: ./release.sh [--verify|--publish]"; exit 2 ;; esac

if [ "$MODE" = verify ]; then
  HASH=$(bash files/setup/source-hash.sh .)
  T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
  PILL_BAR_NO_PREBUILT=0 bash files/setup/fetch-prebuilt.sh . "$HASH" "$T/waybar-notch" || { echo "verify: FAILED, the committed pin does not authorize an artifact for these sources"; exit 1; }
  echo "verify: ok, the pinned artifact matches the current sources ($HASH)"
  exit 0
fi

[ -z "$(git status --porcelain)" ] || { echo "release: commit your changes first (the binary must match the pushed sources)"; exit 1; }
git fetch -q origin
COMMIT=$(git rev-parse HEAD)
[ "$COMMIT" = "$(git rev-parse origin/main)" ] || { echo "release: push main first"; exit 1; }

VERSION=$(jq -r '.version' manifest.json)
TAG="notch-$COMMIT"
HASH=$(bash files/setup/source-hash.sh .)
OUT=$(mktemp -d); trap 'rm -rf "$OUT"' EXIT

# Only a confirmed 404 for this exact tag permits creating it.
if ERR=$(gh api "repos/{owner}/{repo}/releases/tags/$TAG" 2>&1 >/dev/null); then
  echo "release: candidate already exists; refusing to overwrite"; exit 1
fi
case "$ERR" in *"HTTP 404"*) ;; *) echo "release: could not check for $TAG: $ERR"; exit 1 ;; esac

export CARGO_TARGET_DIR="$OUT/target"
cargo test --locked --manifest-path notch/Cargo.toml
cargo build --release --locked --manifest-path notch/Cargo.toml
BIN="$CARGO_TARGET_DIR/release/waybar-notch"
zstd -19 -q -o "$OUT/waybar-notch-x86_64.zst" "$BIN"
jq -n --arg c "$COMMIT" --arg t "$TAG" --arg h "$HASH" --arg s "$(sha256sum "$BIN" | cut -d' ' -f1)" \
  '{source_commit:$c, tag:$t, source_hash:$h, arch:"x86_64", binary_sha256:$s}' > "$OUT/release-info.json"

gh release create "$TAG" "$OUT/waybar-notch-x86_64.zst" "$OUT/release-info.json" \
  --target "$COMMIT" --title "Notch candidate $COMMIT" \
  --notes "Candidate prebuilt notch (Pill Bar $VERSION). Installs use it only after its pin is committed to files/setup/prebuilt.json."
echo "release: published candidate $TAG (source $HASH)"
echo "release: candidate pin (review and commit as files/setup/prebuilt.json):"
cat "$OUT/release-info.json"
