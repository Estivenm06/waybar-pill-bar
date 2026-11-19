//! Status of the app buttons in the left pill (running / idle, WhatsApp unread / offline).
//!
//! A `Tracker` lives in the desktop watcher thread and is fed `j/clients` snapshots; it also keeps background
//! apps alive (`waybar-bgapp.sh ensure`). The WhatsApp plugin's daemon status is streamed by `watch_whatsapp`.
//! Both publish their result to `State::apps` through the `Poster`.
use crate::apps::{AppButton, AppState, WaView};
use crate::wayland::Poster;
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const CATALOG: &str = include_str!("../../files/setup/apps-catalog.json");

#[derive(Clone, Debug)]
struct CatalogApp {
    id: String,
    name: String,
    glyph: String,
    class: String,
    proc_name: Option<String>,
    exe: Vec<String>,
    web: Option<String>,
}

fn catalog() -> Result<Vec<CatalogApp>, String> {
    let v: Value = serde_json::from_str(CATALOG).map_err(|e| format!("apps catalog: {e}"))?;
    let arr = v.as_array().ok_or("apps catalog is not an array")?;
    arr.iter().map(|a| {
        let s = |k: &str| a[k].as_str().map(str::to_string);
        Ok(CatalogApp {
            id: s("id").ok_or("catalog entry without id")?,
            name: s("name").ok_or("catalog entry without name")?,
            glyph: s("glyph").ok_or("catalog entry without glyph")?,
            class: s("class").ok_or("catalog entry without class")?,
            proc_name: s("proc"),
            exe: a["exe"].as_array().map(|e| e.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            web: s("web"),
        })
    }).collect()
}

pub(crate) fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// Launch argv, same preference order as the setup helper; `None` when the app is not installed.
fn launch_argv(a: &CatalogApp, installed: &dyn Fn(&str) -> bool) -> Option<Vec<String>> {
    if let Some(url) = &a.web {
        return installed("omarchy-launch-webapp").then(|| vec!["omarchy-launch-webapp".to_string(), url.clone()]);
    }
    a.exe.iter().find(|e| installed(e)).map(|e| vec![e.clone()])
}

struct Tracked {
    id: String,
    name: String,
    glyph: String,
    class: String,
    matcher: Regex,
    proc_name: Option<String>,
    background: bool,
    launch: Vec<String>,
    /// Last status published (`None` = nothing published yet).
    status: Option<bool>,
}

pub(crate) struct Tracker {
    apps: Vec<Tracked>,
    clients: Option<Value>,
    procs: HashSet<String>,
    ensure_script: PathBuf,
    ensuring: BTreeMap<String, Child>,
}

impl Tracker {
    /// `document` is the root user configuration (`apps` array); an app shows only when it is installed.
    pub(crate) fn new(document: &Value) -> Result<Self, String> {
        Self::with_installed(document, &on_path)
    }

    fn with_installed(document: &Value, installed: &dyn Fn(&str) -> bool) -> Result<Self, String> {
        let known = catalog()?;
        let apps_cfg = match document.get("apps") {
            None | Some(Value::Null) => &Vec::new(),
            Some(Value::Array(a)) => a,
            Some(_) => return Err("configuration `apps` is not an array".into()),
        };
        let mut apps = Vec::new();
        for e in apps_cfg {
            let Some(id) = e["id"].as_str() else { continue };
            let Some(c) = known.iter().find(|c| c.id == id) else { continue };
            if apps.iter().any(|a: &Tracked| a.id == id) { continue; }
            let Some(launch) = launch_argv(c, installed) else { continue };
            let matcher = RegexBuilder::new(&c.class).case_insensitive(true).build().map_err(|e| format!("class of {id}: {e}"))?;
            apps.push(Tracked {
                id: id.to_string(), name: c.name.clone(), glyph: c.glyph.clone(), class: c.class.clone(), matcher,
                proc_name: c.proc_name.clone(), background: e["background"].as_bool().unwrap_or(false), launch, status: None,
            });
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        Ok(Self {
            apps, clients: None, procs: HashSet::new(),
            ensure_script: home.join(".config/waybar/scripts/waybar-bgapp.sh"), ensuring: BTreeMap::new(),
        })
    }

    /// The buttons as the pill shows them (before the WhatsApp plugin override).
    pub(crate) fn buttons(&self) -> Vec<AppButton> {
        self.apps.iter().map(|a| AppButton {
            id: a.id.clone(), name: a.name.clone(), glyph: a.glyph.clone(), class: a.class.clone(),
            background: a.background, launch: a.launch.clone(),
            state: if self.running(a) { AppState::Running } else { AppState::Idle },
        }).collect()
    }

    fn has_window(&self, a: &Tracked) -> bool {
        self.clients.as_ref().and_then(Value::as_array)
            .is_some_and(|cs| cs.iter().any(|c| a.matcher.is_match(c["class"].as_str().unwrap_or(""))))
    }

    fn running(&self, a: &Tracked) -> bool {
        a.background || self.has_window(a) || a.proc_name.as_ref().is_some_and(|p| self.procs.contains(p))
    }

    /// Record changed statuses; true when any changed.
    fn publish(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.apps.len() {
            let run = self.running(&self.apps[i]);
            if self.apps[i].status == Some(run) { continue; }
            self.apps[i].status = Some(run);
            changed = true;
        }
        changed
    }

    /// A fresh `j/clients` answer. Only invalid input is an error (the previous status stays valid).
    pub(crate) fn update_clients(&mut self, clients: Value) -> Result<bool, String> {
        if !clients.is_array() { return Err("hyprland clients is not an array".into()); }
        self.clients = Some(clients);
        let changed = self.publish();
        self.maintain();
        Ok(changed)
    }

    /// Periodic work: process-name scan (shared by all apps), background-app maintenance.
    pub(crate) fn refresh(&mut self) -> bool {
        if self.clients.is_none() { return false; }
        let wanted: HashSet<&str> = self.apps.iter().filter(|a| !a.background && !self.has_window(a)).filter_map(|a| a.proc_name.as_deref()).collect();
        let mut found = HashSet::new();
        if !wanted.is_empty() {
            if let Ok(rd) = std::fs::read_dir("/proc") {
                for e in rd.flatten() {
                    if !e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) { continue; }
                    if let Ok(c) = std::fs::read_to_string(e.path().join("comm")) {
                        let c = c.trim_end_matches('\n');
                        if wanted.contains(c) { found.insert(c.to_string()); }
                    }
                }
            }
        }
        self.procs = found;
        let changed = self.publish();
        self.maintain();
        changed
    }

    /// Background apps whose window is gone (they must come back on the hidden workspace).
    fn missing_background(&self) -> Vec<usize> {
        if self.clients.is_none() { return Vec::new(); }
        (0..self.apps.len()).filter(|&i| self.apps[i].background && !self.has_window(&self.apps[i])).collect()
    }

    /// At most one live `waybar-bgapp.sh ensure` per app.
    fn maintain(&mut self) {
        self.ensuring.retain(|_, c| matches!(c.try_wait(), Ok(None)));
        for i in self.missing_background() {
            let a = &self.apps[i];
            if self.ensuring.contains_key(&a.id) { continue; }
            let mut cmd = Command::new(&self.ensure_script);
            cmd.arg("ensure").arg(&a.id).arg(&a.class).args(&a.launch)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
            match cmd.spawn() {
                Ok(child) => { self.ensuring.insert(a.id.clone(), child); }
                Err(e) => eprintln!("waybar-notch: cannot start ensure for {}: {e}", a.id),
            }
        }
    }
}

/// Window-event lines that can change which windows exist (class matching only needs open/close/title).
pub(crate) fn title_event(line: &str) -> bool {
    line.split(">>").next().is_some_and(|n| matches!(n, "openwindow" | "closewindow" | "windowtitle" | "windowtitlev2"))
}

// ------------------------------------------------------------------ WhatsApp plugin status

const WHATSAPP_PLUGIN: &str = "io.github.ricky.whatsapp";
const MAX_LINE: usize = 512 * 1024;
const RECONNECT: Duration = Duration::from_secs(2);

/// Whether the Omarchy WhatsApp plugin is enabled (a failing or unintelligible catalog means no).
pub(crate) fn whatsapp_plugin_enabled() -> bool {
    if !on_path("omarchy-plugin-list") { return false; }
    let Ok(out) = Command::new("omarchy").args(["plugin", "list", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output() else { return false };
    if !out.status.success() { return false; }
    serde_json::from_slice::<Value>(&out.stdout).ok()
        .and_then(|v| v.as_array().map(|a| a.iter().any(|p| p["id"] == WHATSAPP_PLUGIN && p["enabled"] == true)))
        .unwrap_or(false)
}

/// Settings of the WhatsApp entry in `~/.config/omarchy/shell.json`; the widget's own defaults otherwise.
fn widget_settings(shell_json: Option<&Value>) -> (String, bool) {
    let (mut socket, mut show) = (String::new(), true);
    let layout = shell_json.and_then(|c| c["bar"]["layout"].as_object());
    for entries in layout.into_iter().flat_map(|l| l.values()).filter_map(Value::as_array) {
        for e in entries.iter().filter(|e| e["id"] == WHATSAPP_PLUGIN) {
            if let Some(s) = e["socketPath"].as_str() { socket = s.to_string(); }
            if let Some(b) = e["showUnreadCount"].as_bool() { show = b; }
        }
    }
    (socket, show)
}

fn socket_path(configured: &str) -> PathBuf {
    if !configured.is_empty() { return configured.into(); }
    PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("omarchy-whatsapp.sock")
}

/// Connection/link/unread facts from the daemon. Chat and message contents are never read out of the frames.
pub(crate) struct WaStatus {
    show_count: bool,
    online: bool,
    linked: bool,
    needs_login: bool,
    connection: String,
    unread: u32,
}

fn count(v: &Value) -> u32 { v.as_u64().map_or(0, |n| n.min(u32::MAX as u64) as u32) }

impl WaStatus {
    pub(crate) fn new(show_count: bool) -> Self {
        WaStatus { show_count, online: false, linked: false, needs_login: false, connection: String::new(), unread: 0 }
    }

    /// A stale badge must not outlive its connection.
    fn reset(&mut self) { *self = WaStatus::new(self.show_count); }

    fn apply(&mut self, frame: &Value) {
        match frame["t"].as_str() {
            Some("state") => {
                self.online = true;
                self.connection = frame["connection"].as_str().unwrap_or("").to_string();
                self.needs_login = frame["needsLogin"] == true;
                self.linked = frame["linked"] == true;
                self.unread = count(&frame["unread"]);
            }
            // unread-only update: keep connection/link flags
            Some("chats" | "message") if frame.get("unread").is_some() => self.unread = count(&frame["unread"]),
            _ => {}
        }
    }

    pub(crate) fn view(&self) -> WaView {
        let bare = |state| WaView { state, badge: None };
        if !self.online { return bare(AppState::Offline); }
        if self.needs_login || !self.linked { return bare(AppState::Unlinked); }
        if self.connection != "open" { return bare(AppState::Offline); }
        let n = self.unread;
        if n == 0 { return bare(AppState::Running); }
        WaView { state: AppState::Unread, badge: self.show_count.then(|| if n > 99 { "99+".into() } else { n.to_string() }) }
    }
}

/// Feed frames until the connection ends or misbehaves (EOF, oversized or malformed line).
fn read_session(mut r: impl BufRead, status: &mut WaStatus, emit: &mut dyn FnMut(&WaStatus)) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() == Some(&b'\n') { line.pop(); } else { return; } // EOF mid-line or oversized
        if line.len() > MAX_LINE { return; }
        let Ok(frame) = serde_json::from_slice::<Value>(&line) else { return };
        if frame.is_object() {
            status.apply(&frame);
            emit(status);
        }
    }
}

/// Stream the WhatsApp daemon's state into `State::apps.wa` (reconnects every 2s).
pub(crate) fn watch_whatsapp(poster: Poster) {
    std::thread::Builder::new().name("whatsapp-status".into()).spawn(move || {
        let shell = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .and_then(|b| std::fs::read_to_string(b.join("omarchy/shell.json")).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let (configured, show) = widget_settings(shell.as_ref());
        let path = socket_path(&configured);
        let mut status = WaStatus::new(show);
        let mut last: Option<WaView> = None;
        let mut emit = |s: &WaStatus| {
            let v = s.view();
            if last.as_ref() != Some(&v) {
                last = Some(v.clone());
                poster.post(move |st, _| st.apps.wa = Some(v));
            }
        };
        emit(&status);
        loop {
            if let Ok(mut s) = UnixStream::connect(&path) {
                if s.write_all(b"{\"t\":\"hello\"}\n").is_ok() {
                    read_session(std::io::BufReader::new(s), &mut status, &mut emit);
                }
            }
            status.reset();
            emit(&status);
            std::thread::sleep(RECONNECT);
        }
    }).expect("spawn whatsapp-status");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    fn tracker(apps: Value) -> Tracker {
        let mut t = Tracker::with_installed(&json!({ "apps": apps }), &|_| true).unwrap();
        t.ensure_script = std::env::temp_dir().join("app-status-missing-script");
        t
    }

    fn states(t: &Tracker) -> Vec<AppState> { t.buttons().iter().map(|b| b.state).collect() }

    #[test]
    fn window_class_matches_case_insensitively_and_changes_once() {
        let mut t = tracker(json!([{"id": "spotify", "background": false}]));
        assert!(t.update_clients(json!([{"class": "Spotify"}])).unwrap());
        assert_eq!(states(&t), [AppState::Running]);
        assert!(!t.update_clients(json!([{"class": "SPOTIFY"}, {"class": "kitty"}])).unwrap());
        assert!(t.update_clients(json!([])).unwrap());
        assert_eq!(states(&t), [AppState::Idle]);
    }

    #[test]
    fn process_only_app_appears_and_disappears() {
        let mut t = tracker(json!([{"id": "steam", "background": false}]));
        t.update_clients(json!([])).unwrap();
        t.procs.insert("steam".into());
        assert!(t.publish());
        assert_eq!(t.apps[0].status, Some(true));
        t.procs.clear();
        assert!(t.publish());
        assert_eq!(t.apps[0].status, Some(false));
    }

    #[test]
    fn background_app_stays_running_and_asks_for_one_ensure_only_while_missing() {
        let mut t = tracker(json!([{"id": "steam", "background": true}]));
        assert!(t.missing_background().is_empty(), "no clients known yet: do not guess");
        t.update_clients(json!([])).unwrap();
        assert_eq!(t.apps[0].status, Some(true));
        assert_eq!(t.missing_background(), vec![0]);
        t.update_clients(json!([{"class": "steam"}])).unwrap();
        assert!(t.missing_background().is_empty());
    }

    #[test]
    fn invalid_client_answer_keeps_last_known_state() {
        let mut t = tracker(json!([{"id": "spotify", "background": false}]));
        t.update_clients(json!([{"class": "spotify"}])).unwrap();
        assert!(t.update_clients(json!({"error": 1})).is_err());
        assert_eq!(t.apps[0].status, Some(true));
        assert!(!t.refresh());
    }

    #[test]
    fn only_installed_catalog_apps_in_config_order_without_duplicates() {
        let doc = json!({"apps": [{"id": "steam"}, {"id": "nope"}, {"id": "discord"}, {"id": "steam"}, {"id": "spotify"}]});
        let t = Tracker::with_installed(&doc, &|e| e != "spotify").unwrap();
        let ids: Vec<_> = t.buttons().into_iter().map(|b| b.id).collect();
        assert_eq!(ids, ["steam", "discord"]);
        assert!(Tracker::with_installed(&json!({"apps": 3}), &|_| true).is_err());
        assert!(Tracker::with_installed(&json!({}), &|_| true).unwrap().buttons().is_empty());
        // web apps need the webapp launcher
        let wa = json!({"apps": [{"id": "whatsapp"}]});
        assert!(Tracker::with_installed(&wa, &|e| e != "omarchy-launch-webapp").unwrap().buttons().is_empty());
        assert_eq!(Tracker::with_installed(&wa, &|_| true).unwrap().apps[0].launch, ["omarchy-launch-webapp", "https://web.whatsapp.com/"]);
    }

    #[test]
    fn title_events_are_window_events_only() {
        assert!(title_event("openwindow>>a,1,b,c"));
        assert!(title_event("windowtitlev2>>x"));
        assert!(!title_event("workspace>>1"));
    }

    fn wa(frames: &[Value], show: bool) -> WaView {
        let mut s = WaStatus::new(show);
        for f in frames { s.apply(f); }
        s.view()
    }

    #[test]
    fn whatsapp_status_states() {
        let state = |conn: &str, linked: bool, login: bool, unread: i64| json!({"t":"state","connection":conn,"linked":linked,"needsLogin":login,"unread":unread});
        assert_eq!(wa(&[], true).state, AppState::Offline);
        assert_eq!(wa(&[state("open", false, true, 0)], true).state, AppState::Unlinked);
        assert_eq!(wa(&[state("open", false, false, 0)], true).state, AppState::Unlinked);
        assert_eq!(wa(&[state("connecting", true, false, 0)], true).state, AppState::Offline);
        assert_eq!(wa(&[state("open", true, false, 0)], true), WaView { state: AppState::Running, badge: None });
        assert_eq!(wa(&[state("open", true, false, 26)], true), WaView { state: AppState::Unread, badge: Some("26".into()) });
        assert_eq!(wa(&[state("open", true, false, 150)], true).badge.as_deref(), Some("99+"));
        // unread shows as a state but without the number when the widget says so
        assert_eq!(wa(&[state("open", true, false, 4)], false), WaView { state: AppState::Unread, badge: None });
        // unread-only updates keep link state; negatives and bools count as zero
        let v = wa(&[state("open", true, false, 0), json!({"t":"message","unread":3})], true);
        assert_eq!(v.badge.as_deref(), Some("3"));
        assert_eq!(wa(&[state("open", true, false, 3), json!({"t":"chats","unread":-1})], true).state, AppState::Running);
        assert_eq!(wa(&[state("open", true, false, 3), json!({"t":"chats","unread":true})], true).state, AppState::Running);
        // frames of other kinds change nothing
        assert_eq!(wa(&[state("open", true, false, 3), json!({"t":"typing"})], true).badge.as_deref(), Some("3"));
    }

    #[test]
    fn session_stops_on_malformed_oversized_or_unterminated_input() {
        let run = |data: Vec<u8>| {
            let mut s = WaStatus::new(true);
            let mut n = 0;
            read_session(Cursor::new(data), &mut s, &mut |_| n += 1);
            n
        };
        let ok = br#"{"t":"state","connection":"open","linked":true,"unread":2}"#;
        let mut two = [ok.as_slice(), b"\n", ok.as_slice(), b"\n"].concat();
        assert_eq!(run(two.clone()), 2);
        two.extend_from_slice(b"not json\n{\"t\":\"state\"}\n");
        assert_eq!(run(two), 2, "reading stops at the malformed line");
        assert_eq!(run([ok.as_slice()].concat()), 0, "a line without its newline is dropped");
        let mut big = vec![b'x'; MAX_LINE + 10];
        big.push(b'\n');
        assert_eq!(run(big), 0);
        assert_eq!(run(b"[1,2]\n".to_vec()), 0, "non-object frames are ignored");
    }

    #[test]
    fn widget_settings_and_socket_path() {
        let cfg = json!({"bar": {"layout": {"left": [{"id": "x"}], "right": [{"id": WHATSAPP_PLUGIN, "socketPath": "/run/wa.sock", "showUnreadCount": false}]}}});
        assert_eq!(widget_settings(Some(&cfg)), ("/run/wa.sock".to_string(), false));
        assert_eq!(widget_settings(None), (String::new(), true));
        assert_eq!(widget_settings(Some(&json!({"bar": 3}))), (String::new(), true));
        assert_eq!(socket_path("/a"), PathBuf::from("/a"));
        assert!(socket_path("").ends_with("omarchy-whatsapp.sock"));
    }
}
