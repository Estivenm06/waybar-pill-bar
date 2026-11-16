"""WhatsApp adapter patcher + install record + the real Quickshell content host (run: python3 -m unittest tests/test_widget_adapters.py).
Everything lives in temporary HOME/XDG/runtime directories; the live desktop's plugins are never touched."""
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ADAPTERS = os.path.join(ROOT, "files", "setup", "widget-adapters.py")
STATEPY = os.path.join(ROOT, "files", "setup", "plugin-state.py")
SHELL_DIR = "/usr/share/omarchy/shell"

FIXTURES = os.path.join(ROOT, "tests", "fixtures")
STOCK = open(os.path.join(FIXTURES, "whatsapp-1.1.0", "Panel.qml"), encoding="utf-8").read()  # stock 1.1.0, MIT (see LICENSE there)

# The previous, placement-only managed blocks (what an earlier Pill Bar installed), reproduced to test migration.
OLD_PROPERTIES = """  // >>> waybar-pill-bar anchor properties BEGIN
  WaybarPillAnchor { id: pillAnchor; nativeAnchor: root.anchorItem; nativeBar: root.bar; owner: root }
  // <<< waybar-pill-bar anchor properties END
"""
OLD_OPEN = """  // >>> waybar-pill-bar anchor open BEGIN
  function open() { pillAnchor.active = false; root.controller.show() }
  // <<< waybar-pill-bar anchor open END
"""
OLD_KEYBOARD = """    // >>> waybar-pill-bar anchor keyboard BEGIN
    id: panel
    anchorItem: pillAnchor.active ? pillAnchor : root.anchorItem
    owner: root.hostWidget || root
    bar: pillAnchor.active ? pillAnchor.panelBar : root.bar
    open: root.opened
    gap: Style.gapsOut + (pillAnchor.active && pillAnchor.nativeWindow ? pillAnchor.barBottom - pillAnchor.nativeWindow.height : 0)
    // <<< waybar-pill-bar anchor keyboard END
"""
OLD_ADAPTER = "// placement-only adapter of an earlier waybar-pill-bar\nimport QtQuick\nItem {}\n"


def previous_version_panel(stock=STOCK):
    out = stock.replace('  property string pluginDir: ""\n', '  property string pluginDir: ""\n' + OLD_PROPERTIES, 1)
    out = out.replace("  function open() { root.controller.show() }\n", OLD_OPEN, 1)
    return out.replace("    id: panel\n    anchorItem: root.anchorItem\n    owner: root.hostWidget || root\n"
                       "    bar: root.bar\n    open: root.opened\n", OLD_KEYBOARD, 1)


def load_adapters():
    spec = importlib.util.spec_from_file_location("widget_adapters", ADAPTERS)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class Base(unittest.TestCase):
    def setUp(self):
        sys.dont_write_bytecode = True
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.cfg = os.path.join(self.home, ".config")
        self.plugin = os.path.join(self.cfg, "omarchy", "plugins", "io.github.ricky.whatsapp")
        self.panel = os.path.join(self.plugin, "Panel.qml")
        self.anchor = os.path.join(self.plugin, "WaybarPillAnchor.qml")
        self.m = load_adapters()
        # the module reads the install record from the environment: keep it inside the temporary HOME
        for k, v in (("HOME", self.home), ("XDG_STATE_HOME", os.path.join(self.home, ".local", "state")),
                     ("XDG_CONFIG_HOME", self.cfg)):
            old = os.environ.get(k)
            os.environ[k] = v
            self.addCleanup(lambda k=k, old=old: os.environ.pop(k, None) if old is None else os.environ.__setitem__(k, old))
        old_umask = os.umask(0o022)
        self.addCleanup(os.umask, old_umask)

    def install_panel(self, text=STOCK):
        os.makedirs(self.plugin, exist_ok=True)
        with open(self.panel, "w") as f:
            f.write(text)

    def read(self, p):
        with open(p) as f:
            return f.read()

    def env(self):
        rt = os.path.join(self.tmp, "rt")
        os.makedirs(rt, mode=0o700, exist_ok=True)
        return dict(os.environ, HOME=self.home, XDG_CONFIG_HOME=self.cfg, XDG_RUNTIME_DIR=rt,
                    XDG_STATE_HOME=os.path.join(self.home, ".local", "state"))

    def cli(self, script, *args):
        return subprocess.run([sys.executable, script, *args], env=self.env(), capture_output=True, text=True)

    def write_plan(self):
        plan = self.m.planned_whatsapp_adapter(self.cfg)
        for p, c in plan.items():
            with open(p, "w") as f:
                f.write(c)
        return plan

    def full_install(self):
        paths = self.cli(ADAPTERS, "paths").stdout.split()
        for step in (("begin", *paths), ("finish",)):
            if step[0] == "begin":
                self.assertEqual(self.cli(STATEPY, *step).returncode, 0)
                r = self.cli(ADAPTERS, "apply")
                self.assertEqual(r.returncode, 0, r.stderr)
            else:
                self.assertEqual(self.cli(STATEPY, *step).returncode, 0)
        return paths


