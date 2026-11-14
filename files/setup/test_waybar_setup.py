"""Behavior tests for waybar-setup.py (run: python3 -m unittest files/setup/test_waybar_setup.py).
Everything happens in a temporary HOME; the real configuration is never touched."""
import importlib.util
import json
import os
import shutil
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))

BASE_CONFIG = {
    "group/left": {"orientation": "horizontal", "modules": ["custom/launcher", "hyprland/workspaces", "group/apps"]},
    "group/apps": {"orientation": "horizontal", "modules": []},
}

BLOCK = """-- >>> waybar-pill-bar BEGIN (managed block, do not edit) >>>
o.exec_on_start("omarchy-toggle-bar on")
-- Popup animation: pop in
hl.layer_rule({ match = { namespace = "waybar-popup" }, animation = "popin 88%" })
-- <<< waybar-pill-bar END <<<
"""


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        for exe in ("spotify", "steam", "omarchy-launch-webapp"):
            p = os.path.join(self.bin, exe)
            open(p, "w").close()
            os.chmod(p, 0o755)
        old = dict(os.environ)
        os.environ.update(HOME=self.home, PATH=self.bin, XDG_CONFIG_HOME=os.path.join(self.home, ".config"))
        self.addCleanup(lambda: (os.environ.clear(), os.environ.update(old)))
        spec = importlib.util.spec_from_file_location("waybar_setup_under_test", os.path.join(HERE, "waybar-setup.py"))
        self.m = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.m)

    def cfg(self, apps):
        return {"apps": [{"id": i, "background": b} for i, b in apps]}

    def render(self, apps, conf=None):
        conf = json.loads(json.dumps(conf or BASE_CONFIG))
        self.m.render_apps(conf, self.cfg(apps))
        return conf

    def test_empty_then_reselected_restores_group_and_removal_drops_stale_modules(self):
        conf = self.render([("spotify", False), ("steam", False)])
        self.assertEqual(conf["group/apps"]["modules"], ["custom/spotify", "custom/steam"])
        self.assertEqual(conf["group/left"]["modules"], ["custom/launcher", "hyprland/workspaces", "group/apps"])
        conf = self.render([], conf)
        self.assertNotIn("group/apps", conf["group/left"]["modules"])
        self.assertFalse([k for k in conf if k.startswith("custom/")])
        conf = self.render([("steam", True)], conf)
        self.assertEqual(conf["group/left"]["modules"], ["custom/launcher", "hyprland/workspaces", "group/apps"])
        self.assertEqual(conf["group/apps"]["modules"], ["custom/steam"])
        self.assertNotIn("custom/spotify", conf)

    def test_group_returns_at_the_end_when_workspaces_module_is_absent(self):
        base = {"group/left": {"modules": ["custom/launcher"]}}
        conf = self.render([("steam", False)], base)
        self.assertEqual(conf["group/left"]["modules"], ["custom/launcher", "group/apps"])

    def test_order_background_and_shell_quoting(self):
        conf = self.render([("whatsapp", True), ("spotify", False)])
        self.assertEqual(conf["group/apps"]["modules"], ["custom/whatsapp", "custom/spotify"])
        wa = conf["custom/whatsapp"]
        self.assertTrue(wa["exec"].endswith("waybar-notch' --waybar-app-status whatsapp") or wa["exec"].endswith("waybar-notch --waybar-app-status whatsapp"))
        self.assertNotIn("interval", wa)
        self.assertIn("waybar-bgapp.sh toggle whatsapp", wa["on-click"])
        self.assertIn("'web\\.whatsapp\\.com'", wa["on-click"])
        self.assertIn("waybar-app.sh click", conf["custom/spotify"]["on-click"])

    def test_script_path_with_spaces_is_quoted(self):
        self.m.WAYBAR = os.path.join(self.tmp, "my bar")
        conf = self.render([("steam", True)])
        self.assertTrue(conf["custom/steam"]["on-click"].startswith("'%s/scripts/waybar-bgapp.sh'" % self.m.WAYBAR))

    def test_status_signal_is_shared_and_avoids_other_modules(self):
        base = json.loads(json.dumps(BASE_CONFIG))
        base["custom/other"] = {"signal": 1}
        base["custom/media"] = {"signal": 2}
        conf = self.render([("spotify", False), ("steam", True)], base)
        self.assertEqual({conf["custom/spotify"]["signal"], conf["custom/steam"]["signal"]}, {3})
        again = self.render([("spotify", False)], conf)  # its own old signal must not count as taken
        self.assertEqual(again["custom/spotify"]["signal"], 3)

    def test_all_signals_taken_is_an_error_and_leaves_config_untouched(self):
        base = json.loads(json.dumps(BASE_CONFIG))
        for n in range(1, 31):
            base["custom/x%d" % n] = {"signal": n}
        conf = json.loads(json.dumps(base))
        with self.assertRaises(self.m.ConfigError):
            self.m.render_apps(conf, self.cfg([("steam", False)]))
        self.assertEqual(conf, base)

    def test_autostart_only_bg_rules_inside_the_block_change(self):
        text = "-- mine\nbefore()\n" + BLOCK + "after()\n"
        out = self.m.replace_bg_rules(text, ["rule -- @bg"])
        self.assertTrue(out.startswith("-- mine\nbefore()\n") and out.endswith("after()\n"))
        self.assertIn("rule -- @bg\n-- Popup animation:", out)
        again = self.m.replace_bg_rules(out, [])
        self.assertEqual(again, text)  # fully reversible, no @bg residue

    def test_missing_or_duplicate_anchors_change_nothing(self):
        for bad in ("nothing here\n", BLOCK + BLOCK, BLOCK.replace("-- Popup animation:", "-- renamed:")):
            with self.assertRaises(self.m.ConfigError) as cm:
                self.m.replace_bg_rules(bad, ["x -- @bg"])
            self.assertIn("install.sh", str(cm.exception))

    def test_plugin_render_drops_startup_launches_but_keeps_window_rules(self):
        cfg = self.cfg([("steam", True)])
        std = self.m.bg_lines(cfg)
        plug = self.m.bg_lines(cfg, plugin=True)
        self.assertEqual(len(std), 2)
        self.assertEqual(len(plug), 1)
        self.assertIn("o.window(", plug[0])
        self.assertFalse([ln for ln in plug if "exec_on_start" in ln])
        src = os.path.join(self.tmp, "in.lua")
        with open(src, "w") as f:
            f.write(BLOCK)
        for plugin, expect_hide in ((False, True), (True, False)):
            out = os.path.join(self.tmp, "out%d.lua" % plugin)
            self.m.cmd_render_autostart(src, out, plugin)
            with open(out) as f:
                self.assertEqual('o.exec_on_start("omarchy-toggle-bar on")' in f.read(), expect_hide)

    def test_apply_in_plugin_mode_rolls_back_both_files_and_never_starts_services(self):
        wb = self.m.WAYBAR
        auto = os.path.join(self.home, ".config", "hypr", "autostart.lua")
        os.makedirs(os.path.join(wb, "scripts"))
        os.makedirs(os.path.dirname(auto))
        os.makedirs(os.path.dirname(self.m.CONFIG), exist_ok=True)
        with open(self.m.CONFIG, "w") as f:
            json.dump(self.cfg([("steam", True)]), f)
        conf_text = json.dumps(BASE_CONFIG, indent=2) + "\n"
        with open(os.path.join(wb, "config.jsonc"), "w") as f:
            f.write(conf_text)
        with open(auto, "w") as f:
            f.write(BLOCK)
        log = os.path.join(self.tmp, "calls.log")
        stubs = {
            "systemctl": 'echo "systemctl $*" >> "%s"' % log,
            "hyprctl": 'echo "hyprctl $*" >> "%s"; [ "$1" = configerrors ] && [ -f "%s/bad" ] && /usr/bin/grep -q o.window "%s" && echo "bad rule"; exit 0' % (log, self.tmp, auto),
        }
        for n, body in stubs.items():
            with open(os.path.join(self.bin, n), "w") as f:
                f.write("#!/bin/sh\n" + body + "\n")
            os.chmod(os.path.join(self.bin, n), 0o755)
        helper = os.path.join(wb, "scripts", "waybar-pill-plugin")
        with open(helper, "w") as f:
            f.write('#!/bin/sh\ncase "$1" in mode) echo plugin;; active) exit 1;; *) echo "helper $*" >> "%s";; esac\n' % log)
        os.chmod(helper, 0o755)
        self.m.sys.executable = "/bin/sh"  # run the stub helper directly
        with open(auto) as f:
            before = f.read()
        open(os.path.join(self.tmp, "bad"), "w").close()  # Hyprland "rejects" the new rules
        with self.assertRaises(self.m.ConfigError):
            self.m.cmd_apply()
        with open(auto) as f:
            self.assertEqual(f.read(), before)
        with open(os.path.join(wb, "config.jsonc")) as f:
            self.assertEqual(f.read(), conf_text)
        os.unlink(os.path.join(self.tmp, "bad"))
        self.m.cmd_apply()
        with open(auto) as f:
            text = f.read()
        self.assertIn("o.window(", text)
        self.assertNotIn("exec_on_start(\"%s" % wb, text)
        with open(log) as f:
            calls = f.read()
        self.assertNotIn("systemctl", calls)
        self.assertIn("helper record-generated", calls)
        self.assertNotIn("helper reload", calls)  # plugin disabled: saved only


if __name__ == "__main__":
    unittest.main()
