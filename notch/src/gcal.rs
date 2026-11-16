//! Google Calendar (read-only) via OAuth2 loopback flow.
//! Setup: create an OAuth "Desktop app" client in Google Cloud Console, enable the Calendar API and save
//! `{"client_id": "...", "client_secret": "..."}` to ~/.config/waybar-notch/google.json. The first connect opens the browser;
//! the refresh token is stored in ~/.local/share/waybar-notch/google-token.json (mode 0600).
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crate::private_files;
use chrono::{DateTime, Duration as CDur, Local, NaiveDate, NaiveDateTime};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const EVENTS_URL: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

#[derive(Clone, Debug)]
pub struct Event {
    pub title: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub all_day: bool,
    pub link: Option<String>, // Google Meet / video-call URL
    pub location: String,
}

impl Event {
    /// Does this event touch local day `d`?
    pub fn on_day(&self, d: NaiveDate) -> bool {
        let last = if self.all_day { self.end.date() - CDur::days(1) } else { (self.end - CDur::seconds(1)).date().max(self.start.date()) };
        self.start.date() <= d && d <= last
    }
}

pub enum Update {
    Events(Vec<Event>),
    NoConfig,
    NeedAuth,
    Error(String),
}

fn home() -> String { std::env::var("HOME").unwrap_or_default() }
pub fn config_path() -> String { format!("{}/.config/waybar-notch/google.json", home()) }
fn token_path() -> String { format!("{}/.local/share/waybar-notch/google-token.json", home()) }

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into()
}

/// `Ok(None)` only when the config genuinely does not exist or has no client id; unsafe files are errors.
fn creds() -> Result<Option<(String, String)>, String> {
    let Some(v) = private_files::read_private_json(std::path::Path::new(&config_path()))? else { return Ok(None) };
    let Some(id) = v["client_id"].as_str().filter(|s| !s.is_empty()) else { return Ok(None) };
    let secret = v["client_secret"].as_str().unwrap_or("").to_string();
    Ok(Some((id.to_string(), secret)))
}

/// Only plain HTTPS links with a host, no userinfo and no control characters may become a Join target.
/// The provider is not restricted: Meet, Zoom, Teams and others all use HTTPS.
fn join_url(value: &str) -> Option<String> {
    if value.is_empty() || value.chars().any(|c| c.is_control()) { return None; }
    let uri = gtk4::glib::Uri::parse(value, gtk4::glib::UriFlags::NONE).ok()?;
    if uri.scheme() != "https" || uri.userinfo().is_some() { return None; }
    if uri.host().map_or(true, |h| h.is_empty()) { return None; }
    Some(value.to_string())
}

/// First valid HTTPS target among `hangoutLink` and the video entry points.
fn event_link(it: &Value) -> Option<String> {
    it["hangoutLink"].as_str().and_then(join_url).or_else(|| {
        it["conferenceData"]["entryPoints"].as_array()?.iter()
            .filter(|e| e["entryPointType"].as_str() == Some("video"))
            .find_map(|e| e["uri"].as_str().and_then(join_url))
    })
}

fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { o.push(b as char) } else { o.push_str(&format!("%{b:02X}")) }
    }
    o
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => { out.push(v); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            c => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn token_request(form: &[(&str, &str)]) -> Result<Value, String> {
    let mut resp = agent().post(TOKEN_URL).send_form(form.iter().copied()).map_err(|e| format!("token request failed: {e}"))?;
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically publishes `value` at `path`: the secret never exists in a file readable by others (the
/// temporary file is created 0600), and the old file survives any failure before the final rename.
fn write_token_at(path: &std::path::Path, value: &Value) -> Result<(), String> {
    private_files::write_private_file(path, value.to_string().as_bytes(), ".google-token.tmp", &TMP_SEQ)
}

fn save_token(refresh: &str, access: &str, expires_in: i64) -> Result<(), String> {
    let v = serde_json::json!({ "refresh_token": refresh, "access_token": access, "expires_at": Local::now().timestamp() + expires_in });
    write_token_at(std::path::Path::new(&token_path()), &v)
}

fn random_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("no secure randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

/// RFC 7636 S256 code challenge.
fn pkce_challenge(verifier: &str) -> Result<String, String> {
    use gtk4::glib::{Checksum, ChecksumType};
    let mut c = Checksum::new(ChecksumType::Sha256).ok_or("SHA-256 unavailable")?;
    c.update(verifier.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(c.digest()))
}

const MAX_REQUEST: usize = 4096;

fn io_timeout(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(5)).max(Duration::from_millis(1))
}

enum Callback { Code(String), Denied }

/// Reads one bounded request header; `None` for anything truncated, oversized or slow.
fn read_header(s: &mut std::net::TcpStream, deadline: Instant) -> Option<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        if Instant::now() >= deadline { return None; }
        s.set_read_timeout(Some(io_timeout(deadline))).ok()?;
        let n = s.read(&mut chunk).ok()?;
        if n == 0 { return None; }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        if buf.len() >= MAX_REQUEST { return None; }
    }
    if buf.len() > MAX_REQUEST { return None; }
    String::from_utf8(buf).ok()
}

/// Accepts only `GET /?state=<ours>&code=<c>` or `...&error=<e>`; everything else is not our redirect.
fn parse_callback(req: &str, state: &str) -> Option<Callback> {
    let mut parts = req.lines().next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || method != "GET" || !matches!(version, "HTTP/1.1" | "HTTP/1.0") { return None; }
    let query = target.strip_prefix("/?")?;
    let (mut states, mut codes, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "state" => states.push(dec(v)),
            "code" => codes.push(dec(v)),
            "error" => errors.push(dec(v)),
            _ => {}
        }
    }
    if states.len() != 1 || states[0] != state { return None; }
    match (codes.len(), errors.len()) {
        (1, 0) if !codes[0].is_empty() => Some(Callback::Code(codes.remove(0))),
        (0, 1) if !errors[0].is_empty() => Some(Callback::Denied),
        _ => None,
    }
}

fn respond(s: &mut std::net::TcpStream, deadline: Instant, ok: bool) {
    let (status, body) = if ok {
        ("200 OK", "Sign-in received. You can close this tab and return to the notch.")
    } else {
        ("400 Bad Request", "Not a valid sign-in response.")
    };
    let _ = s.set_write_timeout(Some(io_timeout(deadline)));
    let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
}

/// Waits for the OAuth redirect on `listener`; ignores everything that does not carry our `state`.
fn receive_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    loop {
        if Instant::now() >= deadline { return Err("sign-in timed out".into()); }
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let cb = read_header(&mut s, deadline).and_then(|r| parse_callback(&r, state));
                respond(&mut s, deadline, cb.is_some());
                match cb {
                    Some(Callback::Code(c)) => return Ok(c),
                    Some(Callback::Denied) => return Err("access denied".into()),
                    None => {}
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
}

pub const ERR_NO_CONFIG: &str = "missing ~/.config/waybar-notch/google.json";

/// Browser sign-in: blocks until the redirect arrives (up to 3 minutes). Run off the UI thread.
/// `open` receives the consent URL (the app launches a browser, the CLI can print it instead).
pub fn authorize(open: &dyn Fn(&str)) -> Result<(), String> {
    let (id, secret) = creds()?.ok_or(ERR_NO_CONFIG)?;
    let state = random_token()?;
    let verifier = random_token()?;
    let challenge = pkce_challenge(&verifier)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        enc(&id), enc(&redirect), enc(SCOPE));
    open(&url);

    let code = receive_code(&listener, &state, Duration::from_secs(180))?;
    let v = token_request(&[
        ("code", &code), ("client_id", &id), ("client_secret", &secret),
        ("redirect_uri", &redirect), ("grant_type", "authorization_code"), ("code_verifier", &verifier),
    ])?;
    let refresh = v["refresh_token"].as_str().ok_or_else(|| format!("no refresh token: {}", v["error_description"].as_str().unwrap_or("?")))?;
    save_token(refresh, v["access_token"].as_str().unwrap_or(""), v["expires_in"].as_i64().unwrap_or(3000))?;
    Ok(())
}

