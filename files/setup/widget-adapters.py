#!/usr/bin/env python3
"""Pill Bar's narrow patch of the installed WhatsApp plugin so its existing, complete panel can live inside the notch.

  widget-adapters.py stage DIR   write the proposed files into the private staging directory DIR and print a JSON
                                 list of {"path": destination, "source": staged file, "mode": "644"}; nothing outside
                                 DIR is touched. `[]` when WhatsApp is absent or disabled. An enabled source this
                                 patch does not support stages nothing, reports why on stderr and still exits 0.

Panel.qml gets named, paired managed blocks (`// >>> waybar-pill-bar anchor NAME BEGIN` ... `END`) at structural anchors
of the stock 1.1.0 file, plus a small, count-checked rewrite of the stock design tokens outside those blocks.
WaybarPillAnchor.qml is a new sibling file (the content-host adapter). Credentials, the manifest, BarWidget.qml and the
daemon are never touched. Reinstalling replaces only managed blocks; unmanaged edits survive."""
import sys

sys.dont_write_bytecode = True  # nothing this helper loads may leave bytecode in a watched plugin directory

import hashlib  # noqa: E402
import importlib.machinery  # noqa: E402
import importlib.util  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import re  # noqa: E402
import stat  # noqa: E402

PLUGIN_ID = "io.github.ricky.whatsapp"
ANCHOR_FILE = "WaybarPillAnchor.qml"
HERE = os.path.dirname(os.path.abspath(__file__))

# One validation policy for the install record and filesystem trust: the plugin helper owns it.
_loader = importlib.machinery.SourceFileLoader("waybar_pill_plugin", os.path.join(HERE, "..", "scripts", "waybar-pill-plugin"))
_spec = importlib.util.spec_from_loader("waybar_pill_plugin", _loader)
_helper = importlib.util.module_from_spec(_spec)
_loader.exec_module(_helper)


class AdapterError(Exception):
    pass


class SourceUnsupported(AdapterError):
    """The installed WhatsApp source is not the one this patch understands (or its adapter is not ours to replace)."""


# ---------------------------------------------------------------------------------------------------- managed blocks

MARK = "waybar-pill-bar anchor"
MARKER_RE = re.compile(r"^[ \t]*// (>>>|<<<) " + MARK + r" (\S+) (BEGIN|END)[ \t]*$")


def marker_block(name, body, indent):
    return "%s// >>> %s %s BEGIN\n%s%s// <<< %s %s END\n" % (indent, MARK, name, body, indent, MARK, name)


class Block:
    """kind: 'replace' (the stock `anchor` text is replaced by the block), 'after' / 'before' (the block is inserted
    next to the untouched `anchor`)."""

    def __init__(self, name, kind, anchor, body):
        self.name, self.kind, self.anchor, self.body = name, kind, anchor, body

    @property
    def indent(self):
        return re.match(r"[ \t]*", self.anchor).group(0)

    def full(self):
        return marker_block(self.name, self.body, self.indent)


def scan_blocks(text, path):
    """[(start, end, name)] spans of the managed blocks; rejects nested, crossed, duplicated or unpaired markers."""
    spans, open_name, open_start, seen, pos = [], None, 0, set(), 0
    for line in text.splitlines(keepends=True):
        m = MARKER_RE.match(line.rstrip("\n"))
        if m:
            kind, name, edge = m.groups()
            if (kind == ">>>") != (edge == "BEGIN"):
                raise AdapterError("%s: malformed managed marker for '%s'" % (path, name))
            if edge == "BEGIN":
                if open_name is not None:
                    raise AdapterError("%s: managed block '%s' starts inside '%s'" % (path, name, open_name))
                if name in seen:
                    raise AdapterError("%s: duplicate managed block '%s'" % (path, name))
                open_name, open_start = name, pos
                seen.add(name)
            else:
                if open_name != name:
                    raise AdapterError("%s: managed block '%s' ends where '%s' is open" % (path, name, open_name))
                spans.append((open_start, pos + len(line), name))
                open_name = None
        pos += len(line)
    if open_name is not None:
        raise AdapterError("%s: managed block '%s' is not closed" % (path, open_name))
    return spans


def unmanaged_segments(text, spans):
    """[(start, end)] of the text outside every managed block."""
    segs, pos = [], 0
    for s, e, _ in spans:
        if s > pos:
            segs.append((pos, s))
        pos = e
    if pos < len(text):
        segs.append((pos, len(text)))
    return segs


