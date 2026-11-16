#!/usr/bin/env python3
"""Install record for plugin mode (install.sh / uninstall.sh).

  plugin-state.py begin FILE...   preflight, back up originals once, write the record (hashes pending)
  plugin-state.py finish          record the sha256 of every installed file
  plugin-state.py uninstall       restore the per-file baseline, or report conflicts and change nothing
  plugin-state.py validate        read-only trust/schema/footprint check of the record and recovery journal

Every path in the record must be one of the exact destinations install.sh writes (see waybar-pill-plugin
installation_paths); nothing else is ever restored or deleted.

The record lives in ~/.local/state/waybar-notch/plugin-install.json (see waybar-pill-plugin)."""
import importlib.machinery
import importlib.util
import json
import os
import secrets
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
        p = os.path.join(h, ".config", "hypr", n)
        try:
            dfd = helper.open_directory_chain(os.path.dirname(p), helper.trust_root(p))
            if dfd is None:
                continue
            try:
                fd = helper.open_leaf(dfd, n, p)
            finally:
                os.close(dfd)
            with os.fdopen(fd, "rb") as f:
                if b"waybar-pill-bar BEGIN" in f.read(8 << 20):
                    return True
        except (OSError, PluginError):
            pass  # an unsafe or unreadable file is rejected separately where it would be written
    return False


def check_private_root(path, missing_ok=False):
    """Validate a recorded backup root (real directory, ours, not writable by others) without changing it. True when
    it exists; a missing root is an error unless `missing_ok`."""
    fd = helper.open_directory_chain(path, own_from=helper.trust_root(path, default=path))
    if fd is None:
        if missing_ok:
            return False
        raise PluginError("backup directory %s is missing; the install record was left intact" % path)
    os.close(fd)
    return True


def private_root(path):
    """Validate a recorded backup root and tighten it to 0700 before it is reused."""
    fd = helper.open_directory_chain(path, own_from=helper.trust_root(path, default=path))
    if fd is None:
        raise PluginError("backup directory %s is missing; the install record was left intact" % path)
    try:
        os.fchmod(fd, 0o700)
    finally:
        os.close(fd)


def new_private_root(base):
    """Create the backup root itself as 0700 (exclusive mkdir, so nothing existing or dangling is reused)."""
    pfd = helper.open_directory_chain(os.path.dirname(base), own_from=helper.trust_root(base, default=os.path.dirname(base)),
                                      create=True, create_mode=0o755)
    try:
        bk, n = base, 1
        while True:
            try:
                os.mkdir(os.path.basename(bk), 0o700, dir_fd=pfd)
                return bk
            except FileExistsError:
                n += 1
                bk = "%s-%d" % (base, n)
    finally:
        os.close(pfd)


def backup_path(m, p):
    return m["backup_dir"] + p


def generated_paths():
    """Product outputs that are rebuilt or regenerated after setup (a dev/updated binary, the theme stylesheet the
    theme/font hooks rewrite). A changed hash on these is normal, never a user edit, so it must not block a
    re-setup or an uninstall. Everything else keeps the strict modified-file protection."""
    return {os.path.join(helper.home(), ".local", "bin", "waybar-notch"),
            os.path.join(helper.home(), ".config", "waybar", "pill-theme.css")}


def backup_present(m, p):
    """Whether a safe backup copy of `p` exists; a link or foreign/special file there is an error."""
    src = backup_path(m, p)
    dfd = helper.open_directory_chain(os.path.dirname(src), own_from=helper.home())
    if dfd is None:
        return False
    try:
        try:
            st = os.stat(os.path.basename(src), dir_fd=dfd, follow_symlinks=False)
        except FileNotFoundError:
            return False
        helper.check_leaf(st, src)
        return True
    finally:
        os.close(dfd)


def check_footprint(m):
    """Read-only structural check of everything a later action would touch."""
    root = m["backup_dir"]
    have_root = False
    if helper.validate_directory_chain(os.path.dirname(root), own_from=helper.home()):
        have_root = check_private_root(root, missing_ok=True)
    for e in m["files"]:
        helper.leaf_present(e["path"])
        if have_root:
            backup_present(m, e["path"])


def copy_private(src_fd, dparent, name, final_mode=None):
    """Copy an open file into `dparent` as `name` through an exclusive private temporary and atomic replace,
    keeping the source's bytes, mode and mtime. Returns nothing; leaves no temporary file on failure."""
    tmp = ".restore.%s" % secrets.token_hex(8)
    tfd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=dparent)
    try:
        with open(src_fd, "rb", closefd=False) as r, os.fdopen(tfd, "wb") as w:
            shutil.copyfileobj(r, w)
            w.flush()
            os.fsync(w.fileno())
            st = os.fstat(src_fd)
            os.fchmod(w.fileno(), stat.S_IMODE(st.st_mode))
            os.utime(w.fileno(), ns=(st.st_atime_ns, st.st_mtime_ns))
        os.replace(tmp, name, src_dir_fd=dparent, dst_dir_fd=dparent)
    except BaseException:
        try:
            os.unlink(tmp, dir_fd=dparent)
        except OSError:
            pass
        raise


def backup_original(m, p):
    """Keep the first original of `p` in the private backup root."""
    sparent = helper.open_directory_chain(os.path.dirname(p), own_from=helper.trust_root(p))
    if sparent is None:
        return
    try:
        sfd = helper.open_leaf(sparent, os.path.basename(p), p)
    finally:
        os.close(sparent)
    try:
        dst = backup_path(m, p)
        dparent = helper.open_directory_chain(os.path.dirname(dst), own_from=helper.home(), create=True)
        try:
            copy_private(sfd, dparent, os.path.basename(dst))
        finally:
            os.close(dparent)
    finally:
        os.close(sfd)


