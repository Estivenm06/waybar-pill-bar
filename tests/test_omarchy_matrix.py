"""Omarchy 4.x compatibility matrix (run: python3 -m unittest tests/test_omarchy_matrix.py).
Covers catalog permutations of the bridge, the real Quickshell runtime loading Service.qml, and a sandboxed
install/uninstall cycle. Everything runs in a temporary HOME/XDG/runtime; desktop commands are faked."""
import hashlib
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_plugin_bridge import Base, Fixture, ROOT, SCRIPTS  # noqa: E402

STOCK = [{"id": "omarchy.notifications"}, {"id": "omarchy.osd"}, {"id": "omarchy.bar"}]
CLONES = [
    ("cloned notifications", [{"id": "luigiwonka.notifications", "clonedFrom": "omarchy.notifications"}],
     ["luigiwonka.notifications"]),
    ("cloned osd", [{"id": "me.osd", "clonedFrom": "omarchy.osd"}], ["me.osd"]),
    ("cloned bar is left alone", [{"id": "me.bar", "clonedFrom": "omarchy.bar"}], []),
    ("unrelated plugin is left alone", [{"id": "someone.weather"}], []),
]


class CatalogMatrix(Fixture):
    def activate_with(self, catalog, disabled=()):
        import test_plugin_bridge as tpb
        self.addCleanup(setattr, tpb, "CATALOG", tpb.CATALOG)
        tpb.CATALOG = STOCK + catalog
        self.set_shell({"disabledPlugins": list(disabled)})
        s = self.session()
        s.activate()
        return s

    def test_stock_omarchy_disables_notifications_and_osd_and_restores(self):
        s = self.activate_with([])
        self.assertCountEqual(self.get_shell()["disabledPlugins"], ["omarchy.notifications", "omarchy.osd"])
        self.assertTrue(os.path.exists(self.flag))  # native bar hidden
        s.cleanup()
        self.assertEqual(self.get_shell()["disabledPlugins"], [])
        self.assertFalse(os.path.exists(self.flag))

    def test_clone_variants(self):
        for label, extra, expect in CLONES:
            with self.subTest(label):
                self.procs.clear()
                for n in ("waybar-popupd.fifo", "waybar-notch"):
                    shutil.rmtree(os.path.join(self.rt, n), True)
                    if os.path.exists(os.path.join(self.rt, n)):
                        os.unlink(os.path.join(self.rt, n))
                self.set_shell({"disabledPlugins": []})
                if os.path.exists(self.flag):
                    os.unlink(self.flag)
                s = self.activate_with(extra)
                off = set(self.get_shell()["disabledPlugins"])
                self.assertLessEqual(set(expect), off)
                if not expect:
                    self.assertEqual(off, {"omarchy.notifications", "omarchy.osd"})
                s.cleanup()
                self.assertEqual(self.get_shell()["disabledPlugins"], [])
                for p in self.procs:
                    if p.poll() is None:
                        p.kill()
                        p.wait()

    def test_already_disabled_clone_stays_disabled(self):
        s = self.activate_with(CLONES[0][1], disabled=["luigiwonka.notifications"])
        s.cleanup()
        self.assertEqual(self.get_shell()["disabledPlugins"], ["luigiwonka.notifications"])


