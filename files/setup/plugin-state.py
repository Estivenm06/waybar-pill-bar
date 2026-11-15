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
import tempfile
import shutil
import stat
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


def private_root(path):
    """Validate a recorded backup root (real directory, ours) and tighten it to 0700 before it is reused."""
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        raise PluginError("backup directory %s is missing; the install record was left intact" % path)
    if not stat.S_ISDIR(st.st_mode):
        raise PluginError("backup path %s is not a plain directory" % path)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except OSError as e:
        raise PluginError("cannot open backup directory %s: %s" % (path, e.strerror))
    try:
        fst = os.fstat(fd)
        if not stat.S_ISDIR(fst.st_mode) or fst.st_uid != os.geteuid():
            raise PluginError("backup directory %s is not owned by you" % path)
        os.fchmod(fd, 0o700)
    finally:
        os.close(fd)


def new_private_root(base):
    """Create the backup root itself as 0700 (exclusive mkdir, so nothing existing or dangling is reused)."""
    os.makedirs(os.path.dirname(base), exist_ok=True)
    bk, n = base, 1
    while True:
        try:
            os.mkdir(bk, 0o700)
            return bk
        except FileExistsError:
            n += 1
            bk = "%s-%d" % (base, n)


def begin(files):
    m = helper.read_marker()
    if active_session():
        return fail("the plugin is active: run `omarchy plugin disable waybar.pill-bar` first")
    if m is None and legacy_present():
        return fail("Legacy standalone files detected; remove the old standalone installation with its original uninstaller before running ./install.sh.")
    paths = [os.path.abspath(p) for p in files]
    try:
        if m is None:
            base = os.path.join(helper.home(), ".config", "waybar", "backup-" + time.strftime("%Y%m%d-%H%M%S"))
            m = {"version": 1, "mode": "plugin", "backup_dir": new_private_root(base), "files": []}
        else:
            private_root(m["backup_dir"])
            mod = [e["path"] for e in m["files"] if e.get("installed_sha256") and os.path.exists(e["path"])
                   and helper.sha256_of(e["path"]) != e["installed_sha256"]]
            if mod:
                return fail("modified since the last setup, not overwriting: " + ", ".join(mod))
    except PluginError as e:
        return fail(str(e))
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


def restore_atomic(src, dst):
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(prefix=".restore.", dir=os.path.dirname(dst))
        with open(src, "rb") as r, os.fdopen(fd, "wb") as w:
            shutil.copyfileobj(r, w)
            w.flush()
            os.fsync(w.fileno())
        shutil.copystat(src, tmp)
        os.replace(tmp, dst)
    except OSError as exc:
        if tmp is not None:
            try:
                os.unlink(tmp)
            except OSError:
                pass
        raise PluginError("cannot restore %s: %s" % (dst, exc.strerror or type(exc).__name__)) from exc


def uninstall():
    m = helper.read_marker()
    if m is None:
        return fail("no plugin install record; nothing to restore")
    if active_session():
        return fail("the plugin is active: disable it first")
    if any(e["existed"] for e in m["files"]):
        try:
            private_root(m["backup_dir"])
        except PluginError as e:
            return fail(str(e))
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
            restore_atomic(m["backup_dir"] + p, p)
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
