"""Security tests for the optional media helpers and waybar-bgapp.sh
(run: python3 -m unittest tests/test_helper_security.py). Fake commands, in-memory sockets, no network."""
import importlib.util
import ipaddress
import io
import json
import os
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from importlib.machinery import SourceFileLoader
from unittest import mock

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPTS = os.path.join(ROOT, "files", "scripts")
ART = os.path.join(SCRIPTS, "media-art.py")
EQ = os.path.join(SCRIPTS, "media-eq.py")
BGAPP = os.path.join(SCRIPTS, "waybar-bgapp.sh")
POLICY = os.path.join(ROOT, "tests", "fixtures", "network-address-policy.json")
sys.dont_write_bytecode = True


def load(name, path):
    loader = SourceFileLoader(name, path)
    mod = importlib.util.module_from_spec(importlib.util.spec_from_loader(name, loader))
    loader.exec_module(mod)
    return mod


def addrinfo(*ips, port=80):
    out = []
    for ip in ips:
        fam = socket.AF_INET6 if ":" in ip else socket.AF_INET
        out.append((fam, socket.SOCK_STREAM, 6, "", (ip, port) if fam == socket.AF_INET else (ip, port, 0, 0)))
    return out


class FakeSock:
    def __init__(self, response):
        self.response = response
        self.sent = b""

    def sendall(self, data):
        self.sent += data

    def makefile(self, *a, **k):
        return io.BytesIO(self.response)

    def settimeout(self, t):
        pass

    def close(self):
        pass


def http_response(status=200, body=b"", headers=()):
    head = [f"HTTP/1.1 {status} X", f"Content-Length: {len(body)}"] + list(headers)
    return ("\r\n".join(head) + "\r\n\r\n").encode() + body


class FakeNet:
    """Scripted connector: responses are consumed one per connection; every attempt is recorded."""

    def __init__(self, *responses):
        self.responses = list(responses)
        self.attempts = []

    def connect(self, family, sockaddr, timeout):
        self.attempts.append(sockaddr[0])
        return FakeSock(self.responses.pop(0))


class FakeTLS:
    def wrap_socket(self, sock, server_hostname=None):
        return sock


class Clock:
    def __init__(self):
        self.t = 0.0

    def __call__(self):
        return self.t


