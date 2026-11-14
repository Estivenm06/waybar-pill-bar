#!/bin/bash
# Install the pill-style Waybar setup (Omarchy 4.x, Lua Hyprland config).
# Usage: ./install.sh [--no-restart] [--google | --no-google] [--reconfigure]
#   --reconfigure asks again which AI usage tiles and apps to show (the first run asks in a terminal).
# Backs up everything it replaces into ~/.config/waybar/backup-<timestamp>/ (never deletes).
set -euo pipefail
SRC="$(dirname "$(readlink -f "$0")")/files"
ROOT="$(dirname "$SRC")"
W="$HOME/.config/waybar"; H="$HOME/.config/hypr"
BK="$W/backup-$(date +%Y%m%d-%H%M%S)"
STATE="$HOME/.local/state/waybar-notch"
TOKEN="$HOME/.local/share/waybar-notch/google-token.json"
RESTART=1; GOOGLE=ask; RECONFIGURE=0
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

# 12h or 24h clock, following the system locale
if [ -n "$(date +%p)" ]; then CLOCK_FMT='{:%I:%M %p}'; else CLOCK_FMT='{:%H:%M}'; fi

echo "== Backing up to $BK"
mkdir -p "$BK/scripts" "$BK/hypr" "$W/scripts" "$H" "$HOME/.config/systemd/user"
for f in config.jsonc style.css; do [ -e "$W/$f" ] && cp -a "$W/$f" "$BK/$f"; done
for f in "$SRC"/scripts/*; do b=$(basename "$f"); [ -e "$W/scripts/$b" ] && cp -a "$W/scripts/$b" "$BK/scripts/$b"; done
for f in autostart.lua bindings.lua; do [ -e "$H/$f" ] && cp -a "$H/$f" "$BK/hypr/$f"; done

echo "== Installing Waybar files"
sed -e "s|__HOME__|$HOME|g" -e "s|__CLOCK_FMT__|$CLOCK_FMT|g" "$SRC/config.jsonc" > "$W/config.jsonc"
sed "s|__HOME__|$HOME|g" "$SRC/style.css" > "$W/style.css"
for f in "$SRC"/scripts/*; do sed "s|__HOME__|$HOME|g" "$f" > "$W/scripts/$(basename "$f")"; chmod +x "$W/scripts/$(basename "$f")"; done
rm -f "$W/scripts/waybar-whatsapp.sh"   # replaced by waybar-bgapp.sh (the backup loop above already saved it)
[ -e "$W/scripts/wifi-status.sh" ] && cp -a "$W/scripts/wifi-status.sh" "$BK/scripts/wifi-status.sh"
rm -f "$W/scripts/wifi-status.sh"   # replaced by the native Waybar network module

echo "== Choosing AI usage tiles and apps"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/waybar-notch/config.json"
if [ -t 0 ] && { [ ! -f "$CFG" ] || [ "$RECONFIGURE" = 1 ]; }; then
  python3 "$SRC/setup/waybar-setup.py" ask
else
  python3 "$SRC/setup/waybar-setup.py" defaults
fi
python3 "$SRC/setup/waybar-setup.py" render-config "$W/config.jsonc"
python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$W/config.jsonc" && echo "config.jsonc is valid JSON"

echo "== Building the Rust notch"
( cd "$ROOT/notch" && cargo build --release -q )
# the unit may be running the old binary: install via a temp name so the copy never hits ETXTBSY
install -Dm755 "$ROOT/notch/target/release/waybar-notch" "$HOME/.local/bin/waybar-notch.new" && mv -f "$HOME/.local/bin/waybar-notch.new" "$HOME/.local/bin/waybar-notch"
cp "$SRC/systemd/waybar-notch.service" "$HOME/.config/systemd/user/waybar-notch.service"
# the Settings page re-renders the bar's apps through this helper, so it must live outside the source checkout
install -Dm755 "$SRC/setup/waybar-setup.py" "$HOME/.local/share/waybar-notch/setup/waybar-setup.py"
install -Dm644 "$SRC/setup/apps-catalog.json" "$HOME/.local/share/waybar-notch/setup/apps-catalog.json"
# waybar.service runs the per-output supervisor (a drop-in, so the packaged unit stays untouched)
DROPIN_DIR="$HOME/.config/systemd/user/waybar.service.d"; DROPIN="$DROPIN_DIR/50-waybar-notch.conf"
mkdir -p "$DROPIN_DIR"
if [ -e "$DROPIN" ]; then
  if grep -q '^# Managed by waybar-pill-bar' "$DROPIN"; then
    cp -a "$DROPIN" "$BK/50-waybar-notch.conf"
  else
    echo "Refusing to overwrite $DROPIN: it is not managed by waybar-pill-bar. Move it away and re-run."; exit 1
  fi
fi
install -m644 "$SRC/systemd/waybar-notch-waybar.conf" "$DROPIN"

echo "== Generating the Waybar theme from the active Omarchy theme and font"
"$HOME/.local/bin/waybar-notch" --gen-waybar-theme   # style.css imports pill-theme.css, so this must succeed before Waybar restarts
echo "== Installing the theme/font hooks"
for d in theme-set.d font-set.d; do
  mkdir -p "$HOME/.config/omarchy/hooks/$d"
  sed "s|__HOME__|$HOME|g" "$SRC/hooks/pill-theme-hook" > "$HOME/.config/omarchy/hooks/$d/50-waybar-pill-bar"
  chmod +x "$HOME/.config/omarchy/hooks/$d/50-waybar-pill-bar"
done

echo "== Patching Hyprland (Omarchy Lua config)"
patch_block() {  # file snippet
  [ -e "$1" ] || touch "$1"
  if grep -q "waybar-pill-bar BEGIN" "$1"; then
    # refresh the managed block
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
AUTOSTART_SNIP=$(mktemp)
trap 'rm -f "$AUTOSTART_SNIP"' EXIT
python3 "$SRC/setup/waybar-setup.py" render-autostart "$SRC/hypr/autostart.snippet.lua" "$AUTOSTART_SNIP"
patch_block "$H/autostart.lua" "$AUTOSTART_SNIP"
patch_block "$H/bindings.lua"  "$SRC/hypr/bindings.snippet.lua"
# Waybar is started by systemd (waybar.service); drop Omarchy's plain launch line if present
sed -i 's|^\(o\.launch_on_start("waybar")\)|-- (disabled by waybar-pill-bar) \1|' "$H/autostart.lua"

if [ "$RESTART" = 1 ]; then
  echo "== Handing notifications and the volume/brightness OSD to the notch"
  mkdir -p "$STATE"
  if have omarchy-plugin-list && have omarchy; then
    for id in omarchy.notifications omarchy.osd; do
      if omarchy-plugin-list | awk -v i="$id" '$1==i && $2=="enabled"{f=1} END{exit !f}'; then
        omarchy plugin disable "$id" || true
        echo "$id" >> "$STATE/disabled-plugins"
        if omarchy-plugin-list | awk -v i="$id" '$1==i && $2=="disabled"{f=1} END{exit !f}'; then
          echo "Disabled Omarchy plugin $id"
        else
          echo "Could not confirm that $id is disabled. Add \"$id\" to \"disabledPlugins\" in ~/.config/omarchy/shell.json by hand."
        fi
      fi
    done
  fi

  echo "== Enabling and (re)starting the services"
  systemctl --user daemon-reload
  systemctl --user enable waybar.service waybar-notch.service
  hyprctl reload || true
  omarchy-toggle-bar on || true            # hide Omarchy's built-in bar; Waybar is the visible bar
  # waybar.service must now run the supervisor; another drop-in overriding ExecStart would break the per-output pills
  if ! systemctl --user show waybar.service -p ExecStart --value | grep -q -- '--waybar-supervisor'; then
    echo "waybar.service does not run the waybar-notch supervisor; another unit override wins:"
    systemctl --user cat waybar.service
    echo "Remove the conflicting ExecStart override (see above) and re-run ./install.sh"; exit 1
  fi
  main_pid=$(systemctl --user show waybar.service -p MainPID --value)
  is_supervisor() { [ "${1:-0}" -gt 0 ] && tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null | grep -q -- '--waybar-supervisor'; }
  if ! systemctl --user is-active --quiet waybar.service; then
    systemctl --user start waybar.service
  elif is_supervisor "$main_pid"; then
    if cmp -s "/proc/$main_pid/exe" "$HOME/.local/bin/waybar-notch"; then
      # SIGUSR2: the supervisor re-reads config/style and rebuilds its own workers; nothing else is touched
      systemctl --user reload waybar.service
    else
      # the supervisor binary itself changed: a restart only stops the supervisor and its workers (KillMode=process)
      systemctl --user restart waybar.service
    fi
  else
    # One-time cutover from a plain Waybar. KillMode=process must be loaded first, otherwise the restart would
    # also kill every app that was started from the bar and still sits in the service's cgroup.
    if [ "$(systemctl --user show waybar.service -p KillMode --value)" != process ]; then
      echo "KillMode=process is not loaded for waybar.service; not restarting it (apps started from the bar would die)."
      systemctl --user cat waybar.service; exit 1
    fi
    systemctl --user restart waybar.service
  fi
  systemctl --user restart waybar-notch.service
  sleep 2; systemctl --user is-active waybar.service waybar-notch.service || echo "WARNING: see: journalctl --user -u waybar -u waybar-notch"
  comm=$(busctl --user status org.freedesktop.Notifications 2>/dev/null | sed -n 's/^Comm=//p')
  if [ "$comm" != "waybar-notch" ]; then
    echo "Another notification daemon owns the bus: ${comm:-none}. Stop/disable it, then: systemctl --user restart waybar-notch"
  fi

  echo "== Google Calendar"
  run_google=0
  if [ "$GOOGLE" = yes ]; then run_google=1
  elif [ "$GOOGLE" = ask ] && [ -t 0 ] && [ ! -e "$TOKEN" ]; then
    read -r -p "Connect Google Calendar now? [Y/n] " ans || ans=n
    case ${ans:-y} in [Nn]*) ;; *) run_google=1 ;; esac
  fi
  if [ "$run_google" = 1 ]; then
    if [ -n "$NO_PW" ]; then
      echo "Skipping Google Calendar (install python-playwright, then run ~/.config/waybar/scripts/google-setup.py)"
    elif ! python3 "$W/scripts/google-setup.py"; then
      echo "Google Calendar not connected; run ~/.config/waybar/scripts/google-setup.py later"
    fi
  fi
else
  echo "Services were not touched (--no-restart). The next 'systemctl --user daemon-reload && systemctl --user restart waybar waybar-notch' activates the per-output bar management."
fi
echo "Done. Backup of your previous files: $BK"
