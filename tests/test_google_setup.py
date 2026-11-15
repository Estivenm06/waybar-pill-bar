"""Tests for the manual Google Calendar guide (run: python3 -m unittest tests/test_google_setup.py).
The real script runs on a pseudo-terminal with a temporary HOME and fake `xdg-open` / `waybar-notch`
executables; dummy credentials only."""
import importlib.machinery
import importlib.util
import json
import os
import pty
import select
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "files", "scripts", "google-setup.py")

CID = "123456789-abcdef0123.apps.googleusercontent.com"
SECRET = "GOCSPX-dummy-secret-value"
PID = "waybar-notch-123456"


class GuideTests(unittest.TestCase):
    def setUp(self):
        sys.dont_write_bytecode = True
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = os.path.join(self.tmp, "home")
        self.bin = os.path.join(self.tmp, "bin")
        self.log = os.path.join(self.tmp, "calls.log")
        os.makedirs(os.path.join(self.home, ".local/bin"))
        os.makedirs(self.bin)
        self.conf = os.path.join(self.home, ".config/waybar-notch")
        self.data = os.path.join(self.home, ".local/share/waybar-notch")
        self.make_exe(os.path.join(self.bin, "xdg-open"), 'echo "open $*" >> "$CALLS"\nexit ${OPEN_RC:-0}\n')
        self.install_notch(0)

    def make_exe(self, path, body):
        with open(path, "w") as f:
            f.write("#!/bin/sh\n" + body)
        os.chmod(path, 0o755)

    def install_notch(self, rc):
        self.make_exe(os.path.join(self.home, ".local/bin/waybar-notch"), f'echo "notch $*" >> "$CALLS"\nexit {rc}\n')

    def env(self, **extra):
        e = {"HOME": self.home, "PATH": self.bin + ":/usr/bin:/bin", "CALLS": self.log, "TERM": "dumb"}
        e.update(extra)
        return e

    def drive(self, script, env=None):
        """script: list of (expected prompt substring, bytes to send). Returns (exit code, output)."""
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(sys.executable, [sys.executable, SCRIPT], env or self.env())
        out = b""
        pos = 0

        def pump(until=None):
            nonlocal out
            end = time.time() + 30
            while time.time() < end:
                if until is not None and until.encode() in out[pos:]:
                    return True
                r, _, _ = select.select([fd], [], [], 0.5)
                if not r:
                    continue
                try:
                    chunk = os.read(fd, 4096)
                except OSError:
                    return False
                if not chunk:
                    return False
                out += chunk
            return False

        for expect, reply in script:
            if not pump(expect):
                break
            pos = out.rfind(expect.encode()) + len(expect)
            os.write(fd, reply)
        pump()
        _, status = os.waitpid(pid, 0)
        os.close(fd)
        return os.waitstatus_to_exitcode(status), out.decode(errors="replace")

    def full_script(self):
        return [("Project ID (", PID.encode() + b"\n"), ("Press Enter", b"\n"), ("Press Enter", b"\n"),
                ("Press Enter", b"\n"), ("Client ID (", CID.encode() + b"\n"),
                ("Client secret (input hidden): ", SECRET.encode() + b"\n")]

    def calls(self):
        try:
            with open(self.log) as f:
                return f.read().splitlines()
        except FileNotFoundError:
            return []

    def test_success_writes_private_credentials_and_runs_login(self):
        os.makedirs(self.conf)
        os.chmod(self.conf, 0o755)
        rc, out = self.drive(self.full_script())
        self.assertEqual(rc, 0, out)
        self.assertIn("Google Calendar connected", out)
        p = os.path.join(self.conf, "google.json")
        self.assertEqual(stat.S_IMODE(os.stat(p).st_mode), 0o600)
        with open(p) as f:
            self.assertEqual(json.load(f), {"client_id": CID, "client_secret": SECRET})
        self.assertNotIn(SECRET, out)
        self.assertEqual([n for n in os.listdir(self.conf) if n.startswith(".tmp-")], [])
        calls = self.calls()
        self.assertEqual([c for c in calls if c.startswith("notch")], ["notch --google-login"])
        opens = [c for c in calls if c.startswith("open")]
        self.assertEqual(len(opens), 5)
        self.assertTrue(all(c.startswith("open https://console.cloud.google.com/") for c in opens), opens)
        self.assertEqual(len(calls), 6, calls)

    def test_failed_login_is_failure_even_with_stale_token(self):
        os.makedirs(self.data)
        with open(os.path.join(self.data, "google-token.json"), "w") as f:
            f.write('{"refresh_token": "stale"}')
        self.install_notch(1)
        rc, out = self.drive(self.full_script())
        self.assertNotEqual(rc, 0)
        self.assertNotIn("connected", out)
        self.assertIn("login failed", out)
        self.assertNotIn(SECRET, out)

    def test_missing_notch_is_failure(self):
        os.unlink(os.path.join(self.home, ".local/bin/waybar-notch"))
        rc, out = self.drive(self.full_script())
        self.assertNotEqual(rc, 0)
        self.assertNotIn("connected", out)
        self.assertIn("missing", out)

    def test_opener_failure_keeps_manual_urls_and_succeeds(self):
        rc, out = self.drive(self.full_script(), self.env(OPEN_RC="1"))
        self.assertEqual(rc, 0, out)
        self.assertIn("https://console.cloud.google.com/projectcreate", out)
        self.assertIn("open the URL above by hand", out)

    def test_missing_opener_binary_only_prints_instructions(self):
        os.unlink(os.path.join(self.bin, "xdg-open"))
        rc, out = self.drive(self.full_script(), self.env(PATH=self.bin))
        self.assertEqual(rc, 0, out)
        self.assertIn("calendar-json.googleapis.com?project=" + PID, out)

    def test_eof_exits_nonzero_without_traceback_or_credentials(self):
        rc, out = self.drive([("Project ID (", b"\x04")])
        self.assertNotEqual(rc, 0)
        self.assertNotIn("Traceback", out)
        self.assertFalse(os.path.exists(os.path.join(self.conf, "google.json")))

    def test_interrupt_exits_nonzero_without_traceback(self):
        rc, out = self.drive([("Project ID (", b"\x03")])
        self.assertNotEqual(rc, 0)
        self.assertNotIn("Traceback", out)

    def test_noninteractive_changes_nothing(self):
        r = subprocess.run([sys.executable, SCRIPT], stdin=subprocess.DEVNULL, capture_output=True, text=True, env=self.env())
        self.assertEqual(r.returncode, 1)
        self.assertIn("interactive terminal", r.stderr)
        self.assertFalse(os.path.exists(self.conf))
        self.assertEqual(self.calls(), [])

    @unittest.skipIf(os.geteuid() == 0, "directory permissions do not bind root")
    def test_replacement_failure_preserves_old_credentials(self):
        os.makedirs(self.conf)
        with open(os.path.join(self.conf, "project-id"), "w") as f:
            f.write(PID + "\n")
        old = b'{"client_id": "old", "client_secret": "old"}'
        with open(os.path.join(self.conf, "google.json"), "wb") as f:
            f.write(old)
        os.chmod(self.conf, 0o500)
        self.addCleanup(os.chmod, self.conf, 0o700)
        script = [("Press Enter", b"\n")] * 3 + [("Client ID (", CID.encode() + b"\n"),
                                                  ("Client secret (input hidden): ", SECRET.encode() + b"\n")]
        rc, out = self.drive(script)
        self.assertNotEqual(rc, 0)
        self.assertNotIn(SECRET, out)
        with open(os.path.join(self.conf, "google.json"), "rb") as f:
            self.assertEqual(f.read(), old)
        self.assertEqual([c for c in self.calls() if c.startswith("notch")], [])

    def test_saved_project_is_reused(self):
        os.makedirs(self.conf)
        with open(os.path.join(self.conf, "project-id"), "w") as f:
            f.write(PID + "\n")
        script = [("Press Enter", b"\n")] * 3 + self.full_script()[4:]
        rc, out = self.drive(script)
        self.assertEqual(rc, 0, out)
        self.assertIn(f"Reusing project {PID}", out)

    def test_no_browser_automation_left(self):
        with open(SCRIPT) as f:
            src = f.read()
        for word in ("playwright", "remote-debugging", "chromium", "9222"):
            self.assertNotIn(word, src.lower())

    def test_write_private_never_exposes_content_and_cleans_up(self):
        loader = importlib.machinery.SourceFileLoader("gs_under_test", SCRIPT)
        spec = importlib.util.spec_from_loader("gs_under_test", loader)
        m = importlib.util.module_from_spec(spec)
        loader.exec_module(m)
        target = os.path.join(self.conf, "x.json")
        os.makedirs(target)  # rename onto a directory must fail
        with self.assertRaises(RuntimeError) as cm:
            m.write_private(target, "SECRET")
        self.assertNotIn("SECRET", str(cm.exception))
        self.assertEqual(os.listdir(self.conf), ["x.json"])


if __name__ == "__main__":
    unittest.main()
