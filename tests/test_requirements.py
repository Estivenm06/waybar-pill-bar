"""requirements.sh installs the manifest's missing packages (stubbed pacman/pkexec/sudo; nothing real is touched)."""
import json
import os
import shutil
import subprocess
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "files", "setup", "requirements.sh")

PACMAN = """#!/bin/bash
d=$(dirname "$0")
case "$1" in
  -T) shift; for p in "$@"; do grep -qx "$p" "$d/installed" 2>/dev/null || echo "$p"; done; [ -z "$(for p in "$@"; do grep -qx "$p" "$d/installed" 2>/dev/null || echo x; done)" ] || exit 127 ;;
  -S) shift; echo "pacman $*" >> "$d/log"; [ -f "$d/fail" ] && exit 1; for p in "$@"; do case $p in -*) ;; *) echo "$p" >> "$d/installed";; esac; done ;;
esac
"""
PKEXEC = "#!/bin/bash\necho \"pkexec $*\" >> \"$(dirname \"$0\")/log\"\nexec \"$@\"\n"


class RequirementsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        for name, body in (("pacman", PACMAN), ("pkexec", PKEXEC)):
            path = os.path.join(self.bin, name)
            open(path, "w").write(body)
            os.chmod(path, 0o755)
        self.manifest = os.path.join(self.tmp, "manifest.json")
        json.dump({"requirements": {"pacman": ["alpha", "beta", "rust"], "optional": ["gamma"]}}, open(self.manifest, "w"))
        self.set_installed([])

    def set_installed(self, names):
        open(os.path.join(self.bin, "installed"), "w").write("".join(n + "\n" for n in names))

    def log(self):
        p = os.path.join(self.bin, "log")
        return open(p).read().splitlines() if os.path.exists(p) else []

    def run_script(self, *args, env=None):
        e = {"PATH": self.bin + ":/usr/bin:/bin", "HOME": self.tmp}
        e.update(env or {})
        return subprocess.run(["bash", SCRIPT, *args, self.manifest], env=e, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=60)

    def test_check_lists_missing_required_and_installs_nothing(self):
        r = self.run_script("--check")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(sorted(r.stdout.split()), ["alpha", "beta", "rust"])
        self.assertEqual(self.log(), [])

    def test_missing_packages_are_installed_through_pkexec_without_a_terminal(self):
        r = self.run_script()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        lines = self.log()
        self.assertTrue(any(l.startswith("pkexec") and "pacman -S --needed --noconfirm alpha beta rust" in l for l in lines), lines)
        self.assertTrue(any("gamma" in l for l in lines), "optional packages are installed too")

    def test_present_packages_are_not_reinstalled(self):
        self.set_installed(["alpha", "beta", "rust", "gamma"])
        r = self.run_script()
        self.assertEqual(r.returncode, 0)
        self.assertEqual(self.log(), [])

    def test_failed_install_reports_and_fails_only_for_required_packages(self):
        open(os.path.join(self.bin, "fail"), "w").close()
        r = self.run_script()
        self.assertEqual(r.returncode, 1)
        self.assertIn("still missing", r.stdout)
        self.set_installed(["alpha", "beta", "rust"])  # only the optional one fails
        r = self.run_script()
        self.assertEqual(r.returncode, 0)
        self.assertIn("optional packages skipped", r.stdout)

    def test_opt_out_only_lists(self):
        r = self.run_script(env={"PILL_BAR_NO_AUTO_INSTALL": "1"})
        self.assertEqual(r.returncode, 1)
        self.assertIn("automatic install is off", r.stdout)
        self.assertEqual(self.log(), [])

    def test_unsafe_package_names_never_reach_pacman(self):
        json.dump({"requirements": {"pacman": ["ok", "--noconfirm; rm -rf ~"]}}, open(self.manifest, "w"))
        r = self.run_script()
        self.assertEqual(r.returncode, 2)
        self.assertEqual(self.log(), [])


if __name__ == "__main__":
    unittest.main()
