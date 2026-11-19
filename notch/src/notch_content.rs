//! Private transport that lets the WhatsApp plugin's QML content sit inside the notch's WhatsApp page.
//! GTK owns the notch backplate, the page slot and every timer; Quickshell connects to this socket, asks for the page
//! (`open`), receives the settled slot rectangle plus the notch's theme (`layout`) and reports `mapped`, `interaction`,
//! `close` and `hidden`. NDJSON, protocol version 1, frames of at most 16 KiB. Only the expected Quickshell PID of the
//! current user may connect. Chat data is never forwarded and message contents are never logged.
//!
//! QML -> Rust: `open`, `mapped`, `interaction`, `close`, `hidden`.   Rust -> QML: `layout`, `suspend`, `hide`, `error`.
//! The connection ID doubles as the wire `session`; every post-open frame must carry it.
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Where connection events go; returns false when the consumer is gone.
pub type Sink = Arc<dyn Fn(Event) -> bool + Send + Sync>;
use std::time::Duration;

pub const VERSION: u64 = 1;
const MAX_FRAME: usize = 16 * 1024;
const OPEN_WAIT: Duration = Duration::from_secs(5);
const WRITE_WAIT: Duration = Duration::from_secs(1);
const MAX_OUTPUT_NAME: usize = 256;

/// Initial mapping must complete within this long, else the open fails.
pub const MAP_TIMEOUT_MS: u64 = 5000;
/// An ordinary close waits this long for `hidden` before disconnecting.
pub const CLOSE_TIMEOUT_MS: u64 = 250;

/// Something a connection reported; `conn` is the connection (= session) ID.
#[derive(Debug, PartialEq)]
pub enum Event {
    Open { conn: u64, output: String },
    Mapped { conn: u64, serial: u64 },
    Interaction { conn: u64, hovered: bool, busy: bool },
    Close { conn: u64, home: bool },
    Hidden { conn: u64 },
    /// EOF, socket error or protocol violation: the connection is finished.
    Gone { conn: u64 },
    /// The listener itself failed.
    Failed(String),
}

pub fn socket_path() -> io::Result<PathBuf> {
    let rt = std::env::var("XDG_RUNTIME_DIR").map_err(|_| io::Error::other("XDG_RUNTIME_DIR is not set"))?;
    Ok(Path::new(&rt).join("waybar-pill-plugin").join("notch-content.sock"))
}

type Conns = Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>;

/// Outbound side of the listener. Frames are queued to a per-connection writer thread, so the GTK thread never blocks.
pub struct Bridge {
    conns: Conns,
}

impl Bridge {
    /// Queue a frame; false when the connection is gone.
    pub fn send(&self, connection: u64, frame: Value) -> bool {
        self.conns.lock().unwrap().get(&connection).is_some_and(|tx| tx.send(frame).is_ok())
    }

    /// Flush what is queued, then close that stream. Unknown connections are ignored.
    pub fn disconnect(&self, connection: u64) {
        self.conns.lock().unwrap().remove(&connection);
    }
}

pub fn start(shell_pid: i32, tx: Sink) -> io::Result<Bridge> {
    start_at(&socket_path()?, shell_pid, tx)
}

pub fn start_at(path: &Path, shell_pid: i32, tx: Sink) -> io::Result<Bridge> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let conns: Conns = Arc::new(Mutex::new(HashMap::new()));
    let next = Arc::new(AtomicU64::new(1));
    let reg = conns.clone();
    std::thread::spawn(move || loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) => {
                let _ = tx(Event::Failed(format!("accept failed: {e}")));
                return;
            }
        };
        match peer_cred(&stream) {
            Ok((pid, uid)) if pid == shell_pid && uid == unsafe { libc::getuid() } => {}
            _ => continue, // wrong peer: dropped (closed); keep serving the expected one
        }
        let conn = next.fetch_add(1, Ordering::Relaxed);
        let Ok(writer) = stream.try_clone() else { continue };
        let _ = writer.set_write_timeout(Some(WRITE_WAIT));
        let (wtx, wrx) = mpsc::channel::<Value>();
        reg.lock().unwrap().insert(conn, wtx);
        std::thread::spawn(move || write_loop(writer, wrx));
        let (tx, reg) = (tx.clone(), reg.clone());
        std::thread::spawn(move || {
            read_loop(conn, stream, &tx, &reg);
            reg.lock().unwrap().remove(&conn);
            let _ = tx(Event::Gone { conn });
        });
    });
    Ok(Bridge { conns })
}

