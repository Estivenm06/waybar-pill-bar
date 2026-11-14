#!/bin/bash
# Restore the newest backup made by install.sh and remove everything the installer added.
# Plugin installs (marker ~/.local/state/waybar-notch/plugin-install.json) take the plugin branch below.
# Usage: ./uninstall.sh [--purge]   (--purge also deletes your Google OAuth client, token and sign-in browser profile)
set -euo pipefail
PURGE=0
for a in "$@"; do
  case $a in
    --purge) PURGE=1 ;;
    *) echo "Unknown option: $a"; echo "Usage: ./uninstall.sh [--purge]"; exit 1 ;;
  esac
done
W="$HOME/.config/waybar"; H="$HOME/.config/hypr"
STATE="$HOME/.local/state/waybar-notch"
GCONF="$HOME/.config/waybar-notch"; GDATA="$HOME/.local/share/waybar-notch"
STATE_PLUGIN="${XDG_STATE_HOME:-$HOME/.local/state}/waybar-notch/plugin-install.json"
if [ -e "$STATE_PLUGIN" ]; then
  HELPER_DIR="$(dirname "$(readlink -f "$0")")/files"
  if command -v omarchy >/dev/null 2>&1; then
    # an enabled plugin owns the session; disabling closes its lease and restores the native desktop
    omarchy plugin disable waybar.pill-bar >/dev/null 2>&1 || true
    for _ in $(seq 50); do python3 "$HELPER_DIR/scripts/waybar-pill-plugin" active || break; sleep 0.2; done
  fi
  python3 "$HELPER_DIR/scripts/waybar-pill-plugin" recover || true
  python3 "$HELPER_DIR/setup/plugin-state.py" uninstall || { echo "Plugin left disabled; nothing was removed."; exit 1; }
  hyprctl reload || true
  echo "Plugin files restored. Preferences and credentials are kept; remove the checkout with: omarchy plugin remove waybar.pill-bar"
  if [ "$PURGE" = 1 ]; then
    rm -rf "$GCONF/google.json" "$GCONF/project-id" "$GDATA/google-token.json" "$GDATA/browser-profile"
    echo "Deleted the Google OAuth client, token and sign-in browser profile."
  fi
  exit 0
fi
BK=$(ls -td "$W"/backup-[0-9]* 2>/dev/null | head -1 || true)
[ -n "$BK" ] || { echo "No backup found in $W"; exit 1; }
echo "Restoring from $BK"
for f in config.jsonc style.css; do [ -e "$BK/$f" ] && cp -a "$BK/$f" "$W/$f"; done
[ -d "$BK/scripts" ] && cp -a "$BK/scripts/." "$W/scripts/"
for f in autostart.lua bindings.lua; do
  [ -e "$BK/hypr/$f" ] && cp -a "$BK/hypr/$f" "$H/$f"
done
systemctl --user disable --now waybar-notch.service 2>/dev/null || true
# stop the supervisor while its KillMode=process drop-in is still loaded: it shuts down its own Waybar workers and
# nothing else (apps started from the bar keep running)
DROPIN="$HOME/.config/systemd/user/waybar.service.d/50-waybar-notch.conf"
if [ -e "$DROPIN" ] && grep -q '^# Managed by waybar-pill-bar' "$DROPIN"; then
  systemctl --user stop waybar.service 2>/dev/null || true
  rm -f "$DROPIN"; rmdir "$(dirname "$DROPIN")" 2>/dev/null || true
fi
rm -f "$HOME/.config/systemd/user/waybar-notch.service"; systemctl --user daemon-reload
rm -f "$HOME/.config/omarchy/hooks/theme-set.d/50-waybar-pill-bar" "$HOME/.config/omarchy/hooks/font-set.d/50-waybar-pill-bar"
rm -f "$W/pill-theme.css" "$W/pill-theme.css.tmp"
if [ -f "$STATE/disabled-plugins" ]; then
  while read -r id; do
    [ -n "$id" ] && { omarchy plugin enable "$id" || echo "Could not re-enable $id: run: omarchy plugin enable $id"; }
  done < "$STATE/disabled-plugins"
  rm -f "$STATE/disabled-plugins"
fi
omarchy-toggle-bar off 2>/dev/null || true   # show Omarchy's built-in bar again
hyprctl reload || true
systemctl --user start waybar.service || true   # the stock single Waybar again
echo "Restored. (Scripts added by this plugin are left in $W/scripts; delete them manually if you want.)"
if [ "$PURGE" = 1 ]; then
  rm -rf "$GCONF/google.json" "$GCONF/project-id" "$GDATA/google-token.json" "$GDATA/browser-profile"
  echo "Deleted the Google OAuth client, token and sign-in browser profile."
else
  echo "Kept (delete with ./uninstall.sh --purge):"
  echo "  $GCONF/google.json"
  echo "  $GDATA/google-token.json"
  echo "  $GDATA/browser-profile"
fi
