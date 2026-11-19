#!/bin/bash
# Provision the Omarchy service plugin waybar.pill-bar (Omarchy 4.x, Lua Hyprland config).
#
# You normally never run this. Enabling the plugin (`omarchy plugin enable waybar.pill-bar`) starts its backend, which
# runs `install.sh --provision` while the native bar, notifications and OSD stay untouched, and restores every file it
# wrote when the plugin is disabled or removed. Build output, staging and the log live outside the plugin directory:
#   cargo target   ${XDG_CACHE_HOME:-~/.cache}/waybar-notch/target
#   provision log  ${XDG_STATE_HOME:-~/.local/state}/waybar-notch/provision.log (written by the backend)
#
# Usage: ./install.sh --provision [--no-restart] [--reconfigure]
#   --provision    build the notch and install the managed files (the developer/recovery entry point)
#   --no-restart   skip the Hyprland reload and config-error check
#   --reconfigure  ask again which AI usage tiles and apps to show (needs a terminal; automatic setup never prompts)
# Missing packages from manifest.json "requirements" are installed with pacman (pkexec dialog, or sudo in a terminal;
# PILL_BAR_NO_AUTO_INSTALL=1 only lists them). No OAuth sign-in is requested. Undo: disable or remove the
# plugin, or run ./uninstall.sh.
set -euo pipefail
SRC="$(dirname "$(readlink -f "$0")")/files"
ROOT="$(dirname "$SRC")"
W="$HOME/.config/waybar"; H="$HOME/.config/hypr"
PROVISION=0; RESTART=1; RECONFIGURE=0
for a in "$@"; do
  case $a in
    --provision) PROVISION=1 ;;
    --no-restart) RESTART=0 ;;
    --reconfigure) RECONFIGURE=1 ;;
    *) echo "Unknown option: $a"; echo "Usage: ./install.sh --provision [--no-restart] [--reconfigure]"; exit 1 ;;
  esac
done
if [ "$PROVISION" != 1 ]; then
  echo "Setup is automatic: omarchy plugin enable waybar.pill-bar"
  echo "To provision by hand (developer use) run: ./install.sh --provision. Undo with ./uninstall.sh."
  exit 1
fi
export PYTHONDONTWRITEBYTECODE=1
have() { command -v "$1" >/dev/null 2>&1; }

echo "== Installing missing packages listed in manifest.json (requirements)"
bash "$SRC/setup/requirements.sh" "$ROOT/manifest.json" || true
echo "== Checking dependencies"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
missing=()
for c in playerctl jq pactl pamixer python3 bluetoothctl nmcli upower powerprofilesctl hyprctl cargo fc-match; do
  have "$c" || missing+=("$c")
