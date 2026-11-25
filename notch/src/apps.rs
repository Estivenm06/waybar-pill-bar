//! App buttons of the left pill: state slice plus the click behaviour of the former `waybar-app.sh click` /
//! `waybar-bgapp.sh toggle` helpers. Status comes from `app_status` (windows/processes, WhatsApp daemon).
use crate::app_status::on_path;
use crate::config::PinnedApp;
use crate::desktop_entries::{self, DesktopEntry, DesktopIndex};
use crate::hypr;
use crate::render::Image;
use crate::wayland::Poster;
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppState {
    #[default]
    Idle,
    Running,
    /// Running with unread messages.
    Unread,
    /// Bridge down or reconnecting.
    Offline,
    /// Login needed.
    Unlinked,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppButton {
    pub id: String,
    pub name: String,
    /// Nerd Font glyph.
    pub glyph: String,
    /// Window-class regex (case-insensitive).
    pub class: String,
    /// Kept alive on a hidden special workspace; the click toggles it.
    pub background: bool,
    pub launch: Vec<String>,
    pub state: AppState,
}

/// What the WhatsApp plugin's daemon reports.
#[derive(Clone, Debug, PartialEq)]
pub struct WaView {
    pub state: AppState,
    /// Unread count text next to the icon ("26", "99+").
    pub badge: Option<String>,
}

/// One mapped application window of `j/clients`.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenWindow {
    pub address: String,
    pub class: String,
    pub initial_class: String,
    pub title: String,
    pub workspace: i64,
    pub workspace_name: String,
    /// `focusHistoryID`: 0 = most recently focused, negative = unknown.
    pub focus_order: i64,
}

/// All windows of one application (configured app, or lowercase initial class).
#[derive(Clone, Debug, PartialEq)]
pub struct OpenApp {
    pub key: String,
    pub name: String,
    pub configured_id: Option<String>,
    pub desktop_id: Option<String>,
    /// Most recently focused first.
    pub windows: Vec<OpenWindow>,
    pub active: bool,
}

/// One slot of the shortcut row shared by the left pill and the collapsed notch.
#[derive(Clone, Debug, PartialEq)]
pub struct Shortcut {
    /// `cfg:<id>` for configured apps, `app:<class>` otherwise; stable across focus changes.
    pub key: String,
    pub name: String,
    pub configured_id: Option<String>,
    pub pin: Option<PinnedApp>,
    pub desktop_id: Option<String>,
    /// Image cache key (`desktop id` or `class:<class>`); empty for configured apps (they use their glyph).
    pub icon_key: String,
    /// Normalized window class (empty for configured apps).
    pub class: String,
    pub windows: usize,
    pub active: bool,
}

struct IconReq {
    key: String,
    desktop_id: Option<String>,
    class: String,
}

#[derive(Default)]
pub struct Apps {
    /// Configured and installed apps in config order.
    pub buttons: Vec<AppButton>,
    /// Every open application (configured or not), from the last good client snapshot.
    pub open: Vec<OpenApp>,
    /// Persistent extra launchers (`notch.pinned_apps`).
    pub pins: Vec<PinnedApp>,
    /// Saved tile arrangement (`notch.app_order`).
    pub order: Vec<String>,
    /// Presentation list: configured buttons, pins in saved order, then other open apps by name.
    pub shortcuts: Vec<Shortcut>,
    /// Decoded icons by `Shortcut::icon_key`; `None` = resolved but unavailable (letter tile).
    pub icons: HashMap<String, Option<Image>>,
    /// Installed applications, refreshed when the Applications page opens.
    pub catalog: Vec<DesktopEntry>,
    /// `catalog` has been read at least once (an empty list before that is not "unavailable").
    pub catalog_ready: bool,
    /// Group the Applications page should open expanded (set together with `nav = Page("apps")`).
    pub page_focus: Option<String>,
    /// Outcome of the last pin save, consumed by the Applications page.
    pub pin_result: Option<Result<(), String>>,
    icon_tx: Option<Sender<IconReq>>,
    icon_pending: HashSet<String>,
    /// The WhatsApp plugin is enabled: its daemon drives the icon and the left click opens its notch page.
    pub wa_plugin: bool,
    pub wa: Option<WaView>,
}

