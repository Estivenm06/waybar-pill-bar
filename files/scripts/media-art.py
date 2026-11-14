#!/usr/bin/env /usr/bin/python3
"""Print the path of the current track's cover (cached); print nothing when there is none."""
import glob, hashlib, os, subprocess, urllib.request

RT = os.getenv('XDG_RUNTIME_DIR', '/tmp')
PLAYERS = 'spotify,%any'

try:
    url = subprocess.run(['playerctl', '-p', PLAYERS, 'metadata', 'mpris:artUrl'],
                         capture_output=True, text=True, timeout=3).stdout.strip()
except Exception:
    url = ''
if url:
    path = f'{RT}/waybar-art-{hashlib.md5(url.encode()).hexdigest()[:12]}'
    if not os.path.exists(path):
        try:
            if url.startswith('file://'):
                path = url[7:]
            else:
                data = urllib.request.urlopen(url, timeout=3).read()
                for old in glob.glob(f'{RT}/waybar-art-*'):   # keep only the current cover
                    os.unlink(old)
                with open(path, 'wb') as f:
                    f.write(data)
        except Exception:
            path = ''
    if path:
        print(path)
