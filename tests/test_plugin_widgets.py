"""waybar-plugin-widgets and its Waybar rendering (run: python3 -m unittest tests/test_plugin_widgets.py).
A temporary Unix socket stands in for the WhatsApp daemon; omarchy-shell/notify-send/omarchy are fakes on PATH."""
import importlib.machinery
import importlib.util
import json
import os
import select
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HELPER_SRC = os.path.join(ROOT, "files", "scripts", "waybar-plugin-widgets")
SETUP_SRC = os.path.join(ROOT, "files", "setup", "waybar-setup.py")
GLYPH = "\uf232"
WHATSAPP, POWER = "io.github.ricky.whatsapp", "patcastle.power"


class Env(unittest.TestCase):
    def setUp(self):
        sys.dont_write_bytecode = True
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.bin = os.path.join(self.tmp, "bin")
        self.scripts = os.path.join(self.home, ".config", "waybar", "scripts")
        os.makedirs(self.bin)
        os.makedirs(self.scripts)
        self.helper = os.path.join(self.scripts, "waybar-plugin-widgets")
        shutil.copy(HELPER_SRC, self.helper)
        self.log = os.path.join(self.tmp, "calls.log")
        self.sock_path = os.path.join(self.tmp, "wa.sock")
        self.env = dict(os.environ, HOME=self.home, XDG_CONFIG_HOME=os.path.join(self.home, ".config"),
                        XDG_RUNTIME_DIR=self.tmp, PATH=self.bin + os.pathsep + os.environ["PATH"])

    def fake(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(p, 0o755)

    def shell_config(self, **settings):
        d = os.path.join(self.home, ".config", "omarchy")
        os.makedirs(d, exist_ok=True)
        entry = dict({"id": WHATSAPP, "socketPath": self.sock_path}, **settings)
        with open(os.path.join(d, "shell.json"), "w") as f:
            json.dump({"bar": {"layout": {"left": [], "right": [{"id": "omarchy.tray"}, entry]}}}, f)


class Daemon:
    """The WhatsApp bridge's socket: accepts connections one after another and records the hello."""

    def __init__(self, path):
        self.srv = socket.socket(socket.AF_UNIX)
        self.srv.bind(path)
        self.srv.listen(2)

    def accept(self):
        self.srv.settimeout(10)
        self.conn, _ = self.srv.accept()
        self.conn.settimeout(10)
        self.hello = self.conn.makefile("rb").readline()
        return self

    def send(self, **frame):
        self.conn.sendall((json.dumps(frame) + "\n").encode())

    def drop(self):
        self.conn.close()

    def close(self):
        self.srv.close()


STATE = dict(t="state", connection="open", linked=True, needsLogin=False, unread=0,
             chats=[{"jid": "secret@s.whatsapp.net", "name": "Private person", "preview": "private text"}])


class StatusTests(Env):
    def start(self, **settings):
        self.shell_config(**settings)
        self.daemon = Daemon(self.sock_path)
        self.addCleanup(self.daemon.close)
        self.proc = subprocess.Popen([sys.executable, self.helper, "whatsapp-status"], env=self.env,
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: self.proc.poll() is None and (self.proc.kill(), self.proc.wait()))
        self.addCleanup(self.proc.stdout.close)
        self.addCleanup(self.proc.stderr.close)

    def view(self, timeout=10):
        r, _, _ = select.select([self.proc.stdout], [], [], timeout)
        self.assertTrue(r, "no output from the status stream")
        return json.loads(self.proc.stdout.readline())

    def test_badge_progression_flags_retention_disconnect_and_reconnect(self):
        self.start()
        self.assertEqual(self.view()["class"], ["offline"])  # bridge not connected yet
        d = self.daemon.accept()
        self.assertEqual(json.loads(d.hello), {"t": "hello"})
        d.send(**STATE)
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH, ["running"]))
        d.send(t="message", unread=7, jid="x", message={"text": "private text"})
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH + " 7", ["running", "unread"]))  # flags kept on unread-only frame
        d.send(t="chats", unread=120, chats=[])
        self.assertEqual(self.view()["text"], GLYPH + " 99+")
        d.send(t="unrelated", unread=5)
        d.send(**dict(STATE, unread=3))
        self.assertEqual(self.view()["text"], GLYPH + " 3")
        d.drop()
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH, ["offline"]))  # stale badge cleared
        d = self.daemon.accept()  # reconnects by itself
        d.send(**dict(STATE, unread=2))
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH + " 2", ["running", "unread"]))
        self.proc.kill()
        self.proc.wait()
        self.assertNotIn("private", self.proc.stderr.read().decode())

    def test_login_required_is_not_connected(self):
        self.start()
        self.view()
        d = self.daemon.accept()
        d.send(**dict(STATE, linked=False, needsLogin=True, connection="close", unread=4))
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH, ["unlinked"]))
        self.assertIn("login", v["tooltip"])

    def test_count_hidden_by_widget_setting_but_state_kept(self):
        self.start(showUnreadCount=False)
        self.view()
        d = self.daemon.accept()
        d.send(**dict(STATE, unread=9))
        v = self.view()
        self.assertEqual((v["text"], v["class"]), (GLYPH, ["running", "unread"]))

    def test_malformed_input_drops_the_connection_and_it_recovers(self):
        self.start()
        self.view()
        d = self.daemon.accept()
        d.send(**dict(STATE, unread=1))
        self.view()
        d.conn.sendall(b"not json\n")
        self.assertEqual(self.view()["class"], ["offline"])
        d = self.daemon.accept()
        d.send(**dict(STATE, unread=6))
        self.assertEqual(self.view()["text"], GLYPH + " 6")

    def test_stream_exits_promptly_when_waybar_closes_the_pipe(self):
        self.start()
        self.view()
        self.daemon.accept()  # connected and silent
        self.proc.stdout.close()
        self.proc.wait(5)
        self.assertEqual(self.proc.returncode, 0)
        self.assertNotIn("Traceback", self.proc.stderr.read().decode())


