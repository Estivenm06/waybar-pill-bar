#!/bin/bash
# Install the REQUIRED packages listed in manifest.json ("requirements.pacman") that this machine is missing.
# Optional packages are only mentioned, never installed.
#
# Called by install.sh --provision, so enabling the plugin through Omarchy also installs what it needs:
#   as root           pacman directly
#   in a terminal     sudo pacman (asks for your password in that terminal)
#   from the shell    pkexec pacman (the Omarchy polkit agent shows a password dialog)
# Only names from the manifest are ever passed to pacman, with --needed. Set PILL_BAR_NO_AUTO_INSTALL=1 to turn
# this off: the missing packages are then only listed.
#
# Usage: requirements.sh [--check] [MANIFEST]     --check lists missing required packages and installs nothing.
# Exit: 0 all required packages present (or installed), 1 required packages still missing, 2 unusable manifest.
set -uo pipefail

CHECK=0
if [ "${1:-}" = "--check" ]; then CHECK=1; shift; fi
MANIFEST="${1:-$(dirname "$(readlink -f "$0")")/../../manifest.json}"

command -v jq >/dev/null 2>&1 || { echo "requirements: jq is needed to read the manifest (sudo pacman -S --needed jq)"; exit 2; }
[ -f "$MANIFEST" ] || { echo "requirements: manifest not found: $MANIFEST"; exit 2; }
valid='^[a-z0-9@._+-]+$'
mapfile -t required < <(jq -r '.requirements.pacman[]? // empty' "$MANIFEST")
mapfile -t optional < <(jq -r '.requirements.optional[]? // empty' "$MANIFEST")
[ "${#required[@]}" -gt 0 ] || { echo "requirements: no requirements.pacman in $MANIFEST"; exit 2; }
for p in "${required[@]}" "${optional[@]}"; do
  [[ $p =~ $valid ]] || { echo "requirements: refusing package name '$p'"; exit 2; }
done
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

# `pacman -T` knows provides (rustup provides rust); a cargo already on PATH also satisfies the compiler.
missing_of() {
  local out p
  out=$(pacman -T "$@" 2>/dev/null) || true
  for p in $out; do
    [ "$p" = rust ] && command -v cargo >/dev/null 2>&1 && continue
    printf '%s\n' "$p"
  done
}
mapfile -t miss_req < <(missing_of "${required[@]}")
mapfile -t miss_opt < <([ "${#optional[@]}" -gt 0 ] && missing_of "${optional[@]}")

if [ "${#miss_req[@]}" -eq 0 ]; then
  [ "${#miss_opt[@]}" -gt 0 ] && echo "requirements: optional, not installed: ${miss_opt[*]} (sudo pacman -S --needed ${miss_opt[*]})"
  echo "requirements: all required packages present"; exit 0
fi
[ "$CHECK" = 1 ] && { printf '%s\n' "${miss_req[@]}"; [ "${#miss_req[@]}" -eq 0 ]; exit; }

install_pkgs() {
  local -a cmd
  if [ "$(id -u)" = 0 ]; then cmd=(pacman -S --needed --noconfirm "$@")
  elif [ -t 0 ] && [ -t 1 ] && command -v sudo >/dev/null 2>&1; then cmd=(sudo pacman -S --needed "$@")
  elif command -v pkexec >/dev/null 2>&1; then cmd=(timeout 900 pkexec pacman -S --needed --noconfirm "$@")
  else echo "requirements: no way to get root here (no terminal, no pkexec)"; return 1
  fi
  echo "requirements: installing $* (${cmd[0]})"
  "${cmd[@]}"
}

if [ "${PILL_BAR_NO_AUTO_INSTALL:-0}" = 1 ]; then
  echo "requirements: missing ${miss_req[*]} (automatic install is off: sudo pacman -S --needed ${miss_req[*]})"
  [ "${#miss_req[@]}" -eq 0 ]; exit
fi
if [ "${#miss_req[@]}" -gt 0 ]; then
  install_pkgs "${miss_req[@]}" || echo "requirements: could not install ${miss_req[*]}: sudo pacman -S --needed ${miss_req[*]}"
  mapfile -t miss_req < <(missing_of "${required[@]}")
fi
# optional packages are never installed automatically: they are only mentioned
if [ "${#miss_opt[@]}" -gt 0 ]; then
  echo "requirements: optional, not installed: ${miss_opt[*]} (sudo pacman -S --needed ${miss_opt[*]})"
fi
if [ "${#miss_req[@]}" -gt 0 ]; then echo "requirements: still missing ${miss_req[*]}"; exit 1; fi
echo "requirements: ok"
