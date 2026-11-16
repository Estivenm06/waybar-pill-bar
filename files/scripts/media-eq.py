#!/usr/bin/env /usr/bin/python3
"""Equalizer bars for the notch pill: real audio from cava, only while a player is Playing."""
import importlib.util, json, os, secrets, stat, subprocess, sys, threading, time
from importlib.machinery import SourceFileLoader

PLAYERS = 'spotify,%any'
BLOCKS = '▁▂▃▄▅▆▇█'
BARS = 6
CONF = None
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

CAVA_CONF = f"""[general]
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
"""


def publish_conf(rt, text):
    """Write the cava config to a 0600 exclusive temp file and atomically replace the owned regular leaf."""
    dir_fd = os.open(rt, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    tmp = '.tmp-waybar-cava-' + secrets.token_hex(8)
    try:
        try:
            st = os.lstat('waybar-cava.conf', dir_fd=dir_fd)
        except FileNotFoundError:
            st = None
        if st is not None and (not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid()):
            raise OSError('existing cava config is not an owned regular file')
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=dir_fd)
        try:
            with os.fdopen(fd, 'w') as f:
                f.write(text)
                f.flush()
                os.fsync(f.fileno())
            os.replace(tmp, 'waybar-cava.conf', src_dir_fd=dir_fd, dst_dir_fd=dir_fd)
            tmp = None
        finally:
            if tmp:
                try:
                    os.unlink(tmp, dir_fd=dir_fd)
                except OSError:
                    pass
    finally:
        os.close(dir_fd)
    return os.path.join(rt, 'waybar-cava.conf')


def main():
    global CONF, cur
    try:
        here = os.path.dirname(os.path.realpath(__file__))
        loader = SourceFileLoader('waybar_pill_plugin', os.path.join(here, 'waybar-pill-plugin'))
        plug = importlib.util.module_from_spec(importlib.util.spec_from_loader('waybar_pill_plugin', loader))
        loader.exec_module(plug)
        CONF = publish_conf(plug.runtime_dir(), CAVA_CONF)
    except Exception:
        print('media-eq: runtime directory or cava config is not trustworthy', file=sys.stderr)
        return 1
    run_loop()


def run_loop():
    global cur
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


if __name__ == '__main__':
    sys.exit(main())
