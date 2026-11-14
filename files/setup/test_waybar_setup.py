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


if __name__ == "__main__":
    unittest.main()
