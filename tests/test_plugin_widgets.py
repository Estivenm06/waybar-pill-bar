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


class RenderTests(Env):
    def setUp(self):
        super().setUp()
        for exe in ("spotify", "omarchy-launch-webapp"):
            self.fake(exe, "exit 0")
        self.catalog = [{"id": WHATSAPP, "enabled": True}, {"id": POWER, "enabled": True}]
        os.environ_saved = dict(os.environ)
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(os.environ_saved)))
        os.environ.update(HOME=self.home, XDG_CONFIG_HOME=os.path.join(self.home, ".config"),
                          PATH=self.bin + os.pathsep + "/usr/bin" + os.pathsep + "/bin")
        spec = importlib.util.spec_from_file_location("waybar_setup_widgets", SETUP_SRC)
        self.m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.m)

    def plugins(self, catalog, rc=0):
        self.fake("omarchy-plugin-list", "exit 0")
        self.fake("omarchy", 'if [ "$1 $2" = "plugin list" ]; then cat <<\'EOF\'\n%s\nEOF\nexit %d; fi' % (json.dumps(catalog), rc))

    def conf(self, signals_used=0):
        c = {"group/left": {"orientation": "horizontal", "modules": ["hyprland/workspaces", "group/apps"]},
             "group/apps": {"modules": []},
             "battery": {"format": "{capacity}%", "on-click": os.path.join(self.m.WAYBAR, "scripts", "waybar-popupctl") + " battery"}}
        for n in range(1, signals_used + 1):
            c["custom/other%d" % n] = {"signal": n}
        return c

    def render(self, apps, conf):
        self.m.render_apps(conf, {"apps": [{"id": i, "background": b} for i, b in apps]})
        return conf

    def test_whatsapp_module_streams_status_and_keeps_the_launcher_on_right_click(self):
        self.plugins(self.catalog)
        conf = self.render([("whatsapp", True), ("spotify", False)], self.conf())
        wa = conf["custom/whatsapp"]
        self.assertEqual((wa["format"], wa["return-type"], wa["tooltip"]), ("{}", "json", True))
        self.assertNotIn("signal", wa)
        self.assertNotIn("interval", wa)
        self.assertTrue(wa["exec"].endswith("waybar-plugin-widgets whatsapp-status"))
        self.assertTrue(wa["on-click"].endswith("waybar-plugin-widgets open whatsapp"))
        self.assertIn("waybar-bgapp.sh toggle whatsapp", wa["on-click-right"])  # background toggle preserved
        self.assertIn("signal", conf["custom/spotify"])  # other apps still use the shared signal
        self.assertEqual(conf["group/apps"]["modules"], ["custom/whatsapp", "custom/spotify"])
        self.assertTrue(conf["battery"]["on-click"].endswith("waybar-popupctl battery"))  # notch Battery & Power page
        self.assertTrue(conf["battery"]["on-click-right"].endswith("waybar-plugin-widgets open power"))
        again = self.render([("whatsapp", True), ("spotify", False)], json.loads(json.dumps(conf)))
        self.assertEqual(again, conf)  # re-rendering the installed config is stable

    def test_whatsapp_only_needs_no_free_signal_but_other_apps_still_do(self):
        self.plugins(self.catalog)
        conf = self.render([("whatsapp", False)], self.conf(signals_used=30))
        self.assertNotIn("signal", conf["custom/whatsapp"])
        with self.assertRaises(self.m.ConfigError):
            before = self.conf(signals_used=30)
            try:
                self.render([("whatsapp", False), ("spotify", False)], before)
            finally:
                self.assertNotIn("custom/whatsapp", before)  # nothing was mutated before the failure

    def test_disabled_plugins_keep_the_original_behaviour_and_undo_a_previous_render(self):
        self.plugins(self.catalog)
        conf = self.render([("whatsapp", True)], self.conf())
        self.plugins([{"id": WHATSAPP, "enabled": False}, {"id": POWER, "enabled": False}])
        conf = self.render([("whatsapp", True)], conf)
        self.assertEqual(conf["custom/whatsapp"]["exec"].split()[1:], ["--waybar-app-status", "whatsapp"])
        self.assertIn("signal", conf["custom/whatsapp"])
        self.assertTrue(conf["battery"]["on-click"].endswith("waybar-popupctl battery"))
        self.assertNotIn("on-click-right", conf["battery"])

    def test_disabled_power_removes_only_managed_right_clicks(self):
        self.plugins(self.catalog)
        conf = self.render([], self.conf())
        self.plugins([{"id": WHATSAPP, "enabled": True}, {"id": POWER, "enabled": False}])
        conf = self.render([], conf)
        self.assertNotIn("on-click-right", conf["battery"])
        conf["battery"]["on-click-right"] = "my-own-command"
        self.assertEqual(self.render([], conf)["battery"]["on-click-right"], "my-own-command")
        # a config rendered by the previous release (left = Power, right = notch) is migrated
        old = self.conf()
        old["battery"]["on-click"] = self.m.WIDGETS_HELPER + " open power"
        old["battery"]["on-click-right"] = os.path.join(self.m.WAYBAR, "scripts", "waybar-popupctl") + " battery"
        self.plugins(self.catalog)
        new = self.render([], old)["battery"]
        self.assertTrue(new["on-click"].endswith("waybar-popupctl battery"))
        self.assertTrue(new["on-click-right"].endswith("waybar-plugin-widgets open power"))

    def test_rendered_battery_clicks_reach_the_notch_and_the_power_panel(self):
        self.plugins(self.catalog)
        battery = self.render([], self.conf())["battery"]
        shutil.copy(os.path.join(ROOT, "files", "scripts", "waybar-popupctl"), self.scripts)
        os.chmod(os.path.join(self.scripts, "waybar-popupctl"), 0o755)
        guard = os.path.join(self.scripts, "waybar-pill-plugin")
        with open(guard, "w") as f:
            f.write("import sys\n")
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo ok' % self.log)
        fifo = os.path.join(self.tmp, "waybar-popupd.fifo")
        os.mkfifo(fifo)
        fd = os.open(fifo, os.O_RDONLY | os.O_NONBLOCK)
        self.addCleanup(os.close, fd)
        r = subprocess.run(["sh", "-c", battery["on-click"]], env=self.env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertRegex(os.read(fd, 100).decode(), r"^battery \d+\n$")
        self.assertFalse(os.path.exists(self.log))  # the Power card was not summoned
        r = subprocess.run(["sh", "-c", battery["on-click-right"]], env=self.env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(self.log) as f:
            self.assertEqual(f.read().splitlines(), ["shell shell summon patcastle.power {}"])

    def test_missing_battery_stays_missing_and_bad_catalogs_are_errors(self):
        self.plugins(self.catalog)
        conf = self.conf()
        del conf["battery"]
        self.assertNotIn("battery", self.render([], conf))
        self.plugins([], rc=1)
        with self.assertRaises(self.m.ConfigError):
            self.render([], self.conf())
        self.plugins({"not": "a list"})
        with self.assertRaises(self.m.ConfigError):
            self.render([], self.conf())

    def test_rendered_exec_runs_against_the_daemon_socket(self):
        self.plugins(self.catalog)
        wa = self.render([("whatsapp", False)], self.conf(signals_used=30))["custom/whatsapp"]
        self.shell_config()
        daemon = Daemon(self.sock_path)
        self.addCleanup(daemon.close)
        env = dict(self.env)
        proc = subprocess.Popen(["sh", "-c", wa["exec"]], env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.addCleanup(lambda: proc.poll() is None and (proc.kill(), proc.wait()))
        self.addCleanup(proc.stdout.close)
        self.assertEqual(json.loads(proc.stdout.readline())["class"], ["offline"])
        d = daemon.accept()
        d.send(**dict(STATE, unread=7))
        v = json.loads(proc.stdout.readline())
        self.assertEqual((v["text"], v["class"]), (GLYPH + " 7", ["running", "unread"]))


def mon(name, x, y, w, h, scale=1.0, transform=0, disabled=False):
    return {"name": name, "x": x, "y": y, "width": w, "height": h, "scale": scale, "transform": transform,
            "disabled": disabled}


def layer(ns, x, y, w, h):
    return {"namespace": ns, "x": x, "y": y, "w": w, "h": h}


def layers_of(**by_output):
    return {o: {"levels": {"0": [], "2": ls}} for o, ls in by_output.items()}


class AnchorTests(Env):
    def setUp(self):
        super().setUp()
        loader = importlib.machinery.SourceFileLoader("waybar_plugin_widgets", HELPER_SRC)
        spec = importlib.util.spec_from_loader("waybar_plugin_widgets", loader)
        self.h = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.h)

    def test_scaled_primary_output_resolves_by_logical_coordinates(self):
        mons = [mon("eDP-1", 0, 0, 1920, 1200, 1.6)]
        layers = layers_of(**{"eDP-1": [layer("omarchy-bar", 0, -20, 1200, 20), layer("waybar", 6, 0, 1188, 26)]})
        self.assertEqual(self.h.whatsapp_output({"x": 41.2, "y": 12}, mons, layers), "eDP-1")

    def test_second_output_with_negative_origin_resolves_its_name(self):
        mons = [mon("eDP-1", 0, 0, 1920, 1200, 1.6), mon("DP-2", -1920, -300, 1920, 1080)]
        layers = layers_of(**{"eDP-1": [layer("waybar", 6, 0, 1188, 26)],
                              "DP-2": [layer("waybar", -1914, -300, 1908, 30)]})
        self.assertEqual(self.h.whatsapp_output({"x": -1850, "y": -290}, mons, layers), "DP-2")

    def test_rotated_output_swaps_logical_size_and_vertical_bars_are_refused(self):
        mons = [mon("DP-3", 100, 50, 1080, 1920, 1.0, transform=1)]  # logical 1920x1080
        layers = layers_of(**{"DP-3": [layer("waybar", 106, 50, 1908, 26)]})
        self.assertEqual(self.h.whatsapp_output({"x": 2010, "y": 60}, mons, layers), "DP-3")
        side = layers_of(**{"DP-3": [layer("waybar", 100, 50, 26, 900)]})
        with self.assertRaises(self.h.AnchorError):
            self.h.whatsapp_output({"x": 110, "y": 100}, mons, side)

    def test_bad_geometry_is_an_error_not_a_guess(self):
        mons = [mon("eDP-1", 0, 0, 1920, 1200, 1.6)]
        bar = layers_of(**{"eDP-1": [layer("waybar", 6, 0, 1188, 26)]})
        for cursor, m, ls in [({"x": 300, "y": 400}, mons, bar),  # not on the bar
                              ({"x": 5000, "y": 5}, mons, bar),  # on no output
                              ({"x": 300, "y": 5}, [mon("eDP-1", 0, 0, 1920, 1200, 1.6, disabled=True)], bar),
                              ({"x": 300, "y": 5}, mons, layers_of(**{"eDP-1": [layer("omarchy-bar", 0, -20, 1200, 20)]})),
                              ({"x": 300, "y": 5}, mons, layers_of(**{"eDP-1": [layer("waybar", 6, 0, 1188, 26),
                                                                                layer("waybar", 6, 0, 1188, 26)]})),
                              ({"x": 300, "y": 5}, mons, layers_of(**{"eDP-1": [layer("waybar", 6, 700, 1188, 26)]})),
                              ({"x": "a", "y": 5}, mons, bar), (None, mons, bar), ({"x": 3, "y": 5}, {}, bar)]:
            with self.assertRaises(self.h.AnchorError):
                self.h.whatsapp_output(cursor, m, ls)


class WhatsAppOpenTests(Env):
    SIG = "testsig_1_2"

    def setUp(self):
        super().setUp()
        self.fake("notify-send", 'echo "notify $*" >> %s' % self.log)
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo ok' % self.log)
        with open(os.path.join(self.scripts, "waybar-pill-plugin"), "w") as f:
            f.write("import sys\n")
        self.q = os.path.join(self.tmp, "q")
        os.makedirs(self.q)
        self.fake("hyprctl", 'echo "hypr $*" >> %s\n[ "$1 $2" = "-i %s" ] || exit 1\ncat %s/"$4".json'
                  % (self.log, self.SIG, self.q))
        os.makedirs(os.path.join(self.tmp, "hypr", self.SIG))
        self.env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)

    def compositor(self, cursor, monitors, layers):
        for n, v in (("cursorpos", cursor), ("monitors", monitors), ("layers", layers)):
            with open(os.path.join(self.q, n + ".json"), "w") as f:
                f.write(v if isinstance(v, str) else json.dumps(v))

    def click(self):
        return subprocess.run([sys.executable, self.helper, "open", "whatsapp"], env=self.env, capture_output=True, text=True)

    def calls(self):
        with open(self.log) as f:
            return f.read().splitlines()

    def test_click_on_the_second_output_targets_its_toggle_endpoint_without_coordinates(self):
        self.compositor({"x": -1850, "y": -290},
                        [mon("eDP-1", 0, 0, 1920, 1200, 1.6), mon("DP-2", -1920, -300, 1920, 1080)],
                        layers_of(**{"eDP-1": [layer("waybar", 6, 0, 1188, 26)], "DP-2": [layer("waybar", -1914, -300, 1908, 30)]}))
        r = self.click()
        self.assertEqual(r.returncode, 0, r.stderr)
        calls = self.calls()
        self.assertEqual(calls[-1], "shell waybar-pill-whatsapp-DP-2 toggleNotch")
        self.assertTrue(all("summon" not in c for c in calls))
        self.assertTrue(all(c.startswith("hypr -i %s -j" % self.SIG) for c in calls if c.startswith("hypr")))

    def test_failures_are_visible_and_never_fall_back_to_summon(self):
        bar = layers_of(**{"eDP-1": [layer("waybar", 6, 0, 1188, 26)]})
        mons = [mon("eDP-1", 0, 0, 1920, 1200, 1.6)]
        self.compositor({"x": 300, "y": 500}, mons, bar)  # pointer not on the bar
        r = self.click()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Waybar", r.stderr)
        self.compositor("not json", mons, bar)
        self.assertNotEqual(self.click().returncode, 0)
        self.compositor({"x": 20, "y": 5}, mons, bar)
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo "Target not found"' % self.log)  # adapter not loaded
        r = self.click()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("adapter", r.stderr)
        self.assertTrue(any(c.startswith("notify") for c in self.calls()), "the failure is shown to the user")
        self.assertTrue(all("summon" not in c for c in self.calls()))
        self.fake("omarchy-shell", 'echo "shell $*" >> %s; echo "error: output is not connected"' % self.log)
        self.assertNotEqual(self.click().returncode, 0, "only an exact ok succeeds")
        self.assertTrue(all("summon" not in c for c in self.calls()))

    def test_scaled_primary_click_targets_its_endpoint(self):
        self.compositor({"x": 41.2, "y": 12}, [mon("eDP-1", 0, 0, 1920, 1200, 1.6)],
                        layers_of(**{"eDP-1": [layer("omarchy-bar", 0, -20, 1200, 20), layer("waybar", 6, 0, 1188, 26)]}))
        r = self.click()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls()[-1], "shell waybar-pill-whatsapp-eDP-1 toggleNotch")

    def test_power_still_summons_after_an_active_guard(self):
        r = subprocess.run([sys.executable, self.helper, "open", "power"], env=self.env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls()[-1], "shell shell summon patcastle.power {}")


if __name__ == "__main__":
    unittest.main()
