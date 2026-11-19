#!/bin/bash
# Reverse ./install.sh: restore the files recorded by plugin-state.py and remove what the setup added.
# Usage: ./uninstall.sh [--purge]   (--purge also deletes your Google OAuth client, token and sign-in browser profile)
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
PURGE=0
for a in "$@"; do
  case $a in
    --purge) PURGE=1 ;;
    *) echo "Unknown option: $a"; echo "Usage: ./uninstall.sh [--purge]"; exit 1 ;;
  esac
done
GCONF="$HOME/.config/waybar-notch"; GDATA="$HOME/.local/share/waybar-notch"
STATE_PLUGIN="${XDG_STATE_HOME:-$HOME/.local/state}/waybar-notch/plugin-install.json"
HELPER_DIR="$(dirname "$(readlink -f "$0")")/files"
# validate the record and recovery journal before any desktop command; a dangling or linked record must not slip past `-e`
python3 "$HELPER_DIR/setup/plugin-state.py" validate || { echo "The install record or recovery journal failed validation; nothing was changed."; exit 1; }
if command -v omarchy >/dev/null 2>&1; then
  # an enabled plugin owns the session; disabling closes its lease and restores the native desktop
  omarchy plugin disable waybar.pill-bar >/dev/null 2>&1 || true
fi
rc=75
for _ in $(seq 120); do
  if python3 "$HELPER_DIR/scripts/waybar-pill-plugin" recover; then rc=0; break; else rc=$?; fi
  [ "$rc" = 75 ] || break
  sleep 1
done
[ "$rc" = 0 ] || { echo "Recovery could not finish; keep the checkout and resolve the reported conflict."; exit 1; }
echo "Plugin files restored. Preferences and credentials are kept; remove the checkout with: omarchy plugin remove waybar.pill-bar"
if [ "$PURGE" = 1 ]; then
  rm -rf "$GCONF/google.json" "$GCONF/project-id" "$GDATA/google-token.json" "$GDATA/browser-profile"
  echo "Deleted the Google OAuth client, token and sign-in browser profile."
fi
