#!/usr/bin/env python3
"""Installer helper: which AI usage tiles and which apps the Waybar setup shows.

  waybar-setup.py defaults                          write the config file if it does not exist
  waybar-setup.py ask                               interactive choice, writes the config file
  waybar-setup.py render-config <config.jsonc>      generate custom/<app> modules + group/apps from the config
  waybar-setup.py render-autostart <in.lua> <out> [--plugin]   add the hidden-workspace rules for background apps
  waybar-setup.py apply                             re-render the installed Waybar config + autostart from the saved apps (Settings page)
"""
import fcntl
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
HOME = os.path.expanduser("~")
CONFIG = os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.join(HOME, ".config"), "waybar-notch", "config.json")
WAYBAR = os.path.join(HOME, ".config", "waybar")
NOTCH_BIN = os.path.join(HOME, ".local", "bin", "waybar-notch")
RECENT_SECS = 30 * 86400


def catalog():
    with open(os.path.join(HERE, "apps-catalog.json")) as f:
        return json.load(f)


def claude_dirs():
    v = os.environ.get("CLAUDE_CONFIG_DIR", "")
    if v.strip():
        return [os.path.join(d.strip(), "projects") for d in v.split(",")]
    return [os.path.join(HOME, ".config/claude/projects"), os.path.join(HOME, ".claude/projects")]


# Mirrors Sources::detect in notch/src/usage.rs
PROVIDERS = [
    ("claude", "Claude Code", claude_dirs),
    ("gemini", "Gemini / Antigravity", lambda: [os.path.join(HOME, ".gemini/antigravity-cli/history.jsonl")]),
    ("omp", "OMP", lambda: [os.path.join(HOME, ".omp/agent/sessions")]),
    ("copilot", "Copilot CLI", lambda: [os.path.join(HOME, ".copilot/session-store.db")]),
]
UNSUPPORTED = [
    ("Codex", "~/.codex"),
    ("OpenCode", "~/.local/share/opencode"),
    ("Cursor", "~/.cursor"),
    ("Cline", "~/.cline"),
]


def newest_mtime(path):
    """Newest mtime of a usage source: newest *.jsonl up to depth 3 for directories, the file (and its -wal) otherwise."""
    best = 0.0
    if os.path.isdir(path):
        base_depth = path.rstrip("/").count("/")
        for root, dirs, files in os.walk(path):
            if root.rstrip("/").count("/") - base_depth >= 3:
                dirs[:] = []
            for n in files:
                if n.endswith(".jsonl"):
                    try:
                        best = max(best, os.stat(os.path.join(root, n)).st_mtime)
                    except OSError:
                        pass
    else:
        for p in (path, path + "-wal"):
            try:
                best = max(best, os.stat(p).st_mtime)
            except OSError:
                pass
    return best


def detect_ai():
    """[(id, label, used_recently)] for providers whose source exists."""
    out = []
    now = time.time()
    for pid, label, paths in PROVIDERS:
        found = [p for p in paths() if os.path.exists(p)]
        if not found:
            continue
        used = any(now - newest_mtime(p) < RECENT_SECS for p in found)
        out.append((pid, label, used))
    return out


def installed_apps():
    out = []
    for a in catalog():
        if "web" in a:
            ok = shutil.which("omarchy-launch-webapp") is not None
        else:
            ok = launch_exe(a) is not None
        if ok:
            out.append(a)
    return out


def launch_exe(a):
    for e in a.get("exe", []):
        if shutil.which(e):
            return e
    return None


def default_config():
    ai = [pid for pid, _, used in detect_ai() if used] or [pid for pid, _, _ in detect_ai()]
    apps = [{"id": a["id"], "background": bool(a.get("default_bg"))} for a in installed_apps()]
    return {"ai": ai, "apps": apps}


class ConfigError(Exception):
    pass


