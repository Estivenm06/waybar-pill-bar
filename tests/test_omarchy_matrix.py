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
    ("cloned notifications", [{"id": "example.notifications", "clonedFrom": "omarchy.notifications"}],
     ["example.notifications"]),
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
        self.activate(s)
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
        s = self.activate_with(CLONES[0][1], disabled=["example.notifications"])
        s.cleanup()
        self.assertEqual(self.get_shell()["disabledPlugins"], ["example.notifications"])


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
# stand-in for the notch: listen on the relay socket, then ask the shell to attach to it
relay = socket.socket(socket.AF_UNIX)
relay.bind(os.path.join(d, "notifications.sock"))
relay.listen(1)
content = socket.socket(socket.AF_UNIX)
content.bind(os.path.join(d, "notch-content.sock"))
content.listen(1)
c.sendall(b'{"ok": true, "active": false, "stage": "connect-relay"}\\n')
r, _ = relay.accept()
rb = r.makefile("rb")
open(%r, "a").write("HELLO " + rb.readline().decode())
r.sendall(b'{"op":"ready","version":2}\\n')
open(%r, "a").write("ACK " + buf.readline().decode())
c.sendall(b'{"ok": true, "active": true}\\n')
while c.recv(1024):
    pass
open(%r, "a").write("EOF\\n")
''' % (log, log, log, log))
        # a private session bus: Quickshell's notification server must never touch the user's real one
        bus = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address=1"], stdout=subprocess.PIPE, text=True,
                               start_new_session=True)
        self.addCleanup(lambda: bus.poll() is None and (bus.kill(), bus.wait()))
        address = bus.stdout.readline().strip()
        self.assertTrue(address)
        env = dict(os.environ, QT_QPA_PLATFORM="offscreen", DBUS_SESSION_BUS_ADDRESS=address)
        env.pop("WAYLAND_DISPLAY", None)
        qs = subprocess.Popen(["quickshell", "-p", os.path.join(ROOT, "Service.qml")], env=env,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, start_new_session=True)
        self.addCleanup(lambda: qs.poll() is None and (qs.kill(), qs.wait()))
        def log_text():
            try:
                with open(log) as f:
                    return f.read()
            except FileNotFoundError:
                return ""
        deadline = time.time() + 20
        while time.time() < deadline and "ACK" not in log_text():
            if qs.poll() is not None:
                self.fail("quickshell exited early: " + qs.stdout.read())
            time.sleep(0.1)
        seen = log_text()
        self.assertIn('ACK {"op":"relay-ready","version":1}', seen, "staged activation never completed: " + seen)
        self.assertLess(seen.index("LINE"), seen.index("HELLO"))  # lease first, then the relay hello
        self.assertIn('HELLO {"op":"hello","version":2}', seen)
        self.assertNotIn("EOF", seen)  # the lease stays open while the shell is alive
        qs.terminate()
        qs.wait(10)
        for _ in range(50):
            if "EOF" in log_text():
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
                  "hyprctl", "fc-match"):
            self.stub(n, "exit 0")
        self.calls_log = os.path.join(self.tmp, "calls.log")
        self.stub("systemctl", 'echo "systemctl $*" >> "%s"; exit 0' % self.calls_log)
        self.stub("omarchy", 'echo "omarchy $*" >> "%s"; [ "$1 $2" = "plugin list" ] && echo "[]"; exit 0' % self.calls_log)
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
        r = self.sh("bash", "install.sh", "--no-restart", "--no-google")
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
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "systemd")))  # no units
        with open(self.calls_log) as f:
            calls = f.read()
        self.assertNotIn("systemctl", calls)
        self.assertNotIn("plugin enable", calls)
        r = self.sh("bash", "uninstall.sh")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertFalse(os.path.exists(marker))
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(self.home, ".config", "hypr", n)) as f:
                self.assertEqual(f.read(), "-- user %s\n" % n)
        self.assertFalse(os.path.exists(helper))

    def test_removed_flag_is_a_usage_error_before_any_change(self):
        r = self.sh("bash", "install.sh", "--plugin", "--no-restart", "--no-google")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Unknown option", r.stdout)
        self.assertFalse(os.path.exists(os.path.join(self.home, ".local")))
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "waybar")))

    def test_uninstall_without_a_record_changes_nothing_even_with_purge(self):
        sentinels = [os.path.join(self.home, ".config", "waybar", "backup-20200101-000000", "config.jsonc"),
                     os.path.join(self.home, ".config", "waybar-notch", "google.json"),
                     os.path.join(self.home, ".local", "share", "waybar-notch", "google-token.json")]
        for p in sentinels:
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write("sentinel")
        for args in ((), ("--purge",)):
            r = self.sh("bash", "uninstall.sh", *args)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("No plugin install record; nothing was changed.", r.stdout)
        for p in sentinels:
            with open(p) as f:
                self.assertEqual(f.read(), "sentinel")
        self.assertFalse(os.path.exists(self.calls_log))  # Omarchy was never called

    def test_legacy_standalone_files_fail_setup_without_writes(self):
        drop = os.path.join(self.home, ".config", "systemd", "user", "waybar.service.d", "50-waybar-notch.conf")
        os.makedirs(os.path.dirname(drop))
        with open(drop, "w") as f:
            f.write("# Managed by waybar-pill-bar\n")
        r = self.sh("bash", "install.sh", "--no-restart", "--no-google")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Legacy standalone files detected", r.stdout + r.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.home, ".local", "state", "waybar-notch", "plugin-install.json")))
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "waybar", "config.jsonc")))
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(self.home, ".config", "hypr", n)) as f:
                self.assertEqual(f.read(), "-- user %s\n" % n)


if __name__ == "__main__":
    unittest.main()
