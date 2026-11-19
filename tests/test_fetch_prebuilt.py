"""fetch-prebuilt.sh only accepts a download built from these exact sources, with a matching checksum."""
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FETCH = os.path.join(ROOT, "files", "setup", "fetch-prebuilt.sh")
HASH = os.path.join(ROOT, "files", "setup", "source-hash.sh")


class FetchPrebuiltTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.root = os.path.join(self.tmp, "plugin")
        os.makedirs(os.path.join(self.root, "notch", "src"))
        with open(os.path.join(self.root, "manifest.json"), "w") as f:
            json.dump({"version": "9.9.9"}, f)
        for name, body in (("Cargo.toml", "[package]\n"), ("Cargo.lock", "# lock\n"), ("src/main.rs", "fn main(){}\n")):
            with open(os.path.join(self.root, "notch", name), "w") as f:
                f.write(body)
        self.rel = os.path.join(self.tmp, "release")
        os.makedirs(self.rel)
        self.dest = os.path.join(self.tmp, "cache", "waybar-notch")
        self.binary = b"#!/bin/sh\necho notch\n"

    def source_hash(self):
        return subprocess.run(["bash", HASH, self.root], capture_output=True, text=True, check=True).stdout.strip()

    def publish(self, source_hash=None, sum_=None, arch="x86_64"):
        raw = os.path.join(self.tmp, "raw")
        with open(raw, "wb") as f:
            f.write(self.binary)
        subprocess.run(["zstd", "-q", "-f", "-o", os.path.join(self.rel, "waybar-notch-x86_64.zst"), raw], check=True)
        info = {"source_hash": source_hash or self.source_hash(), "binary_sha256": sum_ or hashlib.sha256(self.binary).hexdigest(), "arch": arch}
        with open(os.path.join(self.rel, "release-info.json"), "w") as f:
            json.dump(info, f)

    def fetch(self, env=None):
        e = {"PATH": os.environ["PATH"], "HOME": self.tmp, "PILL_BAR_RELEASE_URL": "file://" + self.rel}
        e.update(env or {})
        return subprocess.run(["bash", FETCH, self.root, self.source_hash(), self.dest], env=e, capture_output=True, text=True, timeout=60)

    def test_matching_release_is_installed_executable(self):
        self.publish()
        r = self.fetch()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(open(self.dest, "rb").read(), self.binary)
        self.assertTrue(os.access(self.dest, os.X_OK))

    def test_release_built_from_other_sources_is_refused(self):
        self.publish(source_hash="0" * 64)
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertFalse(os.path.exists(self.dest))

    def test_checksum_mismatch_is_refused(self):
        self.publish(sum_="f" * 64)
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("checksum mismatch", r.stdout)
        self.assertFalse(os.path.exists(self.dest))

    def test_missing_release_and_opt_out_fall_back_to_a_build(self):
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.publish()
        r = self.fetch({"PILL_BAR_NO_PREBUILT": "1"})
        self.assertEqual(r.returncode, 1)
        self.assertFalse(os.path.exists(self.dest))

    def test_source_hash_changes_with_the_sources_only(self):
        a = self.source_hash()
        os.utime(os.path.join(self.root, "notch", "src", "main.rs"), (1, 1))
        self.assertEqual(self.source_hash(), a)
        with open(os.path.join(self.root, "notch", "src", "main.rs"), "a") as f:
            f.write("// x\n")
        self.assertNotEqual(self.source_hash(), a)


if __name__ == "__main__":
    unittest.main()