done
pkg-config --exists xkbcommon wayland-client 2>/dev/null || missing+=("libxkbcommon + wayland")
have cava || echo "Note: cava not found, the notch equalizer shows decorative bars."
have wl-copy || echo "Note: wl-clipboard not found, the clipboard tab cannot paste."
have sqlite3 || echo "Note: sqlite3 not found, Copilot CLI usage stays hidden."
if [ ${#missing[@]} -gt 0 ]; then
  echo "Missing: ${missing[*]}"
  echo "Install them (see manifest.json requirements): sudo pacman -S --needed $(jq -r '.requirements.pacman | join(" ")' "$ROOT/manifest.json")"
  echo "Then enable the plugin again."; exit 1
fi
if [ ! -e "$H/autostart.lua" ] && [ ! -e "$H/bindings.lua" ]; then
  echo "This plugin targets Omarchy 4.x (Lua Hyprland config)."; exit 1
fi
[ -f "$ROOT/manifest.json" ] || { echo "manifest.json missing: run this from the plugin checkout"; exit 1; }
have omarchy && omarchy plugin list --json >/dev/null 2>&1 || { echo "Omarchy plugin support not found (omarchy plugin list --json failed)."; exit 1; }
STATEPY="$SRC/setup/plugin-state.py"
python3 "$STATEPY" validate || { echo "The plugin install record failed validation; nothing was changed."; exit 1; }
python3 "$SRC/scripts/waybar-pill-plugin" validate-checkout "$ROOT"

# Build products and staging stay outside the plugin directory, which Omarchy watches for changes.
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/waybar-notch"
export CARGO_TARGET_DIR="$CACHE/target"
echo "== Building the Rust notch"
( cd "$ROOT/notch" && cargo build --release --locked -q )
BIN="$CARGO_TARGET_DIR/release/waybar-notch"
[ -x "$BIN" ] || { echo "The build did not produce $BIN"; exit 1; }

mkdir -p -m 700 "$CACHE"
STAGE=$(mktemp -d "$CACHE/stage.XXXXXX")
BEGUN=0; DONE=0
cleanup() {
  local rc=$?
  rm -rf "$STAGE"
  # Run by the plugin backend (WAYBAR_PILL_SESSION), the backend itself restores whatever was written; run by hand, this
  # script undoes its own partial work.
  if [ "$BEGUN" = 1 ] && [ "$DONE" != 1 ] && [ -z "${WAYBAR_PILL_SESSION:-}" ]; then
    echo "Setup did not finish; restoring the files it changed" >&2
    python3 "$STATEPY" uninstall >&2 || echo "Restoring failed; resolve the conflicts above, then run ./uninstall.sh" >&2
    [ "$RESTART" = 1 ] && { hyprctl reload >/dev/null 2>&1 || true; }
  fi
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 143' TERM INT HUP

# Render every destination first; nothing managed is written before all of them exist.
DEST=(); SRCF=(); MODE=()
add() { DEST+=("$1"); SRCF+=("$2"); MODE+=("$3"); }
mode_of() { stat -c '%a' "$1" 2>/dev/null || echo "$2"; }
echo "== Rendering the managed files"
mkdir -p "$STAGE/scripts"
for f in "$SRC"/scripts/*; do
  [ -f "$f" ] || continue
  n=$(basename "$f")
  sed "s|__HOME__|$HOME|g" "$f" > "$STAGE/scripts/$n"
  add "$W/scripts/$n" "$STAGE/scripts/$n" 755
done
for d in theme-set.d font-set.d; do
  sed "s|__HOME__|$HOME|g" "$SRC/hooks/pill-theme-hook" > "$STAGE/hook-$d"
  add "$HOME/.config/omarchy/hooks/$d/50-waybar-pill-bar" "$STAGE/hook-$d" 755
done
cp "$BIN" "$STAGE/waybar-notch"
add "$HOME/.local/bin/waybar-notch" "$STAGE/waybar-notch" 755
cp "$SRC/setup/waybar-setup.py" "$STAGE/waybar-setup.py"
add "$HOME/.local/share/waybar-notch/setup/waybar-setup.py" "$STAGE/waybar-setup.py" 755
cp "$SRC/setup/apps-catalog.json" "$STAGE/apps-catalog.json"
add "$HOME/.local/share/waybar-notch/setup/apps-catalog.json" "$STAGE/apps-catalog.json" 644
# the user's own preferences (kept when the plugin is disabled): written only when missing, never restored
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/waybar-notch/config.json"
if [ -z "${WAYBAR_PILL_SESSION:-}" ] && [ -t 0 ] && { [ ! -f "$CFG" ] || [ "$RECONFIGURE" = 1 ]; }; then
  python3 "$SRC/setup/waybar-setup.py" ask
else
  python3 "$SRC/setup/waybar-setup.py" defaults
fi
python3 "$SRC/setup/waybar-setup.py" render-autostart "$SRC/hypr/autostart.snippet.lua" "$STAGE/autostart.snippet.lua"
python3 "$SRC/setup/waybar-setup.py" patch-lua "$H/autostart.lua" "$STAGE/autostart.snippet.lua" "$STAGE/autostart.lua"
python3 "$SRC/setup/waybar-setup.py" patch-lua "$H/bindings.lua" "$SRC/hypr/bindings.plugin.snippet.lua" "$STAGE/bindings.lua"
add "$H/autostart.lua" "$STAGE/autostart.lua" "$(mode_of "$H/autostart.lua" 644)"
add "$H/bindings.lua" "$STAGE/bindings.lua" "$(mode_of "$H/bindings.lua" 644)"
ADAPTERS=$(python3 "$SRC/setup/widget-adapters.py" stage "$STAGE/adapter") || { echo "WhatsApp adapter staging failed; nothing was changed."; exit 1; }
ENTRIES=$(printf '%s' "$ADAPTERS" | jq -r '.[] | [.path, .source, .mode] | @tsv') || { echo "WhatsApp adapter staging returned unreadable output; nothing was changed."; exit 1; }
while IFS=$'\t' read -r p s m; do
  if [ -n "$p" ]; then add "$p" "$s" "$m"; fi
done <<< "$ENTRIES"

# Record every destination; publish itself skips byte-identical writes.
WRITE=("${!DEST[@]}")

ERR_BEFORE=""
[ "$RESTART" = 1 ] && ERR_BEFORE=$(hyprctl configerrors 2>&1 || true)
if [ ${#WRITE[@]} -gt 0 ]; then
  echo "== Recording originals and publishing ${#WRITE[@]} files"
  paths=()
  for i in "${WRITE[@]}"; do paths+=("${DEST[$i]}"); done
  python3 "$STATEPY" begin "${paths[@]}"
  BEGUN=1
  for i in "${WRITE[@]}"; do python3 "$STATEPY" prepare "${DEST[$i]}" "${SRCF[$i]}" "${MODE[$i]}"; done
  for i in "${WRITE[@]}"; do python3 "$STATEPY" publish "${DEST[$i]}"; done
  python3 "$STATEPY" finish
fi
if [ "$RESTART" = 1 ]; then
  hyprctl reload >/dev/null || true
  ERR_AFTER=$(hyprctl configerrors 2>&1 || true)
  if [ "$ERR_AFTER" != "$ERR_BEFORE" ]; then
    echo "Hyprland rejected the managed blocks:"; echo "$ERR_AFTER"
    exit 1
  fi
fi
DONE=1
echo "Provisioned. Disabling or removing the plugin restores these files."