def apply_block(text, blk, known, path):
    spans = scan_blocks(text, path)
    for s, e, name in spans:
        if name not in known:
            raise AdapterError("%s: unknown managed block '%s'" % (path, name))
        if name == blk.name:
            return text[:s] + blk.full() + text[e:]
    hits = []
    for s, e in unmanaged_segments(text, spans):
        i = text.find(blk.anchor, s, e)
        while i != -1:
            hits.append(i)
            i = text.find(blk.anchor, i + 1, e)
    if len(hits) != 1:
        raise AdapterError("%s: expected exactly one structural anchor for '%s', found %d (the WhatsApp plugin changed; "
                           "not patching)" % (path, blk.name, len(hits)))
    i = hits[0]
    end = i + len(blk.anchor)
    if blk.kind == "replace":
        return text[:i] + blk.full() + text[end:]
    if blk.kind == "after":
        return text[:end] + blk.full() + text[end:]
    return text[:i] + blk.full() + text[i:]


# Stock design tokens rewritten outside the managed blocks; the counts are those of the stock 1.1.0 file.
TOKENS = (
    ("font", re.compile(r"Style\.font\.(body|caption|subtitle|title|icon)\b"), re.compile(r"root\.pillFont\.(body|caption|subtitle|title|icon)\b"),
     r"root.pillFont.\1", 21),
    ("radius", re.compile(r"Style\.cornerRadius\b"), re.compile(r"root\.pillRadius\b"), "root.pillRadius", 4),
    ("accent", re.compile(r"root\.bar \? root\.bar\.urgent : Color\.accent\b"), re.compile(r"root\.pillAccent\b"), "root.pillAccent", 12),
    ("alert", re.compile(r"root\.bar \? root\.bar\.urgent : Color\.urgent\b"), re.compile(r"root\.pillAlert\b"), "root.pillAlert", 3),
)


def apply_tokens(text, path):
    spans = scan_blocks(text, path)
    segs = unmanaged_segments(text, spans)
    for name, stock, patched, repl, expect in TOKENS:
        s_count = sum(len(stock.findall(text[a:b])) for a, b in segs)
        p_count = sum(len(patched.findall(text[a:b])) for a, b in segs)
        if (s_count, p_count) == (0, expect):
            continue
        if (s_count, p_count) != (expect, 0):
            raise AdapterError("%s: the '%s' design tokens are neither stock (%d) nor patched (%d): found %d/%d; the "
                               "WhatsApp plugin changed; not patching" % (path, name, expect, expect, s_count, p_count))
        out, pos = [], 0
        for a, b in segs:
            out.append(text[pos:a])
            out.append(stock.sub(repl, text[a:b]))
            pos = b
        out.append(text[pos:])
        text = "".join(out)
        spans = scan_blocks(text, path)
        segs = unmanaged_segments(text, spans)
    return text


def b(name, kind, anchor, body):
    return Block(name, kind, anchor, body)


ORIG_TOKENS = r'''  readonly property string fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
  // Popup content must use the theme foreground. barForeground can switch to
  // a wallpaper-contrast color when the bar is transparent, which may be dark
  // even though the popup surface remains dark.
  readonly property color foreground: root.bar ? root.bar.foreground : Color.foreground
  readonly property color secondaryForeground: Qt.darker(root.foreground, 1.5)
'''
ORIG_KEYBOARD = ("    id: panel\n    anchorItem: root.anchorItem\n    owner: root.hostWidget || root\n"
                 "    bar: root.bar\n    open: root.opened\n")