class PatcherTests(Base):
    def test_patch_preserves_unmanaged_content_and_is_stable(self):
        self.install_panel(STOCK + "// user addition\n")
        plan = self.m.planned_whatsapp_adapter(self.cfg)
        self.assertEqual(sorted(plan), sorted([self.panel, self.anchor]))
        out = plan[self.panel]
        self.assertIn("// user addition", out)
        self.assertIn("function open() { pillAnchor.releaseForNative(); root.controller.show() }", out)
        self.assertIn("open: root.opened && !pillAnchor.active", out)
        self.assertIn("parent: pillAnchor.active ? pillAnchor.contentItem : pillNativeHolder", out)
        for stale in ("pillAnchor.panelBar", "barBottom", "openAt"):
            self.assertNotIn(stale, out)
        self.assertEqual(out.count("Style.font.body"), 1)  # only the native fallback inside the managed token block
        # re-patching the patched Panel (what a reinstall computes) changes nothing, user addition included
        self.assertEqual(self.m.patch_panel(out, "p"), out)
        self.assertEqual(self.m.patch_panel(out + "// later edit\n", "p"), out + "// later edit\n")

    def test_previous_placement_only_install_migrates_to_the_same_result(self):
        self.install_panel(previous_version_panel() + "// user addition\n")
        migrated = self.m.planned_whatsapp_adapter(self.cfg)[self.panel]
        self.install_panel(STOCK + "// user addition\n")
        self.assertEqual(migrated, self.m.planned_whatsapp_adapter(self.cfg)[self.panel])

    def test_absent_whatsapp_changes_nothing(self):
        self.assertEqual(self.m.planned_whatsapp_adapter(self.cfg), {})
        env = self.env()
        for cmd in ("paths", "apply"):
            r = subprocess.run([sys.executable, ADAPTERS, cmd], env=env, capture_output=True, text=True)
            self.assertEqual((r.returncode, r.stdout, r.stderr), (0, "", ""))
        self.assertFalse(os.path.exists(os.path.join(self.cfg, "omarchy")))

    def refused(self, panel_text, why):
        self.install_panel(panel_text)
        before = self.read(self.panel)
        r = self.cli(ADAPTERS, "apply")
        self.assertNotEqual(r.returncode, 0, why)
        self.assertEqual(self.read(self.panel), before, why)
        self.assertFalse(os.path.exists(self.anchor), why)
        return r

    def test_changed_or_ambiguous_panels_are_refused_before_writing(self):
        r = self.refused(STOCK.replace("  function open() { root.controller.show() }\n", ""), "missing anchor")
        self.assertIn("anchor", r.stderr)
        self.refused(STOCK.replace('  property string pluginDir: ""\n', '  property string pluginDir: ""\n  property string pluginDir: ""\n'), "ambiguous anchor")
        self.refused(STOCK.replace("PanelSeparator { foreground: root.foreground }", "PanelSeparator {  foreground: root.foreground }"), "moved structure")
        self.refused(STOCK + "  property int x: Style.font.body\n", "a stock token outside the known count")
        r = self.refused(STOCK.replace("Style.font.caption", "root.pillFont.caption", 1), "half-patched tokens")
        self.assertIn("design tokens", r.stderr)

    def test_malformed_managed_markers_are_refused_before_writing(self):
        patched = self.m.patch_panel(STOCK, "p")
        begin = "// >>> waybar-pill-bar anchor open BEGIN\n"
        end = "// <<< waybar-pill-bar anchor open END\n"
        close_begin = "// >>> waybar-pill-bar anchor close BEGIN\n"
        cases = {
            "missing end": patched.replace(end, "", 1),
            "missing begin": patched.replace(begin, "", 1),
            "duplicate": patched.replace(begin, begin + begin, 1),
            "nested": patched.replace(begin, close_begin + begin, 1),
            "crossed": patched.replace(end, "// >>> waybar-pill-bar anchor close BEGIN\n" + end, 1),
            "unknown": patched + "// >>> waybar-pill-bar anchor mystery BEGIN\n// <<< waybar-pill-bar anchor mystery END\n",
        }
        for why, text in cases.items():
            self.refused(text, why)

    def test_symlinked_panel_and_adapter_are_refused(self):
        self.install_panel()
        real = os.path.join(self.tmp, "real.qml")
        os.rename(self.panel, real)
        os.symlink(real, self.panel)
        self.assertNotEqual(self.cli(ADAPTERS, "apply").returncode, 0)
        self.assertTrue(os.path.islink(self.panel))
        os.unlink(self.panel)
        self.install_panel()
        os.symlink(real, self.anchor)
        self.assertNotEqual(self.cli(ADAPTERS, "apply").returncode, 0)
        self.assertEqual(self.read(self.panel), STOCK)

    def test_unrecorded_adapter_files_are_collisions_even_with_the_management_comment(self):
        self.install_panel()
        for content in ("// somebody else's file\n", "// waybar-pill-bar anchor adapter\nItem {}\n", self.read(os.path.join(ROOT, "files", "qml", "WaybarPillAnchor.qml"))):
            with open(self.anchor, "w") as f:
                f.write(content)
            for cmd in ("paths", "apply"):
                r = self.cli(ADAPTERS, cmd)
                self.assertNotEqual(r.returncode, 0, content[:20])
                self.assertIn("not a Pill Bar adapter", r.stderr)
            self.assertEqual(self.read(self.anchor), content)
            self.assertEqual(self.read(self.panel), STOCK)


