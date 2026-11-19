//! Google Calendar selected-calendar reads and explicitly authorized primary-calendar creation.
//! Setup: create an OAuth "Desktop app" client in Google Cloud Console, enable the Calendar API and save
//! `{"client_id": "...", "client_secret": "..."}` to ~/.config/waybar-notch/google.json. The first connect opens the browser;
//! the refresh token is stored in ~/.local/share/waybar-notch/google-token.json (mode 0600).
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crate::private_files;
use chrono::{DateTime, Datelike, Duration as CDur, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const EVENTS_URL: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";
const WRITE_SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.owned";
const CALENDARS_URL: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";
const MAX_PAGE: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalendarError {
    NeedAuth,
    NeedWriteAccess,
    UnknownSave,
    Message(String),
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedAuth => f.write_str("Google Calendar needs sign-in"),
            Self::NeedWriteAccess => f.write_str("Enable Google event creation to save"),
            Self::UnknownSave => f.write_str("Save outcome unknown; refresh before trying again"),
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl From<String> for CalendarError {
    fn from(message: String) -> Self { Self::Message(message) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessMode { ReadOnly, CreateOwnedEvents }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CalendarRange {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl CalendarRange {
    /// Sunday before/on the first through Saturday after/on the last, with an exclusive end.
    pub fn for_month(date: NaiveDate) -> Result<Self, CalendarError> {
        let first = date.with_day(1).ok_or_else(|| message("Invalid calendar month"))?;
        let next = first.checked_add_months(chrono::Months::new(1)).ok_or_else(|| message("Calendar month out of range"))?;
        let start = first.checked_sub_signed(CDur::days(first.weekday().num_days_from_sunday() as i64))
            .ok_or_else(|| message("Calendar month out of range"))?;
        let padding = (7 - next.weekday().num_days_from_sunday()) % 7;
        let end = next.checked_add_signed(CDur::days(padding as i64)).ok_or_else(|| message("Calendar month out of range"))?;
        Ok(Self { start, end })
    }

    fn bounds(self) -> Result<(String, String), CalendarError> {
        if self.end <= self.start { return Err(message("Invalid calendar range")); }
        Ok((local_midnight(self.start)?.to_rfc3339(), local_midnight(self.end)?.to_rfc3339()))
    }
}

fn message(text: &str) -> CalendarError { CalendarError::Message(text.into()) }

fn local_midnight(date: NaiveDate) -> Result<DateTime<Local>, CalendarError> {
    let time = date.and_hms_opt(0, 0, 0).ok_or_else(|| message("Invalid date"))?;
    Local.from_local_datetime(&time).single().ok_or_else(|| message("Local time is ambiguous or does not exist"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub calendar_id: String,
    pub calendar_name: String,
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
        let Some(start) = d.and_hms_opt(0, 0, 0) else { return false };
        let Some(end) = d.succ_opt().and_then(|v| v.and_hms_opt(0, 0, 0)) else { return false };
        self.start < end && (self.end > start || (self.start == self.end && self.start >= start))
    }
}

#[derive(Clone, Debug)]
pub struct EventDraft {
    pub id: String,
    pub title: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub all_day: bool,
    pub alert_minutes: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub events: Vec<Event>,
    pub primary_selected: bool,
    pub write_access: bool,
}

/// Requests for the worker started by `start_worker`. `generation` is chosen by the UI and echoed in the
/// matching `Update`, so replies for superseded requests can be dropped. A newer `Refresh` replaces queued ones.
#[derive(Clone, Debug)]
pub enum Command {
    Refresh { range: CalendarRange, generation: u64, visible: bool },
    Authorize { write: bool },
    Create { draft: EventDraft, generation: u64 },
}

/// Worker replies, delivered through the `publish` callback of `start_worker` (UI code forwards them with
/// `Poster::post`). `Reconciled` follows a `Create` whose outcome was uncertain (network error after sending).
#[derive(Debug)]
pub enum Update {
    Loading { range: CalendarRange, generation: u64 },
    Refreshed { range: CalendarRange, generation: u64, result: Result<Snapshot, CalendarError> },
    Authorized { write: bool, result: Result<(), CalendarError> },
    Created { generation: u64, result: Result<Event, CalendarError> },
    Reconciled { id: String, generation: u64, result: Result<Option<Event>, CalendarError> },
}

fn home() -> String { std::env::var("HOME").unwrap_or_default() }
pub fn config_path() -> String { format!("{}/.config/waybar-notch/google.json", home()) }
fn token_path() -> String { format!("{}/.local/share/waybar-notch/google-token.json", home()) }

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(0).http_status_as_error(false).build().into()
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
    if !crate::art_http::explicit_authority(value) { return None; }
    let uri = url::Url::parse(value).ok()?;
    if uri.scheme() != "https" || !uri.username().is_empty() || uri.password().is_some() { return None; }
    if uri.host_str().map_or(true, |h| h.is_empty()) { return None; }
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

fn read_json(response: &mut ureq::http::Response<ureq::Body>, limit: u64) -> Result<Value, CalendarError> {
    let text = response.body_mut().with_config().limit(limit).read_to_string()
        .map_err(|_| message("Google response is unreadable or too large"))?;
    serde_json::from_str(&text).map_err(|_| message("Google returned invalid JSON"))
}

fn oauth_result(status: u16, value: Value) -> Result<Value, CalendarError> {
    if value["error"].as_str() == Some("invalid_grant") { return Err(CalendarError::NeedAuth); }
    if !(200..300).contains(&status) || value.get("error").is_some() {
        return Err(message("Google authorization failed; previous authorization was preserved"));
    }
    Ok(value)
}

fn token_request(form: &[(&str, &str)]) -> Result<Value, CalendarError> {
    let mut response = agent().post(TOKEN_URL).send_form(form.iter().copied())
        .map_err(|_| message("Google token request failed"))?;
    let status = response.status().as_u16();
    oauth_result(status, read_json(&mut response, 64 * 1024)?)
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically publishes `value` at `path`: the secret never exists in a file readable by others (the
/// temporary file is created 0600), and the old file survives any failure before the final rename.
fn write_token_at(path: &std::path::Path, value: &Value) -> Result<(), String> {
    private_files::write_private_file(path, value.to_string().as_bytes(), ".google-token.tmp", &TMP_SEQ)
}

fn has_scope(token: &Value, scope: &str) -> bool {
    token["scope"].as_str().is_some_and(|s| s.split_whitespace().any(|part| part == scope))
}

fn token_value(response: &Value, previous: Option<&Value>, mode: Option<AccessMode>) -> Result<Value, CalendarError> {
    let access = response["access_token"].as_str().filter(|s| !s.is_empty()).ok_or_else(|| message("Google did not return an access token"))?;
    let refresh = response["refresh_token"].as_str().filter(|s| !s.is_empty())
        .or_else(|| previous.and_then(|p| p["refresh_token"].as_str()).filter(|s| !s.is_empty()))
        .ok_or_else(|| message("Google did not return a refresh token; previous authorization was preserved"))?;
    let scope = response.get("scope").or_else(|| previous.and_then(|p| p.get("scope")));
    let mut token = serde_json::json!({
        "refresh_token": refresh, "access_token": access,
        "expires_at": Local::now().timestamp().saturating_add(response["expires_in"].as_i64().unwrap_or(3000).max(0)),
    });
    if let Some(scope) = scope { token["scope"] = scope.clone(); }
    if let Some(mode) = mode {
        if !has_scope(&token, SCOPE) || (mode == AccessMode::CreateOwnedEvents && !has_scope(&token, WRITE_SCOPE)) {
            return Err(CalendarError::NeedWriteAccess);
        }
    }
    Ok(token)
}

fn save_token(value: &Value) -> Result<(), CalendarError> {
    write_token_at(std::path::Path::new(&token_path()), value).map_err(CalendarError::Message)
}

fn random_token() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("no secure randomness: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(b))
}

/// RFC 7636 S256 code challenge.
fn pkce_challenge(verifier: &str) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
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
pub fn authorize(mode: AccessMode, open: &dyn Fn(&str)) -> Result<(), CalendarError> {
    let (id, secret) = creds()?.ok_or_else(|| message(ERR_NO_CONFIG))?;
    let state = random_token()?;
    let verifier = random_token()?;
    let challenge = pkce_challenge(&verifier)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|_| message("Cannot bind Google sign-in callback"))?;
    let port = listener.local_addr().map_err(|_| message("Cannot read Google sign-in callback address"))?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let scope = match mode {
        AccessMode::ReadOnly => SCOPE.to_string(),
        AccessMode::CreateOwnedEvents => format!("{SCOPE} {WRITE_SCOPE}"),
    };
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        enc(&id), enc(&redirect), enc(&scope));
    open(&url);
    let code = receive_code(&listener, &state, Duration::from_secs(180))?;
    let response = token_request(&[
        ("code", &code), ("client_id", &id), ("client_secret", &secret),
        ("redirect_uri", &redirect), ("grant_type", "authorization_code"), ("code_verifier", &verifier),
    ])?;
    // Consent must return its own refresh token and full scopes before replacing the old grant.
    save_token(&token_value(&response, None, Some(mode))?)
}

fn access_token(write: bool) -> Result<(String, bool), CalendarError> {
    let (id, secret) = creds()?.ok_or_else(|| message(ERR_NO_CONFIG))?;
    let token = private_files::read_private_json(std::path::Path::new(&token_path()))?.ok_or(CalendarError::NeedAuth)?;
    if write && !has_scope(&token, WRITE_SCOPE) { return Err(CalendarError::NeedWriteAccess); }
    let refresh = token["refresh_token"].as_str().filter(|s| !s.is_empty()).ok_or(CalendarError::NeedAuth)?;
    if let (Some(access), Some(expires)) = (token["access_token"].as_str(), token["expires_at"].as_i64()) {
        if !access.is_empty() && expires > Local::now().timestamp() + 60 {
            return Ok((access.into(), has_scope(&token, WRITE_SCOPE)));
        }
    }
    let response = token_request(&[
        ("client_id", &id), ("client_secret", &secret), ("refresh_token", refresh), ("grant_type", "refresh_token"),
    ])?;
    let refreshed = token_value(&response, Some(&token), None)?;
    save_token(&refreshed)?;
    let writable = has_scope(&refreshed, WRITE_SCOPE);
    if write && !writable { return Err(CalendarError::NeedWriteAccess); }
    Ok((refreshed["access_token"].as_str().unwrap_or_default().into(), writable))
}

fn parse_time(v: &Value) -> Option<(NaiveDateTime, bool)> {
    if let Some(s) = v["dateTime"].as_str() {
        let dt = DateTime::parse_from_rfc3339(s).ok()?.with_timezone(&Local);
        return Some((dt.naive_local(), false));
    }
    let d = NaiveDate::parse_from_str(v["date"].as_str()?, "%Y-%m-%d").ok()?;
    Some((d.and_hms_opt(0, 0, 0)?, true))
}

/// Listing and insertion share event parsing and safe Join-link policy.
pub fn parse_event(value: &Value, calendar_id: &str, calendar_name: &str) -> Option<Event> {
    if value["status"].as_str() == Some("cancelled") { return None; }
    let id = value["id"].as_str().filter(|s| !s.is_empty())?;
    let (start, all_day) = parse_time(&value["start"])?;
    let (end, end_all_day) = parse_time(&value["end"])?;
    if end < start || all_day != end_all_day || (all_day && end == start) { return None; }
    Some(Event {
        id: id.into(), calendar_id: calendar_id.into(), calendar_name: calendar_name.into(),
        title: value["summary"].as_str().unwrap_or("(no title)").into(),
        start, end, all_day, link: event_link(value),
        location: value["location"].as_str().unwrap_or_default().into(),
    })
}

#[derive(Debug)]
struct Calendar { id: String, name: String, primary: bool }

fn selected_calendar(value: &Value) -> Option<Calendar> {
    if value["selected"].as_bool() != Some(true)
        || value["hidden"].as_bool() == Some(true) || value["deleted"].as_bool() == Some(true)
        || !matches!(value["accessRole"].as_str(), Some("reader" | "writerWithoutPrivateAccess" | "writer" | "owner")) {
        return None;
    }
    Some(Calendar {
        id: value["id"].as_str().filter(|s| !s.is_empty())?.into(),
        name: value["summaryOverride"].as_str().filter(|s| !s.trim().is_empty())
            .or_else(|| value["summary"].as_str().filter(|s| !s.trim().is_empty())).unwrap_or("Calendar").into(),
        primary: value["primary"].as_bool() == Some(true),
    })
}

fn api_status(status: u16) -> Result<(), CalendarError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(CalendarError::NeedAuth),
        403 => Err(message("Google Calendar denied access; check permissions")),
        _ => Err(message(&format!("Google Calendar request failed (HTTP {status})"))),
    }
}

fn get_json(http: &ureq::Agent, token: &str, url: &str) -> Result<Option<Value>, CalendarError> {
    let mut response = http.get(url).header("Authorization", &format!("Bearer {token}")).call()
        .map_err(|_| message("Google Calendar network request failed"))?;
    let status = response.status().as_u16();
    if status == 404 { return Ok(None); }
    api_status(status)?;
    read_json(&mut response, MAX_PAGE).map(Some)
}

/// Pagination completes before any caller receives the new snapshot.
fn pages<F, V>(base: &str, get: &mut F, mut visit: V) -> Result<(), CalendarError>
where F: FnMut(&str) -> Result<Value, CalendarError>, V: FnMut(&Value) -> Result<(), CalendarError> {
    let mut next = String::new();
    let mut seen = HashSet::new();
    loop {
        let url = if next.is_empty() { base.to_string() } else {
            format!("{base}{}pageToken={}", if base.contains('?') { '&' } else { '?' }, enc(&next))
        };
        let page = get(&url)?;
        if !page.is_object() { return Err(message("Invalid Google Calendar page")); }
        if let Some(items) = page.get("items") {
            for item in items.as_array().ok_or_else(|| message("Invalid Google Calendar items"))? { visit(item)?; }
        }
        match page.get("nextPageToken") {
            None | Some(Value::Null) => return Ok(()),
            Some(Value::String(token)) if token.is_empty() => return Ok(()),
            Some(Value::String(token)) if seen.insert(token.clone()) => next.clone_from(token),
            _ => return Err(message("Invalid or repeated Google Calendar page token")),
        }
    }
}

fn fetch_with<F>(range: CalendarRange, write_access: bool, mut get: F) -> Result<Snapshot, CalendarError>
where F: FnMut(&str) -> Result<Value, CalendarError> {
    let (from, to) = range.bounds()?;
    let mut calendars = Vec::new();
    let mut calendar_ids = HashSet::new();
    pages(&format!("{CALENDARS_URL}?maxResults=250"), &mut get, |value| {
        if let Some(calendar) = selected_calendar(value) {
            if calendar_ids.insert(calendar.id.clone()) { calendars.push(calendar); }
        }
        Ok(())
    })?;
    let primary_selected = calendars.iter().any(|calendar| calendar.primary);
    let mut events = HashMap::new();
    for calendar in calendars {
        // This stable alias also identifies primary events returned by insertion.
        let identity = if calendar.primary { "primary" } else { &calendar.id };
        let url = format!(
            "https://www.googleapis.com/calendar/v3/calendars/{}/events?timeMin={}&timeMax={}&singleEvents=true&orderBy=startTime&maxResults=250",
            enc(&calendar.id), enc(&from), enc(&to));
        pages(&url, &mut get, |value| {
            let Some(id) = value["id"].as_str() else { return Ok(()) };
            let key = (identity.to_string(), id.to_string());
            if value["status"].as_str() == Some("cancelled") {
                events.remove(&key);
            } else if let Some(event) = parse_event(value, identity, &calendar.name) {
                events.insert(key, event);
            }
            Ok(())
        })?;
    }
    let mut events: Vec<_> = events.into_values().collect();
    events.sort_by(|a, b| b.all_day.cmp(&a.all_day).then(a.start.cmp(&b.start))
        .then(a.title.cmp(&b.title)).then(a.calendar_id.cmp(&b.calendar_id)).then(a.id.cmp(&b.id)));
    Ok(Snapshot { events, primary_selected, write_access })
}

pub fn fetch(range: CalendarRange) -> Result<Snapshot, CalendarError> {
    let (token, writable) = access_token(false)?;
    let http = agent();
    fetch_with(range, writable, |url| get_json(&http, &token, url)?.ok_or_else(|| message("Selected Google calendar was not found")))
}

pub fn new_event_id() -> Result<String, CalendarError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| message("Secure randomness unavailable"))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in bytes { id.push(HEX[(byte >> 4) as usize] as char); id.push(HEX[(byte & 15) as usize] as char); }
    Ok(id)
}

fn draft_body(draft: &EventDraft) -> Result<Value, CalendarError> {
    if draft.title.trim().is_empty() || draft.title.chars().count() > 200 { return Err(message("Title must contain 1–200 characters")); }
    if draft.id.len() != 32 || !draft.id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(message("Invalid client event ID"));
    }
    if draft.end <= draft.start { return Err(message("Event duration must be positive")); }
    for time in [draft.start, draft.end] {
        if Local.from_local_datetime(&time.naive_local()).single() != Some(time) {
            return Err(message("Local time is ambiguous or does not exist"));
        }
    }
    let (start, end) = if draft.all_day {
        let next = draft.start.date_naive().succ_opt().ok_or_else(|| message("Invalid all-day date"))?;
        if draft.start != local_midnight(draft.start.date_naive())? || draft.end != local_midnight(next)? {
            return Err(message("All-day events must span one local date"));
        }
        (serde_json::json!({"date": draft.start.format("%Y-%m-%d").to_string()}),
         serde_json::json!({"date": next.to_string()}))
    } else {
        (serde_json::json!({"dateTime": draft.start.to_rfc3339()}), serde_json::json!({"dateTime": draft.end.to_rfc3339()}))
    };
    let overrides = match draft.alert_minutes {
        Some(minutes) if [0, 5, 15, 30, 60].contains(&minutes) => vec![serde_json::json!({"method": "popup", "minutes": minutes})],
        Some(_) => return Err(message("Invalid alert choice")),
        None => Vec::new(),
    };
    Ok(serde_json::json!({
        "id": draft.id, "summary": draft.title, "start": start, "end": end,
        "reminders": {"useDefault": false, "overrides": overrides},
    }))
}

fn matching_event(value: &Value, draft: &EventDraft) -> Result<Event, CalendarError> {
    let event = parse_event(value, "primary", "Primary").ok_or_else(|| message("Saved Google event is invalid"))?;
    let reminders_match = value["reminders"]["useDefault"].as_bool() == Some(false)
        && match draft.alert_minutes {
            Some(minutes) => value["reminders"]["overrides"].as_array().is_some_and(|rows|
                rows.len() == 1 && rows[0]["method"].as_str() == Some("popup") && rows[0]["minutes"].as_u64() == Some(minutes as u64)),
            None => value["reminders"]["overrides"].as_array().map_or(true, |rows| rows.is_empty()),
        };
    let times_match = if draft.all_day {
        event.start == draft.start.naive_local() && event.end == draft.end.naive_local()
    } else {
        let instant = |key: &str| value[key]["dateTime"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        instant("start").is_some_and(|v| v == draft.start) && instant("end").is_some_and(|v| v == draft.end)
    };
    if event.id != draft.id || event.title != draft.title || event.all_day != draft.all_day || !times_match || !reminders_match {
        return Err(message("An existing Google event does not match this draft; it was not changed"));
    }
    Ok(event)
}

fn lookup_with<F>(draft: &EventDraft, mut get: F) -> Result<Option<Event>, CalendarError>
where F: FnMut(&str) -> Result<Option<Value>, CalendarError> {
    let value = get(&format!("{EVENTS_URL}/{}", enc(&draft.id)))?;
    value.map(|value| matching_event(&value, draft)).transpose()
}

fn reconcile_event(draft: &EventDraft) -> Result<Option<Event>, CalendarError> {
    let (token, _) = access_token(true)?;
    let http = agent();
    lookup_with(draft, |url| get_json(&http, &token, url))
}

fn create_with<G, P>(draft: &EventDraft, mut get: G, mut post: P) -> Result<Event, CalendarError>
where G: FnMut(&str) -> Result<Option<Value>, CalendarError>,
      P: FnMut(&Value) -> Result<(u16, Value), CalendarError> {
    let body = draft_body(draft)?;
    // Always reconcile this ID before insertion, including after restarting the UI.
    if let Some(event) = lookup_with(draft, &mut get)? { return Ok(event); }
    match post(&body) {
        Ok((409, _)) => {
            let value = get(&format!("{EVENTS_URL}/{}", enc(&draft.id))).map_err(|_| CalendarError::UnknownSave)?
                .ok_or(CalendarError::UnknownSave)?;
            matching_event(&value, draft)
        }
        Ok((status, value)) => {
            if status >= 500 { return Err(CalendarError::UnknownSave); }
            api_status(status)?;
            matching_event(&value, draft)
        }
        Err(_) => Err(CalendarError::UnknownSave),
    }
}

pub fn create_event(draft: &EventDraft) -> Result<Event, CalendarError> {
    draft_body(draft)?;
    let (token, _) = access_token(true)?;
    let http = agent();
    create_with(draft, |url| get_json(&http, &token, url), |body| {
        let bytes = serde_json::to_vec(body).map_err(|_| message("Cannot encode Google event"))?;
        let mut response = http.post(EVENTS_URL).header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json").send(bytes.as_slice()).map_err(|_| CalendarError::UnknownSave)?;
        let status = response.status().as_u16();
        // Error bodies can include private event content; never surface them.
        if !(200..300).contains(&status) { return Ok((status, Value::Null)); }
        Ok((status, read_json(&mut response, MAX_PAGE).map_err(|_| CalendarError::UnknownSave)?))
    })
}

pub fn poll_delay(visible: bool, success: bool) -> Duration {
    Duration::from_secs(if visible || !success { 30 } else { 600 })
}

fn enqueue(queue: &mut VecDeque<Command>, command: Command) {
    if matches!(command, Command::Refresh { .. }) {
        queue.retain(|queued| !matches!(queued, Command::Refresh { .. }));
    }
    queue.push_back(command);
}

/// Library entry points for the calendar UI: `start_worker` (+ `Command`/`Update`) for listing, consent and event
/// creation; `authorize` and `config_path` for the sign-in flow; the pure helpers (`parse_event`, `CalendarRange`)
/// are testable without the network.
///
/// The only GUI Google worker: serializes token refresh, consent, listing, and writes.
/// `publish` returns false when the UI receiver has closed.
pub fn start_worker<P, O>(publish: P, open: O) -> Sender<Command>
where P: Fn(Update) -> bool + Send + 'static, O: Fn(&str) + Send + 'static {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || run_worker(receiver, publish, open));
    sender
}

fn run_worker(receiver: Receiver<Command>, publish: impl Fn(Update) -> bool, open: impl Fn(&str)) {
    let mut queue = VecDeque::new();
    let mut current: Option<(CalendarRange, u64, bool)> = None;
    let mut next_poll = Instant::now();
    let mut uncertain: HashMap<String, (EventDraft, u64)> = HashMap::new();
    loop {
        if queue.is_empty() {
            let received = if current.is_some() {
                receiver.recv_timeout(next_poll.saturating_duration_since(Instant::now()))
            } else { receiver.recv().map_err(|_| RecvTimeoutError::Disconnected) };
            match received {
                Ok(command) => enqueue(&mut queue, command),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    if let Some((range, generation, visible)) = current {
                        enqueue(&mut queue, Command::Refresh { range, generation, visible });
                    }
                }
            }
        }
        while let Ok(command) = receiver.try_recv() { enqueue(&mut queue, command); }
        let Some(command) = queue.pop_front() else { continue };
        match command {
            Command::Refresh { range, generation, visible } => {
                current = Some((range, generation, visible));
                for id in uncertain.keys().cloned().collect::<Vec<_>>() {
                    let (draft, saved_generation) = &uncertain[&id];
                    let result = reconcile_event(draft);
                    let saved_generation = *saved_generation;
                    if result.is_ok() { uncertain.remove(&id); }
                    if !publish(Update::Reconciled { id, generation: saved_generation, result }) { return; }
                }
                if !publish(Update::Loading { range, generation }) { return; }
                let result = fetch(range);
                next_poll = Instant::now() + poll_delay(visible, result.is_ok());
                if !publish(Update::Refreshed { range, generation, result }) { return; }
            }
            Command::Authorize { write } => {
                let mode = if write { AccessMode::CreateOwnedEvents } else { AccessMode::ReadOnly };
                let result = authorize(mode, &open);
                let succeeded = result.is_ok();
                if !publish(Update::Authorized { write, result }) { return; }
                if succeeded { next_poll = Instant::now(); }
            }
            Command::Create { draft, generation } => {
                let result = if uncertain.contains_key(&draft.id) {
                    Err(CalendarError::UnknownSave)
                } else { create_event(&draft) };
                if matches!(result, Err(CalendarError::UnknownSave)) {
                    uncertain.entry(draft.id.clone()).or_insert((draft, generation));
                }
                let succeeded = result.is_ok();
                if !publish(Update::Created { generation, result }) { return; }
                if succeeded { next_poll = Instant::now(); }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate { NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap() }

    fn draft(all_day: bool) -> EventDraft {
        let start = local_midnight(date("2026-01-15")).unwrap();
        EventDraft {
            id: "0123456789abcdef0123456789abcdef".into(), title: "Réunion; $(literal)".into(),
            start, end: if all_day { local_midnight(date("2026-01-16")).unwrap() } else { start + CDur::minutes(30) },
            all_day, alert_minutes: Some(15),
        }
    }

    fn listed(id: &str, title: &str) -> Value {
        serde_json::json!({"id": id, "summary": title, "start": {"date": "2026-01-15"}, "end": {"date": "2026-01-16"}})
    }

    #[test]
    fn full_month_grid_crosses_year_and_leap_boundaries() {
        assert_eq!(CalendarRange::for_month(date("2026-01-15")).unwrap(),
            CalendarRange { start: date("2025-12-28"), end: date("2026-02-01") });
        assert_eq!(CalendarRange::for_month(date("2024-02-01")).unwrap(),
            CalendarRange { start: date("2024-01-28"), end: date("2024-03-03") });
        assert!(CalendarRange { start: date("2026-01-01"), end: date("2026-01-01") }.bounds().is_err());
        let (start, end) = CalendarRange::for_month(date("2026-01-15")).unwrap().bounds().unwrap();
        assert_eq!(DateTime::parse_from_rfc3339(&start).unwrap().date_naive(), date("2025-12-28"));
        assert_eq!(DateTime::parse_from_rfc3339(&end).unwrap().date_naive(), date("2026-02-01"));
    }

    #[test]
    fn selected_calendars_and_all_pages_merge_without_identity_collisions() {
        let range = CalendarRange::for_month(date("2026-01-15")).unwrap();
        let mut calls = Vec::new();
        let snapshot = fetch_with(range, true, |url| {
            calls.push(url.to_string());
            Ok(if url.contains("calendarList") && !url.contains("pageToken") {
                serde_json::json!({"nextPageToken": "list/page two", "items": [
                    {"id": "account@example.invalid", "primary": true, "selected": true, "accessRole": "owner", "summary": "Main"},
                    {"id": "unselected", "selected": false, "accessRole": "reader"},
                    {"id": "hidden", "selected": true, "hidden": true, "accessRole": "reader"},
                    {"id": "deleted", "selected": true, "deleted": true, "accessRole": "reader"},
                    {"id": "freebusy", "selected": true, "accessRole": "freeBusyReader"}
                ]})
            } else if url.contains("calendarList") {
                assert!(url.contains("pageToken=list%2Fpage%20two"));
                serde_json::json!({"items": [
                    {"id": "shared/calendar@example.invalid", "selected": true, "accessRole": "writerWithoutPrivateAccess",
                     "summary": "Original", "summaryOverride": "Shared"}
                ]})
            } else if url.contains("account%40example.invalid") {
                serde_json::json!({"items": [listed("same", "Main event")]})
            } else if !url.contains("pageToken") {
                assert!(url.contains("shared%2Fcalendar%40example.invalid/events"));
                assert!(url.contains("singleEvents=true&orderBy=startTime&maxResults=250"));
                serde_json::json!({"nextPageToken": "second", "items": [listed("same", "Shared event"), listed("gone", "Cancelled later")]})
            } else {
                serde_json::json!({"items": [
                    listed("second", "Second page"), listed("same", "Shared event"),
                    {"id": "gone", "status": "cancelled"}
                ]})
            })
        }).unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(snapshot.events.len(), 3);
        assert!(snapshot.primary_selected && snapshot.write_access);
        assert_eq!(snapshot.events.iter().filter(|e| e.id == "same").count(), 2);
        assert!(snapshot.events.iter().any(|e| e.id == "second" && e.calendar_name == "Shared"));
        assert!(!snapshot.events.iter().any(|e| e.id == "gone"));
        assert_eq!(snapshot.events.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            vec!["Main event", "Second page", "Shared event"]);
    }

    #[test]
    fn empty_selection_never_falls_back_and_failed_page_never_returns_partial_events() {
        let range = CalendarRange::for_month(date("2026-01-15")).unwrap();
        let mut calls = 0;
        let empty = fetch_with(range, false, |_| {
            calls += 1;
            Ok(serde_json::json!({"items": []}))
        }).unwrap();
        assert_eq!(calls, 1);
        assert!(empty.events.is_empty() && !empty.primary_selected);
        let mut previous = vec![parse_event(&listed("old", "Old snapshot"), "primary", "Main").unwrap()];
        let before = previous.clone();
        let result = fetch_with(range, false, |url| {
            if url.contains("calendarList") {
                Ok(serde_json::json!({"items": [{"id":"primary", "selected":true, "accessRole":"owner", "primary":true}]}))
            } else if url.contains("pageToken") { Err(message("Synthetic failed page")) }
            else { Ok(serde_json::json!({"items":[listed("new", "Incomplete")], "nextPageToken":"second"})) }
        });
        assert!(result.is_err());
        if let Ok(snapshot) = result { previous = snapshot.events; }
        assert_eq!(previous, before);
    }

    #[test]
    fn calendar_labels_do_not_fall_back_to_account_ids() {
        let calendar = selected_calendar(&serde_json::json!({
            "id": "private@example.invalid", "selected": true, "accessRole": "reader"
        })).unwrap();
        assert_eq!(calendar.name, "Calendar");
        let mut requests = 0;
        let result = pages("https://example.invalid", &mut |_| {
            requests += 1;
            Ok(serde_json::json!({"nextPageToken": "same"}))
        }, |_| Ok(()));
        assert!(result.is_err());
        assert_eq!(requests, 2);
    }

    #[test]
    fn draft_bodies_use_exclusive_dates_offsets_and_explicit_alerts() {
        for all_day in [false, true] {
            for alert in [None, Some(0), Some(5), Some(15), Some(30), Some(60)] {
                let mut draft = draft(all_day);
                draft.alert_minutes = alert;
                let body = draft_body(&draft).unwrap();
                assert_eq!(body["id"], draft.id);
                assert_eq!(body["summary"], draft.title);
                assert_eq!(body["reminders"]["useDefault"], false);
                assert!(body.get("attendees").is_none());
                if all_day {
                    assert_eq!(body["start"]["date"], "2026-01-15");
                    assert_eq!(body["end"]["date"], "2026-01-16");
                } else {
                    assert_eq!(body["start"]["dateTime"], draft.start.to_rfc3339());
                    assert_eq!(body["end"]["dateTime"], draft.end.to_rfc3339());
                }
                let rows = body["reminders"]["overrides"].as_array().unwrap();
                assert_eq!(rows.len(), usize::from(alert.is_some()));
                if let Some(minutes) = alert { assert_eq!(rows[0]["minutes"], minutes); }
                assert!(matching_event(&body, &draft).is_ok());
            }
        }
        let mut invalid = draft(false);
        invalid.title = " ".into();
        assert!(draft_body(&invalid).is_err());
        invalid.title = "é".repeat(201);
        assert!(draft_body(&invalid).is_err());
        invalid.title = "é".repeat(200);
        assert!(draft_body(&invalid).is_ok());
        invalid.end = invalid.start;
        assert!(draft_body(&invalid).is_err());
        let mut invalid = draft(true);
        invalid.end += CDur::days(1);
        assert!(draft_body(&invalid).is_err());
    }

    #[test]
    fn uncertain_insert_reconciles_same_id_without_second_post() {
        let draft = draft(false);
        let body = draft_body(&draft).unwrap();
        let result = create_with(&draft, |_| Ok(None), |_| Err(CalendarError::UnknownSave));
        assert_eq!(result, Err(CalendarError::UnknownSave));
        let mut posts = 0;
        let saved = create_with(&draft, |url| {
            assert!(url.ends_with(&draft.id));
            Ok(Some(body.clone()))
        }, |_| { posts += 1; Ok((200, body.clone())) }).unwrap();
        assert_eq!(saved.id, draft.id);
        assert_eq!(posts, 0);
        let retry = create_with(&draft, |_| Ok(None), |posted| {
            posts += 1;
            assert_eq!(posted["id"], draft.id);
            Ok((200, body.clone()))
        }).unwrap();
        assert_eq!(retry, saved);
        assert_eq!(posts, 1);
        assert_eq!(lookup_with(&draft, |_| Ok(None)).unwrap(), None);
        assert!(lookup_with(&draft, |_| Err(CalendarError::NeedAuth)).is_err());
    }

    #[test]
    fn conflicts_reconcile_and_mismatches_are_never_overwritten() {
        let draft = draft(false);
        let body = draft_body(&draft).unwrap();
        let mut gets = 0;
        let event = create_with(&draft, |_| {
            gets += 1;
            Ok(if gets == 1 { None } else { Some(body.clone()) })
        }, |_| Ok((409, Value::Null))).unwrap();
        assert_eq!(event.id, draft.id);
        assert_eq!(gets, 2);
        let mut mismatched = body.clone();
        mismatched["summary"] = Value::String("Different event".into());
        let result = create_with(&draft, |_| Ok(Some(mismatched.clone())), |_| panic!("must not insert"));
        assert!(matches!(result, Err(CalendarError::Message(_))));
        let mut gets = 0;
        let result = create_with(&draft, |_| {
            gets += 1;
            if gets == 1 { Ok(None) } else { Err(CalendarError::NeedAuth) }
        }, |_| Ok((409, Value::Null)));
        assert_eq!(result, Err(CalendarError::UnknownSave));
    }

    #[test]
    fn scope_upgrades_preserve_old_grants_until_complete() {
        let legacy = serde_json::json!({"refresh_token":"old", "access_token":"old-access"});
        assert!(!has_scope(&legacy, WRITE_SCOPE));
        let old = serde_json::json!({"refresh_token":"old", "access_token":"old-access", "scope":SCOPE});
        let denied = oauth_result(400, serde_json::json!({"error":"access_denied", "error_description":"private"}));
        assert!(denied.is_err());
        assert!(!denied.unwrap_err().to_string().contains("private"));
        let partial = serde_json::json!({"refresh_token":"new", "access_token":"new-access", "scope":SCOPE});
        assert_eq!(token_value(&partial, None, Some(AccessMode::CreateOwnedEvents)), Err(CalendarError::NeedWriteAccess));
        let complete = serde_json::json!({
            "refresh_token":"new", "access_token":"new-access", "scope":format!("{SCOPE} {WRITE_SCOPE}")
        });
        let new = token_value(&complete, None, Some(AccessMode::CreateOwnedEvents)).unwrap();
        assert!(has_scope(&new, WRITE_SCOPE));
        assert_eq!(old["refresh_token"], "old");
        let missing_refresh = serde_json::json!({"access_token":"new-access", "scope":format!("{SCOPE} {WRITE_SCOPE}")});
        assert!(token_value(&missing_refresh, None, Some(AccessMode::CreateOwnedEvents)).is_err());
        let refreshed = token_value(&serde_json::json!({"access_token":"renewed"}), Some(&new), None).unwrap();
        assert_eq!(refreshed["scope"], new["scope"]);
        assert_eq!(refreshed["refresh_token"], "new");
        let legacy_refresh = token_value(&serde_json::json!({"access_token":"renewed"}), Some(&legacy), None).unwrap();
        assert!(!has_scope(&legacy_refresh, WRITE_SCOPE));
        assert_eq!(oauth_result(400, serde_json::json!({"error":"invalid_grant"})), Err(CalendarError::NeedAuth));
    }

    #[test]
    fn refresh_coalescing_preserves_each_write_and_authorization() {
        let range = CalendarRange::for_month(date("2026-01-15")).unwrap();
        let mut queue = VecDeque::new();
        enqueue(&mut queue, Command::Refresh { range, generation: 1, visible: false });
        enqueue(&mut queue, Command::Create { draft: draft(false), generation: 41 });
        enqueue(&mut queue, Command::Authorize { write: true });
        enqueue(&mut queue, Command::Refresh { range, generation: 2, visible: true });
        enqueue(&mut queue, Command::Create { draft: draft(true), generation: 42 });
        enqueue(&mut queue, Command::Refresh { range, generation: 3, visible: false });
        assert_eq!(queue.len(), 4);
        assert!(matches!(queue.pop_front(), Some(Command::Create { generation: 41, .. })));
        assert!(matches!(queue.pop_front(), Some(Command::Authorize { write: true })));
        assert!(matches!(queue.pop_front(), Some(Command::Create { generation: 42, .. })));
        assert!(matches!(queue.pop_front(), Some(Command::Refresh { generation: 3, .. })));
        assert_eq!(poll_delay(false, true), Duration::from_secs(600));
        assert_eq!(poll_delay(false, false), Duration::from_secs(30));
        assert_eq!(poll_delay(true, true), Duration::from_secs(30));
        assert_eq!(poll_delay(true, false), Duration::from_secs(30));
    }

    fn ev(start: &str, end: &str, all_day: bool) -> Event {
        let p = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Event { id: "event".into(), calendar_id: "primary".into(), calendar_name: "Primary".into(),
            title: "t".into(), start: p(start), end: p(end), all_day, link: None, location: String::new() }
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