BLOCKS = [
    b("properties", "after", '  property string pluginDir: ""\n', r'''  WaybarPillAnchor {
    id: pillAnchor
    nativeAnchor: root.anchorItem
    nativeBar: root.bar
    owner: root
    nativePanel: panel
    focusTarget: keyCatcher
    busy: composer.activeFocus || root.logoutConfirmOpen || root.peekActive || root.listPickerOpen
      || chatList.dragging || messageList.dragging || pillViewport.dragging
  }
'''),
    b("tokens", "replace", ORIG_TOKENS, r'''  // In the notch every token comes from the notch's own theme; natively it is exactly the stock value.
  readonly property string fontFamily: pillAnchor.active ? pillAnchor.themeFont : (root.bar ? root.bar.fontFamily : Style.font.family)
  readonly property color foreground: pillAnchor.active ? pillAnchor.themeForeground : (root.bar ? root.bar.foreground : Color.foreground)
  readonly property color secondaryForeground: pillAnchor.active
    ? Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.62) : Qt.darker(root.foreground, 1.5)
  readonly property color pillAccent: pillAnchor.active ? pillAnchor.themeAccent : (root.bar ? root.bar.urgent : Color.accent)
  readonly property color pillAlert: pillAnchor.active ? pillAnchor.themeAlert : (root.bar ? root.bar.urgent : Color.urgent)
  readonly property color pillBg: pillAnchor.active ? pillAnchor.themeBackground : Color.background
  readonly property real pillRadius: pillAnchor.active ? Math.min(10, pillAnchor.themeRadius) : Style.cornerRadius
  readonly property QtObject pillFont: QtObject {
    readonly property real body: pillAnchor.active ? pillAnchor.themeFontSize : Style.font.body
    readonly property real caption: pillAnchor.active ? Math.max(10, Math.round(pillAnchor.themeFontSize * 11 / 12)) : Style.font.caption
    readonly property real subtitle: pillAnchor.active ? Math.round(pillAnchor.themeFontSize * 15 / 12) : Style.font.subtitle
    readonly property real title: pillAnchor.active ? Math.round(pillAnchor.themeFontSize * 20 / 12) : Style.font.title
    readonly property real icon: pillAnchor.active ? Math.round(pillAnchor.themeFontSize * 17 / 12) : Style.font.icon
  }
  // Heights the lists may take inside the notch: the holder minus the fixed chrome that is actually visible. Evaluated
  // only in the notch, so the natural-height native layout keeps no dependency on the viewport.
  readonly property real pillChatBudget: !pillAnchor.active ? 0 : Math.max(Style.space(48), pillViewport.height
    - pillHeader.height - pillSeparator.height - (pillHints.visible ? pillHints.height : 0) - listStrip.height
    - content.spacing * 3 - pillChats.spacing)
  readonly property real pillMessageBudget: !pillAnchor.active ? 0 : Math.max(Style.space(48), pillViewport.height
    - pillHeader.height - pillSeparator.height - pillReply.height
    - (pillEmptyMessages.visible ? pillEmptyMessages.height + pillConversation.spacing : 0)
    - content.spacing * 2 - pillConversation.spacing)
'''),
    b("open", "replace", "  function open() { root.controller.show() }\n", r'''  // Native summon (icon, keyboard, notification): the external layer lets go first, then the stock card opens.
  function open() { pillAnchor.releaseForNative(); root.controller.show() }
  property bool pillComposerFocus: false
  function pillRememberFocus() { root.pillComposerFocus = composer.activeFocus }
  function pillRestoreFocus() {
    if (root.listPickerOpen) listPicker.forceActiveFocus()
    else if (root.logoutConfirmOpen) logoutConfirm.forceActiveFocus()
    else if (root.peekActive) pillEmbeddedPeek.forceActiveFocus()
    else if (root.view === "chat" && root.pillComposerFocus) composer.forceActiveFocus()
    else keyCatcher.forceActiveFocus()
  }
  // Back: a conversation returns to the list; in the notch the list returns to the notch's Home page.
  function pillBack() {
    if (pillAnchor.active && root.view !== "chat") pillAnchor.closeInNotch(true)
    else root.back()
  }
'''),
    b("close", "replace", "  function close() { root.controller.hide() }\n", r'''  function close() {
    if (pillAnchor.active) pillAnchor.closeInNotch(false)
    else root.controller.hide()
  }
'''),
    b("keyboard", "replace", ORIG_KEYBOARD, r'''    id: panel
    anchorItem: root.anchorItem
    owner: root.hostWidget || root
    bar: root.bar
    open: root.opened && !pillAnchor.active
'''),
    b("holder", "before", "    PanelKeyCatcher {\n", r'''    Item { id: pillNativeHolder; anchors.fill: parent }
'''),
    b("parent", "after", "      id: keyCatcher\n", r'''      parent: pillAnchor.active ? pillAnchor.contentItem : pillNativeHolder
'''),
    b("viewport-open", "before", "      Column {\n        id: content\n", r'''      // Outer scroll: natural size natively; in the notch the holder is the viewport and overflow scrolls.
      Flickable {
        id: pillViewport
        width: parent.width
        height: pillAnchor.active ? parent.height : content.implicitHeight
        contentWidth: width
        contentHeight: content.implicitHeight
        clip: pillAnchor.active
        interactive: pillAnchor.active && contentHeight > height
        boundsBehavior: Flickable.StopAtBounds
        ScrollBar.vertical: ScrollBar { policy: pillAnchor.active ? ScrollBar.AsNeeded : ScrollBar.AlwaysOff }
'''),
    b("viewport-close", "before", "      // ── List picker (l)", "      }\n"),
    b("peek-embedded", "before", "      // ── List picker (l)", r'''      // Image preview inside the notch slot (the fullscreen peek window is native-only).
      Rectangle {
        id: pillEmbeddedPeek
        visible: pillAnchor.active && root.peekActive
        anchors.fill: parent
        z: 11
        color: Qt.rgba(root.pillBg.r, root.pillBg.g, root.pillBg.b, 0.96)
        focus: visible
        onVisibleChanged: if (visible) Qt.callLater(function () { pillEmbeddedPeek.forceActiveFocus() })
        Keys.onEscapePressed: function (event) { root.peekImagePath = ""; event.accepted = true }

        MouseArea { anchors.fill: parent; onClicked: root.peekImagePath = "" }

        Image {
          anchors.fill: parent
          anchors.margins: Style.space(8)
          anchors.topMargin: Style.space(40)
          fillMode: Image.PreserveAspectFit
          asynchronous: true
          source: pillEmbeddedPeek.visible ? Qt.resolvedUrl("file://" + root.peekImagePath) : ""
        }

        PanelActionButton {
          anchors.left: parent.left
          anchors.top: parent.top
          anchors.margins: Style.space(6)
          iconText: "\uf060"
          tooltipText: "Back to the conversation"
          foreground: root.foreground
          fontFamily: root.fontFamily
          onClicked: root.peekImagePath = ""
        }
      }

'''),
    # stable ids for the budget maths
    b("id-header", "after", "          implicitHeight: Math.max(headerText.implicitHeight, backButton.height, headerActions.implicitHeight)\n",
      "          id: pillHeader\n"),
    b("id-separator", "replace", "        PanelSeparator { foreground: root.foreground }\n",
      "        PanelSeparator { id: pillSeparator; foreground: root.foreground }\n"),
    b("id-login", "after", "          visible: root.showLogin\n", "          id: pillLogin\n"),
    b("id-chats", "after", '          spacing: Style.space(4)\n          visible: !root.showLogin && root.view === "chats"\n',
      "          id: pillChats\n"),
    b("id-hints", "after", '          visible: !root.showLogin && root.view === "chats"\n          horizontalAlignment: Text.AlignHCenter\n',
      "          id: pillHints\n"),
    b("id-conversation", "after", '          spacing: Style.space(6)\n          visible: !root.showLogin && root.view === "chat"\n',
      "          id: pillConversation\n"),
    b("id-empty-chats", "after", "            visible: root.visibleChats.length === 0\n", "            id: pillEmptyChats\n"),
    b("id-empty-messages", "after", "            visible: root.messages.length === 0\n", "            id: pillEmptyMessages\n"),
    b("id-reply", "after", "            implicitHeight: Math.max(composer.implicitHeight, sendButton.implicitHeight)\n",
      "            id: pillReply\n"),
    b("empty-messages-wrap", "after", '            text: "No messages loaded yet."\n', "            wrapMode: Text.WordWrap\n"),
    # list heights
    b("chat-height", "replace", "            height: Math.min(contentHeight, Style.space(300))\n",
      "            height: pillAnchor.active ? Math.min(contentHeight, root.pillChatBudget) : Math.min(contentHeight, Style.space(300))\n"),
    b("message-height", "replace", "            height: Style.space(300)\n            model: root.messages\n",
      "            height: pillAnchor.active ? root.pillMessageBudget : Style.space(300)\n            model: root.messages\n"),
    # header Back button: visible on the chat list in the notch, where it returns to the notch Home page
    b("back-visible", "replace", '            visible: root.view === "chat"\n            width: Style.space(26)\n',
      '            visible: root.view === "chat" || pillAnchor.active\n            width: Style.space(26)\n'),
    b("back-radius", "replace", "            radius: 0\n            color: backMouse.containsMouse\n",
      "            radius: pillAnchor.active ? root.pillRadius : 0\n            color: backMouse.containsMouse\n"),
    b("back-click", "replace", "              cursorShape: Qt.PointingHandCursor\n              onClicked: root.back()\n",
      "              cursorShape: Qt.PointingHandCursor\n              onClicked: root.pillBack()\n"),
    b("row-radius", "after", "                hasCursor: root.cursorIndex === chatRow.index\n", "                radius: root.pillRadius\n"),
    b("picker-row-radius", "after", "              hasCursor: root.listPickerIndex === pickerRow.index\n", "              radius: root.pillRadius\n"),
    b("login-font", "after", '            text: "Login"\n', "            fontSize: root.pillFont.body\n"),
    b("composer-style", "after", r'''              placeholderText: root.linked ? "Reply\u2026" : "Not connected"''' + "\n", r'''              font.family: root.fontFamily
              font.pixelSize: root.pillFont.body
              background: BorderSurface {
                color: pillAnchor.active ? Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.07)
                  : Style.controlFill(composer._focused, composer._hot, composer.foreground, composer.accent)
                borderSpec: pillAnchor.active
                  ? Border.flat(composer.activeFocus ? root.pillAccent : Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.16), 1)
                  : composer._borderSpec
                radius: root.pillRadius
              }
'''),
    # list picker
    b("picker-color", "replace", "        color: Qt.rgba(Color.background.r, Color.background.g, Color.background.b, 0.94)\n",
      "        color: Qt.rgba(root.pillBg.r, root.pillBg.g, root.pillBg.b, 0.94)\n"),
    b("picker-list", "replace", "          Repeater {\n            model: root.customLists\n\n            CursorSurface {\n", r'''          ListView {
            id: pillPickerList
            width: parent.width
            height: pillAnchor.active ? Math.min(contentHeight, Math.max(Style.space(48), listPicker.height - Style.space(110))) : contentHeight
            interactive: pillAnchor.active && contentHeight > height
            clip: pillAnchor.active
            boundsBehavior: Flickable.StopAtBounds
            model: root.customLists
            currentIndex: root.listPickerIndex
            onCurrentIndexChanged: positionViewAtIndex(currentIndex, ListView.Contain)
            ScrollBar.vertical: ScrollBar { policy: pillAnchor.active ? ScrollBar.AsNeeded : ScrollBar.AlwaysOff }

            delegate: CursorSurface {
'''),
    b("picker-label", "after", r'''                text: (pickerRow.member ? "\uf14a  " : "\uf096  ") + pickerRow.modelData.name''' + "\n",
      "                width: parent.width - Style.space(16)\n                elide: Text.ElideRight\n"),
    # logout confirmation: bounded, scrollable canvas in the notch
    b("logout-open", "before", "      ConfirmDialog {\n", r'''      Flickable {
        id: pillLogoutViewport
        anchors.fill: parent
        z: 10
        visible: root.logoutConfirmOpen
        contentWidth: width
        contentHeight: pillLogoutCanvas.height
        clip: pillAnchor.active
        interactive: pillAnchor.active && contentHeight > height
        boundsBehavior: Flickable.StopAtBounds
        ScrollBar.vertical: ScrollBar { policy: pillAnchor.active ? ScrollBar.AsNeeded : ScrollBar.AlwaysOff }

        Item {
          id: pillLogoutCanvas
          width: pillLogoutViewport.width
          height: pillAnchor.active ? Math.max(pillLogoutViewport.height, 320) : pillLogoutViewport.height
'''),
    b("logout-style", "after", '        confirmText: "Log out"\n', r'''        background: root.pillBg
        scrim: Qt.rgba(root.pillBg.r, root.pillBg.g, root.pillBg.b, 0.7)
        selectedBackground: Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.1)
        selectedText: root.pillAccent
        cornerRadius: root.pillRadius
'''),
    b("logout-close", "after", "          if (handleKey(event)) event.accepted = true\n        }\n      }\n", "        }\n      }\n"),
    # the fullscreen peek surface is native-only
    b("peek-window", "replace", "    visible: root.peekActive\n", "    visible: root.peekActive && !pillAnchor.active\n"),
    b("peek-focus", "replace", "    WlrLayershell.keyboardFocus: root.peekActive ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None\n",
      "    WlrLayershell.keyboardFocus: root.peekActive && !pillAnchor.active ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None\n"),
]
KNOWN = {blk.name for blk in BLOCKS}
assert len(KNOWN) == len(BLOCKS)