@unittest.skipUnless(shutil.which("quickshell"), "quickshell not installed")
class RealQuickshell(Base):
    def test_service_qml_runs_launcher_and_holds_lease_until_exit(self):
        scripts = os.path.join(self.home, ".config", "waybar", "scripts")
        os.makedirs(scripts)
        log = os.path.join(self.tmp, "lease.log")
        # stand-in backend: `start` forks a listener on the real control socket path, records the first line and EOF
        with open(os.path.join(scripts, "waybar-pill-plugin"), "w") as f:
            f.write('''import os, socket, sys
if sys.argv[1:] != ["start"]:
    sys.exit(2)
d = os.path.join(os.environ["XDG_RUNTIME_DIR"], "waybar-pill-plugin")
os.makedirs(d, exist_ok=True)
if os.fork():
    import time
    for _ in range(100):
        if os.path.exists(os.path.join(d, "control.sock")):
            sys.exit(0)
        time.sleep(0.05)
    sys.exit(1)
os.setsid()
s = socket.socket(socket.AF_UNIX)
s.bind(os.path.join(d, "control.sock"))
s.listen(1)
c, _ = s.accept()
buf = c.makefile("rb")
first = buf.readline().decode()
open(%r, "a").write("LINE " + first)
c.sendall(b'{"ok": true, "active": true}\\n')
while c.recv(1024):
    pass
open(%r, "a").write("EOF\\n")
''' % (log, log))
        env = dict(os.environ, QT_QPA_PLATFORM="offscreen")
        env.pop("WAYLAND_DISPLAY", None)
        qs = subprocess.Popen(["quickshell", "-p", os.path.join(ROOT, "Service.qml")], env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        self.addCleanup(lambda: qs.poll() is None and (qs.kill(), qs.wait()))
        deadline = time.time() + 20
        while time.time() < deadline and not os.path.exists(log):
            time.sleep(0.1)
        if qs.poll() is not None and not os.path.exists(log):
            self.fail("quickshell exited early: " + qs.stdout.read())
        self.assertTrue(os.path.exists(log), "Service.qml never connected the lease socket")
        with open(log) as f:
            self.assertIn('LINE {"op":"lease"}', f.read())
        qs.terminate()
        qs.wait(10)
        for _ in range(50):
            with open(log) as f:
                if "EOF" in f.read():
                    return
            time.sleep(0.1)
        self.fail("backend never saw EOF after quickshell exited")


NEEDED = ["jq", "cargo", "python3", "pkg-config"]


@unittest.skipUnless(all(shutil.which(c) for c in NEEDED) and
                     os.path.exists(os.path.join(ROOT, "notch", "target", "release", "waybar-notch")),
                     "needs built notch and cargo")
class SandboxInstall(Base):
    def setUp(self):
        super().setUp()
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.bin)
        for n in ("waybar", "playerctl", "pactl", "pamixer", "bluetoothctl", "nmcli", "upower", "powerprofilesctl",
                  "hyprctl", "fc-match", "systemctl"):
            self.stub(n, "exit 0")
        self.stub("omarchy", '[ "$1 $2" = "plugin list" ] && echo "[]"; exit 0')
        h = os.path.join(self.home, ".config", "hypr")
        os.makedirs(h)
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(h, n), "w") as f:
                f.write("-- user %s\n" % n)
        self.env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"],
                        CARGO_HOME=os.path.join(self.saved["HOME"], ".cargo"), RUSTUP_HOME=os.path.join(self.saved["HOME"], ".rustup"),
                        PYTHONDONTWRITEBYTECODE="1")
        self.env.pop("TERM_PROGRAM", None)

    def stub(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(p, 0o755)

    def sh(self, *args):
        return subprocess.run(list(args), cwd=ROOT, env=self.env, capture_output=True, text=True, stdin=subprocess.DEVNULL)

    def test_install_then_uninstall_restores_the_user_files(self):
        r = self.sh("bash", "install.sh", "--plugin", "--no-restart", "--no-google")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        marker = os.path.join(self.home, ".local", "state", "waybar-notch", "plugin-install.json")
        with open(marker) as f:
            m = json.load(f)
        self.assertEqual(m["mode"], "plugin")
        owned = [e for e in m["files"] if isinstance(e, dict)]
        self.assertTrue(owned)
        helper = os.path.join(self.home, ".config", "waybar", "scripts", "waybar-pill-plugin")
        self.assertTrue(os.stat(helper).st_mode & stat.S_IXUSR)
        self.assertTrue(os.access(os.path.join(self.home, ".local", "bin", "waybar-notch"), os.X_OK))
        with open(os.path.join(self.home, ".config", "hypr", "bindings.lua")) as f:
            self.assertIn("waybar-pill-bar BEGIN", f.read())
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "systemd")))  # no units in plugin mode
        r = self.sh("bash", "uninstall.sh")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertFalse(os.path.exists(marker))
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(self.home, ".config", "hypr", n)) as f:
                self.assertEqual(f.read(), "-- user %s\n" % n)
        self.assertFalse(os.path.exists(helper))


if __name__ == "__main__":
    unittest.main()
