#!/bin/bash
# Set up the Omarchy service plugin waybar.pill-bar (Omarchy 4.x, Lua Hyprland config).
# Usage: ./install.sh [--no-restart] [--google | --no-google] [--reconfigure]
#   Installs files only: no units, no daemons. Enable afterwards with: omarchy plugin enable waybar.pill-bar
#   --no-restart skips the Hyprland reload and config-error check.
#   --google runs the guided Google Calendar sign-in (off by default).
#   --reconfigure asks again which AI usage tiles and apps to show (the first run asks in a terminal).
# The first original of every replaced file is kept by plugin-state.py and restored by ./uninstall.sh.
set -euo pipefail
SRC="$(dirname "$(readlink -f "$0")")/files"
ROOT="$(dirname "$SRC")"
W="$HOME/.config/waybar"; H="$HOME/.config/hypr"
RESTART=1; GOOGLE=no; RECONFIGURE=0
for a in "$@"; do
  case $a in
    --no-restart) RESTART=0 ;;
    --google) GOOGLE=yes ;;
    --no-google) GOOGLE=no ;;
    --reconfigure) RECONFIGURE=1 ;;
    *) echo "Unknown option: $a"; echo "Usage: ./install.sh [--no-restart] [--google | --no-google] [--reconfigure]"; exit 1 ;;
  esac
done
have() { command -v "$1" >/dev/null 2>&1; }

echo "== Checking dependencies (nothing is installed automatically)"
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
missing=()
for c in waybar playerctl jq pactl pamixer python3 bluetoothctl nmcli upower powerprofilesctl hyprctl cargo fc-match; do
  have "$c" || missing+=("$c")