def patch_panel(text, path):
    for _, _, name in scan_blocks(text, path):
        if name not in KNOWN:
            raise AdapterError("%s: unknown managed block '%s'" % (path, name))
    for blk in BLOCKS:
        text = apply_block(text, blk, KNOWN, path)
    # every managed block, unknown or not, was validated while applying; migrate the stock tokens last
    return apply_tokens(text, path)


# ------------------------------------------------------------------------------------------------------ file access

def config_root(config=None):
    """The effective configured root (XDG_CONFIG_HOME or ~/.config). An explicit argument must be that same root, so
    it cannot steer the adapter or the record's destination policy elsewhere."""
    effective = _helper.config_home()
    if config is not None and os.path.normpath(config) != effective:
        raise AdapterError("%s is not the configured Omarchy config root" % config)
    return effective


def read_regular(path):
    """Text of an owned regular file reached without following links; None when it is absent."""
    try:
        dfd = _helper.open_directory_chain(os.path.dirname(path), own_from=_helper.trust_root(path))
        if dfd is None:
            return None
        try:
            fd = _helper.open_leaf(dfd, os.path.basename(path), path)
        except FileNotFoundError:
            return None
        finally:
            os.close(dfd)
    except _helper.PluginError as e:
        raise AdapterError(str(e))
    try:
        with os.fdopen(fd, "rb") as f:
            return f.read().decode("utf-8")
    except (OSError, UnicodeDecodeError) as e:
        raise AdapterError("cannot read %s: %s" % (path, e))


