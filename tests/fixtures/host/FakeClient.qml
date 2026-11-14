import QtQuick

// Synthetic WhatsApp client for the content-host fixture: implements exactly the surface Panel.qml reads and calls,
// records every call, and never talks to a daemon.
QtObject {
  id: c
  property var chats: []
  property int chatsEpoch: 0
  property var lists: []
  property int unread: 0
  property bool daemonOnline: true
  property bool needsLogin: false
  property bool pairingStopped: false
  property bool hasQr: false
  property bool signedIn: true
  property string qrPng: ""
  property string qrFile: ""
  property bool pendingLogin: false
  property bool linkUp: true
  property bool ready: true
  property var thread: []
  property var calls: []

  signal messagesLoaded(string jid, var chat, var messages)
  signal messageArrived(string jid, var message, var chat)
  signal messageStatusChanged(string jid, string messageId, int status)
  signal messageMedia(string jid, string messageId, string imagePath)
  signal commandFailed(string command, string message)

  function rec(name, args) { c.calls = c.calls.concat([{ name: name, args: args }]) }
  function chatOf(jid) {
    for (var i = 0; i < c.chats.length; i++) if (c.chats[i].jid === jid) return c.chats[i]
    return { jid: jid, name: "x" }
  }
  function loadMessages(jid, limit) { rec("loadMessages", [jid]); c.messagesLoaded(jid, c.chatOf(jid), c.thread) }
  function markRead(jid) { rec("markRead", [jid]); return true }
  function refresh() { rec("refresh", []) }
  function requestChats(limit) { rec("requestChats", [limit]) }
  function refreshInbox(jid, a, b) { rec("refreshInbox", [jid]); return true }
  function sendMessage(jid, text) { rec("sendMessage", [jid, text]); return true }
  function setTyping(jid, state) { rec("setTyping", [jid, state]) }
  function setListMember(jid, listId, member) { rec("setListMember", [jid, listId, member]); return true }
  function movePin(jid, delta) { rec("movePin", [jid, delta]); return true }
  function logout() { rec("logout", []) }
  function startLogin() { rec("startLogin", []) }
  function startDaemon() { rec("startDaemon", []) }
}
