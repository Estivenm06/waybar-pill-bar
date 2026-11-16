//! Notification transport for the Omarchy plugin mode. Quickshell owns `org.freedesktop.Notifications`; its
//! `NotificationRelay.qml` forwards each notification over this private Unix socket (NDJSON, protocol version 2) and
//! receives action/close commands back, so the notch's cards stay the only notification UI.
//!
//! QML -> Rust: `hello`, `notify` (upsert by id) and `closed`; the last two carry a monotonically increasing `seq`.
//! Rust -> QML: `ready`, `ack` (exactly once per consumed notify/closed event), `action`, `close`.
//! QML keeps at most 32 events unacknowledged, so the bounded `events` ingress never has to drop anything and the
//! reader thread blocks (instead of queueing without limit) when the GTK consumer falls behind.
//! Only the expected Quickshell PID of the current user may connect. Message contents are never logged.
use crate::{notify, Msg};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub const VERSION: u64 = 2;
const MAX_LINE: usize = 512 * 1024; // JSON text plus its newline, in UTF-8 bytes
const HELLO_WAIT: Duration = Duration::from_secs(5);
/// Highest integer a JavaScript number represents exactly; the QML side counts `seq` in one.
const MAX_SEQ: u64 = 9_007_199_254_740_991;

// Field limits in UTF-16 code units, mirrored from NotificationRelay.qml.
const MAX_APP: usize = 256;
const MAX_SUMMARY: usize = 4096;
const MAX_BODY: usize = 16384;
const MAX_LABEL: usize = 256;
const MAX_KEY: usize = 128;
const MAX_ICON: usize = 4096;
const MAX_ACTIONS: usize = 8;

/// UI -> QML requests.
pub enum Command {
    Action { id: u32, key: String },
    Close { id: u32, reason: u32 },
    /// The event with this sequence number has been consumed (shown, rejected by Do-Not-Disturb or closed).
    Ack { seq: u64 },
}

/// QML -> UI events; every one must be answered with exactly one [`Command::Ack`].
#[derive(Debug)]
pub enum Event {
    Notify { seq: u64, note: notify::Note },
    Closed { seq: u64, id: u32 },
}

pub fn socket_path() -> io::Result<PathBuf> {
    let rt = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(Path::new(&rt).join("waybar-pill-plugin").join("notifications.sock"))
}

/// Bind the relay socket and serve the one authenticated Quickshell connection on worker threads. `tx` carries only
/// transport failures; notification traffic goes through the bounded `events` channel.
pub fn start(shell_pid: i32, tx: async_channel::Sender<Msg>, events: async_channel::Sender<Event>) -> io::Result<mpsc::Sender<Command>> {
    start_at(&socket_path()?, shell_pid, tx, events)
}

pub fn start_at(path: &Path, shell_pid: i32, tx: async_channel::Sender<Msg>, events: async_channel::Sender<Event>) -> io::Result<mpsc::Sender<Command>> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let (ctx, crx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        let stream = loop {
            let conn = match listener.accept() {
                Ok((c, _)) => c,
                Err(e) => return fail(&tx, format!("relay accept failed: {e}")),
            };
            match peer_pid(&conn) {
                Ok((pid, uid)) if pid == shell_pid && uid == unsafe { libc::getuid() } => break conn,
                _ => continue, // wrong peer: dropped (closed); keep waiting for the expected one
            }
        };
        drop(listener);
        if let Err(e) = serve(stream, events, crx) {
            fail(&tx, e.to_string());
        }
    });
    Ok(ctx)
}

fn fail(tx: &async_channel::Sender<Msg>, why: String) {
    let _ = tx.send_blocking(Msg::NotificationRelayFailed(why));
}

fn peer_pid(s: &UnixStream) -> io::Result<(i32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    if r != 0 { return Err(io::Error::last_os_error()); }
    Ok((cred.pid, cred.uid))
}