done
pkg-config --exists gtk4 gtk4-layer-shell-0 2>/dev/null || missing+=("gtk4 + gtk4-layer-shell")
have cava || echo "Note: cava not found, the notch equalizer shows decorative bars. Install with: sudo pacman -S --needed cava"
have wl-copy || echo "Note: wl-clipboard not found, the clipboard tab cannot paste. Install with: sudo pacman -S --needed wl-clipboard"
have sqlite3 || echo "Note: sqlite3 not found, Copilot CLI usage stays hidden. Install with: sudo pacman -S --needed sqlite"
if [ ${#missing[@]} -gt 0 ]; then
  echo "Missing: ${missing[*]}"
  echo "Arch packages: sudo pacman -S --needed waybar playerctl jq libpulse pamixer gtk4 gtk4-layer-shell pkgconf bluez-utils networkmanager upower power-profiles-daemon rust fontconfig wl-clipboard cava"
  have cargo || echo "Rust is required to build the notch: sudo pacman -S --needed rust"
  echo "Install them, then re-run this script."; exit 1
fi
if [ ! -e "$H/autostart.lua" ] && [ ! -e "$H/bindings.lua" ]; then
  echo "This installer targets Omarchy 4.x (Lua Hyprland config)."; exit 1
fi
NO_PW=
if ! python3 -c "import playwright" 2>/dev/null; then
  NO_PW=1
  echo "Note: python-playwright not found, the Google Calendar login step will be skipped. Install with: sudo pacman -S --needed python-playwright"
fi

# Plugin setup: build, back up, install files and managed Lua blocks. No systemd units, no bar hiding,
# no plugin enabling, no daemons: the Omarchy plugin's service starts the product when it is enabled.
[ -f "$ROOT/manifest.json" ] || { echo "manifest.json missing: run this from the plugin checkout"; exit 1; }
have omarchy && omarchy plugin list --json >/dev/null 2>&1 || { echo "Omarchy plugin support not found (omarchy plugin list --json failed)."; exit 1; }
STATEPY="$SRC/setup/plugin-state.py"
echo "== Building the Rust notch"
( cd "$ROOT/notch" && cargo build --release --locked -q )
PFILES=("$HOME/.local/bin/waybar-notch" "$W/config.jsonc" "$W/style.css" "$W/pill-theme.css"
  "$HOME/.config/omarchy/hooks/theme-set.d/50-waybar-pill-bar" "$HOME/.config/omarchy/hooks/font-set.d/50-waybar-pill-bar"
  "$HOME/.local/share/waybar-notch/setup/waybar-setup.py" "$HOME/.local/share/waybar-notch/setup/apps-catalog.json"
  "$H/autostart.lua" "$H/bindings.lua")
for f in "$SRC"/scripts/*; do [ -f "$f" ] || continue; PFILES+=("$W/scripts/$(basename "$f")"); done
ADAPTERPY="$SRC/setup/widget-adapters.py"
ADAPTER_PATHS=$(python3 "$ADAPTERPY" paths) || { echo "WhatsApp adapter preflight failed; nothing was changed."; exit 1; }
while IFS= read -r f; do [ -n "$f" ] && PFILES+=("$f"); done <<< "$ADAPTER_PATHS"
python3 "$STATEPY" begin "${PFILES[@]}"
python3 "$ADAPTERPY" apply
if [ -n "$(date +%p)" ]; then CLOCK_FMT='{:%I:%M %p}'; else CLOCK_FMT='{:%H:%M}'; fi
mkdir -p "$W/scripts" "$H"
echo "== Installing Waybar files"
sed -e "s|__HOME__|$HOME|g" -e "s|__CLOCK_FMT__|$CLOCK_FMT|g" "$SRC/config.jsonc" > "$W/config.jsonc"
sed "s|__HOME__|$HOME|g" "$SRC/style.css" > "$W/style.css"
for f in "$SRC"/scripts/*; do [ -f "$f" ] || continue; sed "s|__HOME__|$HOME|g" "$f" > "$W/scripts/$(basename "$f")"; chmod +x "$W/scripts/$(basename "$f")"; done
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/waybar-notch/config.json"
if [ -t 0 ] && { [ ! -f "$CFG" ] || [ "$RECONFIGURE" = 1 ]; }; then python3 "$SRC/setup/waybar-setup.py" ask; else python3 "$SRC/setup/waybar-setup.py" defaults; fi
python3 "$SRC/setup/waybar-setup.py" render-config "$W/config.jsonc"
install -Dm755 "$ROOT/notch/target/release/waybar-notch" "$HOME/.local/bin/waybar-notch.new" && mv -f "$HOME/.local/bin/waybar-notch.new" "$HOME/.local/bin/waybar-notch"
install -Dm755 "$SRC/setup/waybar-setup.py" "$HOME/.local/share/waybar-notch/setup/waybar-setup.py"
install -Dm644 "$SRC/setup/apps-catalog.json" "$HOME/.local/share/waybar-notch/setup/apps-catalog.json"
"$HOME/.local/bin/waybar-notch" --gen-waybar-theme
for d in theme-set.d font-set.d; do
  mkdir -p "$HOME/.config/omarchy/hooks/$d"
  sed "s|__HOME__|$HOME|g" "$SRC/hooks/pill-theme-hook" > "$HOME/.config/omarchy/hooks/$d/50-waybar-pill-bar"
  chmod +x "$HOME/.config/omarchy/hooks/$d/50-waybar-pill-bar"
done
echo "== Patching Hyprland (Omarchy Lua config)"
patch_block() {  # file snippet
  [ -e "$1" ] || touch "$1"
  if grep -q "waybar-pill-bar BEGIN" "$1"; then
    python3 - "$1" "$2" <<'PY'
import re, sys
f, snip = sys.argv[1], open(sys.argv[2]).read().replace('__HOME__', __import__('os').path.expanduser('~'))
t = open(f).read()
t = re.sub(r'-- >>> waybar-pill-bar BEGIN.*?-- <<< waybar-pill-bar END <<<\n', lambda m: snip, t, flags=re.S)
open(f, 'w').write(t)
PY
  else
    { echo; sed "s|__HOME__|$HOME|g" "$2"; } >> "$1"
  fi
}
AUTOSTART_SNIP=$(mktemp); PRE=$(mktemp -d)
trap 'rm -rf "$AUTOSTART_SNIP" "$PRE"' EXIT
python3 "$SRC/setup/waybar-setup.py" render-autostart "$SRC/hypr/autostart.snippet.lua" "$AUTOSTART_SNIP"
cp -a "$H/autostart.lua" "$PRE/autostart.lua" 2>/dev/null || true; cp -a "$H/bindings.lua" "$PRE/bindings.lua" 2>/dev/null || true
ERR_BEFORE=$(hyprctl configerrors 2>&1 || true)
patch_block "$H/autostart.lua" "$AUTOSTART_SNIP"
patch_block "$H/bindings.lua" "$SRC/hypr/bindings.plugin.snippet.lua"
if [ "$RESTART" = 1 ]; then
  hyprctl reload >/dev/null || true
  ERR_AFTER=$(hyprctl configerrors 2>&1 || true)
  if [ "$ERR_AFTER" != "$ERR_BEFORE" ]; then
    echo "Hyprland rejected the managed blocks; restoring your Lua files:"; echo "$ERR_AFTER"
    for n in autostart.lua bindings.lua; do [ -e "$PRE/$n" ] && cp -a "$PRE/$n" "$H/$n"; done
    hyprctl reload >/dev/null || true
    python3 "$STATEPY" finish
    exit 1
  fi
fi
python3 "$STATEPY" finish
if [ "$GOOGLE" = yes ]; then
  if [ -n "$NO_PW" ]; then echo "Skipping Google Calendar (install python-playwright first)"; else python3 "$W/scripts/google-setup.py" || echo "Google Calendar not connected"; fi
fi
echo "Plugin setup done. Enable with: omarchy plugin enable waybar.pill-bar   (disable: omarchy plugin disable waybar.pill-bar)"
