#!/usr/bin/env python3
"""Install record for plugin mode: the session-owned footprint of the waybar.pill-bar plugin.

  plugin-state.py begin FILE...             preflight, back up originals once, write the record (digests pending)
  plugin-state.py prepare PATH SOURCE MODE  save the after-image of PATH (the staged SOURCE with permission MODE) and
                                            durably record its SHA-256; nothing is published yet
  plugin-state.py publish PATH              atomically replace PATH with its prepared after-image
  plugin-state.py finish                    verify that every recorded file holds its prepared bytes and mode
  plugin-state.py uninstall                 restore the per-file baseline, or report conflicts and change nothing
  plugin-state.py validate                  read-only trust/schema/footprint check of the record and recovery journal
  plugin-state.py cache-prepare             validate (creating 0700 when absent) the build cache and its product leaves
  plugin-state.py cache-stamp HASH          atomically record the 64-hex source hash as the cache stamp

Setup order: `begin` for every destination, `prepare` for every destination, then `publish` each one, then `finish`.
Interrupted at any point, `uninstall` accepts only an absent destination, its original bytes, its prepared bytes, or
(for the two Hyprland Lua files) an untouched product block between the user's own edits; anything else is left alone.

Every path in the record must be one of the exact destinations the setup writes (see waybar-pill-plugin
installation_paths); nothing else is ever restored or deleted.

The record lives in ~/.local/state/waybar-notch/plugin-install.json (see waybar-pill-plugin). Originals are kept in
the record's backup directory; prepared after-images live below its `.installed` subdirectory."""
import sys

sys.dont_write_bytecode = True

import hashlib  # noqa: E402
import importlib.machinery  # noqa: E402
import importlib.util  # noqa: E402
import os  # noqa: E402
import re  # noqa: E402
import secrets  # noqa: E402
import shutil  # noqa: E402
import stat  # noqa: E402
import time  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
_loader = importlib.machinery.SourceFileLoader("waybar_pill_plugin", os.path.join(HERE, "..", "scripts", "waybar-pill-plugin"))
_spec = importlib.util.spec_from_loader("waybar_pill_plugin", _loader)
helper = importlib.util.module_from_spec(_spec)
_loader.exec_module(helper)
PluginError = helper.PluginError

INSTALLED = ".installed"
MAX_TEXT = 8 << 20
BLOCK_BEGIN_RE = re.compile(r"^-- >>> waybar-pill-bar BEGIN\b")
BLOCK_END = "-- <<< waybar-pill-bar END <<<"


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


def installed_path(m, p):
    """Where the prepared after-image of `p` is kept."""
    return os.path.join(m["backup_dir"], INSTALLED) + p


def generated_paths(m=None):
    """Product outputs that are regenerated after setup (the theme stylesheet the theme/font hooks rewrite). A changed
    hash on these is normal, never a user edit. A record written before after-images existed additionally exempts the
    binary, which developers rebuilt in place; a current record keeps the strict check for it."""
    paths = {os.path.join(helper.home(), ".config", "waybar", "pill-theme.css")}
    if m is not None and not has_after_images(m):
        paths.add(os.path.join(helper.home(), ".local", "bin", "waybar-notch"))
    return paths


def has_after_images(m):
    fd = helper.open_directory_chain(os.path.join(m["backup_dir"], INSTALLED), own_from=helper.home())
    if fd is None:
        return False
    os.close(fd)
    return True


def safe_leaf_present(src):
    """Whether a safe owned regular file exists at `src` below the product backup tree; a link or foreign/special file
    there is an error."""
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


def backup_present(m, p):
    """Whether a safe backup copy of `p` exists; a link or foreign/special file there is an error."""
    return safe_leaf_present(backup_path(m, p))


def installed_present(m, p):
    return safe_leaf_present(installed_path(m, p))


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
            installed_present(m, e["path"])


def digest_stream(f):
    h = hashlib.sha256()
    for chunk in iter(lambda: f.read(65536), b""):
        h.update(chunk)
    return h.hexdigest()