class OpenTests(Env):
    def setUp(self):
        super().setUp()
        self.fake("notify-send", 'echo "notify $*" >> %s' % self.log)
        self.guard = os.path.join(self.scripts, "waybar-pill-plugin")
        with open(self.guard, "w") as f:
            f.write('import sys\nsys.exit(0 if sys.argv[1] == "active" else 2)\n')

    def run_open(self, which):
        return subprocess.run([sys.executable, self.helper, "open", which], env=self.env, capture_output=True, text=True)

    def test_summons_the_installed_panel_and_reports_failures(self):
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo ok' % self.log)
        self.assertEqual(self.run_open("power").returncode, 0)
        with open(self.log) as f:
            self.assertEqual(f.read().splitlines(), ["shell shell summon patcastle.power {}"])
        self.fake("omarchy-shell", 'echo "no such widget" >&2; exit 1')
        r = self.run_open("power")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("patcastle.power", r.stderr)
        with open(self.log) as f:
            self.assertIn("notify --app-name=Waybar", f.read().splitlines()[-1])
        self.fake("omarchy-shell", "exit 0")  # success status without the ok reply is still a failure
        self.assertNotEqual(self.run_open("power").returncode, 0)

    def test_inactive_pill_bar_is_not_summoned_from(self):
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo ok' % self.log)
        with open(self.guard, "w") as f:
            f.write('import sys\nsys.exit(1)\n')
        for which in ("whatsapp", "power"):
            r = self.run_open(which)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("not active", r.stderr)
        os.unlink(self.guard)  # no helper at all is also a visible failure, never a native summon
        r = self.run_open("whatsapp")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("missing", r.stderr)
        with open(self.log) as f:
            lines = f.read().splitlines()
        self.assertTrue(lines and all(c.startswith("notify") for c in lines))