/// Lowercase window-class identity of a window: `initialClass`, else `class`.
pub fn norm_class(initial: &str, class: &str) -> String {
    let c = if initial.trim().is_empty() { class } else { initial };
    c.trim().to_lowercase()
}

/// Case-insensitive class rules of the configured buttons (an invalid pattern matches nothing).
fn rules(buttons: &[AppButton]) -> Vec<(String, Regex)> {
    buttons.iter().filter_map(|b| Some((b.id.clone(), RegexBuilder::new(&b.class).case_insensitive(true).build().ok()?))).collect()
}

/// Group key of a window: the first configured app whose class regex matches, else `app:<normalized class>`.
fn group_key(w: &OpenWindow, rules: &[(String, Regex)]) -> String {
    match rules.iter().find(|(_, re)| re.is_match(&w.class)) {
        Some((id, _)) => format!("cfg:{id}"),
        None => format!("app:{}", norm_class(&w.initial_class, &w.class)),
    }
}

fn valid_address(a: &str) -> bool {
    a.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Mapped, visible application windows of a `j/clients` document, most recently focused first.
/// `None` when the document is not an array (an error reply must not clear the inventory).
fn parse_windows(clients: &Value) -> Option<Vec<OpenWindow>> {
    let mut v: Vec<OpenWindow> = clients.as_array()?.iter().filter_map(|c| {
        if !c["mapped"].as_bool().unwrap_or(true) || c["hidden"].as_bool().unwrap_or(false) { return None; }
        let address = c["address"].as_str().filter(|a| valid_address(a))?.to_string();
        let (class, initial) = (c["class"].as_str().unwrap_or("").to_string(), c["initialClass"].as_str().unwrap_or("").to_string());
        if class.trim().is_empty() && initial.trim().is_empty() { return None; }
        Some(OpenWindow {
            address, class, initial_class: initial,
            title: c["title"].as_str().unwrap_or("").to_string(),
            workspace: c["workspace"]["id"].as_i64().unwrap_or(0),
            workspace_name: c["workspace"]["name"].as_str().unwrap_or("").to_string(),
            focus_order: c["focusHistoryID"].as_i64().unwrap_or(-1),
        })
    }).collect();
    v.sort_by(|a, b| {
        let rank = |w: &OpenWindow| if w.focus_order >= 0 { (0, w.focus_order) } else { (1, 0) };
        rank(a).cmp(&rank(b)).then_with(|| a.address.cmp(&b.address))
    });
    Some(v)
}

fn display_name(class: &str) -> String {
    let mut c = class.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// Group the windows of a `j/clients` document by application, ordered by key. `None` for a non-array reply.
pub fn group_clients(clients: &Value, buttons: &[AppButton], index: &DesktopIndex) -> Option<Vec<OpenApp>> {
    let wins = parse_windows(clients)?;
    let rules = rules(buttons);
    let mut groups: Vec<OpenApp> = Vec::new();
    for w in wins {
        let key = group_key(&w, &rules);
        let at = match groups.iter().position(|g| g.key == key) {
            Some(i) => i,
            None => {
                let configured_id = key.strip_prefix("cfg:").map(str::to_string);
                let entry = if configured_id.is_none() { index.find_class(&w.initial_class, &w.class) } else { None };
                let name = match (&configured_id, entry) {
                    (Some(id), _) => buttons.iter().find(|b| &b.id == id).map_or_else(|| id.clone(), |b| b.name.clone()),
                    (None, Some(e)) => e.name.clone(),
                    (None, None) => display_name(if w.initial_class.trim().is_empty() { &w.class } else { &w.initial_class }),
                };
                groups.push(OpenApp { key, name, configured_id, desktop_id: entry.map(|e| e.id.clone()), windows: Vec::new(), active: false });
                groups.len() - 1
            }
        };
        groups[at].active |= w.focus_order == 0;
        groups[at].windows.push(w);
    }
    groups.sort_by(|a, b| a.key.cmp(&b.key));
    Some(groups)
}

/// The window a click should focus: `address` when it still exists in the group, else the group's most recent
/// member. A vanished or recycled address (different group now) is never used.
fn select_target(wins: &[OpenWindow], key: &str, rules: &[(String, Regex)], address: Option<&str>) -> Option<OpenWindow> {
    let mut members = wins.iter().filter(|w| group_key(w, rules) == key);
    let first = members.clone().next();
    address.and_then(|a| members.find(|w| w.address == a)).or(first).cloned()
}

fn pin_launch_argv(desktop_id: &str) -> Vec<String> { vec!["gtk-launch".into(), desktop_id.into()] }

fn launch_pin(pin: &PinnedApp) {
    let known = desktop_entries::cached().get(&pin.desktop_id).is_some() || desktop_entries::refresh().get(&pin.desktop_id).is_some();
    if !known || !on_path("gtk-launch") {
        run_detached("notify-send", &["--app-name=Waybar", "-u", "normal", "Application unavailable", &pin.desktop_id]);
        return;
    }
    launch(&pin_launch_argv(&pin.desktop_id));
}

/// Bring `w` to the user: show its special workspace first when hidden, then focus it. Already-focused ordinary
/// windows are left alone.
fn focus_checked(w: &OpenWindow) {
    let special = w.workspace_name.strip_prefix("special:");
    let mut revealed = false;
    if let Some(name) = special {
        let shown = hypr::json("j/monitors").ok().and_then(|m| m.as_array().map(|a| a.iter().any(|m| m["specialWorkspace"]["name"].as_str() == Some(&w.workspace_name)))).unwrap_or(false);
        if !shown {
            if let Err(e) = hypr::toggle_special(name) { eprintln!("waybar-notch: {e}"); }
            revealed = true;
        }
    }
    if w.focus_order == 0 && !revealed && special.is_none() { return; }
    if let Err(e) = hypr::focus_window(&w.address) { eprintln!("waybar-notch: {e}"); }
}

fn focus_group(key: String, address: Option<String>, rules: Vec<(String, Regex)>, pin: Option<PinnedApp>) {
    // a failed query never launches (it would start a duplicate of a running app)
    let Some(wins) = hypr::json("j/clients").ok().and_then(|c| parse_windows(&c)) else { return };
    match select_target(&wins, &key, &rules, address.as_deref()) {
        Some(w) => focus_checked(&w),
        None => { if let Some(p) = pin { launch_pin(&p); } }
    }
}

/// Worker that resolves and decodes icons; results are posted to the UI thread one at a time.
fn start_icons(poster: Poster) -> Sender<IconReq> {
    let (tx, rx) = channel::<IconReq>();
    std::thread::Builder::new().name("app-icons".into()).spawn(move || {
        while let Ok(req) = rx.recv() {
            let image = icon_path(&req).and_then(|p| std::fs::canonicalize(p).ok())
                .and_then(|p| crate::image_loader::load_file(&p, 64).ok());
            let key = req.key;
            poster.post(move |st, _| st.apps.accept_icon(key, image.and_then(|d| d.into_image())));
        }
    }).expect("spawn app-icons");
    tx
}

fn icon_path(req: &IconReq) -> Option<std::path::PathBuf> {
    let idx = desktop_entries::cached();
    let named = req.desktop_id.as_deref().and_then(|id| idx.get(id)).map(|e| e.icon.clone()).filter(|n| !n.is_empty());
    for n in named.into_iter().chain([req.class.clone()]) {
        if n.starts_with('/') { return Some(n.into()); }
        if let Some(p) = crate::icon_theme::find(&n, 32) { return Some(p); }
    }
    None
}

/// Hand the icon worker to `st.apps` (called once by the desktop watcher).
pub fn install_icon_worker(poster: &Poster) {
    let tx = start_icons(poster.clone());
    poster.post(move |st, _| { st.apps.icon_tx = Some(tx); st.apps.rebuild(); });
}

impl Apps {
    pub fn set_order(&mut self, order: &[String]) {
        if self.order != order { self.order = order.to_vec(); self.rebuild(); }
    }

    pub fn set_pins(&mut self, pins: &[PinnedApp]) {
        if self.pins != pins { self.pins = pins.to_vec(); self.rebuild(); }
    }

    pub fn set_open(&mut self, open: Vec<OpenApp>) {
        if self.open != open { self.open = open; self.rebuild(); }
    }

    pub fn set_buttons(&mut self, buttons: Vec<AppButton>) {
        self.buttons = buttons;
        self.rebuild();
    }

    pub fn button(&self, id: &str) -> Option<&AppButton> { self.buttons.iter().find(|b| b.id == id) }

    pub fn shortcut(&self, key: &str) -> Option<&Shortcut> { self.shortcuts.iter().find(|s| s.key == key) }

    /// A window is open, or a configured app reports a running status.
    pub fn running(&self, s: &Shortcut) -> bool {
        s.windows > 0 || s.configured_id.as_deref().and_then(|id| self.button(id)).is_some_and(|b| self.shown(b).0 != AppState::Idle)
    }

    /// Recompute `shortcuts` and request icons that are not cached yet. Cheap; call after any input changed.
    pub fn rebuild(&mut self) {
        let rules = rules(&self.buttons);
        let mut out: Vec<Shortcut> = Vec::new();
        for b in &self.buttons {
            let g = self.open.iter().find(|o| o.configured_id.as_deref() == Some(b.id.as_str()));
            out.push(Shortcut { key: format!("cfg:{}", b.id), name: b.name.clone(), configured_id: Some(b.id.clone()), pin: None, desktop_id: None,
                icon_key: String::new(), class: String::new(), windows: g.map_or(0, |g| g.windows.len()), active: g.is_some_and(|g| g.active) });
        }
        for p in &self.pins {
            let key = format!("app:{}", p.class);
            if out.iter().any(|s| s.key == key) || rules.iter().any(|(_, re)| re.is_match(&p.class)) { continue; }
            let g = self.open.iter().find(|o| o.key == key);
            let name = g.map(|g| g.name.clone())
                .or_else(|| self.catalog.iter().find(|e| e.id == p.desktop_id).map(|e| e.name.clone()))
                .unwrap_or_else(|| p.desktop_id.trim_end_matches(".desktop").to_string());
            out.push(Shortcut { key, name, configured_id: None, pin: Some(p.clone()), desktop_id: Some(p.desktop_id.clone()),
                icon_key: p.desktop_id.clone(), class: p.class.clone(), windows: g.map_or(0, |g| g.windows.len()), active: g.is_some_and(|g| g.active) });
        }
        let mut rest: Vec<&OpenApp> = self.open.iter().filter(|o| o.configured_id.is_none() && !out.iter().any(|s| s.key == o.key)).collect();
        rest.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.key.cmp(&b.key)));
        for o in rest {
            let class = o.key.strip_prefix("app:").unwrap_or(&o.key).to_string();
            let icon_key = o.desktop_id.clone().unwrap_or_else(|| format!("class:{class}"));
            out.push(Shortcut { key: o.key.clone(), name: o.name.clone(), configured_id: None, pin: None, desktop_id: o.desktop_id.clone(), icon_key, class, windows: o.windows.len(), active: o.active });
        }
        // the saved arrangement wins; apps it does not list keep their natural order after it
        if !self.order.is_empty() { out.sort_by_key(|s| self.order.iter().position(|k| *k == s.key).unwrap_or(usize::MAX)); }
        if out != self.shortcuts { self.shortcuts = out; }
        // icons: drop what is no longer shown, ask for what is missing (once)
        let wanted: HashSet<&str> = self.shortcuts.iter().filter(|s| !s.icon_key.is_empty()).map(|s| s.icon_key.as_str()).collect();
        self.icons.retain(|k, _| wanted.contains(k.as_str()));
        self.icon_pending.retain(|k| wanted.contains(k.as_str()));
        let Some(tx) = &self.icon_tx else { return };
        for s in self.shortcuts.iter().filter(|s| !s.icon_key.is_empty()) {
            if self.icons.contains_key(&s.icon_key) || !self.icon_pending.insert(s.icon_key.clone()) { continue; }
            let _ = tx.send(IconReq { key: s.icon_key.clone(), desktop_id: s.desktop_id.clone(), class: s.class.clone() });
        }
    }

    /// An icon finished decoding; results for keys that left the shortcut row are dropped.
    pub fn accept_icon(&mut self, key: String, img: Option<Image>) {
        self.icon_pending.remove(&key);
        if self.shortcuts.iter().any(|s| s.icon_key == key) { self.icons.insert(key, img); }
    }

    /// Click on a shortcut. Configured apps keep their exact behaviour; others focus their most recent window
    /// (or launch the pinned desktop entry when nothing is open). Secondary clicks of other apps do nothing here.
    pub fn activate_shortcut(&self, key: &str, secondary: bool, notch_output: Option<&str>) {
        let Some(s) = self.shortcut(key) else { return };
        if let Some(b) = s.configured_id.as_deref().and_then(|id| self.button(id)) {
            self.activate(b, secondary, notch_output);
            return;
        }
        if secondary || s.configured_id.is_some() { return; }
        let (key, pin, rules) = (s.key.clone(), s.pin.clone(), rules(&self.buttons));
        std::thread::spawn(move || focus_group(key, None, rules, pin));
    }

    /// Focus one specific window of the group `key` (Applications page rows).
    pub fn activate_window(&self, key: &str, address: &str) {
        let (key, address, rules) = (key.to_string(), address.to_string(), rules(&self.buttons));
        std::thread::spawn(move || focus_group(key, Some(address), rules, None));
    }

    /// State and badge to draw for `b` (the plugin's status replaces the window-based one for WhatsApp).
    pub fn shown<'a>(&'a self, b: &'a AppButton) -> (AppState, Option<&'a str>) {
        match (&self.wa, self.wa_plugin && b.id == "whatsapp") {
            (Some(w), true) => (w.state, w.badge.as_deref()),
            _ => (b.state, None),
        }
    }

    /// Run the click of `b`. Workers do the blocking work.
    /// With the WhatsApp plugin the left click opens its page on `notch_output` (the connector the notch occupies)
    /// and the right click keeps the web-app toggle.
    pub fn activate(&self, b: &AppButton, secondary: bool, notch_output: Option<&str>) {
        let panel = self.wa_plugin && b.id == "whatsapp";
        if panel && !secondary {
            // the plugin's QML adapter connects to the notch socket and asks for the page itself
            let Some(args) = whatsapp_open_args(notch_output) else {
                run_detached("notify-send", &["--app-name=Waybar", "-u", "normal", "WhatsApp unavailable", "The notch has not reported its output yet"]);
                return;
            };
            let helper = format!("{}/.config/waybar/scripts/waybar-plugin-widgets", std::env::var("HOME").unwrap_or_default());
            run_detached(&helper, &args);
            return;
        }
        if secondary && !panel { return; }
        let b = b.clone();
        std::thread::spawn(move || {
            if b.background { toggle_background(&b.id, &b.class, &b.launch) } else { focus_or_launch(&b.class, &b.launch) }
        });
    }
}