def leaf_info(path, root=None):
    """(sha256, permission bits) of an owned regular file reached without following any link, None when absent."""
    dfd = helper.open_directory_chain(os.path.dirname(path), own_from=root if root is not None else helper.trust_root(path))
    if dfd is None:
        return None
    try:
        try:
            fd = helper.open_leaf(dfd, os.path.basename(path), path)
        except FileNotFoundError:
            return None
    finally:
        os.close(dfd)
    with os.fdopen(fd, "rb") as f:
        return digest_stream(f), stat.S_IMODE(os.fstat(f.fileno()).st_mode)


def read_leaf(path, root=None):
    """Bytes of an owned regular file (bounded), None when absent."""
    dfd = helper.open_directory_chain(os.path.dirname(path), own_from=root if root is not None else helper.trust_root(path))
    if dfd is None:
        return None
    try:
        try:
            fd = helper.open_leaf(dfd, os.path.basename(path), path)
        except FileNotFoundError:
            return None
    finally:
        os.close(dfd)
    with os.fdopen(fd, "rb") as f:
        data = f.read(MAX_TEXT + 1)
    if len(data) > MAX_TEXT:
        raise PluginError("%s is too large" % path)
    return data


def copy_private(src_fd, dparent, name, final_mode=None):
    """Copy an open file into `dparent` as `name` through an exclusive private temporary and atomic replace. Without
    `final_mode` the source's mode and mtime are kept (originals); with it the file gets exactly those permission
    bits. Returns the SHA-256 of the bytes written; leaves no temporary file on failure."""
    tmp = ".restore.%s" % secrets.token_hex(8)
    tfd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=dparent)
    try:
        with open(src_fd, "rb", closefd=False) as r, os.fdopen(tfd, "wb") as w:
            shutil.copyfileobj(r, w)
            w.flush()
            os.fsync(w.fileno())
            st = os.fstat(src_fd)
            os.fchmod(w.fileno(), stat.S_IMODE(st.st_mode) if final_mode is None else final_mode)
            if final_mode is None:
                os.utime(w.fileno(), ns=(st.st_atime_ns, st.st_mtime_ns))
        rfd = os.open(tmp, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=dparent)
        with os.fdopen(rfd, "rb") as f:
            digest = digest_stream(f)
        os.replace(tmp, name, src_dir_fd=dparent, dst_dir_fd=dparent)
        os.fsync(dparent)
    except BaseException:
        try:
            os.unlink(tmp, dir_fd=dparent)
        except OSError:
            pass
        raise
    return digest


def write_private(dparent, name, data, mode):
    """Atomically publish `data` as `name` with permission bits `mode` through an exclusive private temporary."""
    tmp = ".restore.%s" % secrets.token_hex(8)
    tfd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=dparent)
    try:
        with os.fdopen(tfd, "wb") as w:
            w.write(data)
            w.flush()
            os.fsync(w.fileno())
            os.fchmod(w.fileno(), mode)
        os.replace(tmp, name, src_dir_fd=dparent, dst_dir_fd=dparent)
        os.fsync(dparent)
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


def entry_for(m, path):
    q = os.path.abspath(path)
    for e in m["files"]:
        if e["path"] == q:
            return e
    raise PluginError("%s is not part of the plugin install record" % path)


def parse_mode(text):
    if not re.fullmatch(r"0?[0-7]{3}", text or ""):
        raise PluginError("%r is not a permission mode like 644 or 755" % text)
    mode = int(text, 8)
    if not mode & 0o400:
        raise PluginError("mode %s would make the file unreadable to its owner" % text)
    return mode