/// One line, at most MAX_LINE bytes including its newline. `Ok(None)` on clean EOF.
fn read_frame(r: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Option<serde_json::Value>> {
    buf.clear();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', buf)?;
    if n == 0 { return Ok(None); }
    if buf.last() != Some(&b'\n') || buf.len() > MAX_LINE {
        return Err(io::Error::other(if buf.len() > MAX_LINE { "relay frame too large" } else { "relay closed mid-frame" }));
    }
    serde_json::from_slice(buf).map(Some).map_err(|_| io::Error::other("relay sent a malformed frame"))
}

fn serve(stream: UnixStream, events: async_channel::Sender<Event>, crx: mpsc::Receiver<Command>) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    stream.set_read_timeout(Some(HELLO_WAIT))?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let hello = read_frame(&mut reader, &mut buf)?.ok_or_else(|| io::Error::other("relay closed before hello"))?;
    if hello["op"] != "hello" || hello["version"] != VERSION {
        return Err(io::Error::other("relay hello has an unsupported version"));
    }
    writer.write_all(format!("{{\"op\":\"ready\",\"version\":{VERSION}}}\n").as_bytes())?;
    reader.get_ref().set_read_timeout(None)?;

    std::thread::spawn(move || {
        // the writer ends when the sender side is gone or the socket breaks; the reader reports the failure
        while let Ok(c) = crx.recv() {
            let line = match c {
                Command::Action { id, key } => serde_json::json!({"op": "action", "id": id, "key": key}),
                Command::Close { id, reason } => serde_json::json!({"op": "close", "id": id, "reason": reason}),
                Command::Ack { seq } => serde_json::json!({"op": "ack", "seq": seq}),
            };
            if writer.write_all(format!("{line}\n").as_bytes()).and_then(|_| writer.flush()).is_err() { return; }
        }
    });

    let mut last_seq = 0u64;
    loop {
        let Some(f) = read_frame(&mut reader, &mut buf)? else { return Err(io::Error::other("relay closed")) };
        let op = f["op"].as_str();
        if !matches!(op, Some("notify") | Some("closed")) {
            return Err(io::Error::other("relay sent an unknown operation"));
        }
        let seq = f["seq"].as_u64().filter(|s| (1..=MAX_SEQ).contains(s) && *s > last_seq)
            .ok_or_else(|| io::Error::other("relay frame has an invalid sequence number"))?;
        last_seq = seq;
        let event = if op == Some("notify") {
            Event::Notify { seq, note: parse_note(&f)? }
        } else {
            Event::Closed { seq, id: parse_id(&f)? }
        };
        // bounded ingress: QML never has more than 32 unacknowledged events, and a stalled consumer blocks only this thread
        if events.send_blocking(event).is_err() {
            return Err(io::Error::other("relay event consumer is gone"));
        }
    }
}

fn parse_id(f: &serde_json::Value) -> io::Result<u32> {
    f["id"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| io::Error::other("relay frame has an invalid id"))
}

/// A borrowed string field of at most `max` UTF-16 code units; checked before any owned copy is made.
fn limited<'a>(v: &'a serde_json::Value, max: usize, what: &str) -> io::Result<&'a str> {
    let bad = || io::Error::other(format!("relay notify frame has an invalid {what}"));
    let s = v.as_str().ok_or_else(bad)?;
    if s.len() > max * 3 || s.encode_utf16().count() > max { return Err(bad()); }
    Ok(s)
}

