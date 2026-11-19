"""Omarchy 4.x compatibility matrix (run: python3 -m unittest tests/test_omarchy_matrix.py).
Covers catalog permutations of the bridge, the real Quickshell runtime loading Service.qml, and a sandboxed
install/uninstall cycle. Everything runs in a temporary HOME/XDG/runtime; desktop commands are faked."""
import json
import os
import shutil
import stat
import subprocess
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_plugin_bridge import Base, Fixture, ROOT, load  # noqa: E402

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


@unittest.skipUnless(shutil.which("quickshell") and shutil.which("dbus-daemon"), "quickshell or dbus-daemon not installed")
class RealQuickshell(Base):
    def test_service_qml_runs_launcher_and_holds_lease_until_exit(self):
        checkout = os.path.join(self.tmp, "checkout with spaces")
        scripts = os.path.join(checkout, "files", "scripts")
        os.makedirs(scripts)
        for name in ("Service.qml", "NotificationRelay.qml"):
            shutil.copy2(os.path.join(ROOT, name), checkout)
        log = os.path.join(self.tmp, "lease.log")
        # stand-in backend: `start` forks a listener on the real control socket path, records the first line and EOF
        with open(os.path.join(scripts, "waybar-pill-plugin"), "w") as f:
            f.write('''import os, socket, sys
if sys.argv[1:] != ["start", "--checkout", os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))]:
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
        env = dict(os.environ, QT_QPA_PLATFORM="offscreen", DBUS_SESSION_BUS_ADDRESS=address,
                   XDG_CACHE_HOME=os.path.join(self.home, ".cache"), XDG_DATA_HOME=os.path.join(self.home, ".local", "share"))
        env.pop("WAYLAND_DISPLAY", None)
        env.pop("DISPLAY", None)
        env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)
        qs = subprocess.Popen(["quickshell", "-p", os.path.join(checkout, "Service.qml")], env=env,
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


NEEDED = ["jq", "python3"]


@unittest.skipUnless(all(shutil.which(c) for c in NEEDED), "needs jq and python3")
class SandboxInstall(Base):
    def setUp(self):
        super().setUp()
        self.checkout = os.path.join(self.tmp, "checkout with spaces")
        os.mkdir(self.checkout)
        for name in ("install.sh", "uninstall.sh", "manifest.json"):
            shutil.copy2(os.path.join(ROOT, name), self.checkout)
        shutil.copytree(os.path.join(ROOT, "files"), os.path.join(self.checkout, "files"))
        os.mkdir(os.path.join(self.checkout, "notch"))
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.bin)
        for n in ("waybar", "playerctl", "pactl", "pamixer", "bluetoothctl", "nmcli", "upower", "powerprofilesctl",
                  "hyprctl", "fc-match", "pkg-config", "busctl", "omarchy-shell", "uwsm-app", "omarchy-notification-send"):
            self.stub(n, "exit 0")
        self.calls_log = os.path.join(self.tmp, "calls.log")
        self.stub("systemctl", 'echo "systemctl $*" >> "%s"; exit 0' % self.calls_log)
        self.stub("omarchy", 'echo "omarchy $*" >> "%s"; [ "$1 $2" = "plugin list" ] && echo "[]"; exit 0' % self.calls_log)
        self.stub("cargo", '''set -eu
[ "$*" = "build --release --locked --color never" ]
case "$CARGO_TARGET_DIR" in "$HOME"/.cache/waybar-notch/target) ;; *) exit 90 ;; esac
echo "cargo $*" >> "%s"
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\\nexit 0\\n' > "$CARGO_TARGET_DIR/release/waybar-notch"
chmod 755 "$CARGO_TARGET_DIR/release/waybar-notch"''' % self.calls_log)
        h = os.path.join(self.home, ".config", "hypr")
        os.makedirs(h)
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(h, n), "w") as f:
                f.write("-- user %s\n" % n)
        self.env = dict(os.environ, PATH=self.bin + ":/usr/bin:/bin",
                        CARGO_HOME=os.path.join(self.home, ".cargo"), RUSTUP_HOME=os.path.join(self.home, ".rustup"),
                        XDG_CACHE_HOME=os.path.join(self.home, ".cache"),
                        XDG_DATA_HOME=os.path.join(self.home, ".local", "share"),
                        PYTHONDONTWRITEBYTECODE="1", PILL_BAR_NO_AUTO_INSTALL="1", PILL_BAR_NO_PREBUILT="1")
        for name in ("TERM_PROGRAM", "DISPLAY", "WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE",
                     "DBUS_SESSION_BUS_ADDRESS", "WAYBAR_PILL_SESSION"):
            self.env.pop(name, None)

    def stub(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(p, 0o755)

    def sh(self, *args):
        return subprocess.run(list(args), cwd=self.checkout, env=self.env, capture_output=True, text=True,
                              stdin=subprocess.DEVNULL, timeout=60)

    def set_shell(self, cfg):
        Fixture.set_shell(self, cfg)

    def get_shell(self):
        return Fixture.get_shell(self)

    def manager_desktop(self):
        self.shell = os.path.join(self.home, ".config", "omarchy", "shell.json")
        os.makedirs(os.path.dirname(self.shell), exist_ok=True)
        self.flag = os.path.join(self.home, ".local", "state", "omarchy", "toggles", "bar-off")
        os.makedirs(os.path.dirname(self.flag), exist_ok=True)
        self.set_shell({"disabledPlugins": [], "bar": {"keep": "me"}})
        self.procs = []
        self.foreign_owner = None
        self.shell_pid = os.getpid()
        self.argv = []
        self.child_exits = False
        self.calls = []
        self.addCleanup(lambda: [p.kill() or p.wait() for p in self.procs if p.poll() is None])

    def manager_enable(self):
        # The manager's service provisions before acquiring the desktop; every external command is a fake.
        r = self.sh("bash", "install.sh", "--provision", "--no-restart")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        state = load(os.path.join(self.checkout, "files", "setup", "plugin-state.py"), "matrix_state")
        session = self.h.Session(lambda cmd: Fixture.runner(self, cmd),
                                 lambda args, **kw: Fixture.popen(self, args, **kw), state=state)
        self.addCleanup(session.cleanup)
        session.activate(self.shell_pid, lambda: None)
        self.assertTrue(session.active)
        self.assertTrue(os.path.exists(self.flag))
        self.assertIn("omarchy.notifications", self.get_shell()["disabledPlugins"])
        return session

    def assert_manager_restored(self):
        self.assertFalse(os.path.exists(self.h.marker_path()))
        self.assertFalse(os.path.exists(self.flag))
        self.assertEqual(self.get_shell(), {"disabledPlugins": [], "bar": {"keep": "me"}})
        self.assertFalse(os.path.exists(os.path.join(self.home, ".local", "bin", "waybar-notch")))
        for name in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(self.home, ".config", "hypr", name)) as f:
                self.assertEqual(f.read(), "-- user %s\n" % name)
        self.assertTrue(all(p.poll() is not None for p in self.procs))

    def test_manager_add_enable_disable_reenable_remove(self):
        self.manager_desktop()
        # Adding a checkout does not run its installer or touch the native desktop.
        self.assert_manager_restored()
        self.assertFalse(os.path.exists(self.calls_log))
        first = self.manager_enable()
        first.cleanup()
        self.assert_manager_restored()
        self.assertTrue(os.path.isdir(self.checkout))
        self.procs.clear()
        fifo = os.path.join(self.rt, "waybar-popupd.fifo")
        if os.path.exists(fifo):
            os.unlink(fifo)
        second = self.manager_enable()
        # Removing an enabled checkout closes the lease; restoration uses the already-loaded state helper.
        shutil.rmtree(self.checkout)
        second.cleanup()
        self.assert_manager_restored()
        self.assertFalse(os.path.exists(self.checkout))

    def test_install_without_provision_is_inert(self):
        self.manager_desktop()
        r = self.sh("bash", "install.sh")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Setup is automatic", r.stdout)
        shutil.rmtree(self.checkout)
        self.assert_manager_restored()
        self.assertFalse(os.path.exists(self.calls_log))

    def test_manager_failed_build_leaves_native_desktop_untouched(self):
        self.manager_desktop()
        self.stub("cargo", "exit 42")
        r = self.sh("bash", "install.sh", "--provision", "--no-restart")
        self.assertNotEqual(r.returncode, 0)
        self.assert_manager_restored()
        self.assertEqual(self.calls, [])

    def test_manager_reprovision_refuses_existing_record(self):
        self.manager_desktop()
        session = self.manager_enable()
        with open(self.h.marker_path(), "rb") as f:
            before = f.read()
        r = self.sh("bash", "install.sh", "--provision", "--no-restart")
        self.assertNotEqual(r.returncode, 0)
        with open(self.h.marker_path(), "rb") as f:
            self.assertEqual(f.read(), before)
        self.assertTrue(session.active)
        session.cleanup()
        self.assert_manager_restored()

    def test_manager_add_then_remove_never_provisions(self):
        self.manager_desktop()
        shutil.rmtree(self.checkout)
        self.assert_manager_restored()
        self.assertFalse(os.path.exists(self.calls_log))

    def test_install_then_uninstall_restores_the_user_files(self):
        r = self.sh("bash", "install.sh", "--provision", "--no-restart")
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
        r = self.sh("bash", "install.sh", "--plugin", "--no-restart")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Unknown option", r.stdout)
        self.assertFalse(os.path.exists(os.path.join(self.home, ".local")))
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "waybar")))

    def test_uninstall_without_a_record_preserves_unmanaged_files_and_purges_only_credentials(self):
        sentinels = [os.path.join(self.home, ".config", "waybar", "backup-20200101-000000", "config.jsonc"),
                     os.path.join(self.home, ".config", "waybar-notch", "google.json"),
                     os.path.join(self.home, ".local", "share", "waybar-notch", "google-token.json")]
        for p in sentinels:
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w") as f:
                f.write("sentinel")
        for args in ((), ("--purge",)):
            r = self.sh("bash", "uninstall.sh", *args)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            self.assertIn("Plugin files restored.", r.stdout)
            if not args:
                for p in sentinels:
                    with open(p) as f:
                        self.assertEqual(f.read(), "sentinel")
        with open(sentinels[0]) as f:
            self.assertEqual(f.read(), "sentinel")
        for p in sentinels[1:]:
            self.assertFalse(os.path.exists(p))

    def test_legacy_standalone_files_fail_setup_without_writes(self):
        drop = os.path.join(self.home, ".config", "systemd", "user", "waybar.service.d", "50-waybar-notch.conf")
        os.makedirs(os.path.dirname(drop))
        with open(drop, "w") as f:
            f.write("# Managed by waybar-pill-bar\n")
        r = self.sh("bash", "install.sh", "--provision", "--no-restart")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Legacy standalone files detected", r.stdout + r.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.home, ".local", "state", "waybar-notch", "plugin-install.json")))
        self.assertFalse(os.path.exists(os.path.join(self.home, ".config", "waybar", "config.jsonc")))
        for n in ("autostart.lua", "bindings.lua"):
            with open(os.path.join(self.home, ".config", "hypr", n)) as f:
                self.assertEqual(f.read(), "-- user %s\n" % n)


if __name__ == "__main__":
    unittest.main()