/// Arguments of the helper call that opens the WhatsApp page on the notch's connector; `None` without a connector
/// (the output is never guessed from the cursor or the focus).
fn whatsapp_open_args(notch_output: Option<&str>) -> Option<[&str; 4]> {
    let output = notch_output.filter(|o| !o.is_empty())?;
    Some(["open", "whatsapp", "--output", output])
}

/// Run `prog` detached from the UI thread (stdio discarded, reaped by a worker).
pub fn run_detached(prog: &str, args: &[&str]) {
    let (prog, args) = (prog.to_string(), args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
    std::thread::spawn(move || {
        if let Ok(mut c) = Command::new(&prog).args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            let _ = c.wait();
        }
    });
}

/// Start `argv` in its own systemd scope so the app never lives in this process's cgroup (a notch restart must
/// not kill it).
fn launch(argv: &[String]) {
    let mut cmd = Command::new("setsid");
    cmd.arg("-f");
    if on_path("uwsm-app") { cmd.args(["uwsm-app", "--"]); } else { cmd.args(["systemd-run", "--user", "--scope", "--quiet", "--collect", "--"]); }
    cmd.args(argv).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Ok(mut c) = cmd.spawn() { let _ = c.wait(); }
}

/// First client whose class matches `class` (case-insensitive regex).
fn find_client(clients: &Value, class: &str) -> Option<Value> {
    let re = RegexBuilder::new(class).case_insensitive(true).build().ok()?;
    clients.as_array()?.iter().find(|c| re.is_match(c["class"].as_str().unwrap_or(""))).cloned()
}