class ArtTests(unittest.TestCase):
    def setUp(self):
        self.m = load("media_art_under_test", ART)
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.rt = os.path.join(self.tmp, "rt")
        os.mkdir(self.rt, 0o700)

    def resolver(self, *ips):
        def resolve(host, port, type=0):
            try:
                return addrinfo(str(ipaddress.ip_address(host)), port=port)
            except ValueError:
                return addrinfo(*ips, port=port)
        return resolve

    def fetch(self, url, net, ips=("93.184.216.34",), **kw):
        return self.m.fetch_cover(self.rt, url, resolver=self.resolver(*ips), connect=net.connect,
                                  tls_context=FakeTLS(), **kw)

    def files(self):
        return sorted(os.listdir(self.rt))

    def test_policy_table(self):
        with open(POLICY) as f:
            table = json.load(f)
        self.assertGreater(len(table), 50)
        for row in table:
            self.assertEqual(self.m.address_allowed(row["address"]), row["allowed"], row["address"])

    def test_forbidden_literal_makes_no_connection(self):
        net = FakeNet()
        for url in ("http://127.0.0.1/a", "http://[::1]/a", "http://169.254.169.254/a", "http://10.1.1.1/a"):
            with self.assertRaises(self.m.PolicyError):
                self.m.fetch_cover(self.rt, url, resolver=socket.getaddrinfo, connect=net.connect)
        self.assertEqual(net.attempts, [])
        self.assertEqual(self.files(), [])

    def test_mixed_dns_answer_is_rejected_whole(self):
        net = FakeNet()
        with self.assertRaises(self.m.PolicyError):
            self.fetch("http://art.example/a", net, ips=("93.184.216.34", "10.0.0.5"))
        self.assertEqual(net.attempts, [])

    def test_private_redirect_is_never_contacted(self):
        net = FakeNet(http_response(302, headers=["Location: http://127.0.0.1:8080/x"]))
        with self.assertRaises(self.m.PolicyError):
            self.fetch("http://art.example/a", net)
        self.assertEqual(net.attempts, ["93.184.216.34"])
        self.assertEqual(self.files(), [])

    def test_relative_redirect_to_public_host_succeeds(self):
        net = FakeNet(http_response(301, headers=["Location: /b.png"]), http_response(200, b"PNGDATA"))
        path = self.fetch("http://art.example/a", net)
        with open(path, "rb") as f:
            self.assertEqual(f.read(), b"PNGDATA")

    def test_rebinding_shaped_resolver_is_caught_on_second_lookup(self):
        answers = [addrinfo("93.184.216.34"), addrinfo("192.168.1.10")]
        net = FakeNet(http_response(302, headers=["Location: http://art.example/again"]))
        with self.assertRaises(self.m.PolicyError):
            self.m.fetch_cover(self.rt, "http://art.example/a", resolver=lambda *a, **k: answers.pop(0),
                               connect=net.connect)
        self.assertEqual(net.attempts, ["93.184.216.34"])

    def test_https_to_http_downgrade_refused_before_contact(self):
        net = FakeNet(http_response(302, headers=["Location: http://art.example/plain"]))
        with self.assertRaises(self.m.PolicyError):
            self.fetch("https://art.example/a", net)
        self.assertEqual(len(net.attempts), 1)

    def test_userinfo_and_scheme_refused(self):
        net = FakeNet()
        for url in ("http://user:pw@art.example/a", "ftp://art.example/a", "gopher://art.example/"):
            with self.assertRaises(self.m.PolicyError):
                self.fetch(url, net)
        self.assertEqual(net.attempts, [])

    def test_redirect_limit(self):
        net = FakeNet(*[http_response(302, headers=["Location: /n"])] * 12)
        with self.assertRaises(self.m.ArtError):
            self.fetch("http://art.example/a", net)
        self.assertEqual(len(net.attempts), 11)

    def test_exact_cap_accepted_and_cap_plus_one_rejected(self):
        net = FakeNet(http_response(200, b"x" * 100))
        path = self.fetch("http://art.example/a", net, max_bytes=100)
        self.assertEqual(os.path.getsize(path), 100)
        self.assertEqual(stat.S_IMODE(os.stat(path).st_mode) & 0o077, 0)
        shutil.rmtree(self.rt)
        os.mkdir(self.rt, 0o700)
        net = FakeNet(http_response(200, b"x" * 101))
        with self.assertRaises(self.m.ArtError):
            self.fetch("http://art.example/a", net, max_bytes=100)
        self.assertEqual(self.files(), [])

    def test_empty_response_rejected(self):
        with self.assertRaises(self.m.ArtError):
            self.fetch("http://art.example/a", FakeNet(http_response(200, b"")))
        self.assertEqual(self.files(), [])

    def test_failure_keeps_previous_cover_and_leaves_no_temporaries(self):
        old = os.path.join(self.rt, self.m.cache_name("http://art.example/old"))
        with open(old, "wb") as f:
            f.write(b"OLD")
        with self.assertRaises(self.m.ArtError):
            self.fetch("http://art.example/new", FakeNet(http_response(200, b"x" * 101)), max_bytes=100)
        self.assertEqual(self.files(), [os.path.basename(old)])
        with open(old, "rb") as f:
            self.assertEqual(f.read(), b"OLD")

    def test_success_prunes_only_own_regular_files(self):
        old = os.path.join(self.rt, self.m.cache_name("http://art.example/old"))
        with open(old, "wb") as f:
            f.write(b"OLD")
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write("KEEP")
        link = os.path.join(self.rt, self.m.cache_name("http://art.example/link"))
        os.symlink(sentinel, link)
        other = os.path.join(self.rt, "unrelated")
        with open(other, "w") as f:
            f.write("U")
        path = self.fetch("http://art.example/new", FakeNet(http_response(200, b"NEW")))
        self.assertFalse(os.path.exists(old))
        self.assertTrue(os.path.islink(link))
        self.assertTrue(os.path.exists(other))
        with open(sentinel) as f:
            self.assertEqual(f.read(), "KEEP")
        with open(path, "rb") as f:
            self.assertEqual(f.read(), b"NEW")

    def test_symlinked_cache_leaf_is_not_trusted_or_written_through(self):
        url = "http://art.example/a"
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write("KEEP")
        link = os.path.join(self.rt, self.m.cache_name(url))
        os.symlink(sentinel, link)
        path = self.fetch(url, FakeNet(http_response(200, b"NEW")))
        with open(sentinel) as f:
            self.assertEqual(f.read(), "KEEP")
        self.assertFalse(os.path.islink(path))
        with open(path, "rb") as f:
            self.assertEqual(f.read(), b"NEW")

    def test_cache_hit_makes_no_request(self):
        url = "http://art.example/a"
        leaf = os.path.join(self.rt, self.m.cache_name(url))
        with open(leaf, "wb") as f:
            f.write(b"C")
        net = FakeNet()
        self.assertEqual(self.fetch(url, net), leaf)
        self.assertEqual(net.attempts, [])

    def test_total_deadline_stops_fetch_without_publication(self):
        clock = Clock()

        class Slow(FakeNet):
            def connect(s, family, sockaddr, timeout):
                clock.t += 11
                return super().connect(family, sockaddr, timeout)

        with self.assertRaises(self.m.ArtError):
            self.fetch("http://art.example/a", Slow(http_response(200, b"x")), clock=clock)
        self.assertEqual(self.files(), [])

    def test_deadline_between_chunks(self):
        clock = Clock()
        net = FakeNet(http_response(200, b"x" * (3 * self.m.CHUNK)))
        real = io.BytesIO

        class Body(real):
            def read(s, n=-1):
                clock.t += 6
                return super().read(n)

        sock = FakeSock(http_response(200, b"x" * (3 * self.m.CHUNK)))
        sock.makefile = lambda *a, **k: Body(sock.response)
        with self.assertRaises(self.m.ArtError):
            self.m.fetch_cover(self.rt, "http://art.example/a", resolver=self.resolver("93.184.216.34"),
                               connect=lambda f, a, t: sock, clock=clock)
        self.assertEqual(self.files(), [])

    def test_local_cover_rules(self):
        good = os.path.join(self.tmp, "cover art.png")
        with open(good, "wb") as f:
            f.write(b"PNG")
        self.assertEqual(self.m.local_cover("file://" + good.replace(" ", "%20")), good)
        self.assertEqual(self.m.local_cover("file://localhost" + good.replace(" ", "%20")), good)
        link = os.path.join(self.tmp, "link.png")
        os.symlink(good, link)
        fifo = os.path.join(self.tmp, "fifo.png")
        os.mkfifo(fifo)
        big = os.path.join(self.tmp, "big.png")
        with open(big, "wb") as f:
            f.write(b"x" * 11)
        for bad in ("file://" + link, "file://" + fifo, "file://" + self.tmp, "file://evil.example" + good,
                    "file://" + os.path.join(self.tmp, "none"), "file:relative.png"):
            with self.assertRaises(self.m.ArtError, msg=bad):
                self.m.local_cover(bad)
        with self.assertRaises(self.m.ArtError):
            self.m.local_cover("file://" + big, max_bytes=10)

    def test_errors_do_not_echo_url_or_paths(self):
        try:
            self.fetch("http://secret-host.example/private?token=abc", FakeNet(http_response(200, b"")))
        except self.m.ArtError as e:
            self.assertNotIn("secret-host", str(e))
            self.assertNotIn(self.rt, str(e))


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


