import QtQuick
import Quickshell
import Quickshell.Wayland
import qs.Ui

ShellRoot {
  // Click-through background stand-in for the native bar window (its anchor item sits inside it).
  PanelWindow {
    id: barWin
    anchors { top: true; left: true }
    implicitWidth: 800
    implicitHeight: 20
    exclusionMode: ExclusionMode.Ignore
    WlrLayershell.layer: WlrLayer.Background
    WlrLayershell.namespace: "pill-host-test"
    mask: Region {}
    color: "transparent"

    Item {
      id: nativeButton
      x: 700; y: 3; width: 30; height: 14

      QtObject {
        id: fakeBar
        property string position: "top"
        property real barSize: 20
        property var activePopout: null
        property var clickTargets: []
        property string fontFamily: "monospace"
        property color foreground: "#cccccc"
        property color urgent: "#ff5555"
        property color barForeground: "#cccccc"
        function requestPopout(o) { activePopout = o }
        function releasePopout(o) { if (activePopout === o) activePopout = null }
        function moduleWidgets(id) { return [] }
        function switchPanelFrom(o, d) { return false }
      }
      QtObject { id: fakeHost; function close() {} }
      FakeClient { id: fakeClient }

      Loader {
        id: loader
        source: "Panel.qml"
        onLoaded: {
          item.bar = fakeBar
          item.anchorItem = nativeButton
          item.hostWidget = fakeHost
          item.client = fakeClient
          item.settings = ({})
          item.pluginDir = ""
        }
      }
    }
  }
}