fn peer_cred(s: &UnixStream) -> io::Result<(i32, u32)> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut cred as *mut _ as *mut libc::c_void, &mut len) };
    if r != 0 { return Err(io::Error::last_os_error()); }
    Ok((cred.pid, cred.uid))
}

fn write_loop(mut stream: UnixStream, rx: mpsc::Receiver<Value>) {
    // ends when the connection is unregistered (every queued frame is still written first) or the peer breaks
    while let Ok(v) = rx.recv() {
        if stream.write_all(format!("{v}\n").as_bytes()).and_then(|_| stream.flush()).is_err() { break; }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// One line of at most MAX_FRAME bytes. `Ok(None)` on clean EOF.
fn read_frame(r: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Option<Value>> {
    buf.clear();
    let n = r.by_ref().take(MAX_FRAME as u64 + 1).read_until(b'\n', buf)?;
    if n == 0 { return Ok(None); }
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::other(if buf.len() > MAX_FRAME { "frame too large" } else { "closed mid-frame" }));
    }
    serde_json::from_slice(buf).map(Some).map_err(|_| io::Error::other("malformed frame"))
}

fn error_frame(message: &str) -> Value { serde_json::json!({"op": "error", "message": message}) }

fn read_loop(conn: u64, stream: UnixStream, tx: &Sink, reg: &Conns) {
    let reject = |why: &str| {
        if let Some(w) = reg.lock().unwrap().get(&conn) { let _ = w.send(error_frame(why)); }
    };
    let _ = stream.set_read_timeout(Some(OPEN_WAIT));
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let first = match read_frame(&mut reader, &mut buf) {
        Ok(Some(f)) => f,
        Ok(None) => return,
        Err(e) => return reject(&e.to_string()),
    };
    let open = match parse_open(&first) {
        Ok(o) => o,
        Err(why) => return reject(why),
    };
    let _ = reader.get_ref().set_read_timeout(None);
    if !tx(Event::Open { conn, output: open }) { return; }
    loop {
        let f = match read_frame(&mut reader, &mut buf) {
            Ok(Some(f)) => f,
            Ok(None) => return,
            Err(e) => return reject(&e.to_string()),
        };
        match parse_frame(conn, &f) {
            Ok(ev) => { if !tx(ev) { return; } }
            Err(why) => return reject(why),
        }
    }
}

fn parse_open(f: &Value) -> Result<String, &'static str> {
    if f["op"] != "open" { return Err("the first frame must be open"); }
    if f["version"] != VERSION { return Err("unsupported protocol version"); }
    match f["output"].as_str() {
        Some(o) if !o.is_empty() && o.len() <= MAX_OUTPUT_NAME && !o.contains(char::is_control) => Ok(o.to_string()),
        _ => Err("open needs a valid output name"),
    }
}

/// Validate one post-open frame for connection `conn`.
fn parse_frame(conn: u64, f: &Value) -> Result<Event, &'static str> {
    if f["session"].as_u64() != Some(conn) { return Err("frame carries the wrong session"); }
    match f["op"].as_str() {
        Some("mapped") => Ok(Event::Mapped { conn, serial: f["serial"].as_u64().ok_or("mapped needs a serial")? }),
        Some("interaction") => match (f["hovered"].as_bool(), f["busy"].as_bool()) {
            (Some(hovered), Some(busy)) => Ok(Event::Interaction { conn, hovered, busy }),
            _ => Err("interaction needs hovered and busy"),
        },
        Some("close") => Ok(Event::Close { conn, home: f["home"].as_bool().ok_or("close needs home")? }),
        Some("hidden") => Ok(Event::Hidden { conn }),
        _ => Err("unknown operation"),
    }
}

