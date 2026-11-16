"""Behavior tests for the plugin bridge (run: python3 -m unittest tests/test_plugin_bridge.py).
Everything uses a temporary HOME/XDG/runtime. Desktop commands go through an injected fake runner; the "product" is a
real throwaway `sleep` process, so signalling and reaping are real."""
import importlib.machinery
import importlib.util
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPTS = os.path.join(ROOT, "files", "scripts")
STATEPY = os.path.join(ROOT, "files", "setup", "plugin-state.py")


def load(path, name):
    loader = importlib.machinery.SourceFileLoader(name, path)
    spec = importlib.util.spec_from_loader(name, loader)
    m = importlib.util.module_from_spec(spec)
    loader.exec_module(m)
    return m


CATALOG = [{"id": "omarchy.notifications"}, {"id": "omarchy.osd"}, {"id": "omarchy.bar"},
           {"id": "user.my-toasts", "clonedFrom": "omarchy.notifications"}]


class Base(unittest.TestCase):
    def setUp(self):
        sys.dont_write_bytecode = True
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.rt = os.path.join(self.tmp, "rt")
        os.makedirs(self.home)
        os.mkdir(self.rt, 0o700)
        self.saved = dict(os.environ)
        self.addCleanup(self.restore_env)
        old_umask = os.umask(0o022)
        self.addCleanup(os.umask, old_umask)
        os.environ.update(HOME=self.home, XDG_RUNTIME_DIR=self.rt,
                          XDG_STATE_HOME=os.path.join(self.home, ".local", "state"),
                          XDG_CONFIG_HOME=os.path.join(self.home, ".config"))
        self.h = load(os.path.join(SCRIPTS, "waybar-pill-plugin"), "pill_plugin_under_test")

    def restore_env(self):
        os.environ.clear()
        os.environ.update(self.saved)

    def write_marker(self, files=()):
        os.makedirs(self.h.state_dir(), exist_ok=True)
        with open(self.h.marker_path(), "w") as f:
            json.dump({"version": 1, "mode": "plugin", "backup_dir": os.path.join(self.home, ".config", "waybar", "backup-20260101-000000"),
                       "files": list(files)}, f)

    def place(self, rel, text=""):
        """A regular file at `rel` under the temporary HOME (parents created)."""
        p = os.path.join(self.home, rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(text)
        return p


class Fixture(Base):
    """A fake Omarchy desktop whose state lives in files under the temporary HOME."""

    def setUp(self):
        super().setUp()
        for rel in (".local/bin/waybar-notch", ".config/waybar/scripts/waybar-popupctl", ".config/waybar/config.jsonc"):
            p = os.path.join(self.home, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            open(p, "w").close()
        self.write_marker()
        self.shell = os.path.join(self.home, ".config", "omarchy", "shell.json")
        os.makedirs(os.path.dirname(self.shell))
        self.flag = os.path.join(self.home, ".local", "state", "omarchy", "toggles", "bar-off")
        os.makedirs(os.path.dirname(self.flag))
        self.set_shell({"disabledPlugins": [], "bar": {"keep": "me"}})
        self.procs = []
        self.foreign_owner = None
        self.shell_pid = os.getpid()
        self.argv = []
        self.relay_calls = 0
        self.relay_failure = None
        self.child_exits = False
        self.calls = []

    def set_shell(self, cfg):
        with open(self.shell, "w") as f:
            json.dump(cfg, f)

    def get_shell(self):
        with open(self.shell) as f:
            return json.load(f)

    def runner(self, cmd, **kw):
        self.calls.append(cmd)
        ok = lambda out="": subprocess.CompletedProcess(cmd, 0, out, "")
        no = subprocess.CompletedProcess(cmd, 1, "", "")
        if cmd[:3] == ["omarchy", "plugin", "list"]:
            off = self.get_shell().get("disabledPlugins", [])
            return ok(json.dumps([dict(p, enabled=p["id"] not in off) for p in CATALOG]))
        if cmd[:1] == ["busctl"]:
            return ok("PID=%d\n" % (self.foreign_owner or self.shell_pid))  # Quickshell owns the name throughout
        if cmd[:1] == ["omarchy-shell"] and cmd[2:3] == ["ping"]:
            off = self.get_shell().get("disabledPlugins", [])
            return no if "omarchy." + cmd[1] in off else ok("ok")
        if cmd[:3] == ["omarchy", "toggle", "bar"]:
            if cmd[3] == "on":
                open(self.flag, "w").close()
            elif os.path.exists(self.flag):
                os.unlink(self.flag)
            return ok()
        if cmd[:2] == ["hyprctl", "layers"]:
            return ok('[{"namespace": "waybar"}]')
        if cmd[:2] == ["systemctl", "--user"]:
            return no  # nothing enabled/active
        return ok()

    def popen(self, args, **kw):
        p = subprocess.Popen(["sleep", "0" if self.child_exits and not self.procs else "60"], start_new_session=True)
        self.procs.append(p)
        self.argv.append(args)
        if self.child_exits and len(self.procs) == 1:
            p.wait()
        if len(self.procs) == 2:
            os.mkfifo(os.path.join(self.rt, "waybar-popupd.fifo"))
            os.makedirs(os.path.join(self.rt, "waybar-notch"), exist_ok=True)
            open(os.path.join(self.rt, "waybar-notch", "bar-control.sock"), "w").close()
            for name in ("notifications.sock", "notch-content.sock"):  # the notch's two listeners
                listener = socket.socket(socket.AF_UNIX)
                listener.bind(os.path.join(self.rt, "waybar-pill-plugin", name))
                self.addCleanup(listener.close)
        return p

    def relay(self):
        """The QML handshake, as a callable the session invokes once the product is ready."""
        self.relay_calls += 1
        if self.relay_failure:
            raise self.h.PluginError(self.relay_failure)

    def activate(self, session):
        session.activate(self.shell_pid, self.relay)

    def session(self):
        s = self.h.Session(self.runner, self.popen)
        self.addCleanup(lambda: [p.kill() or p.wait() for p in self.procs if p.poll() is None])
        return s


class SessionTests(Fixture):
    def test_disable_restores_only_owned_changes_and_keeps_midsession_edits(self):
        self.set_shell({"disabledPlugins": ["omarchy.osd"], "bar": {"keep": "me"}})  # osd was already disabled
        s = self.session()
        self.activate(s)
        cfg = self.get_shell()
        self.assertCountEqual(cfg["disabledPlugins"], ["omarchy.osd", "omarchy.notifications", "user.my-toasts"])
        self.assertTrue(os.path.exists(self.flag))
        cfg["extra"] = "added during the session"
        self.set_shell(cfg)
        s.cleanup()
        cfg = self.get_shell()
        self.assertEqual(cfg["disabledPlugins"], ["omarchy.osd"])
        self.assertEqual(cfg["bar"], {"keep": "me"})
        self.assertEqual(cfg["extra"], "added during the session")
        self.assertFalse(os.path.exists(self.flag))
        self.assertFalse(os.path.exists(self.h.journal_path()))
        self.assertTrue(all(p.poll() is not None for p in self.procs))

    def test_bar_hidden_before_stays_hidden(self):
        open(self.flag, "w").close()
        s = self.session()
        self.activate(s)
        s.cleanup()
        self.assertTrue(os.path.exists(self.flag))
        self.assertNotIn(["omarchy", "toggle", "bar", "off"], self.calls)

    def test_foreign_notification_owner_rolls_everything_back_without_touching_it(self):
        self.foreign_owner = os.getppid()  # a live pid that must never be signalled
        before = self.get_shell()
        with self.assertRaises(self.h.PluginError) as cm:
            self.activate(self.session())
        self.assertIn("owns", str(cm.exception))
        self.assertEqual(self.get_shell(), before)
        self.assertEqual(self.procs, [])
        self.assertFalse(os.path.exists(self.flag))
        self.assertFalse(os.path.exists(self.h.journal_path()))

    def test_child_exit_before_readiness_rolls_back(self):
        self.child_exits = True
        before = self.get_shell()
        with self.assertRaises(self.h.PluginError):
            self.activate(self.session())
        self.assertEqual(self.get_shell(), before)
        self.assertFalse(os.path.exists(self.flag))
        self.assertTrue(all(p.poll() is not None for p in self.procs))

    def test_session_launches_notch_in_relay_mode_for_the_lease_peer_and_keeps_the_owner(self):
        s = self.session()
        self.activate(s)
        self.assertEqual(self.relay_calls, 1)
        self.assertEqual(self.argv[0][1:], ["--waybar-supervisor"])
        self.assertEqual(self.argv[1][1:], ["--notification-relay", str(self.shell_pid)])
        self.assertTrue(s.active)
        socks = [os.path.join(self.rt, "waybar-pill-plugin", n) for n in ("notifications.sock", "notch-content.sock")]
        for sock in socks:
            self.assertTrue(stat.S_ISSOCK(os.lstat(sock).st_mode))
        s.cleanup()
        for sock in socks:
            self.assertFalse(os.path.lexists(sock))

    def test_failed_relay_handshake_rolls_back_without_going_active(self):
        self.set_shell({"disabledPlugins": ["omarchy.osd"], "bar": {"keep": "me"}})
        self.relay_failure = "the lease closed before the notification relay was ready"
        s = self.session()
        with self.assertRaises(self.h.PluginError):
            self.activate(s)
        self.assertFalse(s.active)
        self.assertEqual(self.get_shell()["disabledPlugins"], ["omarchy.osd"])
        self.assertFalse(os.path.exists(self.flag))
        self.assertFalse(os.path.exists(self.h.journal_path()))
        self.assertTrue(all(p.poll() is not None for p in self.procs))
        for name in ("notifications.sock", "notch-content.sock"):
            self.assertFalse(os.path.lexists(os.path.join(self.rt, "waybar-pill-plugin", name)))

    def test_child_exit_during_relay_handshake_rolls_back(self):
        def die():
            self.procs[1].kill()
            self.procs[1].wait()
        s = self.session()
        with self.assertRaises(self.h.PluginError):
            s.activate(self.shell_pid, die)
        self.assertFalse(s.active)
        self.assertEqual(self.get_shell()["disabledPlugins"], [])
        self.assertTrue(all(p.poll() is not None for p in self.procs))

    def test_relay_handshake_needs_the_exact_ack_and_notices_eof_and_timeout(self):
        for payload, why in ((b'{"op":"relay-ready","version":2}\n', "invalid"), (b"garbage\n", "invalid"), (None, "closed")):
            a, b = socket.socketpair()
            self.addCleanup(a.close)
            self.addCleanup(b.close)
            if payload is None:
                b.close()
            else:
                b.sendall(payload)
            with self.assertRaises(self.h.PluginError) as cm:
                self.h.relay_handshake(a, types.SimpleNamespace(check_children=lambda: None), limit=2)
            self.assertIn(why, str(cm.exception))
        a, b = socket.socketpair()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        with self.assertRaises(self.h.PluginError) as cm:
            self.h.relay_handshake(a, types.SimpleNamespace(check_children=lambda: None), limit=0.3)
        self.assertIn("in time", str(cm.exception))

    def test_stale_session_sockets_are_replaced_but_foreign_entries_are_refused(self):
        d = self.h.runtime_dir()
        names = ("notifications.sock", "notch-content.sock")
        for name in names:
            stale = socket.socket(socket.AF_UNIX)
            stale.bind(os.path.join(d, name))
            stale.close()
        s = self.session()
        self.activate(s)  # both stale files were removed, then recreated by the new notch
        s.cleanup()
        for name in names:
            path = os.path.join(d, name)
            open(path, "w").close()  # a regular file is not ours to delete
            with self.assertRaises(self.h.PluginError):
                self.activate(self.session())
            self.assertTrue(os.path.exists(path))
            self.assertEqual(self.get_shell()["disabledPlugins"], [], "a refused socket restores the desktop")
            os.unlink(path)

    def test_only_the_two_known_session_sockets_can_be_prepared(self):
        with self.assertRaises(self.h.PluginError):
            self.session().prepare_session_socket("other.sock")

    def test_content_socket_must_exist_for_readiness(self):
        s = self.session()
        self.activate(s)
        self.assertTrue(s.ready(set()))
        os.unlink(os.path.join(self.rt, "waybar-pill-plugin", "notch-content.sock"))
        self.assertFalse(s.ready(set()))
        s.cleanup()

    def test_missing_setup_changes_nothing(self):
        os.unlink(self.h.marker_path())
        before = self.get_shell()
        with self.assertRaises(self.h.PluginError) as cm:
            self.activate(self.session())
        self.assertIn("run ./install.sh in", str(cm.exception))
        self.assertNotIn("--plugin", str(cm.exception))
        self.assertEqual(self.get_shell(), before)
        self.assertFalse(os.path.exists(self.h.journal_path()))

    def test_malformed_shell_config_is_refused_untouched(self):
        with open(self.shell, "w") as f:
            f.write("{not json")
        with self.assertRaises(self.h.PluginError):
            self.activate(self.session())
        with open(self.shell) as f:
            self.assertEqual(f.read(), "{not json")

    def test_standalone_leftovers_are_refused(self):
        drop = os.path.join(self.home, ".config", "systemd", "user", "waybar.service.d", "50-waybar-notch.conf")
        os.makedirs(os.path.dirname(drop))
        open(drop, "w").close()
        with self.assertRaises(self.h.PluginError) as cm:
            self.activate(self.session())
        self.assertIn("Legacy standalone files detected", str(cm.exception))

    def test_stale_journal_recovery_restores_targets_and_launches_nothing(self):
        self.set_shell({"disabledPlugins": ["omarchy.notifications", "omarchy.osd"]})
        open(self.flag, "w").close()
        self.h.write_atomic(self.h.journal_path(), json.dumps(
            {"version": 1, "phase": "active", "bar_hidden_before": False,
             "disabled_plugins_added": ["omarchy.notifications"], "waybar_was_active": False}))
        self.h.restore(self.h.Session(self.runner, self.popen))
        self.assertEqual(self.get_shell()["disabledPlugins"], ["omarchy.osd"])
        self.assertFalse(os.path.exists(self.flag))
        self.assertFalse(os.path.exists(self.h.journal_path()))
        self.assertEqual(self.procs, [])


class ServeTests(Fixture):
    def start_server(self):
        self.rc = []
        t = threading.Thread(target=lambda: self.rc.append(self.h.serve(self.runner, self.popen)))
        t.start()
        self.addCleanup(t.join, 10)
        for _ in range(100):
            if os.path.exists(os.path.join(self.rt, "waybar-pill-plugin", "control.sock")):
                return t
            time.sleep(0.05)
        self.fail("server did not start")

    def lease(self, ack=True):
        c = socket.socket(socket.AF_UNIX)
        c.connect(os.path.join(self.rt, "waybar-pill-plugin", "control.sock"))
        c.sendall(b'{"op":"lease"}\n')
        f = c.makefile()
        if ack:
            self.assertEqual(json.loads(f.readline())["stage"], "connect-relay")
            c.sendall(b'{"op":"relay-ready","version":1}\n')
        return c, f

    def test_lease_eof_restores_and_second_lease_and_second_backend_do_nothing(self):
        t = self.start_server()
        c, f = self.lease()
        self.assertTrue(json.loads(f.readline())["active"])
        self.assertEqual(len(self.procs), 2)
        self.assertTrue(self.h.request("active")["active"])
        c2, f2 = self.lease(ack=False)
        self.assertFalse(json.loads(f2.readline())["ok"])
        self.assertEqual(self.h.serve(self.runner, self.popen), 0)  # lock held: no second pair
        self.assertEqual(len(self.procs), 2)
        f.close()
        c.close()  # plugin disabled / shell crashed
        t.join(10)
        self.assertFalse(t.is_alive())
        self.assertEqual(self.get_shell()["disabledPlugins"], [])
        self.assertTrue(all(p.poll() is not None for p in self.procs))
        self.assertFalse(os.path.exists(os.path.join(self.rt, "waybar-pill-plugin", "control.sock")))
        self.assertIsNone(self.h.request("active"))

    def test_stop_request_ends_the_session(self):
        t = self.start_server()
        c, f = self.lease()
        f.readline()
        self.assertTrue(self.h.request("stop")["ok"])
        t.join(10)
        self.assertFalse(t.is_alive())
        self.assertTrue(all(p.poll() is not None for p in self.procs))


class RuntimeTests(Base):
    def test_symlinked_or_foreign_runtime_dir_is_refused(self):
        real = os.path.join(self.tmp, "elsewhere")
        os.mkdir(real, 0o700)
        os.symlink(real, os.path.join(self.rt, "waybar-pill-plugin"))
        with self.assertRaises(self.h.PluginError):
            self.h.runtime_dir()

    def test_loose_permissions_are_refused(self):
        d = os.path.join(self.rt, "waybar-pill-plugin")
        os.mkdir(d, 0o755)
        with self.assertRaises(self.h.PluginError):
            self.h.runtime_dir()

    def test_mode_and_malformed_marker(self):
        with self.assertRaises(self.h.PluginError):
            self.h.mode()
        self.write_marker()
        self.assertEqual(self.h.mode(), "plugin")
        with open(self.h.marker_path(), "w") as f:
            f.write("{oops")
        with self.assertRaises(self.h.PluginError):
            self.h.mode()

    def test_record_generated_updates_only_registered_paths(self):
        a = self.place(".config/waybar/style.css", "old")
        b = self.place(".config/waybar/pill-theme.css", "old")
        self.write_marker([{"path": a, "existed": False, "installed_sha256": self.h.sha256_of(a)}])
        with open(a, "w") as f:
            f.write("regenerated")
        with self.assertRaises(self.h.PluginError):
            self.h.record_generated_files([b])
        self.h.record_generated_files([a])
        with open(self.h.marker_path()) as f:
            self.assertEqual(json.load(f)["files"][0]["installed_sha256"], self.h.sha256_of(a))
        self.assertEqual(stat.S_IMODE(os.stat(self.h.marker_path()).st_mode), 0o600)

    def test_runtime_root_must_be_private(self):
        os.chmod(self.rt, 0o750)
        with self.assertRaises(self.h.PluginError):
            self.h.runtime_dir()
        self.assertFalse(os.path.exists(os.path.join(self.rt, "waybar-pill-plugin")))
        r = subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-pill-plugin"), "runtime-dir"],
                           capture_output=True, text=True, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(r.stdout, "")

    def test_runtime_dir_cli_prints_only_the_private_path(self):
        r = subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-pill-plugin"), "runtime-dir"],
                           capture_output=True, text=True, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
        self.assertEqual((r.returncode, r.stdout), (0, os.path.join(self.rt, "waybar-pill-plugin") + "\n"))
        self.assertEqual(stat.S_IMODE(os.stat(os.path.join(self.rt, "waybar-pill-plugin")).st_mode), 0o700)
        for bad in ("", "relative", os.path.join(self.tmp, "missing")):
            r = subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-pill-plugin"), "runtime-dir"],
                               capture_output=True, text=True, env=dict(os.environ, XDG_RUNTIME_DIR=bad))
            self.assertNotEqual(r.returncode, 0, bad)
            self.assertEqual(r.stdout, "")


class ScriptTests(Base):
    """The user-facing scripts, run for real with fake externals on PATH."""

    def setUp(self):
        super().setUp()
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.bin)
        self.log = os.path.join(self.tmp, "calls.log")
        for name in ("systemctl", "omarchy-brightness-display", "omarchy-audio-output-volume", "pamixer"):
            p = os.path.join(self.bin, name)
            with open(p, "w") as f:
                f.write('#!/bin/sh\necho "%s $*" >> "%s"\n' % (name, self.log))
            os.chmod(p, 0o755)
        os.environ["PATH"] = self.bin + os.pathsep + os.environ["PATH"]
        self.write_marker()

    def calls(self):
        try:
            with open(self.log) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def test_popupctl_when_disabled_exits_nonzero_and_starts_nothing(self):
        r = subprocess.run(["bash", os.path.join(SCRIPTS, "waybar-popupctl"), "volume"], capture_output=True)
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_brightness_key_when_inactive_uses_native_osd(self):
        subprocess.run(["bash", os.path.join(SCRIPTS, "waybar-brightness-key"), "+5%"], check=True)
        self.assertEqual(self.calls(), ["omarchy-brightness-display +5%"])

    def test_volume_key_when_inactive_uses_native_volume(self):
        subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-volume-osd.py"), "lower"], check=True)
        self.assertEqual(self.calls(), ["omarchy-audio-output-volume lower"])

    def test_theme_hook_when_disabled_saves_without_reload_or_services(self):
        hook = os.path.join(self.tmp, "hook")
        gen = os.path.join(self.home, ".local", "bin", "waybar-notch")
        os.makedirs(os.path.dirname(gen))
        with open(gen, "w") as f:
            f.write("#!/bin/sh\ntouch '%s/pill-theme.css'\n" % os.path.join(self.home, ".config", "waybar"))
        os.chmod(gen, 0o755)
        wb = os.path.join(self.home, ".config", "waybar")
        os.makedirs(os.path.join(wb, "scripts"))
        for n in ("waybar-pill-plugin", "waybar-popupctl"):
            shutil.copy(os.path.join(SCRIPTS, n), os.path.join(wb, "scripts", n))
        css = os.path.join(wb, "pill-theme.css")
        open(css, "w").close()
        self.write_marker([{"path": css, "existed": False, "installed_sha256": ""}])
        with open(os.path.join(ROOT, "files", "hooks", "pill-theme-hook")) as f, open(hook, "w") as o:
            o.write(f.read().replace("__HOME__", self.home))
        os.chmod(hook, 0o755)
        r = subprocess.run([hook], capture_output=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls(), [])

    def test_unconfigured_helpers_never_start_anything(self):
        os.unlink(self.h.marker_path())
        fifo = os.path.join(self.rt, "waybar-popupd.fifo")
        os.mkfifo(fifo)  # leftover FIFO without a session
        r = subprocess.run(["bash", os.path.join(SCRIPTS, "waybar-popupctl"), "volume"], capture_output=True)
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.calls(), [])
        subprocess.run(["bash", os.path.join(SCRIPTS, "waybar-brightness-key"), "+5%"], check=True)
        subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-volume-osd.py"), "raise"], check=True)
        self.assertEqual(self.calls(), ["omarchy-brightness-display +5%", "omarchy-audio-output-volume raise"])

    def test_theme_hook_without_setup_fails_before_generating_css(self):
        os.unlink(self.h.marker_path())
        hook = os.path.join(self.tmp, "hook")
        gen = os.path.join(self.home, ".local", "bin", "waybar-notch")
        os.makedirs(os.path.dirname(gen))
        with open(gen, "w") as f:
            f.write("#!/bin/sh\necho generated >> '%s'\n" % self.log)
        os.chmod(gen, 0o755)
        wb = os.path.join(self.home, ".config", "waybar", "scripts")
        os.makedirs(wb)
        shutil.copy(os.path.join(SCRIPTS, "waybar-pill-plugin"), wb)
        with open(os.path.join(ROOT, "files", "hooks", "pill-theme-hook")) as f, open(hook, "w") as o:
            o.write(f.read().replace("__HOME__", self.home))
        os.chmod(hook, 0o755)
        self.assertNotEqual(subprocess.run([hook], capture_output=True).returncode, 0)
        self.assertEqual(self.calls(), [])


