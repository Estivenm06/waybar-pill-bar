import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Services.Notifications

// Forwards this shell's freedesktop notifications to the Waybar Pill Bar's cards over a private NDJSON socket
// (protocol version 1; see notch/src/notification_relay.rs) and applies the card's action/close requests back to the
// real notification objects, so the sender receives genuine ActionInvoked/NotificationClosed signals.
// Quickshell keeps owning org.freedesktop.Notifications; this component only attaches to that server.
Item {
    id: root

    property string socketPath: Quickshell.env("XDG_RUNTIME_DIR") + "/waybar-pill-plugin/notifications.sock"
    signal ready()
    signal failed(string message)

    // The pill bar has no inline reply UI; WhatsApp replies live in its own chat panel.
    property bool transportReady: false
    property bool dead: false
    property var live: ({}) // id -> entry {n, pending, actionWatchers}; plain map, never a model

    function send(obj) {
        socket.write(JSON.stringify(obj) + "\n")
        socket.flush()
    }

    function die(message) {
        if (dead) return
        dead = true
        failed(message)
    }

    function iconOf(n) {
        var icon = String(n.appIcon || "")
        if (icon.indexOf("image://") === 0) icon = ""
        if (icon !== "") return icon
        var img = String(n.image || "")
        return (img.charAt(0) === "/" || img.indexOf("file://") === 0) ? img : ""
    }

    function snapshot(n) {
        var actions = []
        var list = n.actions || []
        for (var i = 0; i < list.length; i++)
            actions.push([String(list[i].identifier), String(list[i].text)])
        var timeout = Number(n.expireTimeout) // milliseconds, as received from the sender (verified against the installed Quickshell)
        return {
            op: "notify",
            id: n.id,
            app: String(n.appName || ""),
            icon: iconOf(n),
            summary: String(n.summary || ""),
            body: String(n.body || ""),
            actions: actions,
            urgency: Math.max(0, Math.min(2, Number(n.urgency))),
            timeout_ms: timeout < 0 ? -1 : Math.round(timeout)
        }
    }

    function sendNotify(entry) {
        try {
            send(snapshot(entry.n))
        } catch (e) {
            // object torn down by the server while the update was queued
        }
    }

    function watchActions(entry) {
        for (var i = 0; i < entry.actionWatchers.length; i++) {
            try { entry.actionWatchers[i].sig.disconnect(entry.refresh) } catch (e) {}
        }
        entry.actionWatchers = []
        var list = []
        try { list = entry.n.actions || [] } catch (e) {}
        for (var j = 0; j < list.length; j++) {
            var sig = list[j].textChanged
            if (sig && typeof sig.connect === "function") {
                sig.connect(entry.refresh)
                entry.actionWatchers.push({ sig: sig })
            }
        }
    }

    readonly property var updateSignals: [
        "summaryChanged", "bodyChanged", "appNameChanged", "appIconChanged",
        "imageChanged", "urgencyChanged", "expireTimeoutChanged", "hintsChanged"
    ]

    function track(n) {
        n.tracked = true
        var id = n.id
        var entry = { n: n, pending: false, actionWatchers: [] }
        // a replaces_id update rewrites this same object without a second onNotification
        entry.refresh = function() {
            if (entry.pending) return
            entry.pending = true
            Qt.callLater(function() {
                entry.pending = false
                if (root.live[id] === entry && root.transportReady) root.sendNotify(entry)
            })
        }
        for (var i = 0; i < updateSignals.length; i++) {
            var sig = n[updateSignals[i]]
            if (sig && typeof sig.connect === "function") sig.connect(entry.refresh)
        }
        n.actionsChanged.connect(function() {
            if (root.live[id] !== entry) return
            root.watchActions(entry)
            entry.refresh()
        })
        n.closed.connect(function(reason) {
            if (root.live[id] !== entry) return
            delete root.live[id]
            if (root.transportReady) root.send({ op: "closed", id: id })
        })
        live[id] = entry
        watchActions(entry)
        if (transportReady) sendNotify(entry)
    }

    function handle(msg) {
        if (msg.op === "ready") {
            if (msg.version !== 1 || transportReady) return
            transportReady = true
            for (var id in live) sendNotify(live[id])
            ready()
            return
        }
        var entry = live[msg.id]
        if (!entry) return // stale or already closed
        try {
            if (msg.op === "action") {
                var list = entry.n.actions || []
                for (var i = 0; i < list.length; i++) {
                    if (String(list[i].identifier) === msg.key) {
                        list[i].invoke()
                        return
                    }
                }
            } else if (msg.op === "close") {
                if (msg.reason === 1) entry.n.expire()
                else if (msg.reason === 2) entry.n.dismiss()
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
                root.send({ op: "hello", version: 1 })
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
        for (var id in live) {
            try { live[id].n.expire() } catch (e) {}
        }
        live = ({})
    }
}