def begin(files):
    try:
        allowed = helper.installation_paths()
        paths = []
        for f in files:
            q = os.path.abspath(f)
            if q not in allowed:
                return fail("%s is outside the installation footprint" % f)
            if q not in paths:
                paths.append(q)
        m = helper.read_marker()
        if m is not None:
            check_footprint(m)
        for p in paths:
            helper.leaf_present(p)
        if active_session():
            return fail("the plugin is active: run `omarchy plugin disable waybar.pill-bar` first")
        if m is None and legacy_present():
            return fail("Legacy standalone files detected; remove the old standalone installation with its original uninstaller before running ./install.sh.")
        if m is not None:
            check_private_root(m["backup_dir"])
            mod = [e["path"] for e in m["files"] if e["installed_sha256"] and e["path"] not in generated_paths()
                   and helper.leaf_present(e["path"]) and helper.sha256_of(e["path"]) != e["installed_sha256"]]
            if mod:
                return fail("modified since the last setup, not overwriting: " + ", ".join(mod))
            private_root(m["backup_dir"])
        else:
            base = os.path.join(helper.home(), ".config", "waybar", "backup-" + time.strftime("%Y%m%d-%H%M%S"))
            m = {"version": 1, "mode": "plugin", "backup_dir": new_private_root(base), "files": []}
        known = {e["path"] for e in m["files"]}
        for p in paths:
            if p in known:
                continue
            existed = helper.leaf_present(p)
            if existed:
                backup_original(m, p)
            m["files"].append({"path": p, "existed": existed, "installed_sha256": ""})
        helper.validate_record(m)
        helper.write_state_json(helper.marker_path(), m)
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot record the installation: %s" % (e.strerror or type(e).__name__))
    return 0


def finish():
    try:
        m = helper.read_marker()
        if m is None:
            return fail("no install record")
        check_footprint(m)
        for e in m["files"]:
            if helper.leaf_present(e["path"]):
                e["installed_sha256"] = helper.sha256_of(e["path"])
        helper.write_state_json(helper.marker_path(), m)
    except PluginError as e:
        return fail(str(e))
    return 0


def restore_atomic(m, p):
    """Put the validated backup of `p` back, through descriptors that cannot be redirected by a later path change."""
    src = backup_path(m, p)
    try:
        sparent = helper.open_directory_chain(os.path.dirname(src), own_from=helper.home())
        if sparent is None:
            raise PluginError("backup of %s is missing" % p)
        try:
            sfd = helper.open_leaf(sparent, os.path.basename(src), src)
        finally:
            os.close(sparent)
        try:
            dparent = helper.open_directory_chain(os.path.dirname(p), own_from=helper.trust_root(p), create=True,
                                                  create_mode=0o755)
            try:
                try:
                    helper.check_leaf(os.stat(os.path.basename(p), dir_fd=dparent, follow_symlinks=False), p)
                except FileNotFoundError:
                    pass
                copy_private(sfd, dparent, os.path.basename(p))
            finally:
                os.close(dparent)
        finally:
            os.close(sfd)
    except OSError as exc:
        raise PluginError("cannot restore %s: %s" % (p, exc.strerror or type(exc).__name__)) from exc


def remove_installed(p):
    dparent = helper.open_directory_chain(os.path.dirname(p), own_from=helper.trust_root(p))
    if dparent is None:
        return
    try:
        try:
            helper.check_leaf(os.stat(os.path.basename(p), dir_fd=dparent, follow_symlinks=False), p)
        except FileNotFoundError:
            return
        os.unlink(os.path.basename(p), dir_fd=dparent)
    finally:
        os.close(dparent)


def uninstall_preflight():
    """(record, None) when everything may be acted on, else (None, exit code). Changes nothing."""
    try:
        m = helper.read_marker()
        if m is None:
            return None, fail("no plugin install record; nothing to restore")
        check_footprint(m)
        if active_session():
            return None, fail("the plugin is active: disable it first")
        if any(e["existed"] for e in m["files"]):
            check_private_root(m["backup_dir"])
        conflicts = []
        for e in m["files"]:
            p = e["path"]
            present = helper.leaf_present(p)
            if present and not e["installed_sha256"]:
                conflicts.append(p + " (installation was never finalized)")
            elif present and p not in generated_paths() and helper.sha256_of(p) != e["installed_sha256"]:
                conflicts.append(p + " (modified)")
            if e["existed"] and not backup_present(m, p):
                conflicts.append(p + " (backup missing)")
    except PluginError as e:
        return None, fail(str(e))
    if conflicts:
        return None, fail("footprint left intact; resolve these first: " + "; ".join(conflicts))
    return m, 0


def uninstall():
    m, rc = uninstall_preflight()
    if m is None:
        return rc
    if any(e["existed"] for e in m["files"]):
        private_root(m["backup_dir"])
    for e in m["files"]:
        if e["existed"]:
            restore_atomic(m, e["path"])
        else:
            remove_installed(e["path"])
    helper.remove_state(helper.marker_path())
    return 0


def validate():
    """Read-only: succeeds when there is no record; rejects any existing record or journal that is not trustworthy."""
    try:
        m = helper.read_marker()
        if m is not None:
            check_footprint(m)
        j = helper.trusted_state_json(helper.journal_path())
        if j is not None:
            helper.validate_journal(j)
    except PluginError as e:
        return fail(str(e))
    return 0


def main(argv):
    if argv[:1] == ["begin"] and len(argv) > 1:
        return begin(argv[1:])
    if argv == ["finish"]:
        return finish()
    if argv == ["uninstall"]:
        return uninstall()
    if argv == ["validate"]:
        return validate()
    print(__doc__)
    return 2


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except PluginError as e:
        sys.exit(fail(str(e)))
