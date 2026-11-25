"""NotificationRelay.qml against real Quickshell and a real (private) D-Bus session bus (run:
python3 -m unittest tests/test_notification_relay.py). A temporary Unix server stands in for the notch's relay
transport (protocol version 2: sequence numbers, acknowledgements, bounded queues). Nothing here can touch the user's
session bus or notification daemon.

Large synthetic notifications cannot be passed as one gdbus argument (a Linux argv string is capped near 128 KiB), so
they are sent by a tiny Gio D-Bus client run with the system Python (the default mise Python lacks Gio); it reads
its payload from stdin and connects only to the fixture's private bus address."""
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
SYSTEM_PYTHON = "/usr/bin/python3"
MAX_OUTSTANDING = 32
MAX_SLOTS = 64

SENDER = r'''
import json, sys
from gi.repository import Gio, GLib

req = json.load(sys.stdin)
flags = Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION
conn = Gio.DBusConnection.new_for_address_sync(req["address"], flags, None, None)
DEST = "org.freedesktop.Notifications"
PATH = "/org/freedesktop/Notifications"
last = 0
ids = []
for op in req["ops"]:
    for i in range(op.get("repeat", 1)):
        if op["method"] == "Notify":
            a = op["args"]
            replaces = last if a.get("replaces") == "last" else int(a.get("replaces", 0))
            variant = GLib.Variant("(susssasa{sv}i)", (
                a.get("app", "Test App"), replaces, a.get("icon", "dialog-information"),
                a.get("summary", "s").replace("{i}", str(i)), a.get("body", "body text"),
                a.get("actions", ["default", "Open"]), {}, a.get("timeout", 5000)))
            reply = conn.call_sync(DEST, PATH, DEST, "Notify", variant, GLib.VariantType("(u)"),
                                   Gio.DBusCallFlags.NONE, 60000, None)
            last = reply.unpack()[0]
            ids.append(last)
        elif op["method"] == "CloseNotification":
            target = last if op.get("id") == "last" else int(op["id"])
            conn.call_sync(DEST, PATH, DEST, "CloseNotification", GLib.Variant("(u)", (target,)),
                           GLib.VariantType("()"), Gio.DBusCallFlags.NONE, 60000, None)
print(json.dumps(ids))
'''


