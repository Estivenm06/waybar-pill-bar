"""release.sh publishes candidates only and verifies against the committed pin."""
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMMIT = "d" * 40
BUILT = b"#!/bin/sh\necho built\n"


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.repo = os.path.join(self.tmp, "repo")
        os.makedirs(os.path.join(self.repo, "files", "setup"))
        os.makedirs(os.path.join(self.repo, "notch", "src"))
        shutil.copy2(os.path.join(ROOT, "release.sh"), self.repo)
        for n in ("source-hash.sh", "fetch-prebuilt.sh"):
            shutil.copy2(os.path.join(ROOT, "files", "setup", n), os.path.join(self.repo, "files", "setup"))
        with open(os.path.join(self.repo, "manifest.json"), "w") as f:
            json.dump({"version": "1.2.3"}, f)
        for n, b in (("Cargo.toml", "[package]\n"), ("Cargo.lock", "# l\n"), ("src/main.rs", "fn main(){}\n")):
            with open(os.path.join(self.repo, "notch", n), "w") as f:
                f.write(b)
        self.pin_path = os.path.join(self.repo, "files", "setup", "prebuilt.json")
        with open(self.pin_path, "w") as f:
            f.write("{}\n")
        self.bin = os.path.join(self.tmp, "bin")
        os.mkdir(self.bin)
        self.log = os.path.join(self.tmp, "calls.log")
        self.uploaded = os.path.join(self.tmp, "uploaded")
        self.stub("git", '''case "$*" in
  "status --porcelain") ;;
  "fetch -q origin") ;;
  "rev-parse HEAD"|"rev-parse origin/main") echo %s ;;
  *) exit 9 ;;
esac''' % COMMIT)
        self.stub("cargo", '''echo "cargo $*" >> "%s"
case "$1" in build) mkdir -p "$CARGO_TARGET_DIR/release"; printf '#!/bin/sh\\necho built\\n' > "$CARGO_TARGET_DIR/release/waybar-notch" ;; esac''' % self.log)
        self.stub("gh", '''echo "gh $*" >> "%s"
case "$1" in
  api) case "$GH_API" in
         exists) echo '{}'; exit 0 ;;
         missing) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
         *) echo "gh: Server Error (HTTP 500)" >&2; exit 1 ;;
       esac ;;
  release) mkdir -p "%s"; for a in "$@"; do if [ -f "$a" ]; then cp "$a" "%s/"; fi; done ;;
esac''' % (self.log, self.uploaded, self.uploaded))
        self.env = {"PATH": self.bin + ":" + os.environ["PATH"], "HOME": self.tmp, "GH_API": "missing"}

    def stub(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write("#!/bin/sh\n" + body + "\n")
        os.chmod(p, 0o755)

    def run_release(self, *args, **env):
        e = dict(self.env, **env)
        return subprocess.run(["bash", os.path.join(self.repo, "release.sh"), *args], env=e, capture_output=True,
                              text=True, timeout=60, cwd=self.repo)

    def calls(self):
        return open(self.log).read() if os.path.exists(self.log) else ""

    def test_publish_emits_candidate_pin_for_its_own_bytes_without_touching_the_pin(self):
        r = self.run_release("--publish")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("candidate pin (review and commit as files/setup/prebuilt.json):", r.stdout)
        pin = json.loads(r.stdout[r.stdout.index("{"):])
        src = subprocess.run(["bash", os.path.join(self.repo, "files", "setup", "source-hash.sh"), self.repo],
                             capture_output=True, text=True, check=True).stdout.strip()
        self.assertEqual(pin, {"source_commit": COMMIT, "tag": "notch-" + COMMIT, "source_hash": src,
                               "arch": "x86_64", "binary_sha256": hashlib.sha256(BUILT).hexdigest()})
        self.assertEqual(json.load(open(os.path.join(self.uploaded, "release-info.json"))), pin)
        calls = self.calls()
        self.assertIn("--target " + COMMIT, calls)
        self.assertIn("release create notch-" + COMMIT, calls)
        self.assertNotIn("upload", calls)
        self.assertNotIn("clobber", calls)
        self.assertEqual(open(self.pin_path).read(), "{}\n")

    def test_existing_tag_and_api_failures_abort_before_building(self):
        r = self.run_release("--publish", GH_API="exists")
        self.assertEqual(r.returncode, 1)
        self.assertIn("candidate already exists; refusing to overwrite", r.stdout)
        r = self.run_release("--publish", GH_API="boom")
        self.assertEqual(r.returncode, 1)
        self.assertNotIn("cargo", self.calls())
        self.assertNotIn("release create", self.calls())

    def host(self, binary):
        rel = os.path.join(self.tmp, "rel")
        os.makedirs(rel, exist_ok=True)
        raw = os.path.join(self.tmp, "raw")
        with open(raw, "wb") as f:
            f.write(binary)
        subprocess.run(["zstd", "-q", "-f", "-o", os.path.join(rel, "waybar-notch-x86_64.zst"), raw], check=True)
        return "file://" + rel

    def pin(self, binary):
        src = subprocess.run(["bash", os.path.join(self.repo, "files", "setup", "source-hash.sh"), self.repo],
                             capture_output=True, text=True, check=True).stdout.strip()
        with open(self.pin_path, "w") as f:
            json.dump({"source_commit": "e" * 40, "tag": "notch-" + "e" * 40, "source_hash": src, "arch": "x86_64",
                       "binary_sha256": hashlib.sha256(binary).hexdigest()}, f)

    def test_verify_uses_committed_pin_not_remote_consistency(self):
        url = self.host(BUILT)
        r = self.run_release("--verify", PILL_BAR_RELEASE_URL=url)
        self.assertEqual(r.returncode, 1)  # no pin committed
        self.pin(BUILT)
        r = self.run_release("--verify", PILL_BAR_RELEASE_URL=url)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)  # pin commit differs from HEAD: allowed
        evil = self.host(b"#!/bin/sh\necho evil\n")
        r = self.run_release("--verify", PILL_BAR_RELEASE_URL=evil)
        self.assertEqual(r.returncode, 1)
        self.assertIn("FAILED", r.stdout)


if __name__ == "__main__":
    unittest.main()