class InstallRecordTests(Base):
    def run_state(self, *args):
        return subprocess.run([sys.executable, STATEPY, *args], capture_output=True, text=True,
                              env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))

    def test_uninstall_restores_bytes_and_modes_and_baseline_survives_resetup(self):
        old = self.place(".config/hypr/autostart.lua", "mine")
        new = os.path.join(self.home, ".config", "waybar", "scripts", "waybar-app.sh")
        os.chmod(old, 0o640)
        os.utime(old, (1_000_000_000, 1_000_000_000))
        self.assertEqual(self.run_state("begin", old, new).returncode, 0)
        os.makedirs(os.path.dirname(new), exist_ok=True)
        for p, t in ((old, "managed"), (new, "script")):
            with open(p, "w") as f:
                f.write(t)
        self.assertEqual(self.run_state("finish").returncode, 0)
        # second setup: the original baseline must not become "managed"
        self.assertEqual(self.run_state("begin", old, new).returncode, 0)
        with open(old, "w") as f:
            f.write("managed v2")
        self.assertEqual(self.run_state("finish").returncode, 0)
        r = self.run_state("uninstall")
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(old) as f:
            self.assertEqual(f.read(), "mine")
        st = os.stat(old)
        self.assertEqual((stat.S_IMODE(st.st_mode), int(st.st_mtime)), (0o640, 1_000_000_000))
        self.assertFalse(os.path.exists(new))
        self.assertFalse(os.path.exists(self.h.marker_path()))

    def _managed(self, content=b"mine\x00\xff", mode=0o600):
        old = os.path.join(self.home, ".config", "hypr", "bindings.lua")
        os.makedirs(os.path.dirname(old), exist_ok=True)
        with open(old, "wb") as f:
            f.write(content)
        os.chmod(old, mode)
        os.utime(old, (1_000_000_000, 1_000_000_000))
        self.assertEqual(self.run_state("begin", old).returncode, 0)
        with open(old, "wb") as f:
            f.write(b"managed")
        os.chmod(old, 0o644)
        self.assertEqual(self.run_state("finish").returncode, 0)
        mod = load(STATEPY, "plugin_state_under_test")
        mod.active_session = lambda: False
        return old, mod

    def test_restore_is_private_while_copying_and_keeps_bytes_mode_and_time(self):
        import unittest.mock as um
        real = shutil.copyfileobj
        for mode in (0o600, 0o640):
            with self.subTest(mode=oct(mode)):
                if os.path.exists(self.h.marker_path()):
                    os.unlink(self.h.marker_path())
                old, mod = self._managed(mode=mode)
                seen = []

                def spy(src, dst, *a):
                    seen.append(stat.S_IMODE(os.fstat(dst.fileno()).st_mode))
                    dst.write(src.read(2))
                    dst.flush()
                    seen.append(stat.S_IMODE(os.fstat(dst.fileno()).st_mode))
                    real(src, dst, *a)
                with um.patch.object(mod.shutil, "copyfileobj", spy):
                    self.assertEqual(mod.uninstall(), 0)
                self.assertEqual(seen, [0o600, 0o600])
                with open(old, "rb") as f:
                    self.assertEqual(f.read(), b"mine\x00\xff")
                st = os.stat(old)
                self.assertEqual((stat.S_IMODE(st.st_mode), int(st.st_mtime)), (mode, 1_000_000_000))
                self.assertFalse(os.path.exists(self.h.marker_path()))

    def test_restore_failures_leave_destination_record_and_no_temp_file(self):
        import errno
        import unittest.mock as um
        real = shutil.copyfileobj

        def partial_eio(src, dst, *a):
            dst.write(src.read(2))
            dst.flush()
            raise OSError(errno.EIO, "synthetic failure")
        cases = (("shutil", "copyfileobj", partial_eio),
                 ("os", "utime", um.Mock(side_effect=OSError(errno.EIO, "synthetic failure"))),
                 ("os", "replace", um.Mock(side_effect=PermissionError(errno.EACCES, "synthetic failure"))))
        for module, name, fake in cases:
            with self.subTest(name=name):
                if os.path.exists(self.h.marker_path()):
                    os.unlink(self.h.marker_path())
                old, mod = self._managed()
                with open(self.h.marker_path()) as f:
                    record = f.read()
                with um.patch.object(getattr(mod, module), name, fake):
                    with self.assertRaises(mod.PluginError) as cm:
                        mod.uninstall()
                self.assertNotIn("mine", str(cm.exception))
                with open(old, "rb") as f:
                    self.assertEqual(f.read(), b"managed")
                self.assertEqual(stat.S_IMODE(os.stat(old).st_mode), 0o644)
                with open(self.h.marker_path()) as f:
                    self.assertEqual(f.read(), record)
                self.assertEqual([n for n in os.listdir(os.path.dirname(old)) if n.startswith(".restore.")], [])

    def test_restore_ignores_predictable_symlink_temp_name(self):
        old, mod = self._managed()
        victim = os.path.join(self.tmp, "victim")
        with open(victim, "wb") as f:
            f.write(b"victim")
        link = old + ".restore.%d" % os.getpid()
        os.symlink(victim, link)
        self.assertEqual(mod.uninstall(), 0)
        with open(old, "rb") as f:
            self.assertEqual(f.read(), b"mine\x00\xff")
        self.assertTrue(os.path.islink(link))
        with open(victim, "rb") as f:
            self.assertEqual(f.read(), b"victim")

    def test_modified_owned_file_blocks_uninstall_and_keeps_everything(self):
        f1 = os.path.join(self.home, ".config", "waybar", "style.css")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        self.place(".config/waybar/style.css", "ours")
        self.run_state("finish")
        with open(f1, "w") as f:
            f.write("user edit")
        r = self.run_state("uninstall")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("modified", r.stderr)
        with open(f1) as f:
            self.assertEqual(f.read(), "user edit")
        self.assertTrue(os.path.exists(self.h.marker_path()))

    def test_resetup_refuses_to_overwrite_user_edits(self):
        f1 = os.path.join(self.home, ".config", "waybar", "style.css")
        self.run_state("begin", f1)
        self.place(".config/waybar/style.css", "ours")
        self.run_state("finish")
        with open(f1, "w") as f:
            f.write("user edit")
        self.assertNotEqual(self.run_state("begin", f1).returncode, 0)

    def test_standalone_install_blocks_plugin_setup(self):
        lua = os.path.join(self.home, ".config", "hypr", "autostart.lua")
        os.makedirs(os.path.dirname(lua))
        with open(lua, "w") as f:
            f.write("-- >>> waybar-pill-bar BEGIN (managed block) >>>\n")
        r = self.run_state("begin", os.path.join(self.home, ".config", "waybar", "config.jsonc"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Legacy standalone files detected", r.stderr)
        self.assertFalse(os.path.exists(self.h.marker_path()))


class InstallBackupPrivacyTests(Base):
    """Backups never widen access to a source file that lived in a private directory."""

    def run_state(self, *args):
        return subprocess.run([sys.executable, STATEPY, *args], capture_output=True, text=True,
                              env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))

    def backup_dir(self):
        return self.h.read_marker()["backup_dir"]

    def test_backup_root_is_private_and_restore_keeps_bytes_and_mode(self):
        priv = os.path.join(self.home, ".config", "hypr")
        os.makedirs(priv, 0o700)
        src = os.path.join(priv, "autostart.lua")
        with open(src, "w") as f:
            f.write("synthetic secret")
        os.chmod(src, 0o644)
        env_umask = os.umask(0o022)
        try:
            self.assertEqual(self.run_state("begin", src).returncode, 0)
        finally:
            os.umask(env_umask)
        root = self.backup_dir()
        self.assertEqual(stat.S_IMODE(os.stat(root).st_mode), 0o700)
        with open(src, "w") as f:
            f.write("managed")
        self.run_state("finish")
        r = self.run_state("uninstall")
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(src) as f:
            self.assertEqual(f.read(), "synthetic secret")
        self.assertEqual(stat.S_IMODE(os.stat(src).st_mode), 0o644)
        self.assertEqual(stat.S_IMODE(os.stat(priv).st_mode), 0o700, "the private source directory is never widened")

    def test_recorded_loose_root_is_tightened_before_reuse(self):
        f1 = self.place(".config/waybar/config.jsonc", "x")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        root = self.backup_dir()
        os.chmod(root, 0o755)
        f2 = self.place(".config/waybar/style.css", "y")
        self.assertEqual(self.run_state("begin", f2).returncode, 0)
        self.assertEqual(stat.S_IMODE(os.stat(root).st_mode), 0o700)

    def test_symlinked_or_missing_recorded_root_is_rejected_untouched(self):
        f1 = os.path.join(self.home, ".config", "waybar", "config.jsonc")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        root = self.backup_dir()
        with open(self.h.marker_path()) as f:
            marker = f.read()
        victim = os.path.join(self.tmp, "victim")
        os.mkdir(victim, 0o755)
        shutil.rmtree(root)
        os.symlink(victim, root)
        r = self.run_state("begin", os.path.join(self.home, ".config", "waybar", "style.css"))
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(stat.S_IMODE(os.stat(victim).st_mode), 0o755)
        os.unlink(root)
        r = self.run_state("begin", os.path.join(self.home, ".config", "waybar", "style.css"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("missing", r.stderr)
        with open(self.h.marker_path()) as f:
            self.assertEqual(f.read(), marker)

    def test_foreign_owned_root_is_rejected(self):
        from unittest import mock
        ps = load(STATEPY, "plugin_state_under_test")
        root = os.path.join(self.tmp, "root")
        os.mkdir(root, 0o755)
        with mock.patch.object(ps.os, "geteuid", return_value=os.geteuid() + 1):
            with self.assertRaises(ps.PluginError):
                ps.private_root(root)
        self.assertEqual(stat.S_IMODE(os.stat(root).st_mode), 0o755)

    def test_same_second_installs_pick_distinct_roots(self):
        ps = load(STATEPY, "plugin_state_under_test2")
        base = os.path.join(self.tmp, "w", "backup-1")
        os.makedirs(os.path.dirname(base))
        os.symlink(os.path.join(self.tmp, "nowhere"), base)  # dangling: must not be followed or reused
        a = ps.new_private_root(base)
        b = ps.new_private_root(base)
        self.assertEqual(len({base, a, b}), 3)
        self.assertEqual(stat.S_IMODE(os.stat(a).st_mode), 0o700)
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "nowhere")))


