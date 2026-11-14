#!/bin/bash
# Background app kept alive on a hidden special workspace (same mechanism Omarchy uses for its own windows).
#   waybar-bgapp.sh ensure|toggle <id> <class-regex> <launch command...>
# (the Waybar status comes from the notch supervisor, which also calls `ensure` when the window is gone)
# ensure = start hidden if absent;  toggle = show/hide/launch.
mode=$1; id=$2; re=$3; shift 3
win() { hyprctl clients -j | jq -c --arg re "$re" '[.[]|select((.class // "")|test($re;"i"))][0] // empty'; }
# own systemd scope: the app must NOT live in waybar.service's cgroup (a Waybar restart would kill it)
launch() { if command -v uwsm-app >/dev/null 2>&1; then setsid -f uwsm-app -- "$@"; else setsid -f systemd-run --user --scope --quiet --collect -- "$@"; fi >/dev/null 2>&1; }
wait_win() { for _ in $(seq 100); do [ -n "$(win)" ] && return 0; sleep 0.1; done; return 1; }
case $mode in
  ensure)
    exec 9>"${XDG_RUNTIME_DIR:-/tmp}/waybar-bgapp-$id.lock"; flock -n 9 || exit 0
    [ -n "$(win)" ] || { launch "$@"; wait_win; } ;;
  toggle)
    w=$(win)
    if [ -z "$w" ]; then launch "$@"; wait_win && sleep 0.3; w=$(win); fi
    [ -z "$w" ] && exit 1
    if [ "$(jq -r '.workspace.name' <<<"$w")" = "special:$id" ]; then
      hyprctl dispatch "hl.dsp.workspace.toggle_special(\"$id\")" >/dev/null
    else
      hyprctl dispatch "hl.dsp.focus({ window = \"address:$(jq -r .address <<<"$w")\" })" >/dev/null
    fi ;;
esac