fn access_token() -> Result<String, Update> {
    let (id, secret) = creds().map_err(Update::Error)?.ok_or(Update::NoConfig)?;
    let t = private_files::read_private_json(std::path::Path::new(&token_path())).map_err(Update::Error)?.ok_or(Update::NeedAuth)?;
    let refresh = t["refresh_token"].as_str().ok_or(Update::NeedAuth)?.to_string();
    if let (Some(a), Some(exp)) = (t["access_token"].as_str(), t["expires_at"].as_i64()) {
        if !a.is_empty() && exp > Local::now().timestamp() + 60 { return Ok(a.to_string()); }
    }
    let v = token_request(&[
        ("client_id", &id), ("client_secret", &secret), ("refresh_token", &refresh), ("grant_type", "refresh_token"),
    ]).map_err(Update::Error)?;
    match v["access_token"].as_str() {
        Some(a) => {
            save_token(&refresh, a, v["expires_in"].as_i64().unwrap_or(3000)).map_err(Update::Error)?;
            Ok(a.to_string())
        }
        // revoked or expired grant: sign in again
        None if v["error"].as_str() == Some("invalid_grant") => Err(Update::NeedAuth),
        None => Err(Update::Error(v["error_description"].as_str().unwrap_or("token refresh failed").to_string())),
    }
}

fn parse_time(v: &Value) -> Option<(NaiveDateTime, bool)> {
    if let Some(s) = v["dateTime"].as_str() {
        let dt = DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Local);
        return Some((dt.naive_local(), false));
    }
    let d = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((d.and_hms_opt(0, 0, 0)?, true))
}

