//! App buttons of the left pill: state slice plus the click behaviour of the former `waybar-app.sh click` /
//! `waybar-bgapp.sh toggle` helpers. Status comes from `app_status` (windows/processes, WhatsApp daemon).
use crate::app_status::on_path;
use crate::hypr;
use regex::RegexBuilder;
use serde_json::Value;
use std::process::{Command, Stdio};
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

#[derive(Default)]
pub struct Apps {
    /// Configured and installed apps in config order.
    pub buttons: Vec<AppButton>,
    /// The WhatsApp plugin is enabled: its daemon drives the icon and the left click opens its notch page.
    pub wa_plugin: bool,
    pub wa: Option<WaView>,
}

impl Apps {
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
