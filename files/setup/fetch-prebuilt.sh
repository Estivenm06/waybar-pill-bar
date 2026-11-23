#!/bin/bash
# Use a prebuilt notch only when this checkout itself authorizes its bytes.
#
# files/setup/prebuilt.json (committed, reviewed) pins one artifact:
#   {"source_commit": <40 hex>, "tag": "notch-<source_commit>", "source_hash": <64 hex>, "arch": "x86_64", "binary_sha256": <64 hex>}
# The download (release tag from the pin, asset waybar-notch-x86_64.zst) is used only when the pin's source_hash equals
# this checkout's source hash and the decompressed bytes match binary_sha256, before they are made executable or run.
# Nothing the release host serves (including release-info.json) is trusted. A cached DEST that matches the pin is reused.
# Anything else exits 1 and the caller compiles from source. PILL_BAR_NO_PREBUILT=1 disables prebuilt use entirely.
# PILL_BAR_RELEASE_URL overrides only the transport base URL (tests).
#
# Usage: fetch-prebuilt.sh PLUGIN_ROOT SOURCE_HASH DEST_FILE
set -uo pipefail
ROOT="$1"; WANT="$2"; DEST="$3"
[ "${PILL_BAR_NO_PREBUILT:-0}" = 1 ] && { echo "prebuilt: disabled"; exit 1; }
[ "$(uname -m)" = x86_64 ] || { echo "prebuilt: only x86_64 is published"; exit 1; }
for c in curl jq zstd sha256sum ldd; do command -v "$c" >/dev/null 2>&1 || { echo "prebuilt: $c missing"; exit 1; }; done

PIN="$ROOT/files/setup/prebuilt.json"
nopin() { echo "prebuilt: no trusted pin for these sources"; exit 1; }
[ -f "$PIN" ] || nopin
jq -e --arg h "$WANT" '
  type == "object" and (keys | sort) == ["arch","binary_sha256","source_commit","source_hash","tag"]
  and (.source_commit | type == "string" and test("^[0-9a-f]{40}$"))
  and (.tag == "notch-" + .source_commit)
  and (.source_hash | type == "string" and test("^[0-9a-f]{64}$")) and .source_hash == $h
  and .arch == "x86_64"
  and (.binary_sha256 | type == "string" and test("^[0-9a-f]{64}$"))' "$PIN" >/dev/null 2>&1 || nopin
TAG=$(jq -r '.tag' "$PIN"); SUM=$(jq -r '.binary_sha256' "$PIN")

libs_ok() { ! ldd "$1" 2>&1 | grep -q "not found"; }

[ -L "$DEST" ] && { echo "prebuilt: cache path is a symlink"; exit 1; }
if [ -f "$DEST" ] && [ -x "$DEST" ] && [ "$(sha256sum "$DEST" | cut -d' ' -f1)" = "$SUM" ] && libs_ok "$DEST"; then
  echo "prebuilt: cached copy is authentic"; exit 0
fi

BASE="${PILL_BAR_RELEASE_URL:-https://github.com/Estivenm06/waybar-pill-bar/releases/download/$TAG}"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
echo "prebuilt: downloading waybar-notch ($TAG)"
curl -fsSL --proto '=https,file' --max-time 120 --retry 2 -o "$TMP/bin.zst" "$BASE/waybar-notch-x86_64.zst" || { echo "prebuilt: download failed"; exit 1; }
zstd -d -q -o "$TMP/bin" "$TMP/bin.zst" || { echo "prebuilt: corrupt archive"; exit 1; }
[ "$(sha256sum "$TMP/bin" | cut -d' ' -f1)" = "$SUM" ] || { echo "prebuilt: checksum mismatch, not using it"; exit 1; }
chmod 755 "$TMP/bin"
libs_ok "$TMP/bin" || { echo "prebuilt: needs libraries this system lacks, building instead"; exit 1; }
mkdir -p "$(dirname "$DEST")" && mv -f "$TMP/bin" "$DEST" || exit 1
echo "prebuilt: ok"