def have_system_gio():
    try:
        return subprocess.run([SYSTEM_PYTHON, "-c", "from gi.repository import Gio"], capture_output=True, timeout=20).returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


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
        self.assertEqual(self.read_frame(), {"op": "hello", "version": 2})
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

    def notify(self, summary, replaces=0, timeout=5000, actions="['default','Open']", hints="{}"):
        r = self.gdbus("Notify", "Test App", str(replaces), "dialog-information", summary, "body text", actions, hints, "int32 %d" % timeout)
        return int(re.search(r"uint32 (\d+)", r.stdout).group(1))

    def sender(self, ops, timeout=120):
        """Run the Gio sender (system Python) against the private bus; payloads travel over stdin, never argv."""
        if not have_system_gio():
            self.skipTest("the system python3 cannot import Gio")
        env = {k: v for k, v in self.env.items() if not k.startswith("PYTHON")}
        r = subprocess.run([SYSTEM_PYTHON, "-c", SENDER], input=json.dumps({"address": self.address, "ops": ops}), env=env,
                           capture_output=True, text=True, timeout=timeout)
        # the diagnostics deliberately omit the payload
        self.assertEqual(r.returncode, 0, "sender failed: " + r.stderr[-300:])
        return json.loads(r.stdout)

    @staticmethod
    def op_notify(repeat=1, **args):
        return {"method": "Notify", "args": args, "repeat": repeat}

    def read_frame(self, timeout=10, ack=True):
        """Next frame; notify/closed events are acknowledged unless `ack` is false."""
        end = time.time() + timeout
        while b"\n" not in self.buf:
            left = end - time.time()
            if left <= 0:
                self.fail("no frame from the relay; got %d bytes" % len(self.buf))
            if select.select([self.conn], [], [], left)[0]:
                chunk = self.conn.recv(1 << 20)
                if not chunk:
                    self.fail("relay connection closed")
                self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        frame = json.loads(line)
        if ack and frame.get("op") in ("notify", "closed"):
            self.send({"op": "ack", "seq": frame["seq"]})
        return frame

    def read_notify(self):
        """Next notify frame; `closed` echoes of earlier notifications are skipped."""
        while True:
            f = self.read_frame()
            if f["op"] == "notify":
                return f

    def collect(self, ack=True, quiet=0.8):
        """Every frame until the relay has been silent for `quiet` seconds."""
        frames = []
        while True:
            if b"\n" not in self.buf:
                if not select.select([self.conn], [], [], quiet)[0]:
                    return frames
                chunk = self.conn.recv(1 << 20)
                if not chunk:
                    self.fail("relay connection closed")
                self.buf += chunk
                continue
            frames.append(self.read_frame(ack=ack))

    def no_frame(self, wait=0.7):
        if select.select([self.conn], [], [], wait)[0]:
            self.buf += self.conn.recv(65536)
        self.assertNotIn(b"\n", self.buf, "unexpected frame (%d bytes)" % len(self.buf))

    def send(self, obj):
        self.conn.sendall((json.dumps(obj) + "\n").encode())

    def ready(self):
        self.send({"op": "ready", "version": 2})

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

    def wait_signal_count(self, lines, pattern, count, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if sum(1 for ln in list(lines) if re.search(pattern, ln)) >= count:
                return
            time.sleep(0.1)
        self.fail("fewer than %d signals matching %r (saw %d lines)" % (count, pattern, len(lines)))

    def assert_still_works(self, summary="normal"):
        """The transport survived: an ordinary notification forwards and Quickshell is still running."""
        self.assertIsNone(self.qs.poll())
        nid = self.notify(summary)
        frames = []
        while not any(f["op"] == "notify" and f["id"] == nid for f in frames):
            frames.append(self.read_frame())
        return [f for f in frames if f["op"] == "notify" and f["id"] == nid][0]

    # ---------------------------------------------------------------- baseline behaviour

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
        f = self.read_frame()
        self.assertEqual((f["op"], f["id"]), ("closed", nid))
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 3\)" % nid)

    def test_suppress_sound_hint_is_forwarded(self):
        self.ready()
        time.sleep(0.3)
        self.notify("plain")
        self.assertIs(self.read_frame()["suppress_sound"], False)
        self.notify("quiet", hints="{'suppress-sound': <true>}")
        self.assertIs(self.read_frame()["suppress_sound"], True)
        self.notify("one", hints="{'suppress-sound': <1>}")
        self.assertIs(self.read_frame()["suppress_sound"], True)
        self.notify("other", hints="{'suppress-sound': <'yes'>}")
        self.assertIs(self.read_frame()["suppress_sound"], False)

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

    # ---------------------------------------------------------------- protocol v2: sequence numbers and credits

    def test_browser_copy_of_a_whatsapp_message_is_not_forwarded(self):
        self.ready()
        time.sleep(0.3)
        dup = self.gdbus("Notify", "Brave", "0", "", "Meta AI", "web.whatsapp.com\n\nhello", "['default','Activate']",
                         "{'desktop-entry': <'brave-browser'>}", "int32 -1")
        self.assertEqual(dup.returncode, 0)
        self.notify("ordinary toast")
        frame = self.read_frame()
        self.assertEqual((frame["op"], frame["summary"]), ("notify", "ordinary toast"))

    def test_events_carry_increasing_sequence_numbers(self):
        self.ready()
        time.sleep(0.3)
        for name in ("one", "two", "three"):
            self.notify(name)
        seqs = [f["seq"] for f in self.collect() if f["op"] == "notify"]
        self.assertEqual(seqs, [1, 2, 3])

    def test_a_version_one_ready_is_ignored(self):
        self.send({"op": "ready", "version": 1})
        self.notify("wrong version")
        self.no_frame()
        self.ready()
        self.assertEqual(self.read_frame()["summary"], "wrong version")

    def test_unknown_and_duplicate_acknowledgements_mint_no_credits(self):
        self.ready()
        time.sleep(0.3)
        self.sender([self.op_notify(summary="n{i}", repeat=MAX_OUTSTANDING + 8)])
        first = self.collect(ack=False)
        self.assertEqual(len(first), MAX_OUTSTANDING)
        self.send({"op": "ack", "seq": 999999})  # unknown
        self.send({"op": "ack", "seq": "1"})  # wrong type
        self.send({"op": "ack", "seq": first[0]["seq"]})  # one genuine credit
        self.assertEqual(len(self.collect(ack=False)), 1)
        self.send({"op": "ack", "seq": first[0]["seq"]})  # duplicates and unknowns still mint nothing
        self.send({"op": "ack", "seq": first[0]["seq"]})
        self.send({"op": "ack", "seq": 888888})
        self.assertEqual(self.collect(ack=False), [])

    def test_no_more_than_32_events_are_outstanding_without_acknowledgements(self):
        self.ready()
        time.sleep(0.3)
        ids = self.sender([self.op_notify(summary="flood {i}", repeat=40)])
        first = self.collect(ack=False)
        self.assertEqual([f["op"] for f in first], ["notify"] * MAX_OUTSTANDING)
        self.assertEqual(len({f["id"] for f in first}), MAX_OUTSTANDING)
        self.assertEqual(self.collect(ack=False, quiet=0.5), [], "a paused receiver gets no more than 32 events")
        for f in first:
            self.send({"op": "ack", "seq": f["seq"]})
        rest = self.collect()
        self.assertEqual(len(rest), 8)
        self.assertEqual({f["id"] for f in first} | {f["id"] for f in rest}, set(ids))
        self.assertEqual([f["seq"] for f in first + rest], list(range(1, 41)))

    def test_repeated_replacements_coalesce_to_the_newest_snapshot(self):
        self.ready()
        time.sleep(0.3)
        self.sender([self.op_notify(summary="start"),
                     self.op_notify(summary="update {i}", replaces="last", repeat=60),
                     self.op_notify(summary="final", replaces="last")])
        first = self.collect(ack=False)
        self.assertEqual(len(first), 1, "one in-flight event per notification id")
        self.send({"op": "ack", "seq": first[0]["seq"]})
        second = self.collect(ack=False)
        self.assertEqual([f["summary"] for f in second], ["final"], "the latest pending snapshot flushes after the acknowledgement")
        self.assertEqual(second[0]["id"], first[0]["id"])
        self.send({"op": "ack", "seq": second[0]["seq"]})
        self.assertEqual(self.collect(), [], "intermediate replacements were coalesced away")

    def test_distinct_id_flood_is_bounded_and_overflow_expires_the_sender(self):
        self.ready()
        time.sleep(0.3)
        mon = self.monitor()
        ids = self.sender([self.op_notify(summary="flood {i}", repeat=MAX_SLOTS + 36)])
        first = self.collect(ack=False)
        self.assertEqual(len(first), MAX_OUTSTANDING)
        overflow = 36
        self.wait_signal_count(mon, r"NotificationClosed \(uint32 \d+, uint32 1\)", overflow)
        for f in first:  # the paused receiver resumes: acknowledge what it was holding
            self.send({"op": "ack", "seq": f["seq"]})
        rest = self.collect()  # acknowledging releases the queued remainder, but never past 64 tracked slots
        forwarded = {f["id"] for f in first + rest if f["op"] == "notify"}
        self.assertEqual(len(forwarded), MAX_SLOTS)
        expired = {int(m.group(1)) for ln in mon for m in [re.search(r"NotificationClosed \(uint32 (\d+), uint32 1\)", ln)] if m}
        self.assertEqual(expired, set(ids) - forwarded, "exactly the notifications that found no slot were expired")
        late = self.notify("late")  # all 64 slots are held: the newcomer is expired, never retained
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 1\)" % late)
        self.no_frame()
        victim = sorted(forwarded)[0]
        self.send({"op": "close", "id": victim, "reason": 2})  # the card was dismissed: its slot is released after the ack
        f = self.read_frame()
        self.assertEqual((f["op"], f["id"]), ("closed", victim))
        self.assertEqual(self.assert_still_works("after release")["summary"], "after release")

    def test_close_flood_orders_closed_after_notify_and_stays_bounded(self):
        self.ready()
        time.sleep(0.3)
        ops = []
        for _ in range(100):
            ops.append(self.op_notify(summary="blink"))
            ops.append({"method": "CloseNotification", "id": "last"})
        ids = self.sender(ops)
        first = self.collect(ack=False)
        self.assertLessEqual(len(first), MAX_OUTSTANDING)
        self.assertTrue(all(f["op"] == "notify" for f in first), "closed events wait behind the unacknowledged notify of their id")
        for f in first:
            self.send({"op": "ack", "seq": f["seq"]})
        rest = self.collect()
        frames = first + rest
        notified = [f["id"] for f in frames if f["op"] == "notify"]
        closed = [f["id"] for f in frames if f["op"] == "closed"]
        self.assertEqual(sorted(notified), sorted(closed), "every forwarded notification gets exactly one closed event")
        self.assertTrue(set(notified) <= set(ids))
        at = {f["id"]: n for n, f in enumerate(frames) if f["op"] == "notify"}
        for n, f in enumerate(frames):
            if f["op"] == "closed":
                self.assertLess(at[f["id"]], n)
        self.assertEqual(self.assert_still_works()["summary"], "normal")

    def test_slots_are_released_after_the_closed_event_is_acknowledged(self):
        self.ready()
        time.sleep(0.3)
        for i in range(MAX_SLOTS + 2):
            nid = self.notify("cycle %d" % i)
            self.assertEqual((self.read_frame()["id"]), nid)
            self.gdbus("CloseNotification", str(nid))
            f = self.read_frame()
            self.assertEqual((f["op"], f["id"]), ("closed", nid))
        self.assertEqual(self.assert_still_works("slot free")["summary"], "slot free")

    # ---------------------------------------------------------------- oversized sender content

    def check_oversized(self, **args):
        ids = self.sender([self.op_notify(**args)])
        self.ready()
        frame = self.read_frame()
        self.assertEqual((frame["op"], frame["id"]), ("notify", ids[0]))
        self.assertEqual(self.assert_still_works("second normal")["summary"], "second normal")
        return frame

    def test_oversized_ascii_body_is_truncated_and_does_not_kill_the_relay(self):
        frame = self.check_oversized(body="x" * (600 * 1024))
        self.assertEqual(frame["body"], "x" * 16384)

    def test_oversized_multibyte_body_is_truncated_without_splitting_a_surrogate_pair(self):
        body = "a" + "\U0001F600" * 300000  # the 16384-unit cut would fall inside a pair
        frame = self.check_oversized(body=body)
        self.assertEqual(frame["body"], "a" + "\U0001F600" * 8191)
        self.assertEqual(len(frame["body"].encode("utf-16-le")) // 2, 16383)
        frame["body"].encode("utf-8")  # no lone surrogate survived

    def test_oversized_two_byte_text_is_bounded_by_utf16_units(self):
        frame = self.check_oversized(body="\u00e9" * 300000, summary="\u00e9" * 9000, app="\u00e9" * 600)
        self.assertEqual((len(frame["body"]), len(frame["summary"]), len(frame["app"])), (16384, 4096, 256))

    def test_escape_heavy_body_cannot_exceed_the_frame_limit(self):
        unit = '"\\\n\x01'
        frame = self.check_oversized(body=unit * 150000)
        self.assertEqual(frame["body"], (unit * 150000)[:16384])

    def test_overlong_icon_uses_the_fallback_not_a_truncated_path(self):
        frame = self.check_oversized(icon="/" + "a" * 5000)
        self.assertEqual(frame["icon"], "")

    def test_excess_actions_and_overlong_labels_and_keys_are_bounded(self):
        actions = ["default", "Open", "k" * 129, "key too long"]
        for i in range(30):
            actions += ["k%d" % i, "L" * 600]
        frame = self.check_oversized(actions=actions)
        self.assertEqual(frame["actions"], [["default", "Open"]] + [["k%d" % i, "L" * 256] for i in range(7)])

    def test_sustained_oversized_input_keeps_the_transport_usable(self):
        self.ready()
        time.sleep(0.3)
        mon = self.monitor()
        ids = self.sender([self.op_notify(body="y" * (600 * 1024), summary="big {i}", repeat=5)])
        frames = self.collect()
        self.assertEqual([f["id"] for f in frames if f["op"] == "notify"], ids)
        self.assertTrue(all(len(f["body"]) == 16384 for f in frames if f["op"] == "notify"))
        nid = self.assert_still_works("after big ones")["id"]
        self.send({"op": "action", "id": nid, "key": "default"})
        self.wait_signal(mon, r"ActionInvoked \(uint32 %d, 'default'\)" % nid)
        self.send({"op": "close", "id": nid, "reason": 2})
        self.wait_signal(mon, r"NotificationClosed \(uint32 %d, uint32 2\)" % nid)


if __name__ == "__main__":
    unittest.main()
