#!/usr/bin/env python3
"""Install record for plugin mode (install.sh / uninstall.sh).

  plugin-state.py begin FILE...   preflight, back up originals once, write the record (hashes pending)
  plugin-state.py finish          record the sha256 of every installed file
  plugin-state.py uninstall       restore the per-file baseline, or report conflicts and change nothing

The record lives in ~/.local/state/waybar-notch/plugin-install.json (see waybar-pill-plugin)."""
import importlib.machinery
import importlib.util
import json
import os
import shutil
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
_loader = importlib.machinery.SourceFileLoader("waybar_pill_plugin", os.path.join(HERE, "..", "scripts", "waybar-pill-plugin"))
_spec = importlib.util.spec_from_loader("waybar_pill_plugin", _loader)
helper = importlib.util.module_from_spec(_spec)
_loader.exec_module(helper)
PluginError = helper.PluginError


def fail(msg):
    print("plugin-state: " + msg, file=sys.stderr)
    return 1


def active_session():
    try:
        r = helper.request("active", 1.0)
    except PluginError:
        return False
    return bool(r and r.get("active"))


def legacy_present():
    h = helper.home()
    cfg = os.environ.get("XDG_CONFIG_HOME") or os.path.join(h, ".config")
    if os.path.exists(os.path.join(cfg, "systemd", "user", "waybar.service.d", "50-waybar-notch.conf")) or \
            os.path.exists(os.path.join(cfg, "systemd", "user", "waybar-notch.service")):
        return True
    for n in ("autostart.lua", "bindings.lua"):
        try:
            with open(os.path.join(h, ".config", "hypr", n)) as f:
                if "waybar-pill-bar BEGIN" in f.read():
                    return True
        except OSError:
            pass
    return False


def begin(files):
    m = helper.read_marker()
    if active_session():
        return fail("the plugin is active: run `omarchy plugin disable waybar.pill-bar` first")
    if m is None and legacy_present():
        return fail("Legacy standalone files detected; remove the old standalone installation with its original uninstaller before running ./install.sh.")
    paths = [os.path.abspath(p) for p in files]
    if m is None:
        base = os.path.join(helper.home(), ".config", "waybar", "backup-" + time.strftime("%Y%m%d-%H%M%S"))
        bk, n = base, 1
        while os.path.exists(bk):
            n += 1
            bk = "%s-%d" % (base, n)
        m = {"version": 1, "mode": "plugin", "backup_dir": bk, "files": []}
    else:
        mod = [e["path"] for e in m["files"] if e.get("installed_sha256") and os.path.exists(e["path"])
               and helper.sha256_of(e["path"]) != e["installed_sha256"]]
        if mod:
            return fail("modified since the last setup, not overwriting: " + ", ".join(mod))
    known = {e["path"] for e in m["files"]}
    for p in paths:
        if p in known:
            continue
        existed = os.path.lexists(p)
        if existed:
            dst = m["backup_dir"] + p
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(p, dst)
        m["files"].append({"path": p, "existed": existed, "installed_sha256": ""})
    helper.write_atomic(helper.marker_path(), json.dumps(m, indent=2) + "\n")
    return 0


def finish():
    m = helper.read_marker()
    if m is None:
        return fail("no install record")
    for e in m["files"]:
        if os.path.exists(e["path"]):
            e["installed_sha256"] = helper.sha256_of(e["path"])
    helper.write_atomic(helper.marker_path(), json.dumps(m, indent=2) + "\n")
    return 0


def uninstall():
    m = helper.read_marker()
    if m is None:
        return fail("no plugin install record; nothing to restore")
    if active_session():
        return fail("the plugin is active: disable it first")
    conflicts = []
    for e in m["files"]:
        p = e["path"]
        if e.get("installed_sha256") and os.path.exists(p) and helper.sha256_of(p) != e["installed_sha256"]:
            conflicts.append(p + " (modified)")
        if e["existed"] and not os.path.lexists(m["backup_dir"] + p):
            conflicts.append(p + " (backup missing)")
    if conflicts:
        return fail("footprint left intact; resolve these first: " + "; ".join(conflicts))
    for e in m["files"]:
        p = e["path"]
        if e["existed"]:
            tmp = p + ".restore.%d" % os.getpid()
            shutil.copy2(m["backup_dir"] + p, tmp)
            os.replace(tmp, p)
        elif os.path.lexists(p):
            os.unlink(p)
    os.unlink(helper.marker_path())
    return 0


def main(argv):
    if argv[:1] == ["begin"] and len(argv) > 1:
        return begin(argv[1:])
    if argv == ["finish"]:
        return finish()
    if argv == ["uninstall"]:
        return uninstall()
    print(__doc__)
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except PluginError as e:
        sys.exit(fail(str(e)))
