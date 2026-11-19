"""Security tests for waybar-bgapp.sh.
(run: python3 -m unittest tests/test_helper_security.py). Fake commands, no desktop access."""
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPTS = os.path.join(ROOT, "files", "scripts")
BGAPP = os.path.join(SCRIPTS, "waybar-bgapp.sh")
sys.dont_write_bytecode = True


class CliBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.rt = os.path.join(self.tmp, "rt")
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.rt, 0o700)
        os.mkdir(self.bin)
        self.log = os.path.join(self.tmp, "calls.log")

    def exe(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body)
        os.chmod(p, 0o755)

    def env(self, **extra):
        e = {"PATH": self.bin + ":/usr/bin:/bin", "HOME": self.tmp, "XDG_RUNTIME_DIR": self.rt,
             "LOG": self.log, "PYTHONDONTWRITEBYTECODE": "1"}
        e.update(extra)
        return e

    def calls(self):
        try:
            with open(self.log) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def bad_runtimes(self):
        loose = os.path.join(self.tmp, "loose")
        os.mkdir(loose, 0o755)
        os.chmod(loose, 0o755)
        real = os.path.join(self.tmp, "real")
        os.mkdir(real, 0o700)
        link = os.path.join(self.tmp, "linkrt")
        os.symlink(real, link)
        return {"missing": None, "relative": "rt", "loose": loose, "symlink": link,
                "absent": os.path.join(self.tmp, "nope")}

    def env_for(self, value):
        e = self.env()
        if value is None:
            del e["XDG_RUNTIME_DIR"]
        else:
            e["XDG_RUNTIME_DIR"] = value
        return e


class BgappTests(CliBase):
    def setUp(self):
        super().setUp()
        self.mark = os.path.join(self.tmp, "launched")
        os.mkdir(os.path.join(self.rt, "waybar-pill-plugin"), 0o700)
        self.exe("hyprctl", 'if [ -e "$MARK" ]; then echo \'[{"class":"app","address":"0x1",'
                            '"workspace":{"name":"special:app"}}]\'; else echo "[]"; fi\n')
        self.exe("uwsm-app", 'echo "launch $*" >> "$LOG"\nsleep 0.3\ntouch "$MARK"\n')

    def run_bgapp(self, mode, ident, env=None, *extra):
        return subprocess.run(["bash", BGAPP, mode, ident, "app", "app-cmd", *extra],
                              env=env or self.env(MARK=self.mark), capture_output=True, text=True, timeout=60)

    def test_invalid_ids_rejected_for_ensure_and_toggle(self):
        for ident in ("../x", "a b", "a/b", "", 'a"b', "a;b", "$(x)", "a\nb"):
            for mode in ("ensure", "toggle"):
                r = self.run_bgapp(mode, ident)
                self.assertNotEqual(r.returncode, 0, (mode, ident))
        self.assertEqual(self.calls(), [])
        self.assertEqual(os.listdir(os.path.join(self.rt, "waybar-pill-plugin")), [])

    def test_bad_runtime_exits_nonzero_without_launch_or_fallback(self):
        for label, value in self.bad_runtimes().items():
            r = self.run_bgapp("ensure", "app", env=self.env_for(value) | {"MARK": self.mark})
            self.assertNotEqual(r.returncode, 0, label)
        self.assertEqual(self.calls(), [])
        self.assertFalse(os.path.exists(self.mark))

    def test_symlinked_and_nonregular_lock_refused_sentinel_untouched(self):
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write("KEEP")
        lock = os.path.join(self.rt, "waybar-pill-plugin", "bgapp-app.lock")
        os.symlink(sentinel, lock)
        self.assertNotEqual(self.run_bgapp("ensure", "app").returncode, 0)
        os.unlink(lock)
        os.mkfifo(lock)
        self.assertNotEqual(self.run_bgapp("ensure", "app").returncode, 0)
        with open(sentinel) as f:
            self.assertEqual(f.read(), "KEEP")
        self.assertEqual(self.calls(), [])

    def test_lock_is_private_and_not_truncated(self):
        lock = os.path.join(self.rt, "waybar-pill-plugin", "bgapp-app.lock")
        with open(lock, "w") as f:
            f.write("keep")
        os.chmod(lock, 0o600)
        r = self.run_bgapp("ensure", "app")
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(lock) as f:
            self.assertEqual(f.read(), "keep")
        os.unlink(lock)
        self.run_bgapp("ensure", "app")
        self.assertEqual(stat.S_IMODE(os.stat(lock).st_mode), 0o600)
        self.assertEqual(os.path.getsize(lock), 0)

    def test_launch_arguments_preserved(self):
        r = self.run_bgapp("ensure", "app", None, "--flag", "two words")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.calls(), ["launch -- app-cmd --flag two words"])

    def test_two_concurrent_ensures_launch_once(self):
        procs = [subprocess.Popen(["bash", BGAPP, "ensure", "app", "app", "app-cmd"],
                                  env=self.env(MARK=self.mark), stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL) for _ in range(2)]
        for p in procs:
            self.assertEqual(p.wait(timeout=60), 0)
        self.assertEqual(len(self.calls()), 1)


if __name__ == "__main__":
    unittest.main()