class InstallRecordTests(Base):
    def test_record_restores_the_original_panel_and_removes_the_adapter(self):
        self.install_panel()
        paths = self.cli(ADAPTERS, "paths").stdout.split()
        self.assertEqual(sorted(paths), sorted([self.panel, self.anchor]))
        self.full_install()
        self.assertEqual(oct(os.stat(self.anchor).st_mode & 0o777), "0o644")
        patched = self.read(self.panel)
        self.assertNotEqual(patched, STOCK)
        # a reinstall through the record is byte-stable
        self.assertEqual(self.cli(STATEPY, "begin", *paths).returncode, 0)
        self.assertEqual(self.cli(ADAPTERS, "apply").returncode, 0)
        self.assertEqual(self.read(self.panel), patched)
        self.assertEqual(self.cli(STATEPY, "finish").returncode, 0)
        self.assertEqual(self.cli(STATEPY, "uninstall").returncode, 0)
        self.assertEqual(self.read(self.panel).encode(), STOCK.encode())
        self.assertFalse(os.path.exists(self.anchor))

    def test_upgrade_from_the_placement_only_version_keeps_the_original_as_baseline(self):
        self.install_panel()
        paths = [self.panel, self.anchor]
        # the earlier Pill Bar: begin (backs up the stock Panel), install its three-block Panel + adapter, finish
        self.assertEqual(self.cli(STATEPY, "begin", *paths).returncode, 0)
        self.install_panel(previous_version_panel())
        with open(self.anchor, "w") as f:
            f.write(OLD_ADAPTER)
        self.assertEqual(self.cli(STATEPY, "finish").returncode, 0)
        self.full_install()
        self.assertEqual(self.read(self.panel), self.m.patch_panel(STOCK, "p"))
        self.assertEqual(self.read(self.anchor), self.read(os.path.join(ROOT, "files", "qml", "WaybarPillAnchor.qml")))
        self.assertEqual(self.cli(STATEPY, "uninstall").returncode, 0)
        self.assertEqual(self.read(self.panel).encode(), STOCK.encode(), "the original, not the placement-only Panel")
        self.assertFalse(os.path.exists(self.anchor))

    def test_edited_recorded_targets_are_refused_by_reinstall_and_by_direct_apply(self):
        self.install_panel()
        paths = self.full_install()
        for target, edit in ((self.panel, "// edited after install\n"), (self.anchor, "// edited adapter\n")):
            original = self.read(target)
            with open(target, "a") as f:
                f.write(edit)
            edited = self.read(target)
            r = self.cli(STATEPY, "begin", *paths)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("modified", r.stderr)
            r = self.cli(ADAPTERS, "apply")
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("modified", r.stderr)
            self.assertEqual(self.read(target), edited)
            self.assertNotEqual(self.cli(STATEPY, "uninstall").returncode, 0)
            with open(target, "w") as f:
                f.write(original)  # the user reverts: everything works again
            self.assertEqual(self.cli(STATEPY, "begin", *paths).returncode, 0)
            self.assertEqual(self.cli(ADAPTERS, "apply").returncode, 0)
            self.assertEqual(self.cli(STATEPY, "finish").returncode, 0)