def load_config():
    """The saved object, None when the file does not exist. Unreadable or malformed content raises ConfigError
    (it is never silently replaced)."""
    try:
        with open(CONFIG) as f:
            c = json.load(f)
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as e:
        raise ConfigError("%s is unreadable or not valid JSON (%s); fix or remove it" % (CONFIG, e))
    if not isinstance(c, dict):
        raise ConfigError("%s is not a JSON object; fix or remove it" % CONFIG)
    return c


def merge_into(doc, patch):
    """Recursively merge dicts; every other value (arrays included) replaces."""
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(doc.get(k), dict):
            merge_into(doc[k], v)
        else:
            doc[k] = v
    return doc


def save_patch(patch, only_if_missing=False):
    """Read the latest document under the lock shared with the notch (.config.lock), merge `patch`, replace atomically.
    Returns the written (or, with only_if_missing, the existing) document."""
    d = os.path.dirname(CONFIG)
    os.makedirs(d, exist_ok=True)
    lock_fd = os.open(os.path.join(d, ".config.lock"), os.O_RDWR | os.O_CREAT, 0o600)
    tmp = None
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        doc = load_config()
        if doc is not None and only_if_missing:
            return doc
        doc = merge_into(doc or {}, patch)
        tmp = os.path.join(d, ".config.json.tmp.%d" % os.getpid())
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(doc, f, indent=2)
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, CONFIG)
        tmp = None
        return doc
    finally:
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError:
                pass
        os.close(lock_fd)


def ensure_config():
    c = load_config()
    if c is None:
        c = save_patch(default_config(), only_if_missing=True)
    return c


def prompt(text, default_idx, n):
    """Numbers separated by commas -> list of 0-based indexes; 0 = none; Enter or EOF = default."""
    try:
        raw = input(text).strip()
    except EOFError:
        print()
        return list(default_idx)
    if not raw:
        return list(default_idx)
    if raw == "0":
        return []
    try:
        nums = [int(x) for x in raw.replace(" ", "").split(",") if x]
        if not nums or any(x < 1 or x > n for x in nums):
            raise ValueError
    except ValueError:
        print("Invalid input, using the default.")
        return list(default_idx)
    seen, out = set(), []
    for x in nums:
        if x not in seen:
            seen.add(x)
            out.append(x - 1)
    return out


def nums(idx):
    return ",".join(str(i + 1) for i in idx) or "0"


def cmd_ask():
    existing = load_config()
    base = existing if existing is not None else default_config()
    if "ai" not in base or "apps" not in base:
        d = default_config()
        base.setdefault("ai", d["ai"])
        base.setdefault("apps", d["apps"])

    # 1. AI usage tiles
    detected = detect_ai()
    print("AI tools found on this machine:")
    for i, (_, label, used) in enumerate(detected, 1):
        print("  [%d] %s  (%s)" % (i, label, "used in the last 30 days" if used else "detected, not used recently"))
    if not detected:
        print("  none")
    miss = [name for name, p in UNSUPPORTED if os.path.exists(os.path.expanduser(p))]
    if miss:
        print("Found but not supported yet: " + ", ".join(miss))
    ids = [d[0] for d in detected]
    default_ai = [i for i, pid in enumerate(ids) if pid in base["ai"]]
    if existing is None:
        default_ai = [i for i, d in enumerate(detected) if d[2]] or list(range(len(detected)))
    ai = []
    if detected:
        chosen = prompt("AI usage to show (numbers separated by commas, 0 = none, Enter = %s): " % nums(default_ai),
                        default_ai, len(detected))
        ai = [ids[i] for i in chosen]

    # 2. Apps
    apps_avail = installed_apps()
    print("Apps available for the first pill:")
    for i, a in enumerate(apps_avail, 1):
        print("  [%d] %s" % (i, a["name"]))
    if not apps_avail:
        print("  none")
    chosen_apps = []
    if apps_avail:
        known = {a["id"]: i for i, a in enumerate(apps_avail)}
        prev = [known[a["id"]] for a in base["apps"] if a.get("id") in known]
        default_apps = prev if existing is not None else list(range(len(apps_avail)))
        chosen = prompt("Apps to show in the first pill (numbers, 0 = none, Enter = %s): " % nums(default_apps),
                        default_apps, len(apps_avail))
        if chosen:
            prev_bg = {a["id"] for a in base["apps"] if a.get("background")}
            if existing is not None:
                default_bg = [k for k, i in enumerate(chosen) if apps_avail[i]["id"] in prev_bg]
            else:
                default_bg = [k for k, i in enumerate(chosen) if apps_avail[i].get("default_bg")]
            print("Chosen apps:")
            for k, i in enumerate(chosen, 1):
                print("  [%d] %s" % (k, apps_avail[i]["name"]))
            bg = prompt("Keep running hidden in the background (icon always active; numbers, 0 = none, Enter = %s): "
                        % nums(default_bg), default_bg, len(chosen))
            bg_ids = {apps_avail[chosen[k]]["id"] for k in bg}
            chosen_apps = [{"id": apps_avail[i]["id"], "background": apps_avail[i]["id"] in bg_ids} for i in chosen]

    save_patch({"ai": ai, "apps": chosen_apps})
    print("Saved " + CONFIG)


