#!/usr/bin/env /usr/bin/python3
"""Volume keys: change the volume, then ask the notch daemon to show its overlay."""
import subprocess, sys

ACTIONS = {
    'raise': ['pamixer', '--allow-boost', '--set-limit', '150', '--unmute', '-i', '5'],
    'lower': ['pamixer', '--allow-boost', '--unmute', '-d', '5'],
    'mute-toggle': ['pamixer', '-t'],
}
cmd = ACTIONS.get(sys.argv[1] if len(sys.argv) > 1 else 'raise')
if cmd:
    subprocess.run(cmd)
subprocess.run(['__HOME__/.config/waybar/scripts/waybar-popupctl', 'osd-volume'])