class PrivateAdapterTests(Base):
    SECRET = "// synthetic private comment, not for other accounts\n"

    def test_private_panel_modes_survive_apply_reapply_and_uninstall(self):
        for mode in (0o600, 0o640):
            with self.subTest(mode=oct(mode)):
                self.install_panel(STOCK + self.SECRET)
                os.chmod(self.panel, mode)
                original = self.read(self.panel)
                paths = self.cli(ADAPTERS, "paths").stdout.split()
                for _ in range(2):  # apply, then reapply through the record
                    self.assertEqual(self.cli(STATEPY, "begin", *paths).returncode, 0)
                    r = self.cli(ADAPTERS, "apply")
                    self.assertEqual(r.returncode, 0, r.stderr)
                    self.assertEqual(self.cli(STATEPY, "finish").returncode, 0)
                    self.assertEqual(os.stat(self.panel).st_mode & 0o7777, mode)
                    self.assertIn(self.SECRET, self.read(self.panel))
                    self.assertEqual(os.stat(self.anchor).st_mode & 0o7777, 0o644)
                self.assertEqual([n for n in os.listdir(self.plugin) if n.startswith(".pill-adapter-")], [])
                self.assertEqual(self.cli(STATEPY, "uninstall").returncode, 0)
                self.assertEqual(self.read(self.panel), original)
                self.assertEqual(os.stat(self.panel).st_mode & 0o7777, mode)
                self.assertFalse(os.path.exists(self.anchor))

    def state_path(self):
        return os.path.join(self.home, ".local", "state", "waybar-notch", "plugin-install.json")

    def test_malformed_or_untrusted_records_block_paths_and_apply(self):
        self.install_panel()
        os.chmod(self.panel, 0o600)
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write("keep")
        good = {"version": 1, "mode": "plugin", "files": [],
                "backup_dir": os.path.join(self.home, ".config", "waybar", "backup-20260101-000000")}
        records = {
            "garbage": "{nope",
            "outside path": dict(good, files=[{"path": sentinel, "existed": False, "installed_sha256": ""}]),
            "relative path": dict(good, files=[{"path": "Panel.qml", "existed": False, "installed_sha256": ""}]),
            "backup naming home": dict(good, backup_dir=self.home),
            "no files": {"version": 1, "mode": "plugin"},
            "duplicate": dict(good, files=[{"path": self.anchor, "existed": False, "installed_sha256": ""}] * 2),
        }
        os.makedirs(os.path.dirname(self.state_path()))
        for why, rec in records.items():
            with self.subTest(why):
                with open(self.state_path(), "w") as f:
                    f.write(rec if isinstance(rec, str) else json.dumps(rec))
                for cmd in ("paths", "apply"):
                    r = self.cli(ADAPTERS, cmd)
                    self.assertNotEqual(r.returncode, 0, (why, cmd))
                    self.assertNotIn(sentinel, r.stdout)
                self.assertEqual(self.read(self.panel), STOCK)
                self.assertEqual(os.stat(self.panel).st_mode & 0o777, 0o600)
                self.assertFalse(os.path.exists(self.anchor))
        os.unlink(self.state_path())
        os.symlink(sentinel, self.state_path())
        for cmd in ("paths", "apply"):
            self.assertNotEqual(self.cli(ADAPTERS, cmd).returncode, 0)
        self.assertEqual(self.read(self.panel), STOCK)

    def test_unsafe_plugin_directory_and_foreign_config_roots_are_refused(self):
        self.install_panel()
        os.chmod(self.plugin, 0o775)
        for cmd in ("paths", "apply"):
            self.assertNotEqual(self.cli(ADAPTERS, cmd).returncode, 0)
        self.assertEqual(self.read(self.panel), STOCK)
        self.assertFalse(os.path.exists(self.anchor))
        os.chmod(self.plugin, 0o755)
        with self.assertRaises(self.m.AdapterError):
            self.m.planned_whatsapp_adapter(os.path.join(self.tmp, "other-config"))
        self.assertEqual(self.cli(ADAPTERS, "apply").returncode, 0)

    def test_custom_xdg_config_root_works_with_a_trusted_ancestry(self):
        cfg = os.path.join(self.tmp, "xdg-config")
        plugin = os.path.join(cfg, "omarchy", "plugins", "io.github.ricky.whatsapp")
        os.makedirs(plugin)
        with open(os.path.join(plugin, "Panel.qml"), "w") as f:
            f.write(STOCK)
        env = dict(self.env(), XDG_CONFIG_HOME=cfg)
        run = lambda script, *a: subprocess.run([sys.executable, script, *a], env=env, capture_output=True, text=True)
        paths = run(ADAPTERS, "paths").stdout.split()
        self.assertEqual(sorted(paths), sorted([os.path.join(plugin, "Panel.qml"), os.path.join(plugin, "WaybarPillAnchor.qml")]))
        self.assertEqual(run(STATEPY, "begin", *paths).returncode, 0)
        self.assertEqual(run(ADAPTERS, "apply").returncode, 0)
        self.assertEqual(run(STATEPY, "finish").returncode, 0)
        self.assertEqual(run(STATEPY, "uninstall").returncode, 0)
        with open(os.path.join(plugin, "Panel.qml")) as f:
            self.assertEqual(f.read(), STOCK)