// ------------------------------------------------------------------ session reducer

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase { Open, Closing }

/// What the periodic check decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tick {
    Idle,
    /// The first mapping never arrived.
    MapTimeout,
    /// The bounded close ran out of time.
    CloseTimeout,
}

/// Deterministic state of the one live content session. Time is injected (`now_ms`), never read here.
#[derive(Clone, Debug)]
pub struct Session {
    pub conn: u64,
    pub phase: Phase,
    /// Serial of the newest layout sent; an acknowledgement for any other serial is stale.
    pub serial: u64,
    pub mapped: bool,
    pub ever_mapped: bool,
    pub hovered: bool,
    pub busy: bool,
    born_ms: u64,
    closing_since_ms: u64,
}

impl Session {
    pub fn new(conn: u64, now_ms: u64) -> Session {
        Session { conn, phase: Phase::Open, serial: 0, mapped: false, ever_mapped: false, hovered: false, busy: false, born_ms: now_ms, closing_since_ms: 0 }
    }

    /// A new layout goes out: its serial supersedes every earlier acknowledgement.
    pub fn layout_sent(&mut self) -> u64 {
        self.serial += 1;
        self.mapped = false;
        self.serial
    }

    /// The content was hidden for a resize; the next layout will carry a fresh serial.
    pub fn suspended(&mut self) { self.mapped = false; }

    /// True when the acknowledgement matches the current layout of an open session.
    pub fn on_mapped(&mut self, serial: u64) -> bool {
        if self.phase != Phase::Open || serial != self.serial || self.serial == 0 { return false; }
        self.mapped = true;
        self.ever_mapped = true;
        true
    }

    pub fn on_interaction(&mut self, hovered: bool, busy: bool) {
        if self.phase == Phase::Open { self.hovered = hovered; self.busy = busy; }
    }

    /// Start a bounded close; false when one is already running.
    pub fn begin_close(&mut self, now_ms: u64) -> bool {
        if self.phase == Phase::Closing { return false; }
        self.phase = Phase::Closing;
        self.closing_since_ms = now_ms;
        self.hovered = false;
        self.busy = false;
        true
    }

    /// A pointer or editor in the content keeps the page open.
    pub fn holds_open(&self) -> bool { self.phase == Phase::Open && (self.hovered || self.busy) }

