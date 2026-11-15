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
            json.dump({"version": 1, "mode": "plugin", "backup_dir": os.path.join(self.tmp, "bk"), "files": list(files)}, f)


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
        a = os.path.join(self.tmp, "a.css")
        b = os.path.join(self.tmp, "b.css")
        for p in (a, b):
            with open(p, "w") as f:
                f.write("old")
        self.write_marker([{"path": a, "existed": False, "installed_sha256": self.h.sha256_of(a)}])
        with open(a, "w") as f:
            f.write("regenerated")
        with self.assertRaises(self.h.PluginError):
            self.h.record_generated_files([b])
        self.h.record_generated_files([a])
        with open(self.h.marker_path()) as f:
            self.assertEqual(json.load(f)["files"][0]["installed_sha256"], self.h.sha256_of(a))


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
        old = os.path.join(self.tmp, "old.lua")
        new = os.path.join(self.tmp, "created.sh")
        with open(old, "w") as f:
            f.write("mine")
        os.chmod(old, 0o640)
        os.unlink(self.h.marker_path()) if os.path.exists(self.h.marker_path()) else None
        self.assertEqual(self.run_state("begin", old, new).returncode, 0)
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
        self.assertEqual(stat.S_IMODE(os.stat(old).st_mode), 0o640)
        self.assertFalse(os.path.exists(new))
        self.assertFalse(os.path.exists(self.h.marker_path()))

    def _managed(self, content=b"mine\x00\xff", mode=0o600):
        old = os.path.join(self.tmp, "old.bin")
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
        cases = (("copyfileobj", partial_eio),
                 ("copystat", um.Mock(side_effect=OSError(errno.EIO, "synthetic failure"))),
                 ("replace", um.Mock(side_effect=PermissionError(errno.EACCES, "synthetic failure"))))
        for name, fake in cases:
            with self.subTest(name=name):
                if os.path.exists(self.h.marker_path()):
                    os.unlink(self.h.marker_path())
                old, mod = self._managed()
                with open(self.h.marker_path()) as f:
                    record = f.read()
                target = mod.os if name == "replace" else mod.shutil
                with um.patch.object(target, name, fake):
                    with self.assertRaises(mod.PluginError) as cm:
                        mod.uninstall()
                self.assertNotIn("mine", str(cm.exception))
                with open(old, "rb") as f:
                    self.assertEqual(f.read(), b"managed")
                self.assertEqual(stat.S_IMODE(os.stat(old).st_mode), 0o644)
                with open(self.h.marker_path()) as f:
                    self.assertEqual(f.read(), record)
                self.assertEqual([n for n in os.listdir(self.tmp) if n.startswith(".restore.")], [])

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
        f1 = os.path.join(self.tmp, "owned")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        with open(f1, "w") as f:
            f.write("ours")
        self.run_state("finish")
        with open(f1, "w") as f:
            f.write("user edit")
        r = self.run_state("uninstall")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("owned", r.stderr)
        with open(f1) as f:
            self.assertEqual(f.read(), "user edit")
        self.assertTrue(os.path.exists(self.h.marker_path()))

    def test_resetup_refuses_to_overwrite_user_edits(self):
        f1 = os.path.join(self.tmp, "owned")
        self.run_state("begin", f1)
        with open(f1, "w") as f:
            f.write("ours")
        self.run_state("finish")
        with open(f1, "w") as f:
            f.write("user edit")
        self.assertNotEqual(self.run_state("begin", f1).returncode, 0)

    def test_standalone_install_blocks_plugin_setup(self):
        lua = os.path.join(self.home, ".config", "hypr", "autostart.lua")
        os.makedirs(os.path.dirname(lua))
        with open(lua, "w") as f:
            f.write("-- >>> waybar-pill-bar BEGIN (managed block) >>>\n")
        r = self.run_state("begin", os.path.join(self.tmp, "x"))
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
        priv = os.path.join(self.tmp, "priv")
        os.mkdir(priv, 0o700)
        src = os.path.join(priv, "secret.conf")
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

    def test_recorded_loose_root_is_tightened_before_reuse(self):
        f1 = os.path.join(self.tmp, "a")
        with open(f1, "w") as f:
            f.write("x")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        root = self.backup_dir()
        os.chmod(root, 0o755)
        f2 = os.path.join(self.tmp, "b")
        with open(f2, "w") as f:
            f.write("y")
        self.assertEqual(self.run_state("begin", f2).returncode, 0)
        self.assertEqual(stat.S_IMODE(os.stat(root).st_mode), 0o700)

    def test_symlinked_or_missing_recorded_root_is_rejected_untouched(self):
        f1 = os.path.join(self.tmp, "a")
        self.assertEqual(self.run_state("begin", f1).returncode, 0)
        root = self.backup_dir()
        with open(self.h.marker_path()) as f:
            marker = f.read()
        victim = os.path.join(self.tmp, "victim")
        os.mkdir(victim, 0o755)
        shutil.rmtree(root)
        os.symlink(victim, root)
        r = self.run_state("begin", os.path.join(self.tmp, "b"))
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(stat.S_IMODE(os.stat(victim).st_mode), 0o755)
        os.unlink(root)
        r = self.run_state("begin", os.path.join(self.tmp, "b"))
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