def read_source(path):
    try:
        with open(path, encoding="utf-8") as f:
            return f.read()
    except (OSError, UnicodeDecodeError) as e:
        raise AdapterError("adapter source %s cannot be read: %s" % (path, e))


def sha256_of(path):
    try:
        return _helper.sha256_of(path)
    except _helper.PluginError as e:
        raise AdapterError(str(e))


def install_record():
    """{absolute path: record entry} from the validated plugin install record; {} when plugin mode was never set up."""
    try:
        m = _helper.read_marker()
    except _helper.PluginError as e:
        raise AdapterError(str(e))
    if m is None:
        return {}
    return {e["path"]: e for e in m["files"]}


def check_recorded_unchanged(path, record):
    """The install record is the conflict authority: a recorded file whose hash moved is the user's now."""
    entry = record.get(path)
    if entry and entry["installed_sha256"] and os.path.lexists(path) and sha256_of(path) != entry["installed_sha256"]:
        raise AdapterError("%s was modified since the last setup; not overwriting it" % path)


def check_adapter_ownership(path, record):
    """The adapter was originally absent: it may only be overwritten when the record says Pill Bar created it and it
    is unchanged. A comment inside the file proves nothing."""
    if not os.path.lexists(path):
        return
    entry = record.get(path)
    if entry is None or entry["existed"] is not False:
        raise AdapterError("%s already exists and is not a Pill Bar adapter recorded by the installer" % path)
    check_recorded_unchanged(path, record)


