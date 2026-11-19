#!/bin/bash
# Download the prebuilt notch for exactly these sources, so a first install does not have to compile for minutes.
#
# The GitHub release v<manifest version> carries
#   release-info.json            {"source_hash": ..., "binary_sha256": ..., "arch": "x86_64"}
#   waybar-notch-x86_64.zst      the stripped release binary, zstd-compressed
# The download is used only when source_hash equals this checkout's source hash, the binary's SHA-256 matches and
# every shared library it links is present. Anything else exits 1 and the caller compiles from source instead.
# PILL_BAR_NO_PREBUILT=1 skips the download. PILL_BAR_RELEASE_URL overrides the release base URL (tests).
#
# Usage: fetch-prebuilt.sh PLUGIN_ROOT SOURCE_HASH DEST_FILE
set -uo pipefail
ROOT="$1"; WANT="$2"; DEST="$3"
[ "${PILL_BAR_NO_PREBUILT:-0}" = 1 ] && { echo "prebuilt: disabled"; exit 1; }
[ "$(uname -m)" = x86_64 ] || { echo "prebuilt: only x86_64 is published"; exit 1; }
for c in curl jq zstd sha256sum ldd; do command -v "$c" >/dev/null 2>&1 || { echo "prebuilt: $c missing"; exit 1; }; done

VERSION=$(jq -r '.version' "$ROOT/manifest.json")
BASE="${PILL_BAR_RELEASE_URL:-https://github.com/Estivenm06/waybar-pill-bar/releases/download/v$VERSION}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
get() { curl -fsSL --proto '=https,file' --max-time 120 --retry 2 -o "$2" "$BASE/$1"; }

get release-info.json "$TMP/info.json" || { echo "prebuilt: no release for v$VERSION"; exit 1; }
[ "$(jq -r '.source_hash' "$TMP/info.json")" = "$WANT" ] || { echo "prebuilt: release was built from other sources"; exit 1; }
[ "$(jq -r '.arch' "$TMP/info.json")" = x86_64 ] || { echo "prebuilt: wrong architecture"; exit 1; }
SUM=$(jq -r '.binary_sha256' "$TMP/info.json")
[[ $SUM =~ ^[0-9a-f]{64}$ ]] || { echo "prebuilt: bad checksum in release info"; exit 1; }

echo "prebuilt: downloading waybar-notch for v$VERSION"
get waybar-notch-x86_64.zst "$TMP/bin.zst" || { echo "prebuilt: download failed"; exit 1; }
zstd -d -q -o "$TMP/bin" "$TMP/bin.zst" || { echo "prebuilt: corrupt archive"; exit 1; }
[ "$(sha256sum "$TMP/bin" | cut -d' ' -f1)" = "$SUM" ] || { echo "prebuilt: checksum mismatch, not using it"; exit 1; }
chmod 755 "$TMP/bin"
if ldd "$TMP/bin" 2>&1 | grep -q "not found"; then echo "prebuilt: needs libraries this system lacks, building instead"; exit 1; fi
mkdir -p "$(dirname "$DEST")" && mv "$TMP/bin" "$DEST" || exit 1
echo "prebuilt: ok"
