#!/usr/bin/env /usr/bin/python3
"""Equalizer bars for the notch pill: real audio from cava, only while a player is Playing."""
import json, os, subprocess, sys, threading, time

RT = os.getenv('XDG_RUNTIME_DIR', '/tmp')
PLAYERS = 'spotify,%any'
BLOCKS = '▁▂▃▄▅▆▇█'
BARS = 6
CONF = f'{RT}/waybar-cava.conf'
playing = threading.Event()
cur = None
last = None

def emit(text, cls):
    global last
    if (text, cls) != last:
        last = (text, cls)
        print(json.dumps({'text': text, 'class': cls, 'tooltip': False}), flush=True)

def watch():
    global cur
    while True:
        try:
            p = subprocess.Popen(['playerctl', '-p', PLAYERS, 'status', '-F'],
                                 stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
            for line in p.stdout:
                if line.strip() == 'Playing':
                    playing.set()
                else:
                    playing.clear()
                    if cur:
                        cur.terminate()    # stop cava while paused: no idle CPU
        except Exception:
            pass
        playing.clear()
        time.sleep(1)

with open(CONF, 'w') as f:
    f.write(f"""[general]
bars = {BARS}
framerate = 30
[input]
method = pulse
source = auto
[output]
method = raw
raw_target = /dev/stdout
data_format = ascii
ascii_max_range = 7
bar_delimiter = 59
frame_delimiter = 10
""")

threading.Thread(target=watch, daemon=True).start()
idle = BLOCKS[0] * BARS
while True:
    emit(idle, 'idle')
    playing.wait()
    try:
        cur = subprocess.Popen(['cava', '-p', CONF], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    except FileNotFoundError:
        emit(BLOCKS[1:6][0] * BARS, 'playing')     # shortcut: no cava installed, static bars
        time.sleep(5)
        continue
    for line in cur.stdout:
        vals = [int(v) for v in line.strip().split(';') if v.isdigit()]
        if vals:
            emit(''.join(BLOCKS[min(v, 7)] for v in vals), 'playing')
    time.sleep(0.5)
