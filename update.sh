#!/bin/bash
# Update Pill Bar from GitHub and rebuild it. Safe to run any time.
# Steps: fast-forward the plugin checkout, let the backend restore the native
# desktop, then enable the plugin again so the new sources are built.
set -euo pipefail

ID=waybar.pill-bar

command -v omarchy >/dev/null || { echo "update.sh: the omarchy command is required" >&2; exit 1; }

omarchy plugin update "$ID" --yes

if omarchy plugin list | awk -v id="$ID" '$1 == id && $2 == "enabled" { found = 1 } END { exit !found }'; then
  omarchy plugin disable "$ID"
  # the backend restores the native bar before it exits; wait for the notch to go away
  for _ in $(seq 1 60); do
    pgrep -x waybar-notch >/dev/null || break
    sleep 1
  done
fi

omarchy plugin enable "$ID"
echo "Pill Bar updated. The first start rebuilds the notch; follow ${XDG_STATE_HOME:-$HOME/.local/state}/waybar-notch/provision.log"
echo "If the plugin's Service.qml changed in this release, run: omarchy-restart-shell"