def real_session():
    sock = os.path.join(os.environ.get("XDG_RUNTIME_DIR", ""), os.environ.get("WAYLAND_DISPLAY", ""))
    ok = shutil.which("qs") and shutil.which("dbus-run-session") and os.path.isdir(SHELL_DIR) \
        and os.environ.get("WAYLAND_DISPLAY") and os.path.exists(sock)
    return sock if ok else None


def png(path, size=200):
    import struct
    import zlib
    raw = b"".join(b"\x00" + bytes([40, 40, 40]) * size for _ in range(size))

    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


class NotchServer:
    """The notch's side of notch-content.sock, driven by the test: NDJSON frames in, frames out."""

    def __init__(self, path):
        import socket
        import threading
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.bind(path)
        self.sock.listen(4)
        self.conn = None
        self.session = 0
        self.frames = []
        self.cv = threading.Condition()
        self.alive = True
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        import threading
        while self.alive:
            try:
                c, _ = self.sock.accept()
            except OSError:
                return
            with self.cv:
                self.session += 1
                self.conn = c
                self.cv.notify_all()
            threading.Thread(target=self.read, args=(c, self.session), daemon=True).start()

    def read(self, c, session):
        buf = b""
        while True:
            try:
                data = c.recv(4096)
            except OSError:
                data = b""
            with self.cv:
                if not data:
                    self.frames.append({"op": "<eof>", "_s": session})
                    self.cv.notify_all()
                    return
                buf += data
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    f = json.loads(line)
                    f["_s"] = session
                    self.frames.append(f)
                self.cv.notify_all()

    def send(self, frame):
        self.conn.sendall((json.dumps(frame) + "\n").encode())

    def wait(self, op, timeout=10):
        end = time.time() + timeout
        with self.cv:
            while True:
                for i, f in enumerate(self.frames):
                    if f["op"] == op:
                        return self.frames.pop(i)
                left = end - time.time()
                if left <= 0:
                    raise AssertionError("no %s frame; got %s" % (op, self.frames))
                self.cv.wait(left)

    def pending(self, op):
        with self.cv:
            return [f for f in self.frames if f["op"] == op]

    def hang_up(self):
        try:
            self.conn.shutdown(2)
        except OSError:
            pass
        self.conn.close()

    def close(self):
        self.alive = False
        self.sock.close()


THEME = {"background": "#101820", "foreground": "#e0e8f0", "foreground_hi": "#ffffff", "accent": "#4fa3ff",
         "alert": "#ff4d4d", "font_family": "Monospace", "font_size": 12, "radius": 22, "animations": True}


