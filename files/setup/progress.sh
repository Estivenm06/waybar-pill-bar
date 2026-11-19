# Source me. One desktop notification that is updated in place while the plugin installs, so there is something to
# watch ("Installing packages", "Downloading the notch", "Building 42%", "Ready"). Best effort: no notification
# service means silence, never an error.
PROGRESS_ID=""
progress() {  # progress "message"
  command -v omarchy-notification-send >/dev/null 2>&1 || return 0
  local glyph; glyph=$(printf '\uf013')
  if [ -n "$PROGRESS_ID" ]; then
    omarchy-notification-send -r "$PROGRESS_ID" -g "$glyph" "Pill Bar" "$1" >/dev/null 2>&1 || true
  else
    PROGRESS_ID=$(omarchy-notification-send -p -g "$glyph" "Pill Bar" "$1" 2>/dev/null | tr -dc '0-9') || true
  fi
  return 0
}
