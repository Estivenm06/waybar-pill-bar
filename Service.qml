import QtQuick
import Quickshell
import Quickshell.Io

// Bridge to the external Waybar Pill Bar. The backend owns the product processes; this component only holds a lease
// socket open. Closing it (disable, removal, shell exit or crash) makes the backend stop the product and restore the
// native bar, notifications and OSD. Nothing here installs, builds or reconnects.
Item {
    id: root

    readonly property string home: Quickshell.env("HOME")
    readonly property string runtime: Quickshell.env("XDG_RUNTIME_DIR")

    function fail(what) {
        console.warn("waybar.pill-bar: " + what + ". Run ./install.sh --plugin in the plugin checkout, fix any reported error, then disable and re-enable the plugin.")
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
            }
        }
        onError: error => root.fail("the control socket failed")
    }

    Component.onCompleted: launcher.running = true
}