@unittest.skipUnless(real_session(), "needs quickshell, dbus-run-session, the omarchy shell and a Wayland session")
class HostTests(Base):
    """The actual patched WhatsApp Panel + the real adapter inside real Quickshell on the real compositor, with a synthetic
    client and a temporary notch server. Nothing here touches the live desktop's plugins, bus or daemon."""

    def setUp(self):
        super().setUp()
        cfg = os.path.join(self.tmp, "qs")
        os.makedirs(cfg)
        for d in ("Ui", "Commons"):
            os.symlink(os.path.join(SHELL_DIR, d), os.path.join(cfg, d))
        host = os.path.join(FIXTURES, "host")
        shutil.copy(os.path.join(FIXTURES, "whatsapp-1.1.0", "Model.js"), cfg)
        shutil.copy(os.path.join(ROOT, "files", "qml", "WaybarPillAnchor.qml"), cfg)
        for n in ("FakeClient.qml", "shell.qml"):
            shutil.copy(os.path.join(host, n), cfg)
        panel = self.m.patch_panel(STOCK, "Panel.qml").rstrip()
        assert panel.endswith("}")
        with open(os.path.join(host, "probe.qml.inc")) as f:
            probe = f.read()
        with open(os.path.join(cfg, "Panel.qml"), "w") as f:
            f.write(panel[:-1] + probe + "}\n")
        rt = os.path.join(self.tmp, "rt")
        os.makedirs(os.path.join(rt, "waybar-pill-plugin"), mode=0o700, exist_ok=True)
        self.qr = os.path.join(self.tmp, "qr.png")
        png(self.qr)
        self.srv = NotchServer(os.path.join(rt, "waybar-pill-plugin", "notch-content.sock"))
        self.addCleanup(self.srv.close)
        self.cfgdir = cfg
        self.qenv = {"PATH": os.environ["PATH"], "HOME": os.environ.get("QS_HOME") or os.path.join(self.tmp, "home"), "XDG_RUNTIME_DIR": rt,
                     "WAYLAND_DISPLAY": real_session(), "LANG": "C.UTF-8"}
        os.makedirs(self.qenv["HOME"], exist_ok=True)
        self.proc = subprocess.Popen(["dbus-run-session", "--", "qs", "-p", cfg, "-n"], env=self.qenv,
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
        self.addCleanup(self.stop)
        deadline = time.time() + 30
        while True:
            r = self.ipc("probe", "out")
            if r.returncode == 0 and r.stdout.strip():
                break
            if time.time() > deadline or self.proc.poll() is not None:
                self.fail("quickshell fixture did not start")
            time.sleep(0.3)
        self.output = r.stdout.strip()
        self.target = "waybar-pill-whatsapp-" + self.output
        self.act("qr", self.qr)
        self.serial = 0

    def stop(self):
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except OSError:
            pass
        self.proc.wait(10)
        if os.environ.get("QS_DEBUG"):
            print(self.proc.stdout.read().decode(errors="replace")[-3000:])
        self.proc.stdout.close()

    def ipc(self, target, fn, *args):
        return subprocess.run(["qs", "-p", self.cfgdir, "ipc", "call", target, fn, *map(str, args)], env=self.qenv,
                              capture_output=True, text=True, timeout=15)

    def act(self, name, arg="-"):
        r = self.ipc("probe", "act", name, arg)
        self.assertEqual(r.stdout.strip(), "ok", r.stdout + r.stderr)

    def state(self):
        r = self.ipc("probe", "state")
        try:
            return json.loads(r.stdout)
        except ValueError:
            self.fail("probe state failed: %r %r" % (r.stdout, r.stderr))

    def until(self, pred, why, timeout=8):
        end = time.time() + timeout
        while True:
            s = self.state()
            if pred(s):
                return s
            if time.time() > end:
                self.fail("%s; state=%s" % (why, json.dumps(s)[:1500]))
            time.sleep(0.1)

    def toggle(self):
        r = self.ipc(self.target, "toggleNotch")
        self.assertEqual(r.stdout.strip(), "ok", r.stdout + r.stderr)

    def open_page(self, rect=(100, 40, 488, 420), size=12):
        self.toggle()
        f = self.srv.wait("open")
        self.assertEqual((f["version"], f["output"]), (1, self.output))
        self.serial = 0
        return self.layout(rect, size)

    def layout(self, rect, size=12, suspend=False):
        if suspend:
            self.srv.send({"op": "suspend", "session": self.srv.session})
            self.until(lambda s: not s["surfaceVisible"], "the surface did not hide on suspend")
        self.serial += 1
        x, y, w, h = rect
        self.srv.send({"op": "layout", "version": 1, "session": self.srv.session, "serial": self.serial, "output": self.output,
                       "rect": {"x": x, "y": y, "width": w, "height": h}, "theme": dict(THEME, font_size=size)})
        m = self.srv.wait("mapped")
        self.assertEqual((m["session"], m["serial"]), (self.srv.session, self.serial))
        return self.until(lambda s: s["surfaceMapped"] and s["kcInExternalWindow"], "the content did not reach the external window")

    def hide_page(self):
        self.srv.send({"op": "hide", "session": self.srv.session, "reason": "test"})
        self.srv.wait("hidden")
        self.srv.hang_up()
        return self.until(lambda s: not s["active"] and not s["surfaceVisible"], "the page did not tear down")

    def test_layout_hosts_the_original_content_in_the_slot_with_the_notch_theme(self):
        s0 = self.state()
        self.assertFalse(s0["active"] or s0["surfaceVisible"])
        s = self.open_page()
        self.assertTrue(s["active"] and s["kcUnderHolder"] and s["kcInExternalWindow"])
        self.assertFalse(s["nativeOpen"] or s["nativeMapped"], "the stock card stays unmapped")
        self.assertTrue(s["opened"], "the Panel's own lifecycle still runs")
        self.assertEqual(s["holder"], {"x": 100, "y": 40, "w": 488, "h": 420})
        self.assertEqual(s["mask"], {"x": 100, "y": 40, "w": 488, "h": 420}, "input is confined to the slot")
        self.assertEqual((s["fg"], s["font"], s["body"], s["caption"]), ("#e0e8f0", "Monospace", 12, 11))
        self.assertEqual(s["accent"], "#4fa3ff")
        self.hide_page()
        s = self.state()
        self.assertTrue(s["kcParentNative"] and not s["kcUnderHolder"], "the content returns to its native holder")
        self.assertFalse(s["opened"])

    def test_every_view_fits_or_is_reachable_at_notch_sizes_and_font_sizes(self):
        self.open_page()
        sizes = [((100, 40, 488, 420), 12), ((20, 10, 288, 320), 12), ((20, 10, 288, 180), 12), ((100, 40, 488, 420), 18),
                 ((20, 10, 288, 180), 18)]
        views = ("many", "convo", "login", "loginqr", "offline", "empty")
        for n, (rect, size) in enumerate(sizes):
            if n:
                self.layout(rect, size, suspend=True)
            for view in views:
                with self.subTest(rect=rect, size=size, view=view):
                    self.act("scenario", view)
                    if view == "convo":
                        self.act("openChat", "j1@s.whatsapp.net")
                    self.until(lambda s: (s["view"] == "chat") == (view == "convo"), "view did not settle")
                    time.sleep(0.25)
                    s = self.state()
                    self.assertLessEqual(s["overflowX"], 1.0, "text sticks out horizontally")
                    self.assertEqual((s["vp"]["w"], s["vp"]["h"]), (rect[2], rect[3]))
                    self.assertTrue(s["vp"]["ch"] <= s["vp"]["h"] + 1 or s["vp"]["interactive"],
                                    "content taller than the slot must scroll: %s" % s["vp"])
                    self.assertEqual(s["body"], size)
                    if view == "convo":
                        self.act("scrollEnd")
                        c = self.state()["composer"]
                        self.assertTrue(c["visible"] and c["y"] >= -1 and c["y"] + c["h"] <= rect[3] + 1,
                                        "the composer must be reachable: %s" % c)
                        if rect[3] >= 420 and size == 12:
                            self.act("scroll", "0")
                            s = self.state()
                            self.assertTrue(s["composer"]["y"] + s["composer"]["h"] <= rect[3] + 1, "composer visible unscrolled")
                            self.assertLess(s["msgList"]["h"], s["msgList"]["ch"], "messages scroll, not the composer")
                    if view == "many":
                        self.assertTrue(s["chatList"]["interactive"], "the long chat list scrolls")
                    if view == "loginqr":
                        q = s["qr"]
                        self.assertEqual(q["w"], q["h"], "the QR keeps its aspect")
                        self.assertLessEqual(q["w"], rect[2])
                    self.act("scroll", "0")
        self.hide_page()

    def test_modals_stay_inside_the_slot_and_the_picker_scrolls_the_selected_row_into_view(self):
        self.open_page((20, 10, 288, 220))
        self.act("scenario", "picker")
        self.until(lambda s: s["view"] == "chats", "chats")
        self.act("picker")
        s = self.until(lambda s: s["picker"]["open"], "the list picker did not open")
        self.assertLessEqual(s["picker"]["h"], 220)
        self.assertTrue(s["picker"]["interactive"], "8 long lists do not fit; the picker list scrolls")
        self.act("pickerIndex", "7")
        s = self.until(lambda s: s["picker"]["y"] > 0, "the selected row was not scrolled into view")
        self.assertEqual(s["picker"]["idx"], 7)
        self.act("toggleMember", "7")
        calls = [c for c in self.state()["calls"] if c["name"] == "setListMember"]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["args"][1], "l8")
        self.act("closePicker")
        self.until(lambda s: not s["picker"]["open"], "picker still open")
        self.act("logout")
        s = self.until(lambda s: s["logout"]["open"], "logout confirmation did not open")
        self.assertGreaterEqual(s["logout"]["ch"], 320, "a minimum canvas keeps Cancel/Confirm reachable")
        self.act("cancelLogout")
        s = self.until(lambda s: not s["logout"]["open"], "logout still open")
        self.assertEqual([c for c in s["calls"] if c["name"] == "logout"], [], "cancel never logs out")
        self.act("peek", self.qr)
        s = self.until(lambda s: s["peek"]["active"], "peek")
        self.assertTrue(s["peek"]["embedded"], "the preview is embedded in the slot")
        self.assertFalse(s["peek"]["nativeWin"], "no fullscreen peek surface in the notch")
        self.act("peek", "")
        self.hide_page()

    def test_draft_and_thread_survive_relayout_and_escape_back_follow_the_stock_semantics(self):
        self.open_page()
        self.act("scenario", "convo")
        self.act("openChat", "j3@s.whatsapp.net")
        self.until(lambda s: s["view"] == "chat", "chat")
        self.act("draft", "half a thought")
        self.layout((60, 30, 400, 380), 14, suspend=True)
        s = self.until(lambda s: s["view"] == "chat" and s["draft"] == "half a thought", "draft or thread lost")
        self.assertEqual(s["jid"], "j3@s.whatsapp.net")
        self.act("send")
        sends = [c for c in self.state()["calls"] if c["name"] == "sendMessage"]
        self.assertEqual(len(sends), 1)
        self.assertEqual(sends[0]["args"], ["j3@s.whatsapp.net", "half a thought"])
        self.act("escape")   # conversation -> list: stays in the notch
        self.until(lambda s: s["view"] == "chats", "escape should leave the conversation")
        time.sleep(0.2)
        self.assertEqual(self.srv.pending("close"), [])
        self.act("back")     # explicit Back from the chat list returns to the notch's Home
        f = self.srv.wait("close")
        self.assertEqual((f["session"], f["home"]), (self.srv.session, True))
        self.hide_page()
        self.open_page()
        self.act("escape")   # Escape on the list closes the page: collapse, not Home
        self.assertEqual(self.srv.wait("close")["home"], False)
        self.hide_page()

    def test_native_open_while_embedded_restores_the_stock_card(self):
        self.open_page()
        self.act("nativeOpen")
        s = self.until(lambda s: not s["active"] and not s["surfaceVisible"] and s["nativeOpen"], "native open did not take over")
        self.assertFalse(s["kcUnderHolder"] or s["kcInExternalWindow"])
        self.assertTrue(s["kcParentNative"], "keyCatcher is back in the stock native holder")
        self.assertTrue(s["opened"])
        self.assertEqual(self.srv.wait("<eof>")["op"], "<eof>", "the notch connection was released")
        self.act("closePanel")
        self.until(lambda s: not s["opened"] and not s["nativeOpen"], "the native card did not close")

    def test_eof_malformed_and_stale_frames_never_leave_a_blocker(self):
        self.open_page()
        self.srv.hang_up()
        self.until(lambda s: not s["active"] and not s["surfaceVisible"] and not s["opened"], "EOF left the page up")
        self.toggle()
        self.srv.wait("open")
        self.srv.send({"op": "layout", "version": 1, "session": self.srv.session, "serial": 1, "output": self.output,
                       "rect": {"x": 100000, "y": 0, "width": 100, "height": 100}, "theme": THEME})
        s = self.until(lambda s: s["stage"] == "idle", "an off-screen layout was accepted")
        self.assertFalse(s["active"] or s["surfaceVisible"])
        self.open_page()
        self.srv.send({"op": "hide", "session": self.srv.session, "reason": "t"})
        self.srv.wait("hidden")
        self.srv.send({"op": "layout", "version": 1, "session": self.srv.session, "serial": 99, "output": self.output,
                       "rect": {"x": 10, "y": 10, "width": 100, "height": 100}, "theme": THEME})
        time.sleep(0.5)
        self.assertFalse(self.state()["surfaceVisible"], "a late layout must not remap a closing session")
        self.srv.hang_up()
        self.until(lambda s: not s["active"], "teardown")

    def test_a_second_toggle_closes_and_a_toggle_before_the_first_layout_cancels(self):
        self.toggle()
        self.srv.wait("open")
        self.toggle()  # no session id yet: cancels
        self.until(lambda s: s["stage"] == "idle" and not s["active"], "the early toggle did not cancel")
        self.open_page()
        self.toggle()
        self.assertEqual(self.srv.wait("close")["home"], False)
        self.hide_page()


if __name__ == "__main__":
    unittest.main()