class RecordAttackTests(Base):
    """A forged or unsafe install record must fail validation before any desktop command, deletion, restoration or
    permission change. Fake desktop commands log every call."""

    def setUp(self):
        super().setUp()
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.bin)
        self.log = os.path.join(self.tmp, "calls.log")
        for name in ("omarchy", "omarchy-shell", "systemctl", "busctl", "hyprctl"):
            p = os.path.join(self.bin, name)
            with open(p, "w") as f:
                f.write('#!/bin/sh\necho "%s $*" >> "%s"\nexit 1\n' % (name, self.log))
            os.chmod(p, 0o755)
        self.env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"], PYTHONDONTWRITEBYTECODE="1")
        self.sentinel = os.path.join(self.tmp, "sentinel.txt")
        with open(self.sentinel, "w") as f:
            f.write("keep me")
        self.orig = self.place(".config/hypr/autostart.lua", "mine")
        os.chmod(self.orig, 0o640)
        os.utime(self.orig, (1_000_000_000, 1_000_000_000))
        self.created = os.path.join(self.home, ".config", "waybar", "style.css")
        self.script = os.path.join(self.home, ".config", "waybar", "scripts", "waybar-app.sh")
        self.assertEqual(self.state("begin", self.orig, self.created, self.script).returncode, 0)
        os.makedirs(os.path.dirname(self.script))
        for p in (self.orig, self.created, self.script):
            with open(p, "w") as f:
                f.write("managed " + os.path.basename(p))
        self.assertEqual(self.state("finish").returncode, 0)
        self.root = self.h.read_marker()["backup_dir"]
        with open(self.h.marker_path()) as f:
            self.pristine = json.load(f)

    def state(self, *args):
        return subprocess.run([sys.executable, STATEPY, *args], capture_output=True, text=True, env=self.env)

    def uninstall_sh(self, env=None):
        return subprocess.run(["bash", os.path.join(ROOT, "uninstall.sh")], capture_output=True, text=True,
                              env=env or self.env, stdin=subprocess.DEVNULL)

    def calls(self):
        try:
            with open(self.log) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def snapshot(self):
        """Bytes and permission bits of everything an attack must not disturb (links are not followed)."""
        out = {}
        for top in (self.home, self.sentinel):
            walk = [top] if os.path.isfile(top) else [os.path.join(d, n) for d, ds, fs in os.walk(top) for n in ds + fs] + [top]
            for p in walk:
                st = os.lstat(p)
                body = None
                if stat.S_ISLNK(st.st_mode):
                    body = os.readlink(p)
                elif stat.S_ISREG(st.st_mode):
                    with open(p, "rb") as f:
                        body = f.read()
                out[p] = (stat.S_IMODE(st.st_mode), body)
        return out

    def forge(self, mutate):
        with open(self.h.marker_path()) as f:
            m = json.load(f)
        mutate(m)
        with open(self.h.marker_path(), "w") as f:
            json.dump(m, f)

    def assert_blocked(self, why):
        before = self.snapshot()
        r = self.state("validate")
        self.assertNotEqual(r.returncode, 0, why)
        self.assertEqual(self.state("uninstall").returncode == 0, False, why)
        r = self.uninstall_sh()
        self.assertNotEqual(r.returncode, 0, why)
        self.assertEqual(self.calls(), [], "no desktop command may run: " + why)
        self.assertEqual(self.snapshot(), before, why)
        with open(self.sentinel) as f:
            self.assertEqual(f.read(), "keep me")

    def test_valid_record_validates_and_validate_is_read_only(self):
        before = self.snapshot()
        r = self.state("validate")
        self.assertEqual((r.returncode, r.stdout), (0, ""), r.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_validate_accepts_an_absent_record(self):
        os.unlink(self.h.marker_path())
        self.assertEqual(self.state("validate").returncode, 0)

    def test_validate_does_not_tighten_or_create_state(self):
        os.chmod(self.h.state_dir(), 0o755)
        self.assertEqual(self.state("validate").returncode, 0)
        self.assertEqual(stat.S_IMODE(os.stat(self.h.state_dir()).st_mode), 0o755)
        shutil.rmtree(os.path.dirname(self.h.state_dir()))
        self.assertEqual(self.state("validate").returncode, 0)
        self.assertFalse(os.path.exists(self.h.state_dir()))

    def test_paths_outside_the_footprint_are_rejected(self):
        def add(path):
            return lambda m: m["files"].append({"path": path, "existed": False, "installed_sha256": ""})
        for why, mutate in (
                ("sentinel", add(self.sentinel)),
                ("traversal", add(os.path.join(self.home, ".config", "waybar", "scripts", "..", "..", "..", "..", "sentinel.txt"))),
                ("relative", add("sentinel.txt")),
                ("unknown script name", add(os.path.join(self.home, ".config", "waybar", "scripts", "evil.sh"))),
                ("home itself", add(self.home)),
                ("non-string", lambda m: m["files"].append({"path": 5, "existed": False, "installed_sha256": ""})),
                ("duplicate", lambda m: m["files"].append(dict(m["files"][0]))),
                ("bad existed", lambda m: m["files"][0].update(existed="yes")),
                ("bad hash", lambda m: m["files"][0].update(installed_sha256="ABC")),
                ("extra key", lambda m: m["files"][0].update(extra=1)),
                ("missing key", lambda m: m["files"][0].pop("existed")),
                ("backup naming HOME", lambda m: m.update(backup_dir=self.home)),
                ("backup naming sentinel", lambda m: m.update(backup_dir=self.sentinel)),
                ("backup traversal", lambda m: m.update(backup_dir=os.path.join(self.home, ".config", "waybar", "backup-20260101-000000", ".."))),
                ("missing backup", lambda m: m.pop("backup_dir")),
                ("files not a list", lambda m: m.update(files={})),
                ("version true", lambda m: m.update(version=True)),
                ("mode", lambda m: m.update(mode="standalone"))):
            with self.subTest(why):
                self.forge(mutate)
                self.assert_blocked(why)
                with open(self.h.marker_path(), "w") as f:  # a pristine record for the next case
                    json.dump(self.pristine, f)

    def test_duplicate_json_keys_are_rejected(self):
        with open(self.h.marker_path()) as f:
            text = f.read()
        with open(self.h.marker_path(), "w") as f:
            f.write(text.rstrip().rstrip("}") + ', "files": []}')
        self.assert_blocked("duplicate files key")

    def test_oversized_and_malformed_records_are_rejected(self):
        for why, data in (("malformed", b"{nope"), ("huge", b'{"x":"' + b"a" * (1 << 20) + b'"}'), ("array", b"[]"), ("nan", b'{"version": NaN}')):
            with self.subTest(why):
                with open(self.h.marker_path(), "wb") as f:
                    f.write(data)
                self.assert_blocked(why)

    def test_allowed_leaf_symlink_is_rejected(self):
        os.unlink(self.created)
        os.symlink(self.sentinel, self.created)
        self.assert_blocked("leaf symlink")

    def test_symlinked_parent_is_rejected(self):
        scripts = os.path.dirname(self.script)
        elsewhere = os.path.join(self.tmp, "elsewhere")
        os.rename(scripts, elsewhere)
        os.symlink(elsewhere, scripts)
        self.assert_blocked("symlinked parent")

    def test_symlinked_or_foreign_backup_sources_are_rejected(self):
        src = self.root + self.orig
        os.unlink(src)
        os.symlink(self.sentinel, src)
        self.assert_blocked("backup source symlink")
        os.unlink(src)
        shutil.copy2(self.sentinel, src)
        d = os.path.dirname(src)
        moved = os.path.join(self.tmp, "evil")
        os.rename(d, moved)
        os.symlink(moved, d)
        self.assert_blocked("backup parent symlink")

    def test_symlinked_backup_root_is_rejected(self):
        moved = os.path.join(self.tmp, "movedroot")
        os.rename(self.root, moved)
        os.symlink(moved, self.root)
        self.assert_blocked("backup root symlink")

    def test_group_writable_state_ancestry_is_rejected_without_chmod(self):
        for target in (self.h.state_dir(), os.path.dirname(self.h.state_dir())):
            with self.subTest(target):
                os.chmod(target, 0o775 if target != self.h.state_dir() else 0o770)
                self.assert_blocked("writable " + target)
                self.assertEqual(stat.S_IMODE(os.stat(target).st_mode), 0o775 if target != self.h.state_dir() else 0o770)
                os.chmod(target, 0o755)

    def test_group_writable_footprint_parent_is_rejected(self):
        os.chmod(os.path.dirname(self.script), 0o775)
        self.assert_blocked("writable scripts directory")

    def test_state_record_that_is_a_symlink_or_fifo_is_rejected(self):
        os.rename(self.h.marker_path(), self.sentinel + ".real")
        os.symlink(self.sentinel, self.h.marker_path())
        self.assert_blocked("record symlink")
        os.unlink(self.h.marker_path())
        os.symlink(os.path.join(self.tmp, "dangling"), self.h.marker_path())  # `-e` alone would call this absent
        self.assert_blocked("dangling record")
        os.unlink(self.h.marker_path())
        os.mkfifo(self.h.marker_path())
        self.assert_blocked("record fifo")

    def test_foreign_owner_is_rejected_by_mocked_fstat(self):
        from unittest import mock
        ps = load(STATEPY, "plugin_state_foreign")
        with mock.patch.object(ps.os, "geteuid", return_value=os.geteuid() + 1):
            self.assertNotEqual(ps.validate(), 0)
            with self.assertRaises(ps.PluginError):
                ps.helper.read_marker()
        self.assertEqual(ps.validate(), 0)

    def test_unfinished_install_blocks_uninstall_but_not_validation(self):
        self.forge(lambda m: m["files"][1].update(installed_sha256=""))
        before = self.snapshot()
        self.assertEqual(self.state("validate").returncode, 0)
        r = self.state("uninstall")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("never finalized", r.stderr)
        self.assertEqual(self.snapshot(), before)
        self.assertNotEqual(self.uninstall_sh().returncode, 0)
        with open(self.created) as f:
            self.assertEqual(f.read(), "managed style.css")

    def test_a_missing_destination_still_restores_its_backup(self):
        os.unlink(self.orig)
        r = self.state("uninstall")
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(self.orig) as f:
            self.assertEqual(f.read(), "mine")

    def test_uninstall_script_restores_originals_and_removes_only_known_files(self):
        unrelated = self.place(".config/waybar/scripts/user-own.sh", "mine too")
        r = self.uninstall_sh()
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(self.orig) as f:
            self.assertEqual(f.read(), "mine")
        st = os.stat(self.orig)
        self.assertEqual((stat.S_IMODE(st.st_mode), int(st.st_mtime)), (0o640, 1_000_000_000))
        self.assertFalse(os.path.exists(self.created) or os.path.exists(self.script))
        with open(unrelated) as f:
            self.assertEqual(f.read(), "mine too")
        self.assertFalse(os.path.exists(self.h.marker_path()))
        self.assertIn("hyprctl reload", self.calls())

    def test_custom_xdg_dirs_and_missing_whatsapp_checkout_work(self):
        state = os.path.join(self.tmp, "custom-state")
        cfg = os.path.join(self.tmp, "custom-cfg")
        os.mkdir(state)
        os.mkdir(cfg)
        env = dict(self.env, XDG_STATE_HOME=state, XDG_CONFIG_HOME=cfg)
        panel = os.path.join(cfg, "omarchy", "plugins", "io.github.ricky.whatsapp", "Panel.qml")
        run = lambda *a: subprocess.run([sys.executable, STATEPY, *a], capture_output=True, text=True, env=env)
        self.assertEqual(run("validate").returncode, 0)  # no record in the new state dir
        os.makedirs(os.path.dirname(panel))
        with open(panel, "w") as f:
            f.write("stock")
        self.assertEqual(run("begin", panel).returncode, 0)
        with open(panel, "w") as f:
            f.write("patched")
        self.assertEqual(run("finish").returncode, 0)
        self.assertEqual(run("validate").returncode, 0)
        self.assertEqual(run("uninstall").returncode, 0)
        with open(panel) as f:
            self.assertEqual(f.read(), "stock")
        # and a different configured root does not allow the previous one's destinations
        self.assertEqual(run("begin", os.path.join(self.tmp, "elsewhere", "Panel.qml")).returncode, 1)

    def test_begin_rejects_paths_outside_the_footprint_before_changing_anything(self):
        before = self.snapshot()
        for p in (self.sentinel, os.path.join(self.home, ".bashrc"), "relative"):
            r = self.state("begin", self.created, p)
            self.assertNotEqual(r.returncode, 0, p)
            self.assertIn("footprint", r.stderr)
        self.assertEqual(self.snapshot(), before)

    def test_begin_refuses_unsafe_destination_leaves_without_creating_a_backup(self):
        os.unlink(self.created)
        os.symlink(self.sentinel, self.created)
        before = self.snapshot()
        r = self.state("begin", os.path.join(self.home, ".config", "waybar", "pill-theme.css"), self.created)
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(self.snapshot(), before)


class JournalAttackTests(Fixture):
    """The recovery journal is validated before any shell/plugin/service operation and is never rewritten when bad."""

    def write_raw(self, data):
        os.makedirs(self.h.state_dir(), exist_ok=True)
        with open(self.h.journal_path(), "wb") as f:
            f.write(data)

    GOOD = {"version": 1, "phase": "active", "bar_hidden_before": False,
            "disabled_plugins_added": ["omarchy.notifications"], "waybar_was_active": False}

    def test_malformed_or_untrusted_journals_trigger_nothing_and_are_not_rewritten(self):
        dup = json.dumps(self.GOOD)[:-1] + ', "phase": "active"}'
        cases = {"json": b"{nope", "dup": dup.encode(), "array": b"[]",
                 "version": json.dumps(dict(self.GOOD, version=True)).encode(),
                 "version2": json.dumps(dict(self.GOOD, version=2)).encode(),
                 "phase": json.dumps(dict(self.GOOD, phase="done")).encode(),
                 "bar": json.dumps(dict(self.GOOD, bar_hidden_before="no")).encode(),
                 "waybar": json.dumps(dict(self.GOOD, waybar_was_active=1)).encode(),
                 "plugins": json.dumps(dict(self.GOOD, disabled_plugins_added="omarchy.osd")).encode(),
                 "plugin types": json.dumps(dict(self.GOOD, disabled_plugins_added=[1])).encode(),
                 "plugin dup": json.dumps(dict(self.GOOD, disabled_plugins_added=["a", "a"])).encode()}
        before_shell = self.get_shell()
        for why, data in cases.items():
            with self.subTest(why):
                self.write_raw(data)
                with self.assertRaises(self.h.PluginError):
                    self.h.restore(self.h.Session(self.runner, self.popen))
                self.assertEqual(self.calls, [])
                with open(self.h.journal_path(), "rb") as f:
                    self.assertEqual(f.read(), data)
                self.assertEqual(self.get_shell(), before_shell)

    def test_symlinked_fifo_and_loose_journals_are_refused(self):
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write(json.dumps(self.GOOD))
        os.symlink(sentinel, self.h.journal_path())
        with self.assertRaises(self.h.PluginError):
            self.h.restore(self.h.Session(self.runner, self.popen))
        os.unlink(self.h.journal_path())
        os.mkfifo(self.h.journal_path())
        with self.assertRaises(self.h.PluginError):
            self.h.restore(self.h.Session(self.runner, self.popen))
        os.unlink(self.h.journal_path())
        self.write_raw(json.dumps(self.GOOD).encode())
        os.chmod(self.h.journal_path(), 0o666)
        with self.assertRaises(self.h.PluginError):
            self.h.restore(self.h.Session(self.runner, self.popen))
        self.assertEqual(self.calls, [])
        with open(sentinel) as f:
            self.assertEqual(json.load(f), self.GOOD)

    def test_recover_cli_with_a_bad_journal_runs_no_desktop_command(self):
        bin_dir = os.path.join(self.tmp, "bin")
        os.mkdir(bin_dir)
        log = os.path.join(self.tmp, "cli.log")
        for name in ("omarchy", "omarchy-shell", "systemctl", "busctl"):
            p = os.path.join(bin_dir, name)
            with open(p, "w") as f:
                f.write('#!/bin/sh\necho "%s $*" >> "%s"\nexit 1\n' % (name, log))
            os.chmod(p, 0o755)
        self.write_raw(b'{"version": 1, "phase": "bogus"}')
        r = subprocess.run([sys.executable, os.path.join(SCRIPTS, "waybar-pill-plugin"), "recover"], capture_output=True,
                           text=True, env=dict(os.environ, PATH=bin_dir + os.pathsep + os.environ["PATH"], PYTHONDONTWRITEBYTECODE="1"))
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(os.path.exists(log))
        with open(self.h.journal_path(), "rb") as f:
            self.assertEqual(f.read(), b'{"version": 1, "phase": "bogus"}')

    def test_valid_crash_journal_restores_exactly_the_recorded_clone_ids(self):
        self.set_shell({"disabledPlugins": ["omarchy.notifications", "user.my-toasts", "omarchy.osd", "user.other"]})
        open(self.flag, "w").close()
        journal = dict(self.GOOD, disabled_plugins_added=["omarchy.notifications", "user.my-toasts"])
        self.write_raw(json.dumps(journal).encode())
        self.h.restore(self.h.Session(self.runner, self.popen))
        self.assertEqual(self.get_shell()["disabledPlugins"], ["omarchy.osd", "user.other"])
        self.assertFalse(os.path.exists(self.h.journal_path()))
        self.assertEqual(self.procs, [])


class ShellConfigModeTests(Fixture):
    def test_activate_and_cleanup_keep_private_mode_and_unrelated_fields(self):
        self.set_shell({"disabledPlugins": [], "bar": {"keep": "me"}, "token": "synthetic"})
        os.chmod(self.shell, 0o600)
        s = self.session()
        self.activate(s)
        self.assertEqual(stat.S_IMODE(os.stat(self.shell).st_mode), 0o600)
        cfg = self.get_shell()
        self.assertEqual((cfg["bar"], cfg["token"]), ({"keep": "me"}, "synthetic"))
        s.cleanup()
        self.assertEqual(stat.S_IMODE(os.stat(self.shell).st_mode), 0o600)
        cfg = self.get_shell()
        self.assertEqual((cfg["disabledPlugins"], cfg["bar"], cfg["token"]), ([], {"keep": "me"}, "synthetic"))

    def test_explicitly_shared_mode_is_preserved_too(self):
        os.chmod(self.shell, 0o640)
        s = self.session()
        self.activate(s)
        s.cleanup()
        self.assertEqual(stat.S_IMODE(os.stat(self.shell).st_mode), 0o640)


if __name__ == "__main__":
    unittest.main()