/// Events from the start of last month to three months ahead.
pub fn fetch() -> Update {
    let token = match access_token() { Ok(t) => t, Err(u) => return u };
    let now = Local::now();
    let from = (now - CDur::days(35)).to_rfc3339();
    let to = (now + CDur::days(100)).to_rfc3339();
    let mut resp = match agent().get(EVENTS_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .query("timeMin", &from).query("timeMax", &to)
        .query("singleEvents", "true").query("orderBy", "startTime").query("maxResults", "250")
        .call() {
        Ok(r) => r,
        Err(e) => return Update::Error(format!("calendar request failed: {e}")),
    };
    let text = match resp.body_mut().read_to_string() { Ok(t) => t, Err(e) => return Update::Error(e.to_string()) };
    let v: Value = match serde_json::from_str(&text) { Ok(v) => v, Err(e) => return Update::Error(e.to_string()) };
    let mut out = Vec::new();
    for it in v["items"].as_array().into_iter().flatten() {
        if it["status"].as_str() == Some("cancelled") { continue; }
        let (Some((start, all_day)), Some((end, _))) = (parse_time(&it["start"]), parse_time(&it["end"])) else { continue };
        let link = event_link(it);
        out.push(Event {
            title: it["summary"].as_str().unwrap_or("(no title)").to_string(),
            start, end, all_day, link,
            location: it["location"].as_str().unwrap_or("").to_string(),
        });
    }
    Update::Events(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: &str, end: &str, all_day: bool) -> Event {
        let p = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Event { title: "t".into(), start: p(start), end: p(end), all_day, link: None, location: String::new() }
    }

    #[test]
    fn all_day_end_is_exclusive() {
        let e = ev("2026-11-09 00:00", "2026-11-10 00:00", true);
        assert!(e.on_day(NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()));
        assert!(!e.on_day(NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()));
    }

    #[test]
    fn timed_event_ending_at_midnight_stays_on_start_day() {
        let e = ev("2026-11-09 22:00", "2026-11-10 00:00", false);
        assert!(e.on_day(NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()));
        assert!(!e.on_day(NaiveDate::from_ymd_opt(2026, 11, 10).unwrap()));
    }

    #[test]
    fn percent_decoding_roundtrip() {
        assert_eq!(dec(&enc("4/0Ab_c d")), "4/0Ab_c d");
        assert_eq!(dec("a%2Fb%"), "a/b%");
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn random_tokens_are_distinct_urlsafe() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
    }

    fn send(port: u16, parts: &[&[u8]]) {
        use std::net::TcpStream;
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) else { return };
        for p in parts {
            let _ = s.write_all(p);
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(30));
        }
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    }

    fn listener() -> (TcpListener, u16) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    const ST: &str = "good-state";

    #[test]
    fn wrong_state_is_ignored_then_fragmented_code_wins() {
        let (l, port) = listener();
        std::thread::scope(|sc| {
            let h = sc.spawn(|| receive_code(&l, ST, Duration::from_secs(20)));
            send(port, &[b"GET /?error=access_denied&state=other HTTP/1.1\r\n\r\n"]);
            send(port, &[b"GET /?error=access_denied HTTP/1.1\r\n\r\n"]);
            send(port, &[b"GET /?code=4%2F0", b"Ab&sta", b"te=good-state HTTP/1.1\r\nHost: x\r", b"\n\r\n"]);
            assert_eq!(h.join().unwrap(), Ok("4/0Ab".to_string()));
        });
    }

    #[test]
    fn matching_state_denial_is_access_denied() {
        let (l, port) = listener();
        std::thread::scope(|sc| {
            let h = sc.spawn(|| receive_code(&l, ST, Duration::from_secs(20)));
            send(port, &[b"GET /?state=wrong&error=access_denied HTTP/1.1\r\n\r\n"]);
            send(port, &[b"GET /?state=good-state&error=access_denied HTTP/1.1\r\n\r\n"]);
            assert_eq!(h.join().unwrap(), Err("access denied".to_string()));
        });
    }

    #[test]
    fn malformed_callbacks_never_authorize() {
        let (l, port) = listener();
        let big = format!("GET /?state=good-state&code=x&pad={} HTTP/1.1\r\n\r\n", "a".repeat(5000));
        std::thread::scope(|sc| {
            let h = sc.spawn(|| receive_code(&l, ST, Duration::from_millis(1500)));
            for req in [
                "GET /?state=good-state&state=good-state&code=c HTTP/1.1\r\n\r\n",
                "GET /?state=good-state&code=a&code=b HTTP/1.1\r\n\r\n",
                "GET /?state=good-state&code=a&error=e HTTP/1.1\r\n\r\n",
                "GET /?state=good-state&code= HTTP/1.1\r\n\r\n",
                "GET /other?state=good-state&code=c HTTP/1.1\r\n\r\n",
                "POST /?state=good-state&code=c HTTP/1.1\r\n\r\n",
                "GET /?code=c HTTP/1.1\r\n\r\n",
                "GET /?state=good-state&code=c HTTP/2\r\n\r\n",
                big.as_str(),
                "GET /?state=good-state&code=c HTTP/1.1\r\n",
            ] {
                send(port, &[req.as_bytes()]);
            }
            assert_eq!(h.join().unwrap(), Err("sign-in timed out".to_string()));
        });
    }

    #[test]
    fn invalid_callback_flood_cannot_extend_deadline() {
        let (l, port) = listener();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|sc| {
            let flood = sc.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    send(port, &[b"GET /?state=nope&code=c HTTP/1.1\r\n\r\n"]);
                }
            });
            let start = Instant::now();
            let r = receive_code(&l, ST, Duration::from_millis(800));
            let took = start.elapsed();
            stop.store(true, Ordering::Relaxed);
            flood.join().unwrap();
            assert_eq!(r, Err("sign-in timed out".to_string()));
            assert!(took < Duration::from_secs(5), "took {took:?}");
        });
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("gcal-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Child half of `token_file_is_private_under_umask_022`: runs in a subprocess so umask stays local.
    #[test]
    fn token_umask_child() {
        let Ok(dir) = std::env::var("GCAL_TOKEN_CHILD_DIR") else { return };
        write_token_at(&std::path::Path::new(&dir).join("data/google-token.json"), &serde_json::json!({"refresh_token": "synthetic"})).unwrap();
    }

    #[test]
    fn token_file_is_private_under_umask_022() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        use std::os::unix::process::CommandExt;
        let dir = tmpdir("umask");
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "gcal::tests::token_umask_child", "--nocapture"]).env("GCAL_TOKEN_CHILD_DIR", &dir);
        unsafe { cmd.pre_exec(|| { libc::umask(0o022); Ok(()) }); }
        assert!(cmd.output().unwrap().status.success());
        let f = dir.join("data/google-token.json");
        assert_eq!(std::fs::metadata(&f).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.join("data")).unwrap().permissions().mode() & 0o777, 0o700);
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("data")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_write_refuses_symlink_and_leaves_referent() {
        let dir = tmpdir("symlink");
        let target = dir.join("victim");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.join("google-token.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_token_at(&link, &serde_json::json!({"refresh_token": "r"})).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_write_refuses_symlinked_directory() {
        let dir = tmpdir("symdir");
        let real = dir.join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = dir.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(write_token_at(&linked.join("google-token.json"), &serde_json::json!({"refresh_token": "r"})).is_err());
        assert!(std::fs::read_dir(&real).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_nonregular_destination_is_refused_and_leaves_no_secret() {
        let dir = tmpdir("rename");
        let dest = dir.join("google-token.json");
        std::fs::create_dir(&dest).unwrap();
        let err = write_token_at(&dest, &serde_json::json!({"refresh_token": "SECRETVALUE"})).unwrap_err();
        assert!(err.contains("cannot write") && !err.contains("SECRETVALUE"), "{err}");
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["google-token.json".to_string()], "temporary file left behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_write_failure_keeps_old_token() {
        let dir = tmpdir("keep");
        let dest = dir.join("google-token.json");
        write_token_at(&dest, &serde_json::json!({"refresh_token": "old"})).unwrap();
        let old = std::fs::read(&dest).unwrap();
        // every candidate temporary name is taken: the write fails before touching the old token
        let start = TMP_SEQ.load(Ordering::Relaxed);
        let collisions: Vec<_> = (0..16).map(|i| dir.join(format!(".google-token.tmp.{}.{}", std::process::id(), start + i))).collect();
        for c in &collisions { std::fs::write(c, b"sentinel").unwrap(); }
        let res = write_token_at(&dest, &serde_json::json!({"refresh_token": "new"}));
        assert!(res.is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), old);
        for c in &collisions { assert_eq!(std::fs::read(c).unwrap(), b"sentinel"); }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn join_url_accepts_https_providers() {
        for u in [
            "https://meet.google.com/abc-defg-hij",
            "https://zoom.us/j/123456789?pwd=abc",
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting",
            "https://example.org:8443/room",
        ] {
            assert_eq!(join_url(u).as_deref(), Some(u), "{u}");
        }
    }

    #[test]
    fn join_url_rejects_unsafe_targets() {
        for u in [
            "", "file:///etc/passwd", "javascript:alert(1)", "http://meet.google.com/x", "zoommtg://zoom.us/join?confno=1",
            "steam://run/1", "https://", "https:///path", "https://user:pw@meet.google.com/x", "https://user@meet.google.com/x",
            "not a url", "meet.google.com/abc", "https://meet.google.com/a\nb", "https://meet.google.com/a\u{7}b",
            "https://meet.google.com/\u{0}", "//meet.google.com/x", "/relative",
        ] {
            assert_eq!(join_url(u), None, "{u:?}");
        }
    }

    #[test]
    fn event_link_falls_through_invalid_candidates() {
        let it = serde_json::json!({
            "hangoutLink": "javascript:alert(1)",
            "conferenceData": { "entryPoints": [
                { "entryPointType": "phone", "uri": "https://example.org/phone" },
                { "entryPointType": "video", "uri": "file:///etc/passwd" },
                { "entryPointType": "video", "uri": "https://zoom.us/j/1" },
            ] }
        });
        assert_eq!(event_link(&it).as_deref(), Some("https://zoom.us/j/1"));
        let none = serde_json::json!({ "hangoutLink": "http://x.example/", "conferenceData": { "entryPoints": [{ "entryPointType": "video", "uri": "tel:1" }] } });
        assert_eq!(event_link(&none), None);
        let hang = serde_json::json!({ "hangoutLink": "https://meet.google.com/a-b-c" });
        assert_eq!(event_link(&hang).as_deref(), Some("https://meet.google.com/a-b-c"));
    }
}