def retained_adapter_bytes(record, plugin):
    """Validate every retained destination before treating changed bytes as unsupported source."""
    paths = {os.path.join(plugin, name) for name in ("Panel.qml", "BarWidget.qml", ANCHOR_FILE)}
    if not record or not set(record).issubset(paths):
        return None
    for path in paths:
        preflight_destination(path)
    current = {}
    for path in record:
        if os.path.lexists(path):
            current[path] = sha256_of(path)
    if any(current.get(path) != entry["installed_sha256"] for path, entry in record.items()):
        raise AdapterError("a retained WhatsApp adapter file was modified since the last setup; not overwriting it "
                           "(`waybar-pill-plugin recover` restores the stock files)")
    return current


def planned_whatsapp_adapter(config=None):
    """target absolute path -> proposed content; {} when the WhatsApp plugin is not installed. An untrusted install
    record or directory is an AdapterError; a source this patch cannot handle is its SourceUnsupported subclass."""
    root = config_root(config)
    record = install_record()  # a malformed or untrusted record blocks everything before any file is read
    plugin = os.path.join(root, "omarchy", "plugins", PLUGIN_ID)
    try:
        pfd = _helper.open_directory_chain(plugin, own_from=_helper.trust_root(plugin))
    except _helper.PluginError as e:
        raise AdapterError(str(e))
    if pfd is None:
        return {}
    os.close(pfd)
    retained = retained_adapter_bytes(record, plugin)
    panel_path = os.path.join(plugin, "Panel.qml")
    anchor_path = os.path.join(plugin, ANCHOR_FILE)
    panel = read_regular(panel_path)
    if panel is None:
        raise SourceUnsupported("%s is missing" % panel_path)
    read_regular(anchor_path)  # also rejects symlinks and non-regular files
    source = read_source(os.path.join(HERE, "..", "qml", ANCHOR_FILE))
    check_adapter_ownership(anchor_path, record)
    check_recorded_unchanged(panel_path, record)
    try:
        patched = patch_panel(panel, panel_path)
    except AdapterError as e:
        raise SourceUnsupported(str(e))
    plan = {panel_path: patched, anchor_path: source}
    if retained is not None and any(retained.get(path) != hashlib.sha256(content.encode("utf-8")).hexdigest()
                                    for path, content in plan.items()):
        raise SourceUnsupported("retained WhatsApp adapter is incompatible with this version; "
                                "explicitly recover the previous installation before re-enabling")
    return plan