def open_source(source):
    """Descriptor of an owned regular staged file inside a directory the effective user owns."""
    if not isinstance(source, str) or not os.path.isabs(source) or os.path.normpath(source) != source:
        raise PluginError("%s is not an absolute normalized path" % source)
    dfd = helper.open_directory_chain(os.path.dirname(source), own_from=os.path.dirname(source))
    if dfd is None:
        raise PluginError("staged source %s is missing" % source)
    try:
        try:
            return helper.open_leaf(dfd, os.path.basename(source), source, state=True)
        except FileNotFoundError:
            raise PluginError("staged source %s is missing" % source)
    finally:
        os.close(dfd)


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
            if not m["files"] or any(e["path"] not in helper.adapter_paths() for e in m["files"]):
                return fail("a previous setup is still recorded; restore it first: run `waybar-pill-plugin recover` "
                            "(or ./uninstall.sh) and retry")
            for e in m["files"]:
                if e["path"] in paths:
                    fd, mode = open_after_image(m, e)
                    os.close(fd)
                    if leaf_info(e["path"]) != (e["installed_sha256"], mode):
                        return fail("previous setup: retained adapter %s was modified; restore it first (`waybar-pill-plugin recover`)" % e["path"])
        for p in paths:
            helper.leaf_present(p)
        if active_session():
            return fail("the plugin is active: run `omarchy plugin disable waybar.pill-bar` first")
        if legacy_present():
            return fail("Legacy standalone files detected; remove the old standalone installation with its original uninstaller first.")
        if m is None:
            base = os.path.join(helper.home(), ".config", "waybar", "backup-" + time.strftime("%Y%m%d-%H%M%S"))
            m = {"version": 1, "mode": "plugin", "backup_dir": new_private_root(base), "files": []}
        for p in paths:
            if any(e["path"] == p for e in m["files"]):
                continue  # keep the original stock backup, never snapshot the retained adapter
            existed = helper.leaf_present(p)
            if existed:
                backup_original(m, p)
            m["files"].append({"path": p, "existed": existed, "installed_sha256": ""})
        fd = helper.open_directory_chain(os.path.join(m["backup_dir"], INSTALLED), own_from=helper.home(), create=True)
        os.close(fd)
        helper.validate_record(m)
        helper.write_state_json(helper.marker_path(), m)
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot record the installation: %s" % (e.strerror or type(e).__name__))
    return 0


def prepare(path, source, mode_text):
    """Save the after-image of `path` and durably record its digest. The destination is not touched."""
    try:
        mode = parse_mode(mode_text)
        m = helper.read_marker()
        if m is None:
            return fail("no install record: run `begin` first")
        e = entry_for(m, path)
        check_footprint(m)
        private_root(m["backup_dir"])
        sfd = open_source(source)
        try:
            dst = installed_path(m, e["path"])
            dparent = helper.open_directory_chain(os.path.dirname(dst), own_from=helper.home(), create=True)
            try:
                previous = e["installed_sha256"]
                e["installed_sha256"] = copy_private(sfd, dparent, os.path.basename(dst), final_mode=mode)
            finally:
                os.close(dparent)
        finally:
            os.close(sfd)
        if previous and previous != e["installed_sha256"] and "retained_sha256" not in e:
            e["retained_sha256"] = previous  # the retained adapter this setup adopted and may replace
        helper.validate_record(m)
        helper.write_state_json(helper.marker_path(), m)
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot prepare %s: %s" % (path, e.strerror or type(e).__name__))
    return 0


def open_after_image(m, e):
    """(descriptor, mode) of the after-image whose digest was recorded; the bytes must still match that digest."""
    p, sha = e["path"], e["installed_sha256"]
    if not sha:
        raise PluginError("%s was not prepared" % p)
    src = installed_path(m, p)
    dfd = helper.open_directory_chain(os.path.dirname(src), own_from=helper.home())
    if dfd is None:
        raise PluginError("the prepared image of %s is missing" % p)
    try:
        try:
            fd = helper.open_leaf(dfd, os.path.basename(src), src)
        except FileNotFoundError:
            raise PluginError("the prepared image of %s is missing" % p)
    finally:
        os.close(dfd)
    try:
        with open(fd, "rb", closefd=False) as f:
            if digest_stream(f) != sha:
                raise PluginError("the prepared image of %s no longer matches its recorded digest" % p)
        os.lseek(fd, 0, os.SEEK_SET)
        return fd, stat.S_IMODE(os.fstat(fd).st_mode)
    except BaseException:
        os.close(fd)
        raise


