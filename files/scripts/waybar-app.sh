#!/bin/bash
# Window-class based launcher click helper for Waybar custom modules (status comes from the notch supervisor).
#   waybar-app.sh click  <class-regex> <launch command...>
# Launch inside its own systemd scope so the app is NOT in waybar.service's cgroup: a Waybar restart must never kill it.
launch() { if command -v uwsm-app >/dev/null 2>&1; then setsid -f uwsm-app -- "$@"; else setsid -f systemd-run --user --scope --quiet --collect -- "$@"; fi >/dev/null 2>&1; }
re=$2; shift 2
addr=$(hyprctl clients -j | jq -r --arg re "$re" '[.[] | select((.class // "") | test($re; "i"))][0].address // empty')
if [ -n "$addr" ]; then
  hyprctl dispatch "hl.dsp.focus({ window = \"address:$addr\" })" >/dev/null
else
  launch "$@"
fi