class MediaCliTests(CliBase):
    def test_art_rejects_bad_runtime_before_touching_player(self):
        self.exe("playerctl", 'echo playerctl >> "$LOG"\necho "http://art.example/a"\n')
        for label, value in self.bad_runtimes().items():
            r = subprocess.run([sys.executable, ART], env=self.env_for(value), capture_output=True, text=True)
            self.assertNotEqual(r.returncode, 0, label)
            self.assertEqual(r.stdout, "", label)
        self.assertEqual(self.calls(), [])
        self.assertEqual(os.listdir(self.rt), [])

    def test_art_prints_local_cover_and_nothing_without_one(self):
        cover = os.path.join(self.tmp, "c.png")
        with open(cover, "wb") as f:
            f.write(b"PNG")
        self.exe("playerctl", 'echo "$ART"\n')
        r = subprocess.run([sys.executable, ART], env=self.env(ART="file://" + cover), capture_output=True, text=True)
        self.assertEqual((r.returncode, r.stdout.strip()), (0, cover), r.stderr)
        r = subprocess.run([sys.executable, ART], env=self.env(ART=""), capture_output=True, text=True)
        self.assertEqual((r.returncode, r.stdout), (0, ""))

    def test_art_refuses_private_destination_end_to_end(self):
        self.exe("playerctl", 'echo "http://127.0.0.1:9/a"\n')
        r = subprocess.run([sys.executable, ART], env=self.env(), capture_output=True, text=True)
        self.assertEqual((r.returncode, r.stdout), (0, ""))
        self.assertEqual(os.listdir(os.path.join(self.rt, "waybar-pill-plugin")), [])

    def test_eq_rejects_bad_runtime_without_player_loop(self):
        self.exe("playerctl", 'echo playerctl >> "$LOG"\n')
        for label, value in self.bad_runtimes().items():
            r = subprocess.run([sys.executable, EQ], env=self.env_for(value), capture_output=True, text=True,
                               timeout=20)
            self.assertNotEqual(r.returncode, 0, label)
        self.assertEqual(self.calls(), [])

    def test_eq_publishes_private_config_and_replaces_atomically(self):
        m = load("media_eq_under_test", EQ)
        path = m.publish_conf(self.rt, m.CAVA_CONF)
        self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o600)
        with open(path) as f:
            self.assertIn("bars = 6", f.read())
        m.publish_conf(self.rt, "x")
        self.assertEqual(os.listdir(self.rt), ["waybar-cava.conf"])

    def test_eq_refuses_symlink_and_fifo_leaf(self):
        m = load("media_eq_under_test2", EQ)
        sentinel = os.path.join(self.tmp, "sentinel")
        with open(sentinel, "w") as f:
            f.write("KEEP")
        leaf = os.path.join(self.rt, "waybar-cava.conf")
        os.symlink(sentinel, leaf)
        with self.assertRaises(OSError):
            m.publish_conf(self.rt, "X")
        os.unlink(leaf)
        os.mkfifo(leaf)
        with self.assertRaises(OSError):
            m.publish_conf(self.rt, "X")
        with open(sentinel) as f:
            self.assertEqual(f.read(), "KEEP")
        self.assertEqual(os.listdir(self.rt), ["waybar-cava.conf"])


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
