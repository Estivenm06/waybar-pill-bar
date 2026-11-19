import QtQuick
import Quickshell
import Quickshell.Io
import Quickshell.Wayland

// Content-host adapter installed next to the WhatsApp plugin's Panel.qml by waybar-pill-bar (see
// files/setup/widget-adapters.py). It lets the plugin's existing, complete WhatsApp UI live inside the Pill Bar notch's
// WhatsApp page: the notch (GTK) owns the backplate and the slot; this adapter connects to the notch's private socket
// (notch/src/notch_content.rs, NDJSON v1), places a transparent layer surface over exactly the published slot and visually
// reparents the Panel's own content Item into it. It owns no window chrome, chat client or renderer: the original Panel keeps
// owning every object, model and daemon call.
Item {
  id: root

  property Item nativeAnchor: null
  property var nativeBar: null
  property var owner: null
  property var nativePanel: null
  property Item focusTarget: null
  // Hosting flag: the Panel's content is shown in the notch and the stock card stays closed.
  property bool active: false
  // Set by the Panel: composer focus, logout confirmation, list picker, image preview or a dragged view.
  property bool busy: false

  // Parent of the Panel's content while `active`: clipped to the published slot, drawing nothing of its own.
  readonly property Item contentItem: holder
  readonly property alias surface: win

  // Notch theme, exactly as published by the notch.
  property color themeBackground: "#0e1211"
  property color themeForeground: "#f2f6f4"
  property color themeForegroundHigh: "#ffffff"
  property color themeAccent: "#8fbcd8"
  property color themeAlert: "#e5484d"
  property string themeFont: "monospace"
  property int themeFontSize: 12
  property int themeRadius: 22
  property bool themeAnimations: true

  readonly property var nativeWindow: nativeAnchor ? nativeAnchor.QsWindow.window : null
  readonly property var screenObj: nativeWindow ? nativeWindow.screen : null
  readonly property bool ready: nativeWindow !== null && screenObj !== null && screenObj !== undefined
    && nativeAnchor.width > 0 && nativeAnchor.height > 0 && owner !== null

  // idle | connecting | waiting (open sent) | hosting | closing
  property string stage: "idle"
  property int session: 0
  property real lastSerial: 0
  property var pending: null
  property bool surfaceVisible: false
  property bool primed: false
  property int mapTries: 0
  property real rectX: 0
  property real rectY: 0
  property real rectW: 0
  property real rectH: 0
  property bool pointerPressed: false
  property bool expectDisconnect: false
  property string mappedSent: ""
  property string interactionSent: ""

  readonly property string socketPath: Quickshell.env("XDG_RUNTIME_DIR") + "/waybar-pill-plugin/notch-content.sock"
  readonly property var popoutKey: owner ? (owner.hostWidget || owner) : null

  visible: false
  width: 0
  height: 0

  function send(obj) {
    if (!socket.connected) return false
    socket.write(JSON.stringify(obj) + "\n")
    socket.flush()
    return true
  }

  function notify(message) {
    notifier.command = ["notify-send", "-u", "normal", "-a", "Pill Bar", "WhatsApp", String(message)]
    notifier.running = true
  }

  function isColor(v) { return typeof v === "string" && /^#[0-9a-fA-F]{6}$/.test(v) }
  function isPositive(v) { return typeof v === "number" && isFinite(v) && v > 0 }
  function isCount(v) { return typeof v === "number" && isFinite(v) && v >= 0 }

  function screenNamed(name) {
    var list = Quickshell.screens
    for (var i = 0; i < list.length; i++)
      if (list[i].name === name) return list[i]
    return null
  }

  // Everything that is not this launch's own bookkeeping is released here; `hideController` is false only when the
  // native card is about to take over (the caller then calls controller.show()).
  function releaseSurface() {
    mapTimer.stop()
    closeTimer.stop()
    if (surfaceVisible && owner && typeof owner.pillRememberFocus === "function") owner.pillRememberFocus()
    surfaceVisible = false
    primed = false
    pending = null
  }

  function releasePopout() {
    if (!nativeBar || !popoutKey) return
    try { if (nativeBar.activePopout === popoutKey) nativeBar.releasePopout(popoutKey) } catch (e) {}
  }

  function reset() {
    if (socket.connected) {
      expectDisconnect = true
      socket.connected = false
      expectDisconnect = false
    }
    stage = "idle"
    session = 0
    lastSerial = 0
    mappedSent = ""
    interactionSent = ""
    pointerPressed = false
  }

  // Normal external teardown: hide the controller first, then clear `active`, so the native card cannot flash open.
  function finishExternal() {
    releaseSurface()
    releasePopout()
    if (active) {
      if (owner) owner.controller.hide()
      active = false
    }
    reset()
  }

  // The native card is taking over (icon, keyboard summon, notification): drop the external layer synchronously
  // without hiding the root controller; the caller shows it afterwards.
  function releaseForNative() {
    if (stage === "idle" && !active) return
    releaseSurface()
    releasePopout()
    active = false
    reset()
  }

  function fail(message) {
    var wasIdle = stage === "idle"
    finishExternal()
    if (!wasIdle) notify(message)
  }

  // Ask the notch to close the page (home: back to the notch's Home page, else collapse). The notch answers with `hide`,
  // then disconnects; a bounded local timer covers a silent peer.
  function closeInNotch(home) {
    if (stage === "idle" && !active) return
    if (stage === "connecting" || stage === "waiting" || session === 0 || !socket.connected) {
      finishExternal()
      return
    }
    stage = "closing"
    if (!send({ op: "close", session: session, home: home === true })) { finishExternal(); return }
    closeTimer.restart()
  }

  function closeSiblings() {
    if (!nativeBar || !owner) return
    var others = nativeBar.moduleWidgets("io.github.ricky.whatsapp")
    for (var i = 0; i < others.length; i++) {
      var w = others[i]
      if (w && w !== owner.hostWidget && typeof w.close === "function") w.close()
    }
  }

  function toggleNotchRequest() {
    if (!ready) return "error: WhatsApp panel is not attached to a native bar window"
    if (!socket.path || socket.path.length < 2) return "error: the notch content socket path is unknown"
    if (stage === "connecting" || stage === "waiting") {
      finishExternal() // no session id yet: simply cancel
      return "ok"
    }
    if (stage !== "idle") {
      closeInNotch(false)
      return "ok"
    }
    stage = "connecting"
    expectDisconnect = false
    socket.connected = true
    return "ok"
  }

  function beginHosting() {
    closeSiblings()
    // `active` first: the stock card's `open` is `opened && !active`, so it closes before the external surface maps
    active = true
    stage = "hosting"
    if (nativeBar && popoutKey) { try { nativeBar.requestPopout(popoutKey) } catch (e) {} }
    owner.controller.show()
  }

  function mapSurface() {
    if (surfaceVisible && owner && typeof owner.pillRememberFocus === "function") owner.pillRememberFocus()
    surfaceVisible = false
    primed = false
    mapTries = 0
    mapTimer.restart()
  }

  function handleLayout(msg) {
    if (stage === "idle" || stage === "closing" || stage === "connecting") return
    var rect = msg.rect, theme = msg.theme
    if (typeof msg.session !== "number" || !isFinite(msg.session) || msg.session <= 0
        || typeof msg.serial !== "number" || !isFinite(msg.serial) || msg.serial <= 0
        || typeof msg.output !== "string" || !rect || !theme) return fail("the notch sent a malformed WhatsApp layout")
    if (session !== 0 && msg.session !== session) return // another session's frame
    if (msg.serial <= lastSerial) return // superseded layout
    var scr = screenNamed(msg.output)
    var okRect = isCount(rect.x) && isCount(rect.y) && isPositive(rect.width) && isPositive(rect.height)
    var okTheme = isColor(theme.background) && isColor(theme.foreground) && isColor(theme.foreground_hi)
      && isColor(theme.accent) && isColor(theme.alert) && typeof theme.font_family === "string"
      && isPositive(theme.font_size) && isCount(theme.radius) && typeof theme.animations === "boolean"
    if (!okRect || !okTheme) return fail("the notch sent a malformed WhatsApp layout")
    if (scr === null) return fail("the notch output " + msg.output + " is not known to the shell")
    if (rect.x + rect.width > scr.width + 0.5 || rect.y + rect.height > scr.height + 0.5)
      return fail("the notch slot falls outside its output")
    session = msg.session
    lastSerial = msg.serial
    themeBackground = theme.background
    themeForeground = theme.foreground
    themeForegroundHigh = theme.foreground_hi
    themeAccent = theme.accent
    themeAlert = theme.alert
    themeFont = theme.font_family
    themeFontSize = Math.round(theme.font_size)
    themeRadius = Math.round(theme.radius)
    themeAnimations = theme.animations
    pending = { serial: msg.serial, screen: scr, x: rect.x, y: rect.y, w: rect.width, h: rect.height }
    if (stage === "waiting") beginHosting()
    mapSurface()
  }

  function handleFrame(msg) {
    if (msg.op === "layout") return handleLayout(msg)
    if (msg.op === "error") return fail(typeof msg.message === "string" && msg.message ? msg.message : "the notch refused the WhatsApp page")
    if (msg.session !== session && session !== 0) return
    if (msg.op === "suspend") {
      releaseSurface()
      return
    }
    if (msg.op === "hide") {
      releaseSurface()
      send({ op: "hidden", session: session })
      if (stage !== "closing") stage = "closing"
      closeTimer.restart()
    }
  }

  function reportInteraction() {
    if (stage !== "hosting" || session === 0 || !surfaceVisible) return
    var hovered = hover.hovered
    var b = busy || pointerPressed
    var key = hovered + "/" + b
    if (key === interactionSent) return
    interactionSent = key
    send({ op: "interaction", session: session, hovered: hovered, busy: b })
  }

  onBusyChanged: reportInteraction()
  onPointerPressedChanged: reportInteraction()
  onNativeWindowChanged: {
    if (nativeWindow === null && (active || stage !== "idle")) finishExternal()
  }

  // Maps the surface once the stock card is gone, then confirms to the notch that this layout is on screen.
  Timer {
    id: mapTimer
    interval: 16
    repeat: true
    onTriggered: {
      var p = root.pending
      if (!p) { stop(); return }
      if (root.mapTries++ > 90) { stop(); root.fail("WhatsApp could not be placed inside the notch"); return }
      if (root.nativePanel && root.nativePanel.backingWindowVisible) return // never draw two backplates
      if (!root.surfaceVisible) {
        if (win.backingWindowVisible) return // the previous placement is still unmapping
        root.rectX = p.x
        root.rectY = p.y
        root.rectW = p.w
        root.rectH = p.h
        win.screen = p.screen
        root.surfaceVisible = true
        return
      }
      if (!win.backingWindowVisible) return
      stop()
      root.primed = false
      primeTimer.restart()
      var serial = p.serial
      Qt.callLater(function () {
        if (!root.pending || root.pending.serial !== serial || !root.surfaceVisible) return
        if (root.owner && typeof root.owner.pillRestoreFocus === "function") root.owner.pillRestoreFocus()
        root.interactionSent = ""
        root.send({ op: "mapped", session: root.session, serial: serial })
        root.reportInteraction()
      })
    }
  }

  // Same short Exclusive prime as the stock panel, then OnDemand so other outputs keep pointer input.
  Timer {
    id: primeTimer
    interval: 75
    onTriggered: root.primed = true
  }

  Timer {
    id: closeTimer
    interval: 600
    onTriggered: root.finishExternal()
  }

  Process {
    id: notifier
    command: ["true"]
  }

  Socket {
    id: socket
    path: root.socketPath
    onConnectionStateChanged: {
      if (connected) {
        if (root.stage === "connecting") {
          root.stage = "waiting"
          root.send({ op: "open", version: 1, output: root.screenObj ? root.screenObj.name : "" })
        }
      } else if (!root.expectDisconnect && root.stage !== "idle") {
        // EOF: the notch released us (normal after `hide`) or went away
        var never = root.stage === "connecting" || root.stage === "waiting"
        root.finishExternal()
        if (never) root.notify("WhatsApp could not reach the Pill Bar notch")
      }
    }
    parser: SplitParser {
      onRead: line => {
        var msg = null
        try { msg = JSON.parse(line) } catch (e) {}
        if (msg === null || typeof msg !== "object") {
          root.fail("the notch sent an invalid WhatsApp frame")
          return
        }
        root.handleFrame(msg)
      }
    }
    // Never tear the socket down from inside its own error signal: act on the next turn of the event loop.
    onError: error => {
      Qt.callLater(function () {
        if (root.stage === "connecting" || root.stage === "waiting") root.fail("WhatsApp could not reach the Pill Bar notch")
      })
    }
  }

  // The transparent layer over the whole output: it draws nothing, takes input only inside the published slot.
  PanelWindow {
    id: win
    visible: root.surfaceVisible
    color: "transparent"
    exclusionMode: ExclusionMode.Ignore
    anchors { top: true; bottom: true; left: true; right: true }
    mask: Region {
      x: root.rectX
      y: root.rectY
      width: root.rectW
      height: root.rectH
    }

    WlrLayershell.namespace: "waybar-notch-content"
    WlrLayershell.layer: WlrLayer.Overlay
    WlrLayershell.keyboardFocus: root.surfaceVisible
      ? (root.primed ? WlrKeyboardFocus.OnDemand : WlrKeyboardFocus.Exclusive)
      : WlrKeyboardFocus.None

    Item {
      id: holder
      x: root.rectX
      y: root.rectY
      width: root.rectW
      height: root.rectH
      clip: true

      HoverHandler {
        id: hover
        onHoveredChanged: root.reportInteraction()
      }

      // Passive: observes presses without taking them from chat rows, scrolling or the composer.
      PointHandler {
        acceptedButtons: Qt.AllButtons
        onActiveChanged: root.pointerPressed = active
      }
    }
  }

  IpcHandler {
    enabled: root.ready
    target: "waybar-pill-whatsapp-" + (root.screenObj ? root.screenObj.name : "")

    function ping(): string { return root.ready ? "ok" : "error: WhatsApp panel is not attached to a native bar window" }
    function toggleNotch(): string { return root.toggleNotchRequest() }
  }

  Component.onDestruction: {
    root.releaseSurface()
    root.releasePopout()
    root.expectDisconnect = true
  }
}