    pub fn tick(&self, now_ms: u64) -> Tick {
        match self.phase {
            Phase::Closing if now_ms.saturating_sub(self.closing_since_ms) >= CLOSE_TIMEOUT_MS => Tick::CloseTimeout,
            Phase::Open if !self.ever_mapped && now_ms.saturating_sub(self.born_ms) >= MAP_TIMEOUT_MS => Tick::MapTimeout,
            _ => Tick::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn sock(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nc-test-{}-{tag}", std::process::id()));
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

    fn line(r: &mut BufReader<UnixStream>) -> Option<Value> {
        let mut l = String::new();
        match r.read_line(&mut l) {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(&l).ok(),
        }
    }

    fn sink(tx: mpsc::Sender<Event>) -> Sink {
        let tx = Mutex::new(tx);
        Arc::new(move |e| tx.lock().unwrap().send(e).is_ok())
    }

    fn recv(rx: &mpsc::Receiver<Event>) -> Event {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(e) = rx.try_recv() { return e; }
            assert!(std::time::Instant::now() < end, "timed out waiting for an event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const OPEN: &str = r#"{"op":"open","version":1,"output":"eDP-1"}"#;

    #[test]
    fn open_layout_mapped_hide_hidden_round_trip() {
        let p = sock("flow");
        let (tx, rx) = mpsc::channel();
        let bridge = start_at(&p, me(), sink(tx)).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let (mut s, mut r) = connect(&p);
        writeln!(s, "{OPEN}").unwrap();
        let Event::Open { conn, output } = recv(&rx) else { panic!("expected Open") };
        assert_eq!(output, "eDP-1");
        assert!(bridge.send(conn, serde_json::json!({"op": "layout", "serial": 1})));
        assert_eq!(line(&mut r).unwrap()["op"], "layout");
        writeln!(s, "{{\"op\":\"mapped\",\"session\":{conn},\"serial\":1}}").unwrap();
        writeln!(s, "{{\"op\":\"interaction\",\"session\":{conn},\"hovered\":true,\"busy\":false}}").unwrap();
        assert_eq!(recv(&rx), Event::Mapped { conn, serial: 1 });
        assert_eq!(recv(&rx), Event::Interaction { conn, hovered: true, busy: false });
        assert!(bridge.send(conn, serde_json::json!({"op": "hide", "session": conn, "reason": "x"})));
        assert_eq!(line(&mut r).unwrap()["op"], "hide");
        writeln!(s, "{{\"op\":\"hidden\",\"session\":{conn}}}").unwrap();
        writeln!(s, "{{\"op\":\"close\",\"session\":{conn},\"home\":true}}").unwrap();
        assert_eq!(recv(&rx), Event::Hidden { conn });
        assert_eq!(recv(&rx), Event::Close { conn, home: true });
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn disconnect_keeps_the_listener_and_ids_never_repeat() {
        let p = sock("again");
        let (tx, rx) = mpsc::channel();
        let bridge = start_at(&p, me(), sink(tx)).unwrap();
        let (mut s1, _r1) = connect(&p);
        writeln!(s1, "{OPEN}").unwrap();
        let Event::Open { conn: c1, .. } = recv(&rx) else { panic!() };
        bridge.disconnect(c1);
        assert_eq!(recv(&rx), Event::Gone { conn: c1 });
        assert!(!bridge.send(c1, serde_json::json!({"op": "hide"})), "a released connection takes no frames");
        let (mut s2, _r2) = connect(&p);
        writeln!(s2, "{OPEN}").unwrap();
        let Event::Open { conn: c2, .. } = recv(&rx) else { panic!() };
        assert!(c2 > c1);
        // the old stream's late frames cannot reach the new connection: they were bound to c1's session
        let _ = s1.write_all(format!("{{\"op\":\"hidden\",\"session\":{c1}}}\n").as_bytes());
        writeln!(s2, "{{\"op\":\"hidden\",\"session\":{c1}}}").unwrap();
        assert_eq!(recv(&rx), Event::Gone { conn: c2 }, "a frame for another session ends that connection");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wrong_peer_is_dropped() {
        let p = sock("peer");
        let (tx, rx) = mpsc::channel();
        start_at(&p, me() + 1, sink(tx)).unwrap();
        let (mut s, mut r) = connect(&p);
        let _ = writeln!(s, "{OPEN}");
        assert!(line(&mut r).is_none(), "no reply for a foreign peer");
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_open_frames_are_rejected_with_an_error() {
        let big = format!("{{\"op\":\"open\",\"version\":1,\"output\":\"{}\"}}", "x".repeat(MAX_FRAME));
        let cases = [
            ("ver", r#"{"op":"open","version":2,"output":"A"}"#.to_string()),
            ("noout", r#"{"op":"open","version":1}"#.to_string()),
            ("emptyout", r#"{"op":"open","version":1,"output":""}"#.to_string()),
            ("junk", "not json".to_string()),
            ("notopen", r#"{"op":"mapped","session":1,"serial":1}"#.to_string()),
            ("big", big),
        ];
        for (tag, frame) in cases {
            let p = sock(tag);
            let (tx, rx) = mpsc::channel();
            start_at(&p, me(), sink(tx)).unwrap();
            let (mut s, mut r) = connect(&p);
            let _ = writeln!(s, "{frame}");
            assert_eq!(line(&mut r).map(|f| f["op"].clone()), Some(Value::from("error")), "{tag}");
            // Gone follows the rejection; there is never an Open
            assert!(matches!(recv(&rx), Event::Gone { .. }), "{tag}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn bad_session_frames_and_eof() {
        let p = sock("late");
        let (tx, rx) = mpsc::channel();
        start_at(&p, me(), sink(tx)).unwrap();
        for (tag, frame) in [
            ("op", "{\"op\":\"nope\",\"session\":CONN}"),
            ("serial", "{\"op\":\"mapped\",\"session\":CONN}"),
            ("hover", "{\"op\":\"interaction\",\"session\":CONN,\"hovered\":1,\"busy\":false}"),
            ("home", "{\"op\":\"close\",\"session\":CONN}"),
            ("session", "{\"op\":\"hidden\",\"session\":999}"),
        ] {
            let (mut s, mut r) = connect(&p);
            writeln!(s, "{OPEN}").unwrap();
            let Event::Open { conn, .. } = recv(&rx) else { panic!() };
            writeln!(s, "{}", frame.replace("CONN", &conn.to_string())).unwrap();
            assert_eq!(line(&mut r).map(|f| f["op"].clone()), Some(Value::from("error")), "{tag}");
            assert_eq!(recv(&rx), Event::Gone { conn }, "{tag}");
        }
        // a frame split across two writes is reassembled; EOF mid-frame ends the connection
        let (mut s, _r) = connect(&p);
        writeln!(s, "{OPEN}").unwrap();
        let Event::Open { conn, .. } = recv(&rx) else { panic!() };
        s.write_all(format!("{{\"op\":\"hidden\",").as_bytes()).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        s.write_all(format!("\"session\":{conn}}}\n").as_bytes()).unwrap();
        assert_eq!(recv(&rx), Event::Hidden { conn });
        s.write_all(b"{\"op\":\"hid").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(recv(&rx), Event::Gone { conn });
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stale_serial_and_superseded_session_cannot_act() {
        let mut s = Session::new(7, 0);
        let first = s.layout_sent();
        s.suspended();
        let second = s.layout_sent();
        assert!(!s.on_mapped(first), "an acknowledgement for a superseded layout is stale");
        assert!(!s.mapped);
        assert!(s.on_mapped(second));
        assert!(s.mapped && s.ever_mapped);
        assert!(!s.on_mapped(0), "serial zero was never sent");
        // after a close begins nothing maps again, and interaction cannot hold the page open
        assert!(s.begin_close(100));
        assert!(!s.begin_close(150), "one bounded close at a time");
        let third = s.layout_sent();
        assert!(!s.on_mapped(third));
        s.on_interaction(true, true);
        assert!(!s.holds_open());
    }

    #[test]
    fn timeouts_use_injected_time() {
        let mut s = Session::new(1, 1000);
        assert_eq!(s.tick(1000 + MAP_TIMEOUT_MS - 1), Tick::Idle);
        assert_eq!(s.tick(1000 + MAP_TIMEOUT_MS), Tick::MapTimeout);
        let n = s.layout_sent();
        assert!(s.on_mapped(n));
        assert_eq!(s.tick(1_000_000), Tick::Idle, "a mapped session has no initial timeout");
        s.on_interaction(true, false);
        assert!(s.holds_open());
        s.on_interaction(false, true);
        assert!(s.holds_open(), "busy alone holds the page");
        s.on_interaction(false, false);
        assert!(!s.holds_open());
        assert!(s.begin_close(5000));
        assert_eq!(s.tick(5000 + CLOSE_TIMEOUT_MS - 1), Tick::Idle);
        assert_eq!(s.tick(5000 + CLOSE_TIMEOUT_MS), Tick::CloseTimeout);
    }
}