def app_modules(cfg):
    """[(module name, module dict, catalog entry, config entry)] for configured, installed apps in config order."""
    by_id = {a["id"]: a for a in installed_apps()}
    out = []
    for e in cfg.get("apps", []):
        a = by_id.get(e.get("id"))
        if a is None:
            continue
        out.append(("custom/" + a["id"], a, bool(e.get("background"))))
    return out


def launch_args(a):
    if "web" in a:
        return ["omarchy-launch-webapp", a["web"]]
    return [launch_exe(a)]


def status_signal(conf):
    """Smallest Waybar real-time signal offset (1-30) not used by any other custom module."""
    managed = {"custom/" + a["id"] for a in catalog()}
    used = {m.get("signal") for k, m in conf.items() if k.startswith("custom/") and k not in managed and isinstance(m, dict)}
    for n in range(1, 31):
        if n not in used:
            return n
    raise ConfigError("all Waybar signal numbers 1-30 are used by other custom modules; free one for the app status")


def render_apps(conf, cfg):
    """Rewrite the generated app modules of a Waybar config dict from `cfg` (repeatable: stale entries are removed,
    group/apps is reinserted after hyprland/workspaces when apps return, dropped when none are left).
    Status is read from the supervisor's cache once at start and again on one shared real-time signal."""
    entries = app_modules(cfg)
    sig = status_signal(conf) if entries else None   # may raise: nothing has been modified yet
    ids = {a["id"] for a in catalog()}
    for k in [k for k in conf if k.startswith("custom/") and k[len("custom/"):] in ids]:
        del conf[k]
    s = os.path.join(WAYBAR, "scripts")
    bgapp = shlex.quote(os.path.join(s, "waybar-bgapp.sh"))
    app = shlex.quote(os.path.join(s, "waybar-app.sh"))
    mods = []
    for name, a, bg in entries:
        launch = " ".join(shlex.quote(x) for x in launch_args(a))
        cls = shlex.quote(a["class"])
        if bg:
            click = "%s toggle %s %s %s" % (bgapp, shlex.quote(a["id"]), cls, launch)
        else:
            click = "%s click %s %s" % (app, cls, launch)
        exe = "%s --waybar-app-status %s" % (shlex.quote(NOTCH_BIN), shlex.quote(a["id"]))
        conf[name] = {"format": a["glyph"], "return-type": "json", "exec": exe, "signal": sig,
                      "on-click": click, "tooltip": False}
        mods.append(name)
    group = conf.setdefault("group/apps", {"orientation": "horizontal"})
    group["modules"] = mods
    left = conf["group/left"]["modules"]
    left = [m for m in left if m != "group/apps"]
    if mods:
        at = left.index("hyprland/workspaces") + 1 if "hyprland/workspaces" in left else len(left)
        left.insert(at, "group/apps")
    conf["group/left"]["modules"] = left
    return mods


