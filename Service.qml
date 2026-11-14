import QtQuick
import Quickshell
import Quickshell.Io

// Bridge to the external Waybar Pill Bar. The backend owns the product processes; this component only holds a lease
// socket open and, when the backend asks (stage "connect-relay"), attaches this shell's notification server to the
// notch's cards. Closing the lease (disable, removal, shell exit or crash, or any failure here) makes the backend stop
// the product and restore the native bar, notifications and OSD. Nothing here installs, builds or reconnects.
Item {
    id: root

    readonly property string home: Quickshell.env("HOME")
    readonly property string runtime: Quickshell.env("XDG_RUNTIME_DIR")

    function fail(what) {
        console.warn("waybar.pill-bar: " + what + ". Run ./install.sh in the plugin checkout, fix any reported error, then disable and re-enable the plugin.")
    }

    function abort(what) {
        root.fail(what)
        lease.connected = false
    }

    Process {
        id: launcher
        command: ["python3", root.home + "/.config/waybar/scripts/waybar-pill-plugin", "start"]
        stderr: StdioCollector {
            id: launcherErr
        }
        onExited: (code, status) => {
            if (code === 0) {
                lease.path = root.runtime + "/waybar-pill-plugin/control.sock"
                lease.connected = true
            } else {
                root.fail("the launcher failed: " + launcherErr.text.trim())
            }
        }
    }

    Socket {
        id: lease
        onConnectedChanged: {
            if (connected) {
                write("{\"op\":\"lease\"}\n")
                flush()
            }
        }
        parser: SplitParser {
            onRead: line => {
                var reply = {}
                try { reply = JSON.parse(line) } catch (e) {}
                if (!reply.ok)
                    root.fail("the backend refused the lease: " + (reply.error || "unknown error"))
                else if (reply.stage === "connect-relay")
                    relay.active = true
            }
        }
        onError: error => root.fail("the control socket failed")
    }

    Loader {
        id: relay
        active: false
        source: "NotificationRelay.qml"
        onStatusChanged: {
            if (status === Loader.Error)
                root.abort("the notification relay component failed to load")
        }
        onLoaded: {
            item.ready.connect(function() {
                lease.write("{\"op\":\"relay-ready\",\"version\":1}\n")
                lease.flush()
            })
            item.failed.connect(function(message) { root.abort(message) })
        }
    }

    Component.onCompleted: launcher.running = true
}