def publish(path):
    """Replace `path` by its prepared after-image once its current state is provably one this setup may replace."""
    try:
        m = helper.read_marker()
        if m is None:
            return fail("no install record: run `begin` first")
        e = entry_for(m, path)
        check_footprint(m)
        p, sha = e["path"], e["installed_sha256"]
        fd, mode = open_after_image(m, e)
        try:
            cur = leaf_info(p)
            if cur is not None and cur == (sha, mode):
                return 0  # already published: skip a byte-identical write
            if cur is None:
                if e["existed"]:
                    return fail("%s disappeared since the setup began; not recreating it" % p)
            elif cur[0] != sha:
                orig = leaf_info(backup_path(m, p), helper.home()) if e["existed"] else None
                if (orig is None or cur[0] != orig[0]) and cur[0] != e.get("retained_sha256"):
                    return fail("%s changed since the setup began; not overwriting it" % p)
            dparent = helper.open_directory_chain(os.path.dirname(p), own_from=helper.trust_root(p), create=True,
                                                  create_mode=0o755)
            try:
                try:
                    helper.check_leaf(os.stat(os.path.basename(p), dir_fd=dparent, follow_symlinks=False), p)
                except FileNotFoundError:
                    pass
                copy_private(fd, dparent, os.path.basename(p))
            finally:
                os.close(dparent)
        finally:
            os.close(fd)
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot publish %s: %s" % (path, e.strerror or type(e).__name__))
    return 0


def finish(paths=None):
    """Every recorded file (or only `paths`: retained adapters left alone stay recorded as they are) must hold exactly
    its prepared bytes and mode; nothing is adopted from the current content."""
    try:
        m = helper.read_marker()
        if m is None:
            return fail("no install record")
        check_footprint(m)
        changed = False
        for e in m["files"]:
            p = e["path"]
            if paths is not None and p not in paths:
                continue
            fd, mode = open_after_image(m, e)
            os.close(fd)
            cur = leaf_info(p)
            if cur is None:
                return fail("%s was not published" % p)
            if cur != (e["installed_sha256"], mode):
                return fail("%s does not hold its prepared bytes and mode" % p)
            changed = changed or e.pop("retained_sha256", None) is not None
        if changed:
            helper.write_state_json(helper.marker_path(), m)
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot verify the installation: %s" % (e.strerror or type(e).__name__))
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


def write_text(p, data, mode):
    """Atomically replace an existing owned file by `data` keeping the permission bits `mode`."""
    dparent = helper.open_directory_chain(os.path.dirname(p), own_from=helper.trust_root(p))
    if dparent is None:
        raise PluginError("%s has no parent directory" % p)
    try:
        helper.check_leaf(os.stat(os.path.basename(p), dir_fd=dparent, follow_symlinks=False), p)
        write_private(dparent, os.path.basename(p), data, mode)
    except OSError as exc:
        raise PluginError("cannot restore %s: %s" % (p, exc.strerror or type(exc).__name__)) from exc
    finally:
        os.close(dparent)


# ---- Hyprland Lua files: the product owns one named block; everything around it is the user's

def lua_paths():
    h = helper.home()
    return {os.path.join(h, ".config", "hypr", "autostart.lua"), os.path.join(h, ".config", "hypr", "bindings.lua")}


def find_block(text, what):
    """(start, end) offsets of the one managed block, None when there is none. Duplicated, unpaired or misordered
    markers are an error: nothing is guessed."""
    begins, ends, pos = [], [], 0
    for line in text.splitlines(keepends=True):
        body = line.rstrip("\r\n")
        if BLOCK_BEGIN_RE.match(body):
            begins.append(pos)
        elif body == BLOCK_END:
            ends.append(pos + len(line))
        pos += len(line)
    if not begins and not ends:
        return None
    if len(begins) != 1 or len(ends) != 1 or ends[0] <= begins[0]:
        raise PluginError("%s has duplicated or malformed waybar-pill-bar block markers" % what)
    return begins[0], ends[0]


def decode(data, what):
    try:
        return data.decode("utf-8")
    except UnicodeDecodeError:
        raise PluginError("%s is not valid UTF-8" % what)