def cmd_render_config(path):
    with open(path) as f:
        conf = json.load(f)
    cfg = ensure_config()
    mods = render_apps(conf, cfg)
    with open(path, "w") as f:
        json.dump(conf, f, indent=2)
        f.write("\n")
    print("Apps in the first pill: " + (", ".join(m[len("custom/"):] for m in mods) or "none"))
    print("AI usage tiles: " + (", ".join(cfg.get("ai", [])) or ("all detected" if "ai" not in cfg else "none")))


def hypr_re(cls):
    pre = cls.startswith("^")
    post = cls.endswith("$")
    core = cls[1:] if pre else cls
    core = core[:-1] if post and core.endswith("$") else core
    return ("" if pre else ".*") + core + ("" if post else ".*")


def lua_esc(s):
    return s.replace("\\", "\\\\").replace('"', '\\"')


def bg_lines(cfg, plugin=False):
    lines = []
    for _, a, bg in app_modules(cfg):
        if not bg:
            continue
        launch = " ".join(shlex.quote(x) for x in launch_args(a))
        script = "%s/scripts/waybar-bgapp.sh" % WAYBAR
        cmd = "%s ensure %s %s %s" % (shlex.quote(script), shlex.quote(a["id"]), shlex.quote(a["class"]), launch)
        lines.append('o.window("%s", { workspace = "special:%s silent" }) -- @bg' % (lua_esc(hypr_re(a["class"])), a["id"]))
        if not plugin:  # plugin mode: the active supervisor's Tracker maintains background apps, never autostart
            lines.append('o.exec_on_start("%s") -- @bg' % lua_esc(cmd))
    return lines


def cmd_render_autostart(src, dst, plugin=False):
    cfg = ensure_config()
    lines = bg_lines(cfg, plugin)
    with open(src) as f:
        text = f.read().splitlines()
    out = []
    for ln in text:
        if plugin and ln.strip() == 'o.exec_on_start("omarchy-toggle-bar on")':
            continue
        if ln.startswith("-- Popup animation:"):
            out.extend(lines)
        out.append(ln)
    with open(dst, "w") as f:
        f.write("\n".join(out) + "\n")


BLOCK_RE = re.compile(r"-- >>> waybar-pill-bar BEGIN.*?-- <<< waybar-pill-bar END <<<\n", re.S)
REINSTALL = "re-run ./install.sh to restore the managed block"


def replace_bg_rules(text, lines):
    """Replace only the `-- @bg` rules inside the one managed block; everything else is returned untouched."""
    blocks = BLOCK_RE.findall(text)
    if len(blocks) != 1:
        raise ConfigError("autostart.lua has %d waybar-pill-bar managed blocks (need exactly 1); %s" % (len(blocks), REINSTALL))
    block = blocks[0]
    kept = [ln for ln in block.splitlines() if not ln.rstrip().endswith("-- @bg")]
    anchors = [i for i, ln in enumerate(kept) if ln.startswith("-- Popup animation:")]
    if len(anchors) != 1:
        raise ConfigError("the managed block has %d '-- Popup animation:' anchors (need exactly 1); %s" % (len(anchors), REINSTALL))
    kept[anchors[0]:anchors[0]] = lines
    new_block = "\n".join(kept) + "\n"
    return text.replace(block, new_block, 1)


def write_atomic(path, text):
    tmp = path + ".tmp.%d" % os.getpid()
    with open(tmp, "w") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def hypr_errors():
    r = subprocess.run(["hyprctl", "configerrors"], capture_output=True, text=True)
    return [ln for ln in (r.stdout + r.stderr).splitlines() if ln.strip()]


def hypr_reload():
    subprocess.run(["hyprctl", "reload"], capture_output=True, text=True)


