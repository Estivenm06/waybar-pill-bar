#!/usr/bin/env /usr/bin/python3
"""Volume keys: change the volume, then ask the notch daemon to show its overlay.
Plugin installed but inactive: native Omarchy volume handling instead."""
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ACTIONS = {
    'raise': ['pamixer', '--allow-boost', '--set-limit', '150', '--unmute', '-i', '5'],
    'lower': ['pamixer', '--allow-boost', '--unmute', '-d', '5'],
    'mute-toggle': ['pamixer', '-t'],
}
action = sys.argv[1] if len(sys.argv) > 1 else 'raise'
helper = os.path.join(HERE, 'waybar-pill-plugin')
if subprocess.run([sys.executable, helper, 'mode'], capture_output=True, text=True).stdout.strip() == 'plugin' \
        and subprocess.run([sys.executable, helper, 'active']).returncode != 0:
    sys.exit(subprocess.run(['omarchy-audio-output-volume', action]).returncode)
cmd = ACTIONS.get(action)
if cmd:
    subprocess.run(cmd)
subprocess.run([os.path.join(HERE, 'waybar-popupctl'), 'osd-volume'])