def lua_restore(m, e, current):
    """Merge the user's edits made outside the product block with the restored original: ("write", bytes) or ("delete",).
    Raises PluginError unless the current block is exactly the installed one."""
    p = e["path"]
    cur = decode(current, p)
    fd, _ = open_after_image(m, e)
    with os.fdopen(fd, "rb") as image:
        inst = decode(image.read(), "the prepared image of " + p)
    orig = decode(read_leaf(backup_path(m, p), helper.home()) or b"", "the backup of " + p) if e["existed"] else ""
    ib, cb, ob = find_block(inst, "the prepared image of " + p), find_block(cur, p), find_block(orig, "the backup of " + p)
    if ib is None:
        raise PluginError("the prepared image of %s has no product block" % p)
    if cb is None:
        if ob is None:
            return ("keep", None)
        raise PluginError("the product block of %s was removed" % p)
    if ob is not None and cur[cb[0]:cb[1]] == orig[ob[0]:ob[1]]:
        return ("keep", None)
    if cur[cb[0]:cb[1]] != inst[ib[0]:ib[1]]:
        raise PluginError("the product block of %s was edited" % p)
    before, after = cur[:cb[0]], cur[cb[1]:]
    if ob is None:
        # the setup appended "\n" + block to the original; remove that separator with the block
        if inst[:ib[0]] == orig + "\n" and before.endswith("\n"):
            before = before[:-1]
        text = before + after
    else:
        text = before + orig[ob[0]:ob[1]] + after
    if not e["existed"] and not text.strip():
        return ("delete", None)
    return ("write", text.encode("utf-8"))


def plan_entry(m, e):
    """(action, payload) for one recorded file, or raises ConflictError with the reason. Actions: keep, restore, remove,
    write, delete."""
    p, sha = e["path"], e["installed_sha256"]
    cur = leaf_info(p)
    orig = None
    if e["existed"]:
        if not backup_present(m, p):
            raise Conflict("backup missing")
        orig = leaf_info(backup_path(m, p), helper.home())
        if cur is None:
            return ("restore", None)  # deleted since: the original goes back
        if cur[0] == orig[0]:
            return ("keep", None) if cur[1] == orig[1] else ("restore", None)
    elif cur is None:
        return ("keep", None)
    if sha and cur[0] in (sha, e.get("retained_sha256")):
        return ("restore", None) if e["existed"] else ("remove", None)
    if p in generated_paths(m):
        return ("restore", None) if e["existed"] else ("remove", None)
    if p in lua_paths() and installed_present(m, p) and sha:
        try:
            current = read_leaf(p)
            return lua_restore(m, e, current)
        except PluginError as exc:
            raise Conflict(str(exc))
    raise Conflict("installation was never finalized" if not sha else "modified")


class Conflict(Exception):
    pass


validate_tree = helper.validate_tree
remove_tree = helper.remove_tree


def with_backup_root(m, action):
    """Run `action(parent_fd, name, path)` on the recorded backup directory; a missing one is not an error."""
    root = m["backup_dir"]
    pfd = helper.open_directory_chain(os.path.dirname(root), own_from=helper.home())
    if pfd is None:
        return
    try:
        try:
            os.stat(os.path.basename(root), dir_fd=pfd, follow_symlinks=False)
        except FileNotFoundError:
            return
        action(pfd, os.path.basename(root), root)
    finally:
        os.close(pfd)


def uninstall_plan(active_check=True, retain_adapter=False):
    """(record, actions, 0) when everything may be acted on, (None, None, 0) when nothing is recorded, else
    (None, None, exit code) with the reason printed. Changes nothing."""
    try:
        m = helper.read_marker()
        if m is None:
            return None, None, 0
        check_footprint(m)
        if active_check and active_session():
            return None, None, fail("the plugin is active: disable it first")
        if any(e["existed"] for e in m["files"]):
            check_private_root(m["backup_dir"])
        with_backup_root(m, lambda pfd, name, path: validate_tree(pfd, name, path))
        actions, conflicts = [], []
        for e in m["files"]:
            if retain_adapter and e["path"] in helper.adapter_paths():
                actions.append(("keep", None))
                continue
            try:
                actions.append(plan_entry(m, e))
            except Conflict as c:
                actions.append(("keep", None))
                conflicts.append("%s (%s)" % (e["path"], c))
    except PluginError as e:
        return None, None, fail(str(e))
    except OSError as e:
        return None, None, fail("cannot inspect the installation: %s" % (e.strerror or type(e).__name__))
    if conflicts:
        return None, None, fail("footprint left intact; resolve these first: " + "; ".join(conflicts))
    return m, actions, 0


def uninstall_preflight(active_check=True):
    """(record, 0) when everything may be acted on; (None, 0) when nothing is recorded; else (None, exit code). Changes
    nothing."""
    m, _, rc = uninstall_plan(active_check)
    return m, rc


