//! Notification transport for the Omarchy plugin mode. Quickshell owns `org.freedesktop.Notifications`; its
//! `NotificationRelay.qml` forwards each notification over this private Unix socket (NDJSON, protocol version 1) and
//! receives action/close commands back, so the notch's cards stay the only notification UI.
//!
//! QML -> Rust: `hello`, `notify` (upsert by id), `closed`.  Rust -> QML: `ready`, `action`, `close`.
//! Only the expected Quickshell PID of the current user may connect. Message contents are never logged.
use crate::{notify, Msg};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

pub const VERSION: u64 = 1;
const MAX_LINE: usize = 512 * 1024;
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// UI -> QML requests.
pub enum Command {
    Action { id: u32, key: String },
    Close { id: u32, reason: u32 },
}

pub fn socket_path() -> io::Result<PathBuf> {
    let rt = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(Path::new(&rt).join("waybar-pill-plugin").join("notifications.sock"))
}

/// Bind the relay socket and serve the one authenticated Quickshell connection on worker threads.
pub fn start(shell_pid: i32, tx: async_channel::Sender<Msg>) -> io::Result<mpsc::Sender<Command>> {
    start_at(&socket_path()?, shell_pid, tx)
}

pub fn start_at(path: &Path, shell_pid: i32, tx: async_channel::Sender<Msg>) -> io::Result<mpsc::Sender<Command>> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let (ctx, crx) = mpsc::channel::<Command>();
    let reply = ctx.clone();
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
        if let Err(e) = serve(stream, tx.clone(), crx, reply) {
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

/// One line, at most MAX_LINE bytes. `Ok(None)` on clean EOF.
fn read_frame(r: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Option<serde_json::Value>> {
    buf.clear();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', buf)?;
    if n == 0 { return Ok(None); }
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::other(if buf.len() > MAX_LINE { "relay frame too large" } else { "relay closed mid-frame" }));
    }
    serde_json::from_slice(buf).map(Some).map_err(|_| io::Error::other("relay sent a malformed frame"))
}

fn serve(stream: UnixStream, tx: async_channel::Sender<Msg>, crx: mpsc::Receiver<Command>, reply: mpsc::Sender<Command>) -> io::Result<()> {
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
            };
            if writer.write_all(format!("{line}\n").as_bytes()).and_then(|_| writer.flush()).is_err() { return; }
        }
    });

    loop {
        let Some(f) = read_frame(&mut reader, &mut buf)? else { return Err(io::Error::other("relay closed")) };
        match f["op"].as_str() {
            Some("notify") => {
                let n = parse_note(&f)?;
                if notify::dnd_on() && n.urgency < 2 {
                    let _ = reply.send(Command::Close { id: n.id, reason: notify::REASON_EXPIRED });
                } else {
                    let _ = tx.send_blocking(Msg::Notify(n));
                }
            }
            Some("closed") => {
                let id = f["id"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| io::Error::other("relay frame has an invalid id"))?;
                let _ = tx.send_blocking(Msg::NotifyClose(id));
            }
            _ => return Err(io::Error::other("relay sent an unknown operation")),
        }
    }
}

fn parse_note(f: &serde_json::Value) -> io::Result<notify::Note> {
    let bad = |what: &str| io::Error::other(format!("relay notify frame has an invalid {what}"));
    let s = |k: &str| f[k].as_str().map(str::to_owned).ok_or_else(|| bad(k));
    let mut actions = Vec::new();
    for a in f["actions"].as_array().ok_or_else(|| bad("actions"))? {
        match (a[0].as_str(), a[1].as_str()) {
            (Some(k), Some(l)) => actions.push((k.to_owned(), l.to_owned())),
            _ => return Err(bad("actions")),
        }
    }
    Ok(notify::Note {
        id: f["id"].as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| bad("id"))?,
        app: s("app")?,
        icon: s("icon")?,
        summary: s("summary")?,
        body: s("body")?,
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
        s.write_all(b"{\"op\":\"hello\",\"version\":1}\n").unwrap();
        assert_eq!(line(&mut r)["op"], "ready");
        (s, r)
    }

    fn recv(rx: &async_channel::Receiver<Msg>) -> Msg {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(m) = rx.try_recv() { return m; }
            assert!(std::time::Instant::now() < end, "timed out waiting for a message");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const NOTE: &str = r#"{"op":"notify","id":7,"app":"A","icon":"","summary":"S","body":"B","actions":[["default","Open"]],"urgency":1,"timeout_ms":-1}"#;

    #[test]
    fn notify_replace_close_and_commands() {
        let p = sock("flow");
        let (tx, rx) = async_channel::unbounded();
        let cmds = start_at(&p, me(), tx).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let (mut s, mut r) = handshake(&p);
        writeln!(s, "{NOTE}").unwrap();
        writeln!(s, "{}", NOTE.replace("\"S\"", "\"S2\"")).unwrap();
        writeln!(s, "{{\"op\":\"closed\",\"id\":7}}").unwrap();
        for want in ["S", "S2"] {
            match recv(&rx) {
                Msg::Notify(n) => assert_eq!((n.id, n.summary.as_str(), n.actions.len()), (7, want, 1)),
                _ => panic!("expected Notify"),
            }
        }
        assert!(matches!(recv(&rx), Msg::NotifyClose(7)));
        cmds.send(Command::Action { id: 7, key: "default".into() }).unwrap();
        cmds.send(Command::Close { id: 7, reason: 2 }).unwrap();
        let a = line(&mut r);
        assert_eq!((a["op"].as_str(), a["id"].as_u64(), a["key"].as_str()), (Some("action"), Some(7), Some("default")));
        let c = line(&mut r);
        assert_eq!((c["op"].as_str(), c["id"].as_u64(), c["reason"].as_u64()), (Some("close"), Some(7), Some(2)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wrong_peer_is_rejected() {
        let p = sock("peer");
        let (tx, rx) = async_channel::unbounded();
        start_at(&p, me() + 1, tx).unwrap();
        let (mut s, mut r) = connect(&p);
        let _ = s.write_all(b"{\"op\":\"hello\",\"version\":1}\n");
        let mut l = String::new();
        assert_eq!(r.read_line(&mut l).unwrap_or(0), 0, "no ready for a foreign peer");
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_version_malformed_and_oversize_fail_the_relay() {
        for (tag, payload) in [
            ("ver", b"{\"op\":\"hello\",\"version\":2}\n".to_vec()),
            ("junk", b"not json\n".to_vec()),
        ] {
            let p = sock(tag);
            let (tx, rx) = async_channel::unbounded();
            start_at(&p, me(), tx).unwrap();
            let (mut s, _r) = connect(&p);
            s.write_all(&payload).unwrap();
            assert!(matches!(recv(&rx), Msg::NotificationRelayFailed(_)), "{tag}");
            let _ = std::fs::remove_file(&p);
        }
        for (tag, frame) in [
            ("big", format!("{{\"op\":\"notify\",\"pad\":\"{}\"}}\n", "x".repeat(MAX_LINE))),
            ("badfield", NOTE.replace("\"urgency\":1", "\"urgency\":9") + "\n"),
            ("eof", String::new()),
        ] {
            let p = sock(tag);
            let (tx, rx) = async_channel::unbounded();
            start_at(&p, me(), tx).unwrap();
            let (mut s, _r) = handshake(&p);
            let _ = s.write_all(frame.as_bytes());
            if frame.is_empty() { s.shutdown(std::net::Shutdown::Both).unwrap(); }
            assert!(matches!(recv(&rx), Msg::NotificationRelayFailed(_)), "{tag}");
            let _ = std::fs::remove_file(&p);
        }
    }
}
