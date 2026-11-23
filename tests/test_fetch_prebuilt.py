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
        self.pin_path = os.path.join(self.root, "files", "setup", "prebuilt.json")
        self.stubs = os.path.join(self.tmp, "stubs")
        os.makedirs(self.stubs)
        self.ldd_log = os.path.join(self.tmp, "ldd.log")
        self.curl_log = os.path.join(self.tmp, "curl.log")
        self.stub("ldd", "exit 0")

    def source_hash(self):
        return subprocess.run(["bash", HASH, self.root], capture_output=True, text=True, check=True).stdout.strip()

    def publish(self, binary=None, info_hash=None, pin=True, arch="x86_64", pin_hash=None, pin_sum=None):
        """Put a compressed binary and a (untrusted) release-info.json on the fake release host; pin it locally."""
        binary = self.binary if binary is None else binary
        os.makedirs(self.rel, exist_ok=True)
        raw = os.path.join(self.tmp, "raw")
        with open(raw, "wb") as f:
            f.write(binary)
        subprocess.run(["zstd", "-q", "-f", "-o", os.path.join(self.rel, "waybar-notch-x86_64.zst"), raw], check=True)
        sha = hashlib.sha256(binary).hexdigest()
        with open(os.path.join(self.rel, "release-info.json"), "w") as f:
            json.dump({"source_hash": info_hash or self.source_hash(), "binary_sha256": sha, "arch": arch}, f)
        if pin:
            self.write_pin(source_hash=pin_hash or self.source_hash(), binary_sha256=pin_sum or sha, arch=arch)

    def write_pin(self, **over):
        c = "b" * 40
        pin = {"source_commit": c, "tag": "notch-" + c, "source_hash": self.source_hash(), "arch": "x86_64",
               "binary_sha256": hashlib.sha256(self.binary).hexdigest()}
        pin.update(over)
        os.makedirs(os.path.dirname(self.pin_path), exist_ok=True)
        with open(self.pin_path, "w") as f:
            json.dump(pin, f)

    def stub(self, name, body):
        p = os.path.join(self.stubs, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(p, 0o755)

    def fetch(self, env=None, want=None):
        e = {"PATH": self.stubs + ":" + os.environ["PATH"], "HOME": self.tmp, "PILL_BAR_RELEASE_URL": "file://" + self.rel}
        e.update(env or {})
        return subprocess.run(["bash", FETCH, self.root, want or self.source_hash(), self.dest], env=e, capture_output=True, text=True, timeout=60)

    def test_pinned_release_is_installed_then_reused_offline(self):
        self.publish()
        r = self.fetch()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(open(self.dest, "rb").read(), self.binary)
        self.assertTrue(os.access(self.dest, os.X_OK))
        shutil.rmtree(self.rel)
        r = self.fetch()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("cached", r.stdout)

    def test_paired_replacement_of_binary_and_release_info_is_refused(self):
        self.publish()
        self.write_pin()  # pin stays as committed
        evil = b"#!/bin/sh\necho evil\n"
        pin = open(self.pin_path).read()
        self.publish(binary=evil, pin=False)
        self.assertEqual(open(self.pin_path).read(), pin)
        self.stub("ldd", 'echo called >> "%s"' % self.ldd_log)
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("checksum mismatch", r.stdout)
        self.assertFalse(os.path.exists(self.dest))
        self.assertFalse(os.path.exists(self.ldd_log))

    def test_remote_claims_cannot_replace_a_missing_or_bad_pin(self):
        self.publish(pin=False)
        self.stub("curl", 'echo called >> "%s"; exit 1' % self.curl_log)
        cases = [None, "{}", "not json", json.dumps({"source_hash": self.source_hash()})]
        for body in cases:
            if os.path.exists(self.pin_path):
                os.unlink(self.pin_path)
            if body is not None:
                os.makedirs(os.path.dirname(self.pin_path), exist_ok=True)
                with open(self.pin_path, "w") as f:
                    f.write(body)
            r = self.fetch()
            self.assertEqual(r.returncode, 1)
            self.assertIn("no trusted pin", r.stdout)
        for over in ({"source_hash": "0" * 64}, {"tag": "notch-" + "c" * 40}, {"binary_sha256": "xyz"}, {"arch": "aarch64"},
                     {"source_commit": "B" * 40, "tag": "notch-" + "B" * 40}):
            self.write_pin(**over)
            r = self.fetch()
            self.assertEqual(r.returncode, 1, over)
            self.assertIn("no trusted pin", r.stdout)
        self.assertFalse(os.path.exists(self.curl_log))
        self.assertFalse(os.path.exists(self.dest))

    def test_tampered_or_unpinned_cache_is_not_reused(self):
        self.publish()
        self.assertEqual(self.fetch().returncode, 0)
        shutil.rmtree(self.rel)
        with open(self.dest, "wb") as f:
            f.write(b"#!/bin/sh\necho evil\n")
        os.chmod(self.dest, 0o755)
        self.assertEqual(self.fetch().returncode, 1)
        self.assertIn(b"evil", open(self.dest, "rb").read())  # never replaced by unauthenticated bytes
        # a changed pin disqualifies an old good cache
        self.publish()
        self.assertEqual(self.fetch().returncode, 0)
        shutil.rmtree(self.rel)
        self.write_pin(binary_sha256="e" * 64)
        self.assertEqual(self.fetch().returncode, 1)

    def test_stale_cache_is_replaced_by_authenticated_download(self):
        self.publish()
        os.makedirs(os.path.dirname(self.dest))
        with open(self.dest, "wb") as f:
            f.write(b"stale")
        r = self.fetch()
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertEqual(open(self.dest, "rb").read(), self.binary)

    def test_symlink_destination_is_refused(self):
        self.publish()
        os.makedirs(os.path.dirname(self.dest))
        os.symlink(os.path.join(self.tmp, "elsewhere"), self.dest)
        self.assertEqual(self.fetch().returncode, 1)
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "elsewhere")))

    def test_checksum_mismatch_is_refused(self):
        self.publish(pin_sum="f" * 64)
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("checksum mismatch", r.stdout)
        self.assertFalse(os.path.exists(self.dest))

    def test_missing_library_is_refused_and_not_published(self):
        self.publish()
        self.stub("ldd", 'echo "libx.so => not found"')
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("libraries", r.stdout)
        self.assertFalse(os.path.exists(self.dest))

    def test_transport_and_archive_failures_fall_back(self):
        self.publish()
        os.unlink(os.path.join(self.rel, "waybar-notch-x86_64.zst"))
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("download failed", r.stdout)
        with open(os.path.join(self.rel, "waybar-notch-x86_64.zst"), "wb") as f:
            f.write(b"not zstd")
        r = self.fetch()
        self.assertEqual(r.returncode, 1)
        self.assertIn("corrupt", r.stdout)

    def test_wrong_architecture_is_refused(self):
        self.publish(arch="aarch64")
        self.assertEqual(self.fetch().returncode, 1)

    def test_opt_out_refuses_even_a_valid_cache(self):
        self.publish()
        self.assertEqual(self.fetch().returncode, 0)
        r = self.fetch({"PILL_BAR_NO_PREBUILT": "1"})
        self.assertEqual(r.returncode, 1)
        self.assertIn("disabled", r.stdout)

    def test_source_hash_changes_with_the_sources_only(self):
        a = self.source_hash()
        os.utime(os.path.join(self.root, "notch", "src", "main.rs"), (1, 1))
        self.assertEqual(self.source_hash(), a)
        with open(os.path.join(self.root, "notch", "src", "main.rs"), "a") as f:
            f.write("// x\n")
        self.assertNotEqual(self.source_hash(), a)


if __name__ == "__main__":
    unittest.main()
