
  // ---- test-only probe (appended to a temporary copy of the patched Panel; never installed) ----

  function sameWindow(win, item) {
    try { return win !== null && item !== null && win === item.Window.window } catch (e) { return false }
  }

  // widest visible leaf item beyond the viewport width, in viewport coordinates
  function pillOverflowX() {
    var worst = 0
    var vp = pillViewport.contentItem
    function walk(it, depth) {
      if (!it || !it.visible || depth > 14) return
      if (it !== vp && it.width > 0 && it.height > 0 && it.toString().indexOf("Text") === 0) {
        var p = it.mapToItem(vp, 0, 0)
        worst = Math.max(worst, p.x + it.width - pillViewport.width, -p.x)
      }
      var kids = it.children
      for (var i = 0; i < kids.length; i++) walk(kids[i], depth + 1)
    }
    walk(content, 0)
    return worst
  }


  IpcHandler {
    target: "probe"

    function out(): string { return pillAnchor.screenObj ? pillAnchor.screenObj.name : "" }

    function state(): string {
      var h = pillAnchor.contentItem
      function rect(it) {
        if (!it) return null
        var p = it.mapToItem(h, 0, 0)
        return { x: p.x, y: p.y, w: it.width, h: it.height, visible: it.visible }
      }
      var win = keyCatcher.Window.window
      var m = pillAnchor.surface.mask
      return JSON.stringify({
        active: pillAnchor.active, stage: pillAnchor.stage, session: pillAnchor.session,
        kcUnderHolder: keyCatcher.parent === h,
        kcInExternalWindow: root.sameWindow(win, pillAnchor.surface.contentItem),
        kcParentNative: keyCatcher.parent !== null && keyCatcher.parent !== h,
        holder: { x: h.x, y: h.y, w: h.width, h: h.height },
        mask: { x: m.x, y: m.y, w: m.width, h: m.height },
        surfaceVisible: pillAnchor.surfaceVisible,
        surfaceMapped: pillAnchor.surface.backingWindowVisible,
        nativeMapped: panel.backingWindowVisible,
        nativeOpen: panel.open,
        opened: root.opened,
        fg: String(root.foreground), font: root.fontFamily, body: root.pillFont.body, caption: root.pillFont.caption,
        accent: String(root.pillAccent),
        view: root.view, jid: root.activeJid,
        kcFocus: keyCatcher.activeFocus, composerFocus: composer.activeFocus,
        draft: composer.text,
        vp: { w: pillViewport.width, h: pillViewport.height, ch: pillViewport.contentHeight, y: pillViewport.contentY,
              interactive: pillViewport.interactive, parentH: pillViewport.parent.height },
        header: rect(pillHeader), sep: rect(pillSeparator), hints: rect(pillHints), reply: rect(pillReply),
        chats: rect(chatList), msgs: rect(messageList), composer: rect(composer), strip: rect(listStrip),
        login: rect(pillLogin), qr: rect(qrImage),
        chatList: { h: chatList.height, ch: chatList.contentHeight, interactive: chatList.interactive },
        msgList: { h: messageList.height, ch: messageList.contentHeight },
        picker: { open: root.listPickerOpen, idx: root.listPickerIndex, h: pillPickerList.height, ch: pillPickerList.contentHeight,
                  y: pillPickerList.contentY, interactive: pillPickerList.interactive },
        logout: { open: root.logoutConfirmOpen, vh: pillLogoutViewport.height, ch: pillLogoutViewport.contentHeight },
        peek: { active: root.peekActive, embedded: pillEmbeddedPeek.visible, nativeWin: imagePeekOverlay.visible },
        overflowX: root.pillOverflowX(),
        calls: root.client.calls
      })
    }

    function act(name: string, arg: string): string {
      var c = root.client
      if (name === "scenario") {
        var chats = [], i
        c.needsLogin = false; c.hasQr = false; c.daemonOnline = true; c.pendingLogin = false; c.linkUp = true; c.signedIn = true
        if (arg === "many" || arg === "convo" || arg === "picker") {
          for (i = 0; i < 60; i++)
            chats.push({ jid: "j" + i + "@s.whatsapp.net", name: "A very long conversation name number " + i + " that never fits a notch row at all",
                         lastText: "An equally long preview of the latest message in that conversation " + i + " with more words",
                         lastTs: 1700000000 + i, unread: i % 3, isGroup: i % 4 === 0, favorite: i % 5 === 0, pinned: 0, lists: i % 2 ? ["l1"] : [] })
          var lists = []
          for (i = 0; i < 8; i++) lists.push({ id: "l" + (i + 1), name: "Custom list with a really quite long name " + (i + 1) })
          c.lists = lists
        }
        if (arg === "convo") {
          var msgs = []
          for (i = 0; i < 25; i++)
            msgs.push({ id: "m" + i, fromMe: i % 2 === 0, ts: 1700000000 + i * 60, status: 3,
                        text: "A long wrapped message body number " + i + " that is certainly longer than any single line in the notch could possibly show without wrapping " + "x".repeat(i % 3 ? 0 : 80),
                        senderName: "Someone with a long name" })
          c.thread = msgs
        }
        if (arg === "login" || arg === "loginqr") {
          c.needsLogin = true; c.signedIn = false; c.hasQr = arg === "loginqr"; c.qrPng = root.client.qrFile
        }
        if (arg === "offline") { c.daemonOnline = false; c.linkUp = false; c.signedIn = false; root.statusLine = "Daemon offline" }
        c.chats = chats
        c.chatsEpoch = c.chatsEpoch + 1
        c.chatsChanged()
        root.view = "chats"; root.activeJid = ""; root.cursorIndex = 0
        return "ok"
      }
      if (name === "openChat") { root.selectChat(arg); return "ok" }
      if (name === "draft") { composer.forceActiveFocus(); composer.text = arg; return "ok" }
      if (name === "closePicker") { root.closeListPicker(); return "ok" }
      if (name === "escape") { keyCatcher.closeRequested(); return "ok" }
      if (name === "back") { root.pillBack(); return "ok" }
      if (name === "send") { root.sendReply(); return "ok" }
      if (name === "picker") { root.cursorIndex = 0; root.openListPicker(); return "ok" }
      if (name === "pickerIndex") { root.listPickerIndex = parseInt(arg); return "ok" }
      if (name === "toggleMember") { root.toggleListMembership(parseInt(arg)); return "ok" }
      if (name === "logout") { root.requestLogout(); return "ok" }
      if (name === "cancelLogout") { root.cancelLogout(); return "ok" }
      if (name === "peek") { root.peekImagePath = arg; return "ok" }
      if (name === "nativeOpen") { root.open(); return "ok" }
      if (name === "closePanel") { root.close(); return "ok" }
      if (name === "scroll") { pillViewport.contentY = parseInt(arg); return "ok" }
      if (name === "scrollEnd") { pillViewport.contentY = Math.max(0, pillViewport.contentHeight - pillViewport.height); return "ok" }
      if (name === "qr") { c.qrFile = arg; return "ok" }
      return "unknown"
    }
  }
