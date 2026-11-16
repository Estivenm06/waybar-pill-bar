import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Notifications

// Forwards this shell's freedesktop notifications to the Waybar Pill Bar's cards over a private NDJSON socket
// (protocol version 2; see notch/src/notification_relay.rs) and applies the card's action/close requests back to the
// real notification objects, so the sender gets genuine ActionInvoked/NotificationClosed signals.
// Quickshell keeps owning org.freedesktop.Notifications; this component only attaches to that server.
//
// Resource bounds (everything a sender controls is bounded before it reaches the socket):
//   * at most 64 tracked notification slots (pending, in flight or awaiting a closed acknowledgement);
//   * at most 32 unacknowledged wire events; every notify/closed frame carries `seq` and Rust answers
//     {"op":"ack","seq":N} once the event has been consumed;
//   * one in-flight event per notification id plus one latest pending snapshot, so replacement floods coalesce;
//   * field/action/icon limits and a 512 KiB frame cap, applied before serialisation.
Item {
    id: root

    property string socketPath: Quickshell.env("XDG_RUNTIME_DIR") + "/waybar-pill-plugin/notifications.sock"
    signal ready()
    signal failed(string message)

    readonly property int protocolVersion: 2
    readonly property int maxSlots: 64
    readonly property int maxInflight: 32
    readonly property int maxFrameBytes: 512 * 1024 // JSON text plus the newline, in UTF-8 bytes
    readonly property int maxApp: 256
    readonly property int maxSummary: 4096
    readonly property int maxBody: 16384
    readonly property int maxLabel: 256
    readonly property int maxKey: 128
    readonly property int maxIcon: 4096
    readonly property int maxActions: 8
    readonly property int maxActionScan: 256
    readonly property real maxSeq: 9007199254740991

    // The pill bar has no inline reply UI; WhatsApp replies live in its own chat panel.
    property bool transportReady: false
    property bool dead: false
    property bool pumping: false
    // id -> slot {n, closed, sent, inflight, inflightKind, pending, queued, watchers}; plain maps, never a model
    property var slots: ({})
    property int slotCount: 0
    property var seqs: ({}) // in-flight seq -> notification id
    property int inflight: 0
    property var queue: [] // slots with a pending event, oldest first; a slot is queued at most once
    property real nextSeq: 1

    function writeLine(line) {
        socket.write(line)
        socket.flush()
    }

    function send(obj) {
        writeLine(JSON.stringify(obj) + "\n")
    }

    function die(message) {
        if (dead) return
        dead = true
        failed(message)
    }

    // Number of UTF-8 bytes in a JS string (lone surrogates count as the three bytes of U+FFFD).
    function utf8Length(s) {
        var bytes = 0
        for (var i = 0; i < s.length; i++) {
            var c = s.charCodeAt(i)
            if (c < 0x80) bytes += 1
            else if (c < 0x800) bytes += 2
            else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < s.length && s.charCodeAt(i + 1) >= 0xDC00 && s.charCodeAt(i + 1) <= 0xDFFF) {
                bytes += 4
                i++
            } else bytes += 3
        }
        return bytes
    }

    // A serialised frame (including its newline) must fit the Rust reader's limit in real UTF-8 bytes.
    function fits(line) {
        if (line.length > maxFrameBytes) return false // every UTF-16 unit takes at least one byte
        if (line.length * 3 <= maxFrameBytes) return true // at most three bytes per unit
        return utf8Length(line) <= maxFrameBytes
    }

    // Truncate to `max` UTF-16 units without splitting a surrogate pair.
    function clip(value, max) {
        var s = String(value || "")
        if (s.length <= max) return s
        var cut = max
        var c = s.charCodeAt(cut - 1)
        if (c >= 0xD800 && c <= 0xDBFF) cut--
        return s.substring(0, cut)
    }

    function iconOf(n) {
        var icon = String(n.appIcon || "")
        if (icon.length > maxIcon) return "" // never truncate a path into a different one
        if (icon.indexOf("image://") === 0) icon = ""
        if (icon !== "") return icon
        var img = String(n.image || "")
        if (img.length > maxIcon) return ""
        return (img.charAt(0) === "/" || img.indexOf("file://") === 0) ? img : ""
    }

    // The action objects that are forwarded: at most eight, identifiers kept intact or dropped.
    function retainedActions(n) {
        var out = []
        var list = []
        try { list = n.actions || [] } catch (e) {}
        var scan = Math.min(list.length, maxActionScan)
        for (var i = 0; i < scan && out.length < maxActions; i++) {
            if (String(list[i].identifier).length <= maxKey) out.push(list[i])
        }
        return out
    }

    function snapshot(n) {
        var actions = []
        var kept = retainedActions(n)
        for (var i = 0; i < kept.length; i++)
            actions.push([String(kept[i].identifier), clip(kept[i].text, maxLabel)])
        var timeout = Number(n.expireTimeout) // milliseconds, as received from the sender (verified against the installed Quickshell)
        return {
            op: "notify",
            seq: 0,
            id: n.id,
            app: clip(n.appName, maxApp),
            icon: iconOf(n),
            summary: clip(n.summary, maxSummary),
            body: clip(n.body, maxBody),
            actions: actions,
            urgency: Math.max(0, Math.min(2, Number(n.urgency))),
            timeout_ms: (!isFinite(timeout) || timeout < 0) ? -1 : Math.round(timeout)
        }
    }

    function watchActions(slot) {
        for (var i = 0; i < slot.watchers.length; i++) {
            try { slot.watchers[i].sig.disconnect(slot.refresh) } catch (e) {}
        }
        slot.watchers = []
        var kept = retainedActions(slot.n)
        for (var j = 0; j < kept.length; j++) {
            var sig = kept[j].textChanged
            if (sig && typeof sig.connect === "function") {
                sig.connect(slot.refresh)
                slot.watchers.push({ sig: sig })
            }
        }
    }

    readonly property var updateSignals: [
        "summaryChanged", "bodyChanged", "appNameChanged", "appIconChanged",
        "imageChanged", "urgencyChanged", "expireTimeoutChanged", "hintsChanged"
    ]

    function dropSlot(slot) {
        if (slots[slot.id] !== slot) return
        delete slots[slot.id]
        slotCount--
    }

    // Queue a slot's pending event unless one is already queued or in flight (the acknowledgement re-queues it).
    function enqueue(slot) {
        if (slot.queued || slot.inflight !== 0 || slot.pending === "") return
        slot.queued = true
        queue.push(slot)
        pump()
    }

    function request(slot, kind) {
        if (slot.closed) return
        slot.pending = kind
        enqueue(slot)
    }

    // The notification is gone from the server. A slot that never reached the wire needs no event at all; one that
    // did keeps its slot as a tombstone until the ordered `closed` event is acknowledged.
    function noteClosed(slot) {
        if (slot.closed || slots[slot.id] !== slot) return
        slot.closed = true
        if (!slot.sent) {
            slot.pending = ""
            dropSlot(slot)
            return
        }
        slot.pending = "closed"
        enqueue(slot)
    }

    // A notification that cannot be forwarded is expired towards its sender without logging its content.
    function reject(slot) {
        try { slot.n.expire() } catch (e) {}
        noteClosed(slot)
    }

    function pump() {
        if (pumping || !transportReady || dead) return
        pumping = true
        try {
            while (inflight < maxInflight && queue.length > 0) {
                var slot = queue.shift()
                slot.queued = false
                if (slots[slot.id] !== slot || slot.inflight !== 0 || slot.pending === "") continue
                var kind = slot.pending
                slot.pending = ""
                if (nextSeq > maxSeq) {
                    die("the notification relay sequence is exhausted")
                    return
                }
                var line = null
                if (kind === "closed") {
                    line = JSON.stringify({ op: "closed", seq: nextSeq, id: slot.id }) + "\n"
                } else {
                    try {
                        var obj = snapshot(slot.n)
                        obj.seq = nextSeq
                        line = JSON.stringify(obj) + "\n"
                    } catch (e) {
                        line = null // object torn down by the server while the update was queued
                    }
                    if (line === null || !fits(line)) {
                        reject(slot)
                        continue
                    }
                }
                var seq = nextSeq
                nextSeq = seq + 1
                slot.inflight = seq
                slot.inflightKind = kind
                seqs[seq] = slot.id
                inflight++
                if (kind === "notify") slot.sent = true
                writeLine(line)
            }
        } finally {
            pumping = false
        }
    }

    function acknowledge(seq) {
        if (typeof seq !== "number") return
        var id = seqs[seq]
        if (id === undefined) return // unknown or duplicate: never mints a credit
        delete seqs[seq]
        inflight--
        var slot = slots[id]
        if (slot && slot.inflight === seq) {
            slot.inflight = 0
            slot.inflightKind = ""
            if (slot.pending !== "") enqueue(slot)
            else if (slot.closed) dropSlot(slot) // the closed event was acknowledged: release the slot
        }
        pump()
    }

    // Brave/Chromium's own toast for web.whatsapp.com (app "Brave", body "web.whatsapp.com\n\n<text>"). The WhatsApp
    // plugin's daemon already notifies for the same message, so the browser's copy is a duplicate card.
    function isBrowserWhatsApp(n) {
        var src = (String(n.appName || "") + "\n" + String(n.desktopEntry || "")).toLowerCase()
        var browser = src.indexOf("brave") >= 0 || src.indexOf("chrom") >= 0
        return browser && String(n.body || "").replace(/^\s+/, "").indexOf("web.whatsapp.com") === 0
    }

    function track(n) {
        n.tracked = true
        var id = n.id
        if (isBrowserWhatsApp(n)) {
            try { n.expire() } catch (e) {}
            return
        }
        if (slots[id] !== undefined || slotCount >= maxSlots) {
            // no room: tell the sender it expired and keep nothing
            try { n.expire() } catch (e) {}
            return
        }
        var slot = { id: id, n: n, closed: false, sent: false, inflight: 0, inflightKind: "", pending: "", queued: false,
                     refreshQueued: false, watchers: [] }
        // a replaces_id update rewrites this same object without a second onNotification
        slot.refresh = function() {
            if (slot.refreshQueued) return
            slot.refreshQueued = true
            Qt.callLater(function() {
                slot.refreshQueued = false
                if (root.slots[id] === slot && root.transportReady) root.request(slot, "notify")
            })
        }
        for (var i = 0; i < updateSignals.length; i++) {
            var sig = n[updateSignals[i]]
            if (sig && typeof sig.connect === "function") sig.connect(slot.refresh)
        }
        n.actionsChanged.connect(function() {
            if (root.slots[id] !== slot || slot.closed) return
            root.watchActions(slot)
            slot.refresh()
        })
        n.closed.connect(function(reason) { root.noteClosed(slot) })
        slots[id] = slot
        slotCount++
        watchActions(slot)
        if (transportReady) request(slot, "notify")
    }

    function handle(msg) {
        if (msg.op === "ready") {
            if (msg.version !== protocolVersion || transportReady) return
            transportReady = true
            for (var id in slots) {
                if (!slots[id].closed) request(slots[id], "notify")
            }
            ready()
            return
        }
        if (msg.op === "ack") {
            acknowledge(msg.seq)
            return
        }
        var slot = slots[msg.id]
        if (!slot || slot.closed) return // stale or already closed
        try {
            if (msg.op === "action") {
                var list = retainedActions(slot.n)
                for (var i = 0; i < list.length; i++) {
                    if (String(list[i].identifier) === msg.key) {
                        list[i].invoke()
                        return
                    }
                }
            } else if (msg.op === "close") {
                if (msg.reason === 1) slot.n.expire()
                else if (msg.reason === 2) slot.n.dismiss()
            }
        } catch (e) {
            // notification destroyed meanwhile
        }
    }

    NotificationServer {
        keepOnReload: false
        imageSupported: true
        actionsSupported: true
        persistenceSupported: true
        onNotification: n => root.track(n)
    }

    Socket {
        id: socket
        path: root.socketPath
        onConnectionStateChanged: {
            if (connected) {
                root.send({ op: "hello", version: root.protocolVersion })
            } else if (!root.dead) {
                root.die("the notch notification relay disconnected")
            }
        }
        parser: SplitParser {
            onRead: line => {
                var msg = null
                try { msg = JSON.parse(line) } catch (e) {}
                if (msg === null || typeof msg !== "object") {
                    root.die("the notch notification relay sent an invalid frame")
                    return
                }
                root.handle(msg)
            }
        }
        onError: error => root.die("the notch notification relay socket failed")
    }

    Component.onCompleted: socket.connected = true

    Component.onDestruction: {
        transportReady = false // the closed signals raised below must not write to a dying socket
        for (var id in slots) {
            try { if (!slots[id].closed) slots[id].n.expire() } catch (e) {}
        }
        slots = ({})
        slotCount = 0
        queue = []
        seqs = ({})
    }
}