def whatsapp_disabled():
    """Whether Omarchy's shell.json lists the WhatsApp plugin as disabled. A missing or unreadable file means enabled."""
    text = read_regular(os.path.join(config_root(), "omarchy", "shell.json"))
    if text is None:
        return False
    try:
        cfg = json.loads(text)
    except ValueError:
        return False
    disabled = cfg.get("disabledPlugins", []) if isinstance(cfg, dict) else []
    return isinstance(disabled, list) and PLUGIN_ID in disabled


def preflight_destination(path):
    """The destination's ancestry is trusted and an existing leaf is an owned regular file."""
    try:
        _helper.leaf_present(path)
    except _helper.PluginError as e:
        raise AdapterError(str(e))


def staging_directory(path):
    """The absolute private directory `path` (created 0700 when missing), owned by the effective user."""
    if not os.path.isabs(path) or os.path.normpath(path) != path:
        raise AdapterError("%s is not an absolute normalized path" % path)
    try:
        fd = _helper.open_directory_chain(path, own_from=_helper.trust_root(path), create=True)
    except _helper.PluginError as e:
        raise AdapterError(str(e))
    if fd is None:
        raise AdapterError("%s does not exist" % path)
    try:
        if stat.S_IMODE(os.fstat(fd).st_mode) & 0o077:
            raise AdapterError("%s must not be accessible to other accounts" % path)
    finally:
        os.close(fd)
    return path


def stage(directory):
    """Write the proposed adapter files below `directory` and return the JSON-ready entry list."""
    directory = staging_directory(directory)
    if whatsapp_disabled():
        return []
    try:
        plan = planned_whatsapp_adapter()
    except SourceUnsupported as e:
        print("widget-adapters: the WhatsApp panel is not embedded in the notch: %s" % e, file=sys.stderr)
        return []
    entries = []
    for i, (dest, content) in enumerate(plan.items()):
        preflight_destination(dest)
        mode = 0o644
        if os.path.lexists(dest):
            mode = stat.S_IMODE(os.stat(dest).st_mode)
        source = os.path.join(directory, "%02d-%s" % (i, os.path.basename(dest)))
        dfd = _helper.open_directory_chain(directory, own_from=_helper.trust_root(directory))
        if dfd is None:
            raise AdapterError("%s disappeared" % directory)
        try:
            fd = os.open(os.path.basename(source), os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=dfd)
        finally:
            os.close(dfd)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(content)
            f.flush()
            os.fsync(f.fileno())
        entries.append({"path": dest, "source": source, "mode": "%o" % mode})
    return entries


def main(argv):
    if len(argv) != 2 or argv[0] != "stage":
        print(__doc__, file=sys.stderr)
        return 2
    try:
        entries = stage(argv[1])
    except (AdapterError, OSError) as e:
        print("widget-adapters: %s" % e, file=sys.stderr)
        return 1
    print(json.dumps(entries))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