def plugin_mode():
    """'plugin' or 'standalone' from the install marker; a malformed marker is an error."""
    helper = os.path.join(WAYBAR, "scripts", "waybar-pill-plugin")
    if not os.path.exists(helper):
        return "standalone"
    r = subprocess.run([sys.executable, helper, "mode"], capture_output=True, text=True)
    if r.returncode != 0 or r.stdout.strip() not in ("plugin", "standalone"):
        raise ConfigError("cannot determine install mode: " + r.stderr.strip())
    return r.stdout.strip()


def cmd_apply():
    """Re-render the installed Waybar config and Hyprland autostart from the saved `apps`, validate, apply, and roll back
    both files if Hyprland reports new config errors."""
    cfg = load_config()
    if cfg is None:
        raise ConfigError("%s does not exist" % CONFIG)
    wconf = os.path.join(WAYBAR, "config.jsonc")
    auto = os.path.join(HOME, ".config", "hypr", "autostart.lua")
    with open(wconf) as f:
        conf_text = f.read()
    with open(auto) as f:
        auto_text = f.read()
    conf = json.loads(conf_text)
    render_apps(conf, cfg)
    new_conf = json.dumps(conf, indent=2) + "\n"
    json.loads(new_conf)
    plugin = plugin_mode() == "plugin"
    new_auto = replace_bg_rules(auto_text, bg_lines(cfg, plugin))
    if new_conf == conf_text and new_auto == auto_text:
        print("Nothing to change")
    else:
        bk = os.path.join(WAYBAR, "backup-%s" % time.strftime("%Y%m%d-%H%M%S"))
        os.makedirs(os.path.join(bk, "hypr"), exist_ok=True)
        shutil.copy2(wconf, os.path.join(bk, "config.jsonc"))
        shutil.copy2(auto, os.path.join(bk, "hypr", "autostart.lua"))
        baseline = set(hypr_errors())
        write_atomic(wconf, new_conf)
        write_atomic(auto, new_auto)
        hypr_reload()
        fresh = [e for e in hypr_errors() if e not in baseline]
        if fresh:
            write_atomic(wconf, conf_text)
            write_atomic(auto, auto_text)
            hypr_reload()
            raise ConfigError("Hyprland rejected the new autostart rules, previous files restored: " + "; ".join(fresh))
    if plugin:
        helper = os.path.join(WAYBAR, "scripts", "waybar-pill-plugin")
        if new_conf != conf_text or new_auto != auto_text:
            r = subprocess.run([sys.executable, helper, "record-generated", wconf, auto], capture_output=True, text=True)
            if r.returncode != 0:
                raise ConfigError("could not record the regenerated files: " + r.stderr.strip())
        if subprocess.run([sys.executable, helper, "active"]).returncode != 0:
            print("Saved (plugin disabled)")
            return
        r = subprocess.run([sys.executable, helper, "reload"], capture_output=True, text=True)
        if r.returncode != 0:
            raise ConfigError("waybar-pill-plugin reload failed: " + (r.stderr.strip() or "exit %d" % r.returncode))
        print("Applied")
        return
    r = subprocess.run(["systemctl", "--user", "reload", "waybar.service"], capture_output=True, text=True)
    if r.returncode != 0:
        raise ConfigError("systemctl --user reload waybar.service failed: " + (r.stderr.strip() or "exit %d" % r.returncode))
    print("Applied")


def main(argv):
    if not argv:
        print(__doc__)
        return 1
    c = argv[0]
    if c == "defaults":
        ensure_config()
    elif c == "ask":
        cmd_ask()
    elif c == "render-config" and len(argv) == 2:
        cmd_render_config(argv[1])
    elif c == "render-autostart" and len(argv) in (3, 4) and (len(argv) == 3 or argv[3] == "--plugin"):
        cmd_render_autostart(argv[1], argv[2], len(argv) == 4)
    elif c == "apply" and len(argv) == 1:
        cmd_apply()
    else:
        print(__doc__)
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except ConfigError as e:
        print("waybar-setup: " + str(e), file=sys.stderr)
        sys.exit(1)