class WhatsAppOpenTests(Env):
    def setUp(self):
        super().setUp()
        self.fake("notify-send", 'echo "notify $*" >> %s' % self.log)
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo ok' % self.log)
        self.fake("hyprctl", 'echo "hypr $*" >> %s; exit 1' % self.log)
        with open(os.path.join(self.scripts, "waybar-pill-plugin"), "w") as f:
            f.write("import sys\n")

    def click(self, *args):
        return subprocess.run([sys.executable, self.helper, "open", "whatsapp", *args], env=self.env,
                              capture_output=True, text=True)

    def calls(self):
        with open(self.log) as f:
            return f.read().splitlines()

    def test_explicit_output_pings_then_toggles_its_endpoint_without_hyprctl(self):
        r = self.click("--output", "DP-2")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls(), ["shell waybar-pill-whatsapp-DP-2 ping", "shell waybar-pill-whatsapp-DP-2 toggleNotch"])

    def test_missing_or_invalid_connector_fails_visibly(self):
        for args in ([], ["--output"], ["--output", ""], ["--output", " "], ["--output", "DP-1\nx"],
                     ["--output", "a\x07b"], ["--output", "a\x7fb"], ["--output", "a\x85b"]):
            self.assertNotEqual(self.click(*args).returncode, 0, args)
        r = self.click("--output", "")
        self.assertIn("output", r.stderr)
        calls = self.calls()
        self.assertTrue(calls and all(c.startswith("notify") for c in calls), "no endpoint was contacted")

    def test_unavailable_adapter_fails_visibly_and_never_falls_back_to_summon(self):
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo "Target not found"' % self.log)
        r = self.click("--output", "eDP-1")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("adapter", r.stderr)
        calls = self.calls()
        self.assertTrue(any(c.startswith("notify") for c in calls), "the failure is shown to the user")
        self.assertGreater(sum(c.endswith(" ping") for c in calls), 1, "ping is retried within the wait")
        self.assertTrue(all("toggleNotch" not in c and "summon" not in c for c in calls))

    def test_toggle_must_answer_exactly_ok(self):
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; case "$2" in ping) echo ok;; *) echo "error: busy";; esac'
                  % self.log)
        r = self.click("--output", "DP-2")
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("busy", r.stderr)
        self.assertTrue(all("summon" not in c for c in self.calls()))

    def test_endpoint_can_become_ready_during_the_wait(self):
        marker = os.path.join(self.tmp, "ping-seen")
        self.fake("omarchy-shell", 'echo "shell $*" >> %s\n'
                  'if [ "$2" = ping ] && [ ! -e "%s" ]; then touch "%s"; echo "not ready"; else echo ok; fi'
                  % (self.log, marker, marker))
        r = self.click("--output", "DP-2")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls(), ["shell waybar-pill-whatsapp-DP-2 ping",
                                       "shell waybar-pill-whatsapp-DP-2 ping",
                                       "shell waybar-pill-whatsapp-DP-2 toggleNotch"])

    def test_slow_ping_is_limited_by_the_total_deadline(self):
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; exec sleep 30' % self.log)
        start = time.monotonic()
        r = self.click("--output", "DP-2")
        elapsed = time.monotonic() - start
        self.assertNotEqual(r.returncode, 0)
        self.assertLess(elapsed, 7)
        self.assertGreaterEqual(elapsed, 4.5)
        self.assertTrue(all("toggleNotch" not in c and "summon" not in c for c in self.calls()))

    def test_power_still_summons_after_an_active_guard(self):
        r = subprocess.run([sys.executable, self.helper, "open", "power"], env=self.env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls()[-1], "shell shell summon patcastle.power {}")

    def test_output_flag_is_not_accepted_for_power(self):
        r = subprocess.run([sys.executable, self.helper, "open", "power", "--output", "DP-2"], env=self.env,
                           capture_output=True, text=True)
        self.assertNotEqual(r.returncode, 0)



if __name__ == "__main__":
    unittest.main()