fn window(class: &str) -> Option<Value> {
    find_client(&hypr::json("j/clients").ok()?, class)
}

/// Focus the app's window, else start it.
fn focus_or_launch(class: &str, argv: &[String]) {
    match window(class).and_then(|w| w["address"].as_str().map(str::to_string)) {
        Some(addr) => { if let Err(e) = hypr::focus_window(&addr) { eprintln!("waybar-notch: {e}"); } }
        None => launch(argv),
    }
}

/// Show/hide a background app: start it if absent, hide it when it is on its special workspace, else focus it.
fn toggle_background(id: &str, class: &str, argv: &[String]) {
    let mut w = window(class);
    if w.is_none() {
        launch(argv);
        for _ in 0..100 {
            if window(class).is_some() { break; }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        w = window(class);
    }
    let Some(w) = w else { return };
    let res = if w["workspace"]["name"].as_str() == Some(&format!("special:{id}")) {
        hypr::toggle_special(id)
    } else {
        match w["address"].as_str() { Some(a) => hypr::focus_window(a), None => return }
    };
    if let Err(e) = res { eprintln!("waybar-notch: {e}"); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn btn(id: &str) -> AppButton {
        AppButton { id: id.into(), name: id.into(), glyph: "x".into(), class: "^x$".into(), background: false, launch: vec![], state: AppState::Idle }
    }

    fn client(addr: &str, class: &str, ws: i64, wsname: &str, focus: i64) -> Value {
        json!({"address": addr, "class": class, "initialClass": class, "title": format!("t {addr}"), "mapped": true, "hidden": false,
               "workspace": {"id": ws, "name": wsname}, "focusHistoryID": focus})
    }

    fn open_of(clients: Value, buttons: &[AppButton]) -> Vec<OpenApp> {
        group_clients(&clients, buttons, &DesktopIndex::default()).unwrap()
    }

    #[test]
    fn windows_group_across_workspaces_and_order_by_focus() {
        let g = open_of(json!([
            client("0x2", "Brave-browser", 3, "3", 1), client("0x1", "brave-browser", 1, "1", 0),
            client("0x3", "Code", 2, "2", -1), client("0x4", "kitty", 2, "2", 5),
            {"address": "bad", "class": "x"}, {"address": "0x9", "class": "", "initialClass": ""},
            {"address": "0x8", "class": "gone", "mapped": false}, {"address": "0x7", "class": "gone", "hidden": true},
        ]), &[]);
        assert_eq!(g.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(), ["app:brave-browser", "app:code", "app:kitty"]);
        let b = &g[0];
        assert_eq!((b.windows.len(), b.active, b.windows[0].address.as_str(), b.name.as_str()), (2, true, "0x1", "Brave-browser"));
        assert!(!g[1].active, "unknown focus order is never active");
    }

    #[test]
    fn configured_special_workspace_window_is_one_slot_and_regex_beats_class() {
        let mut wa = btn("whatsapp");
        wa.class = "web\\.whatsapp\\.com".into();
        let g = open_of(json!([client("0x1", "brave-web.whatsapp.com__-Default", -99, "special:whatsapp", 0)]), &[wa.clone()]);
        assert_eq!((g.len(), g[0].key.as_str(), g[0].configured_id.as_deref()), (1, "cfg:whatsapp", Some("whatsapp")));
        let mut apps = Apps { buttons: vec![wa], pins: vec![PinnedApp::new("wa.desktop", "brave-web.whatsapp.com__-default").unwrap()], ..Default::default() };
        apps.set_open(g);
        assert_eq!(apps.shortcuts.len(), 1, "configured app, its pin and its windows share one slot");
        assert_eq!(apps.shortcuts[0].windows, 1);
    }

    #[test]
    fn non_array_reply_is_a_failure_and_empty_array_clears() {
        assert!(group_clients(&json!({"error": 1}), &[], &DesktopIndex::default()).is_none());
        let mut apps = Apps::default();
        apps.set_open(open_of(json!([client("0x1", "kitty", 1, "1", 0)]), &[]));
        assert_eq!(apps.shortcuts.len(), 1);
        apps.set_open(open_of(json!([]), &[]));
        assert!(apps.shortcuts.is_empty());
    }

    #[test]
    fn pinned_apps_stay_after_close_and_order_is_stable() {
        let pins = vec![PinnedApp::new("zed.desktop", "zed").unwrap(), PinnedApp::new("a.desktop", "alpha").unwrap()];
        let mut apps = Apps { pins, ..Default::default() };
        apps.set_open(open_of(json!([client("0x1", "beta", 1, "1", 0), client("0x2", "alpha", 1, "1", 1), client("0x3", "aardvark", 1, "1", 2)]), &[]));
        let names: Vec<_> = apps.shortcuts.iter().map(|s| (s.key.as_str(), s.windows)).collect();
        assert_eq!(names, [("app:zed", 0), ("app:alpha", 1), ("app:aardvark", 1), ("app:beta", 1)]);
        // focus change never reorders slots
        apps.set_open(open_of(json!([client("0x1", "beta", 1, "1", 2), client("0x2", "alpha", 1, "1", 1), client("0x3", "aardvark", 1, "1", 0)]), &[]));
        let keys: Vec<_> = apps.shortcuts.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["app:zed", "app:alpha", "app:aardvark", "app:beta"]);
        apps.set_open(Vec::new());
        assert_eq!(apps.shortcuts.len(), 2, "pins survive closing every window");
    }

    #[test]
    fn stale_address_is_never_focused_and_falls_back_to_a_current_member() {
        let wins = parse_windows(&json!([client("0x1", "kitty", 1, "1", 0), client("0x2", "kitty", 2, "2", 1), client("0x3", "code", 1, "1", 2)])).unwrap();
        let sel = |addr| select_target(&wins, "app:kitty", &[], addr).map(|w| w.address);
        assert_eq!(sel(Some("0x2")).as_deref(), Some("0x2"));
        assert_eq!(sel(Some("0x3")).as_deref(), Some("0x1"), "address now belongs to another app");
        assert_eq!(sel(Some("0xdead")).as_deref(), Some("0x1"));
        assert_eq!(sel(None).as_deref(), Some("0x1"));
        assert!(select_target(&wins, "app:none", &[], Some("0x1")).is_none());
    }

    #[test]
    fn pin_launch_goes_through_gtk_launch_argv_not_a_shell() {
        assert_eq!(pin_launch_argv("a b;rm.desktop"), ["gtk-launch", "a b;rm.desktop"]);
    }

    #[test]
    fn first_matching_client_wins_case_insensitively() {
        let clients = json!([{"class": "kitty", "address": "0x1"}, {"class": "Vesktop", "address": "0x2"}, {"class": "discord", "address": "0x3"}]);
        assert_eq!(find_client(&clients, "^(discord|vesktop)$").unwrap()["address"], "0x2");
        assert!(find_client(&clients, "^steam$").is_none());
        assert!(find_client(&json!({"error": 1}), ".*").is_none());
        assert!(find_client(&clients, "(").is_none(), "an invalid pattern matches nothing");
        let wa = json!([{"class": "brave-web.whatsapp.com__-Default"}]);
        assert!(find_client(&wa, "web\\.whatsapp\\.com").is_some());
    }

    #[test]
    fn whatsapp_plugin_status_overrides_only_whatsapp() {
        let mut apps = Apps { buttons: vec![btn("whatsapp"), btn("steam")], ..Default::default() };
        apps.wa = Some(WaView { state: AppState::Unread, badge: Some("4".into()) });
        let (wa, steam) = (apps.buttons[0].clone(), apps.buttons[1].clone());
        assert_eq!(apps.shown(&wa), (AppState::Idle, None), "plugin disabled: window status");
        apps.wa_plugin = true;
        assert_eq!(apps.shown(&wa), (AppState::Unread, Some("4")));
        assert_eq!(apps.shown(&steam), (AppState::Idle, None));
        apps.wa = None;
        assert_eq!(apps.shown(&wa), (AppState::Idle, None), "no daemon report yet");
    }

    #[test]
    fn secondary_click_on_ordinary_apps_is_inert() {
        let apps = Apps { wa_plugin: true, ..Default::default() };
        apps.activate(&btn("steam"), true, Some("DP-2"));
    }

    #[test]
    fn whatsapp_open_names_the_notch_output_and_never_guesses() {
        assert_eq!(whatsapp_open_args(Some("DP-2")), Some(["open", "whatsapp", "--output", "DP-2"]));
        assert_eq!(whatsapp_open_args(Some("")), None);
        assert_eq!(whatsapp_open_args(None), None);
    }
}
