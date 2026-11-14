//! Google Calendar (read-only) via OAuth2 loopback flow.
//! Setup: create an OAuth "Desktop app" client in Google Cloud Console, enable the Calendar API and save
//! `{"client_id": "...", "client_secret": "..."}` to ~/.config/waybar-notch/google.json. The first connect opens the browser;
//! the refresh token is stored in ~/.local/share/waybar-notch/google-token.json (mode 0600).
use chrono::{DateTime, Duration as CDur, Local, NaiveDate, NaiveDateTime};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpListener;
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

fn read_json(path: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn creds() -> Option<(String, String)> {
    let v = read_json(&config_path())?;
    let id = v["client_id"].as_str().filter(|s| !s.is_empty())?.to_string();
    let secret = v["client_secret"].as_str().unwrap_or("").to_string();
    Some((id, secret))
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

fn save_token(refresh: &str, access: &str, expires_in: i64) {
    let p = token_path();
    if let Some(dir) = std::path::Path::new(&p).parent() { let _ = std::fs::create_dir_all(dir); }
    let v = serde_json::json!({ "refresh_token": refresh, "access_token": access, "expires_at": Local::now().timestamp() + expires_in });
    if std::fs::write(&p, v.to_string()).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
}

pub const ERR_NO_CONFIG: &str = "missing ~/.config/waybar-notch/google.json";

/// Browser sign-in: blocks until the redirect arrives (up to 3 minutes). Run off the UI thread.
/// `open` receives the consent URL (the app launches a browser, the CLI can print it instead).
pub fn authorize(open: &dyn Fn(&str)) -> Result<(), String> {
    let (id, secret) = creds().ok_or(ERR_NO_CONFIG)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let state = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={state}",
        enc(&id), enc(&redirect), enc(SCOPE));
    open(&url);

    let deadline = Instant::now() + Duration::from_secs(180);
    let code = loop {
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let query = req.lines().next().and_then(|l| l.split(' ').nth(1)).and_then(|p| p.split_once('?')).map(|x| x.1.to_string()).unwrap_or_default();
                let get = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}="))).map(dec);
                let ok = get("code").is_some() && get("state").as_deref() == Some(state.as_str());
                let body = if ok { "Connected. You can close this tab and return to the notch." } else { "Sign-in failed. Close this tab and try again." };
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                if ok { break get("code").unwrap(); }
                if get("error").is_some() { return Err("access denied".into()); }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline { return Err("sign-in timed out".into()); }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let v = token_request(&[
        ("code", &code), ("client_id", &id), ("client_secret", &secret),
        ("redirect_uri", &redirect), ("grant_type", "authorization_code"),
    ])?;
    let refresh = v["refresh_token"].as_str().ok_or_else(|| format!("no refresh token: {}", v["error_description"].as_str().unwrap_or("?")))?;
    save_token(refresh, v["access_token"].as_str().unwrap_or(""), v["expires_in"].as_i64().unwrap_or(3000));
    Ok(())
}

fn access_token() -> Result<String, Update> {
    let (id, secret) = creds().ok_or(Update::NoConfig)?;
    let t = read_json(&token_path()).ok_or(Update::NeedAuth)?;
    let refresh = t["refresh_token"].as_str().ok_or(Update::NeedAuth)?.to_string();
    if let (Some(a), Some(exp)) = (t["access_token"].as_str(), t["expires_at"].as_i64()) {
        if !a.is_empty() && exp > Local::now().timestamp() + 60 { return Ok(a.to_string()); }
    }
    let v = token_request(&[
        ("client_id", &id), ("client_secret", &secret), ("refresh_token", &refresh), ("grant_type", "refresh_token"),
    ]).map_err(Update::Error)?;
    match v["access_token"].as_str() {
        Some(a) => {
            save_token(&refresh, a, v["expires_in"].as_i64().unwrap_or(3000));
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
        let link = it["hangoutLink"].as_str().map(str::to_string).or_else(|| {
            it["conferenceData"]["entryPoints"].as_array()?.iter()
                .find(|e| e["entryPointType"].as_str() == Some("video"))?["uri"].as_str().map(str::to_string)
        });
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
}