fn parse_note(f: &serde_json::Value) -> io::Result<notify::Note> {
    let bad = |what: &str| io::Error::other(format!("relay notify frame has an invalid {what}"));
    let list = f["actions"].as_array().ok_or_else(|| bad("actions"))?;
    if list.len() > MAX_ACTIONS { return Err(bad("actions")); }
    let mut actions = Vec::with_capacity(list.len());
    for a in list {
        let key = limited(&a[0], MAX_KEY, "actions")?;
        let label = limited(&a[1], MAX_LABEL, "actions")?;
        actions.push((key.to_owned(), label.to_owned()));
    }
    Ok(notify::Note {
        id: parse_id(f)?,
        app: limited(&f["app"], MAX_APP, "app")?.to_owned(),
        icon: limited(&f["icon"], MAX_ICON, "icon")?.to_owned(),
        summary: limited(&f["summary"], MAX_SUMMARY, "summary")?.to_owned(),
        body: limited(&f["body"], MAX_BODY, "body")?.to_owned(),
        actions,
        urgency: f["urgency"].as_u64().filter(|u| *u <= 2).ok_or_else(|| bad("urgency"))? as u8,
        timeout_ms: f["timeout_ms"].as_i64().and_then(|v| i32::try_from(v).ok()).ok_or_else(|| bad("timeout_ms"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn sock(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nr-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    fn me() -> i32 { std::process::id() as i32 }

    fn connect(p: &Path) -> (UnixStream, BufReader<UnixStream>) {
        let s = UnixStream::connect(p).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let r = BufReader::new(s.try_clone().unwrap());
        (s, r)
    }

    fn line(r: &mut BufReader<UnixStream>) -> serde_json::Value {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap()
    }

    fn handshake(p: &Path) -> (UnixStream, BufReader<UnixStream>) {
        let (mut s, mut r) = connect(p);
        s.write_all(b"{\"op\":\"hello\",\"version\":2}\n").unwrap();
        let ready = line(&mut r);
        assert_eq!((ready["op"].as_str(), ready["version"].as_u64()), (Some("ready"), Some(2)));
        (s, r)
    }

    /// Relay with the production-sized events ingress.
    fn relay(tag: &str) -> (PathBuf, mpsc::Sender<Command>, async_channel::Receiver<Msg>, async_channel::Receiver<Event>) {
        relay_with(tag, 32)
    }

    fn relay_with(tag: &str, capacity: usize) -> (PathBuf, mpsc::Sender<Command>, async_channel::Receiver<Msg>, async_channel::Receiver<Event>) {
        let p = sock(tag);
        let (tx, rx) = async_channel::unbounded();
        let (etx, erx) = async_channel::bounded(capacity);
        let cmds = start_at(&p, me(), tx, etx).unwrap();
        (p, cmds, rx, erx)
    }

    fn wait<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = f() { return v; }
            assert!(std::time::Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn recv(rx: &async_channel::Receiver<Msg>) -> Msg {
        wait("a message", || rx.try_recv().ok())
    }

    fn recv_event(rx: &async_channel::Receiver<Event>) -> Event {
        wait("an event", || rx.try_recv().ok())
    }

    fn note(seq: u64, id: u32, summary: &str) -> String {
        format!(r#"{{"op":"notify","seq":{seq},"id":{id},"app":"A","icon":"","summary":"{summary}","body":"B","actions":[["default","Open"]],"urgency":1,"timeout_ms":-1}}"#)
    }

    const NOTE: &str = r#"{"op":"notify","seq":1,"id":7,"app":"A","icon":"","summary":"S","body":"B","actions":[["default","Open"]],"urgency":1,"timeout_ms":-1}"#;

    #[test]
    fn notify_replace_close_ack_and_commands() {
        let (p, cmds, _rx, erx) = relay("flow");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let (mut s, mut r) = handshake(&p);
        writeln!(s, "{}", note(1, 7, "S")).unwrap();
        writeln!(s, "{}", note(2, 7, "S2")).unwrap();
        writeln!(s, "{{\"op\":\"closed\",\"seq\":3,\"id\":7}}").unwrap();
        for (want_seq, want) in [(1, "S"), (2, "S2")] {
            match recv_event(&erx) {
                Event::Notify { seq, note: n } => assert_eq!((seq, n.id, n.summary.as_str(), n.actions.len()), (want_seq, 7, want, 1)),
                e => panic!("expected Notify, got {e:?}"),
            }
        }
        assert!(matches!(recv_event(&erx), Event::Closed { seq: 3, id: 7 }));
        cmds.send(Command::Action { id: 7, key: "default".into() }).unwrap();
        cmds.send(Command::Close { id: 7, reason: 2 }).unwrap();
        cmds.send(Command::Ack { seq: 3 }).unwrap();
        let a = line(&mut r);
        assert_eq!((a["op"].as_str(), a["id"].as_u64(), a["key"].as_str()), (Some("action"), Some(7), Some("default")));
        let c = line(&mut r);
        assert_eq!((c["op"].as_str(), c["id"].as_u64(), c["reason"].as_u64()), (Some("close"), Some(7), Some(2)));
        let k = line(&mut r);
        assert_eq!((k["op"].as_str(), k["seq"].as_u64()), (Some("ack"), Some(3)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_paused_consumer_blocks_the_reader_instead_of_queueing() {
        let (p, _cmds, rx, erx) = relay_with("pause", 2);
        let (mut s, _r) = handshake(&p);
        for seq in 1..=5u64 { writeln!(s, "{}", note(seq, seq as u32, "x")).unwrap(); }
        wait("the ingress to fill", || (erx.len() == 2).then_some(()));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(erx.len(), 2, "the ingress must stay at its capacity while the consumer is paused");
        assert!(rx.try_recv().is_err(), "a slow consumer is not a transport failure");
        for want in 1..=5u64 {
            match recv_event(&erx) {
                Event::Notify { seq, .. } => assert_eq!(seq, want),
                e => panic!("expected Notify, got {e:?}"),
            }
            assert!(erx.len() <= 2);
        }
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_events_ingress_has_the_documented_capacity() {
        let (_p, _cmds, _rx, erx) = relay("cap");
        assert_eq!(erx.capacity(), Some(32));
    }

    #[test]
    fn wrong_peer_is_rejected() {
        let p = sock("peer");
        let (tx, rx) = async_channel::unbounded();
        let (etx, erx) = async_channel::bounded(32);
        start_at(&p, me() + 1, tx, etx).unwrap();
        let (mut s, mut r) = connect(&p);
        let _ = s.write_all(b"{\"op\":\"hello\",\"version\":2}\n");
        let mut l = String::new();
        assert_eq!(r.read_line(&mut l).unwrap_or(0), 0, "no ready for a foreign peer");
        assert!(rx.try_recv().is_err());
        assert!(erx.try_recv().is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_version_malformed_and_oversize_fail_the_relay() {
        for (tag, payload) in [
            ("ver", b"{\"op\":\"hello\",\"version\":3}\n".to_vec()),
            ("oldver", b"{\"op\":\"hello\",\"version\":1}\n".to_vec()),
            ("junk", b"not json\n".to_vec()),
        ] {
            let (p, _cmds, rx, _erx) = relay(tag);
            let (mut s, _r) = connect(&p);
            s.write_all(&payload).unwrap();
            assert!(matches!(recv(&rx), Msg::NotificationRelayFailed(_)), "{tag}");
            let _ = std::fs::remove_file(&p);
        }
        let long_summary = format!("{{\"op\":\"notify\",\"seq\":1,\"id\":7,\"app\":\"A\",\"icon\":\"\",\"summary\":\"{}\",\"body\":\"B\",\"actions\":[],\"urgency\":1,\"timeout_ms\":-1}}\n", "x".repeat(MAX_SUMMARY + 1));
        let too_many_actions = NOTE.replace(r#"[["default","Open"]]"#, &format!("[{}]", vec![r#"["k","v"]"#; MAX_ACTIONS + 1].join(",")));
        let long_key = NOTE.replace(r#"["default","Open"]"#, &format!(r#"["{}","Open"]"#, "k".repeat(MAX_KEY + 1)));
        // 2049 astral characters are 4098 UTF-16 units although they are only 2049 chars
        let astral_summary = NOTE.replace("\"S\"", &format!("\"{}\"", "\u{1F600}".repeat(MAX_SUMMARY / 2 + 1)));
        let long_icon = NOTE.replace("\"icon\":\"\"", &format!("\"icon\":\"{}\"", "i".repeat(MAX_ICON + 1)));
        for (tag, frame) in [
            ("big", format!("{{\"op\":\"notify\",\"pad\":\"{}\"}}\n", "x".repeat(MAX_LINE))),
            ("exactbig", format!("{}\n", " ".repeat(MAX_LINE))), // MAX_LINE bytes + newline = one over
            ("badfield", NOTE.replace("\"urgency\":1", "\"urgency\":9") + "\n"),
            ("noseq", NOTE.replace("\"seq\":1,", "") + "\n"),
            ("zeroseq", NOTE.replace("\"seq\":1", "\"seq\":0") + "\n"),
            ("hugeseq", NOTE.replace("\"seq\":1", "\"seq\":9007199254740992") + "\n"),
            ("floatseq", NOTE.replace("\"seq\":1", "\"seq\":1.5") + "\n"),
            ("closednoseq", "{\"op\":\"closed\",\"id\":7}\n".into()),
            ("summary", long_summary),
            ("actions", too_many_actions + "\n"),
            ("key", long_key + "\n"),
            ("astral", astral_summary + "\n"),
            ("icon", long_icon + "\n"),
            ("ack", "{\"op\":\"ack\",\"seq\":1}\n".into()),
            ("eof", String::new()),
        ] {
            let (p, _cmds, rx, _erx) = relay(tag);
            let (mut s, _r) = handshake(&p);
            let _ = s.write_all(frame.as_bytes());
            if frame.is_empty() { s.shutdown(std::net::Shutdown::Both).unwrap(); }
            assert!(matches!(recv(&rx), Msg::NotificationRelayFailed(_)), "{tag}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn a_non_increasing_sequence_fails_the_relay() {
        let (p, _cmds, rx, erx) = relay("seq");
        let (mut s, _r) = handshake(&p);
        writeln!(s, "{}", note(5, 1, "a")).unwrap();
        writeln!(s, "{}", note(5, 2, "b")).unwrap();
        assert!(matches!(recv_event(&erx), Event::Notify { seq: 5, .. }));
        assert!(matches!(recv(&rx), Msg::NotificationRelayFailed(_)));
        assert!(erx.try_recv().is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn field_limits_accept_the_exact_boundary() {
        let (p, _cmds, rx, erx) = relay("limits");
        let (mut s, _r) = handshake(&p);
        let actions = vec![format!(r#"["{}","{}"]"#, "k".repeat(MAX_KEY), "l".repeat(MAX_LABEL)); MAX_ACTIONS].join(",");
        let frame = format!(
            "{{\"op\":\"notify\",\"seq\":1,\"id\":9,\"app\":\"{}\",\"icon\":\"{}\",\"summary\":\"{}\",\"body\":\"{}\",\"actions\":[{actions}],\"urgency\":0,\"timeout_ms\":0}}\n",
            "a".repeat(MAX_APP), "i".repeat(MAX_ICON), "\u{1F600}".repeat(MAX_SUMMARY / 2), "b".repeat(MAX_BODY));
        s.write_all(frame.as_bytes()).unwrap();
        match recv_event(&erx) {
            Event::Notify { note, .. } => {
                assert_eq!((note.app.len(), note.icon.len(), note.body.len(), note.actions.len()), (MAX_APP, MAX_ICON, MAX_BODY, MAX_ACTIONS));
                assert_eq!(note.summary.encode_utf16().count(), MAX_SUMMARY);
            }
            e => panic!("expected Notify, got {e:?}"),
        }
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_file(&p);
    }
}
