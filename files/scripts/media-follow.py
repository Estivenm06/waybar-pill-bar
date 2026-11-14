#!/usr/bin/env /usr/bin/python3
"""Event-driven media module for Waybar: follows `playerctl -F metadata`."""
import json, subprocess, sys, time

PLAYERS = 'spotify,%any'   # prefer Spotify, fall back to any MPRIS player
WIDTH = 24                 # visible characters of "artist - title"
ICON = {'Playing': '', 'Paused': '', 'Stopped': ''}
FMT = '{{status}}\t{{artist}}\t{{title}}'
last = None

def emit(cls, text):
    global last
    if (cls, text) != last:
        last = (cls, text)
        print(json.dumps({'text': text, 'class': cls, 'tooltip': False}), flush=True)

def show(status, artist, title):
    t = f'{artist} - {title}' if artist else title
    if len(t) > WIDTH:
        t = t[:WIDTH - 1] + '…'
    emit(status.lower(), t)

while True:
    emit('none', 'No media')
    try:
        p = subprocess.Popen(['playerctl', '-p', PLAYERS, 'metadata', '-F', '--format', FMT],
                             stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        for line in p.stdout:
            parts = (line.rstrip('\n').split('\t') + ['', '', ''])[:3]
            if parts[0]:
                show(*parts)
            else:
                emit('none', 'No media')
    except Exception:
        pass
    time.sleep(1)      # player gone / playerctl exited: retry