def uninstall(active_check=True, retain_adapter=False):
    """Restore every recorded file, then drop the record and the product backup tree. Already restored (nothing
    recorded) is a successful no-op. `active_check=False` is for the session's own backend, which has already stopped
    its children and closed its control socket."""
    m, actions, rc = uninstall_plan(active_check, retain_adapter)
    if m is None:
        return rc
    try:
        if any(e["existed"] for e in m["files"]):
            private_root(m["backup_dir"])
        for e, (action, payload) in zip(m["files"], actions):
            p = e["path"]
            if action == "restore":
                restore_atomic(m, p)
            elif action in ("remove", "delete"):
                remove_installed(p)
            elif action == "write":
                cur = leaf_info(p)
                write_text(p, payload, cur[1] if cur else 0o644)
        # Empty entries durably mark completed restoration without changing the v1 schema. A crash during tree cleanup
        # retries only that cleanup; it never needs originals that may already have been removed.
        m["files"] = [e for e in m["files"] if retain_adapter and e["path"] in helper.adapter_paths()]
        helper.write_state_json(helper.marker_path(), m)
        if m["files"]:
            return 0
        with_backup_root(m, lambda pfd, name, path: remove_tree(pfd, name, path))
        helper.remove_state(helper.marker_path())
    except PluginError as e:
        return fail(str(e))
    except OSError as e:
        return fail("cannot restore the installation: %s" % (e.strerror or type(e).__name__))
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


def cache_dir():
    return os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.join(helper.home(), ".cache"), "waybar-notch")


def cache_root(create):
    cache = cache_dir()
    fd = helper.open_directory_chain(cache, own_from=helper.trust_root(cache, default=cache), create=create)
    return cache, fd


def cache_prepare():
    """Fail closed on links, foreign owners and group/other-writable cache directories or leaves before the installer
    reads, downloads, builds or stages anything there. Absent directories are created 0700."""
    try:
        cache = cache_dir()
        own = helper.trust_root(cache, default=cache)
        for sub in ("", "source-target-v1", "source-target-v1/release", "prebuilt"):
            path = os.path.join(cache, sub) if sub else cache
            create = sub != "source-target-v1/release"
            fd = helper.open_directory_chain(path, own_from=own, create=create)
            if fd is not None:
                os.close(fd)
        for rel in ("source-built-v1.sha256", "source-target-v1/release/waybar-notch", "prebuilt/waybar-notch"):
            path = os.path.join(cache, rel)
            dfd = helper.open_directory_chain(os.path.dirname(path), own_from=own)
            if dfd is None:
                continue
            try:
                try:
                    fd = helper.open_leaf(dfd, os.path.basename(path), path, state=True)
                except FileNotFoundError:
                    continue
                os.close(fd)
            finally:
                os.close(dfd)
    except PluginError as e:
        return fail("unsafe build cache: " + str(e))
    except OSError as e:
        return fail("cannot prepare the build cache: %s" % (e.strerror or type(e).__name__))
    return 0


def cache_stamp(value):
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        return fail("the cache stamp must be 64 lowercase hex characters")
    try:
        cache, dfd = cache_root(False)
        if dfd is None:
            return fail("the build cache is missing")
        try:
            name = "source-built-v1.sha256"
            try:
                os.close(helper.open_leaf(dfd, name, os.path.join(cache, name), state=True))
            except FileNotFoundError:
                pass
            write_private(dfd, name, (value + "\n").encode(), 0o600)
        finally:
            os.close(dfd)
    except PluginError as e:
        return fail("unsafe build cache: " + str(e))
    except OSError as e:
        return fail("cannot write the cache stamp: %s" % (e.strerror or type(e).__name__))
    return 0


def main(argv):
    if argv == ["cache-prepare"]:
        return cache_prepare()
    if argv[:1] == ["cache-stamp"] and len(argv) == 2:
        return cache_stamp(argv[1])
    if argv[:1] == ["begin"] and len(argv) > 1:
        return begin(argv[1:])
    if argv[:1] == ["prepare"] and len(argv) == 4:
        return prepare(argv[1], argv[2], argv[3])
    if argv[:1] == ["publish"] and len(argv) == 2:
        return publish(argv[1])
    if argv[:1] == ["finish"]:
        return finish(argv[1:] or None)
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
