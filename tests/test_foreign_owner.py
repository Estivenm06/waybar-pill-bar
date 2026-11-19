"""Foreign ownership against real root-owned paths (no mocking of geteuid): the trust policy must refuse a
dedicated directory or record that the effective user does not own, while root-owned ancestors stay acceptable.
Skipped when the tests run as root or the system paths are missing."""
import importlib.util
import os
import unittest
import tempfile
from unittest import mock
from importlib.machinery import SourceFileLoader

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load_plugin():
    loader = SourceFileLoader("waybar_pill_plugin", os.path.join(ROOT, "files", "scripts", "waybar-pill-plugin"))
    mod = importlib.util.module_from_spec(importlib.util.spec_from_loader("waybar_pill_plugin", loader))
    loader.exec_module(mod)
    return mod


@unittest.skipIf(os.geteuid() == 0, "needs a non-root user to see foreign ownership")
class ForeignOwnerTests(unittest.TestCase):
    ROOT_DIR = "/etc/ssl"
    ROOT_FILE = "/etc/hostname"

    @classmethod
    def setUpClass(cls):
        cls.p = load_plugin()
        for path in (cls.ROOT_DIR, cls.ROOT_FILE):
            if not os.path.exists(path) or os.stat(path).st_uid != 0:
                raise unittest.SkipTest("%s is not a root-owned system path here" % path)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = temporary.name
        patch = mock.patch.dict(os.environ, {
            "HOME": self.home,
            "XDG_CONFIG_HOME": os.path.join(self.home, "config"),
            "XDG_STATE_HOME": os.path.join(self.home, "state"),
            "XDG_DATA_HOME": os.path.join(self.home, "data"),
            "XDG_CACHE_HOME": os.path.join(self.home, "cache"),
            "XDG_RUNTIME_DIR": os.path.join(self.home, "runtime"),
        })
        patch.start()
        self.addCleanup(patch.stop)

    def test_root_owned_directory_is_a_fine_ancestor(self):
        self.assertTrue(self.p.validate_directory_chain(self.ROOT_DIR))

    def test_root_owned_directory_is_refused_as_a_dedicated_directory(self):
        with self.assertRaises(self.p.PluginError) as cm:
            self.p.validate_directory_chain(self.ROOT_DIR, own_from=self.ROOT_DIR)
        self.assertIn("not owned by you", str(cm.exception))

    def test_root_owned_file_is_refused_as_a_record(self):
        st = os.stat(self.ROOT_FILE)
        with self.assertRaises(self.p.PluginError) as cm:
            self.p.check_leaf(st, self.ROOT_FILE, state=True)
        self.assertIn("not owned by you", str(cm.exception))

    def test_root_owned_file_is_refused_as_a_prepared_source(self):
        state = self.p.load_state_module()
        with self.assertRaises(state.PluginError) as cm:
            state.open_source(self.ROOT_FILE)
        self.assertIn("not owned by you", str(cm.exception))

    def test_root_owned_directory_is_refused_as_lifecycle_storage(self):
        with mock.patch.object(self.p, "data_dir", return_value=self.ROOT_DIR):
            with self.assertRaises(self.p.PluginError) as cm:
                self.p.stage_generation(self.p.validate_checkout(ROOT))
        self.assertIn("not owned by you", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
