"""NotificationRelay.qml against real Quickshell and a real (private) D-Bus session bus (run:
python3 -m unittest tests/test_notification_relay.py). A temporary Unix server stands in for the notch's relay
transport. Nothing here can touch the user's session bus or notification daemon."""
import json
import os
import re
import select
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEST = "org.freedesktop.Notifications"
PATH = "/org/freedesktop/Notifications"


@unittest.skipUnless(all(shutil.which(c) for c in ("quickshell", "dbus-daemon", "gdbus")), "needs quickshell, dbus-daemon, gdbus")
class RelayTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.rt = os.path.join(self.tmp, "rt")
        home = os.path.join(self.tmp, "home")
        os.makedirs(os.path.join(self.rt, "waybar-pill-plugin"), mode=0o700)
        os.makedirs(home)
        self.bus = self.spawn(["dbus-daemon", "--session", "--nofork", "--print-address=1"], stdout=subprocess.PIPE)
        self.address = self.bus.stdout.readline().decode().strip()
        self.assertTrue(self.address)
        self.env = dict(os.environ, HOME=home, XDG_RUNTIME_DIR=self.rt, QT_QPA_PLATFORM="offscreen",
                        DBUS_SESSION_BUS_ADDRESS=self.address)
        self.env.pop("WAYLAND_DISPLAY", None)
        self.server = socket.socket(socket.AF_UNIX)
        self.server.bind(os.path.join(self.rt, "waybar-pill-plugin", "notifications.sock"))
        self.server.listen(1)
        self.addCleanup(self.server.close)
        self.qs = self.spawn(["quickshell", "-p", os.path.join(ROOT, "NotificationRelay.qml")], stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT)
        self.server.settimeout(20)
        self.conn, _ = self.server.accept()
        self.addCleanup(self.conn.close)
        self.buf = b""
        self.assertEqual(self.read_frame(), {"op": "hello", "version": 1})
        self.wait_for_name()

    def spawn(self, cmd, **kw):
        env = getattr(self, "env", None)
        p = subprocess.Popen(cmd, env=env, start_new_session=True, **kw)
        self.addCleanup(lambda: p.poll() is None and (p.kill(), p.wait()))
        return p

    def wait_for_name(self):
        for _ in range(100):
            if self.gdbus("GetServerInformation", check=False).returncode == 0:
                return
            time.sleep(0.1)
        self.fail("Quickshell never claimed the private notification name")

    def gdbus(self, method, *args, check=True):
        r = subprocess.run(["gdbus", "call", "--session", "--dest", DEST, "--object-path", PATH, "--method",
                            DEST + "." + method, *args], env=self.env, capture_output=True, text=True, timeout=10)
        if check:
            self.assertEqual(r.returncode, 0, r.stderr)
        return r

    def notify(self, summary, replaces=0, timeout=5000, actions="['default','Open']"):
        r = self.gdbus("Notify", "Test App", str(replaces), "dialog-information", summary, "body text", actions, "{}", "int32 %d" % timeout)
        return int(re.search(r"uint32 (\d+)", r.stdout).group(1))

    def read_frame(self, timeout=10):
        end = time.time() + timeout
        while b"\n" not in self.buf:
            left = end - time.time()
            if left <= 0:
                self.fail("no frame from the relay; got %r" % self.buf)
            if select.select([self.conn], [], [], left)[0]:
                chunk = self.conn.recv(65536)
                if not chunk:
                    self.fail("relay connection closed")
                self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def read_notify(self):
        """Next notify frame; `closed` echoes of earlier notifications are skipped."""
        while True:
            f = self.read_frame()
            if f["op"] == "notify":
                return f

    def no_frame(self, wait=0.7):
        if select.select([self.conn], [], [], wait)[0]:
            self.buf += self.conn.recv(65536)
        self.assertNotIn(b"\n", self.buf, "unexpected frame %r" % self.buf)

    def send(self, obj):
        self.conn.sendall((json.dumps(obj) + "\n").encode())

    def ready(self):
        self.send({"op": "ready", "version": 1})

    def monitor(self):
        mon = self.spawn(["gdbus", "monitor", "--session", "--dest", DEST], stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        lines = []
        threading.Thread(target=lambda: [lines.append(ln.decode()) for ln in mon.stdout], daemon=True).start()
        time.sleep(0.5)
        return lines

    def wait_signal(self, lines, pattern):
        for _ in range(100):
            if any(re.search(pattern, ln) for ln in lines):
                return
            time.sleep(0.1)
        self.fail("signal %r not observed; saw %r" % (pattern, lines))

    def test_notifications_wait_for_the_acknowledgement_then_flush_latest(self):
        nid = self.notify("early")
        self.notify("early v2", replaces=nid)
        self.no_frame()  # transport not acknowledged yet: nothing may be sent
        self.ready()
        frame = self.read_frame()
        self.assertEqual((frame["op"], frame["id"], frame["summary"]), ("notify", nid, "early v2"))
        self.no_frame()

    def test_closed_before_acknowledgement_is_never_sent(self):
        nid = self.notify("brief")
        self.gdbus("CloseNotification", str(nid))
        time.sleep(0.5)
        self.ready()
        self.no_frame()

    def test_notify_replace_and_external_close(self):
        self.ready()
        time.sleep(0.3)
        mon = self.monitor()
        nid = self.notify("first")
        f = self.read_frame()
        self.assertEqual((f["op"], f["id"], f["app"], f["summary"], f["body"], f["icon"], f["actions"], f["urgency"], f["timeout_ms"]),
                         ("notify", nid, "Test App", "first", "body text", "dialog-information", [["default", "Open"]], 1, 5000))
        self.assertEqual(self.notify("second", replaces=nid, actions="['default','Open','more','More']"), nid)
        f = self.read_frame()
        self.assertEqual((f["id"], f["summary"], f["actions"]), (nid, "second", [["default", "Open"], ["more", "More"]]))
        self.gdbus("CloseNotification", str(nid))
        self.assertEqual(self.read_frame(), {"op": "closed", "id": nid})
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 3\)" % nid)

    def test_default_timeout_is_forwarded_as_default(self):
        self.ready()
        time.sleep(0.3)
        self.notify("t", timeout=-1)
        self.assertEqual(self.read_frame()["timeout_ms"], -1)

    def test_action_dismiss_and_expire_commands_reach_the_sender(self):
        self.ready()
        time.sleep(0.3)
        mon = self.monitor()
        nid = self.notify("act")
        self.read_frame()
        self.send({"op": "action", "id": nid, "key": "default"})
        self.wait_signal(mon, r"ActionInvoked \(uint32 %d, 'default'\)" % nid)

        nid2 = self.notify("dismiss")
        self.assertEqual(self.read_notify()["id"], nid2)
        self.send({"op": "close", "id": nid2, "reason": 2})
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 2\)" % nid2)

        nid3 = self.notify("expire")
        self.assertEqual(self.read_notify()["id"], nid3)
        self.send({"op": "close", "id": nid3, "reason": 1})
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 1\)" % nid3)

    def test_stale_and_unknown_requests_are_harmless(self):
        self.ready()
        time.sleep(0.3)
        nid = self.notify("gone")
        self.read_frame()
        self.send({"op": "close", "id": nid, "reason": 2})
        # the same dismissal again, an unknown id and an unknown action must not break the relay
        self.send({"op": "close", "id": nid, "reason": 2})
        self.send({"op": "action", "id": 99999, "key": "default"})
        nid2 = self.notify("still works")
        seen = []
        while not any(f.get("id") == nid2 and f["op"] == "notify" for f in seen):
            seen.append(self.read_frame())
        self.assertIsNone(self.qs.poll())


if __name__ == "__main__":
    unittest.main()
