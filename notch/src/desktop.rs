//! Compositor state shared by every surface: the Hyprland monitor/workspace/fullscreen snapshot (`Desk`), the
//! output-selection policy, the notch window geometry derived from a monitor's logical size and the watcher that
//! keeps `State::desk` and `State::apps` current (`start`).
use crate::config::{MonitorMode, NotchSettings};
use crate::hypr;
use crate::wayland::Poster;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

// ------------------------------------------------------------------ snapshot

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub id: i64,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub logical_width: f64,
    pub logical_height: f64,
    pub scale: f64,
    pub focused: bool,
    pub active_workspace: i64,
    pub special_workspace: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DesktopSnapshot {
    pub monitors: Vec<MonitorInfo>,
    /// Connectors whose visible workspace hosts a true fullscreen window (maximized does not count).
    pub fullscreen_outputs: HashSet<String>,
}

impl DesktopSnapshot {
    pub fn monitor(&self, name: &str) -> Option<&MonitorInfo> {
        self.monitors.iter().find(|m| m.name == name)
    }
}

/// Pure: build a snapshot from `j/monitors` and `j/clients` documents.
pub fn parse_snapshot(monitors: &Value, clients: &Value) -> DesktopSnapshot {
    let mut mons: Vec<MonitorInfo> = Vec::new();
    for m in monitors.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if m["disabled"].as_bool().unwrap_or(false) { continue; }
        let (Some(name), Some(w), Some(h)) = (m["name"].as_str(), m["width"].as_f64(), m["height"].as_f64()) else { continue };
        let scale = m["scale"].as_f64().filter(|s| s.is_finite() && *s > 0.0).unwrap_or(1.0);
        let (mut lw, mut lh) = (w / scale, h / scale);
        if m["transform"].as_i64().unwrap_or(0) % 2 != 0 { std::mem::swap(&mut lw, &mut lh); }
        mons.push(MonitorInfo {
            id: m["id"].as_i64().unwrap_or(-1),
            name: name.to_string(),
            x: m["x"].as_i64().unwrap_or(0) as i32,
            y: m["y"].as_i64().unwrap_or(0) as i32,
            logical_width: lw,
            logical_height: lh,
            scale,
            focused: m["focused"].as_bool().unwrap_or(false),
            active_workspace: m["activeWorkspace"]["id"].as_i64().unwrap_or(0),
            special_workspace: m["specialWorkspace"]["id"].as_i64().unwrap_or(0),
        });
    }
    let mut fs = HashSet::new();
    for c in clients.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        if !c["mapped"].as_bool().unwrap_or(true) || c["hidden"].as_bool().unwrap_or(false) { continue; }
        if !c["visible"].as_bool().unwrap_or(true) { continue; }
        let true_fullscreen = match &c["fullscreen"] {
            Value::Bool(b) => *b,
            v => v.as_i64() == Some(2), // 1 = maximized, 2 = real fullscreen
        };
        if !true_fullscreen { continue; }
        let (Some(mon), Some(ws)) = (c["monitor"].as_i64(), c["workspace"]["id"].as_i64()) else { continue };
        if let Some(m) = mons.iter().find(|m| m.id == mon) {
            let shown = if m.special_workspace != 0 { m.special_workspace } else { m.active_workspace };
            if ws == shown { fs.insert(m.name.clone()); }
        }
    }
    mons.sort_by(|a, b| a.name.cmp(&b.name));
    DesktopSnapshot { monitors: mons, fullscreen_outputs: fs }
}

/// One Hyprland workspace (`j/workspaces`).
#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    /// Connector the workspace lives on.
    pub monitor: String,
    pub windows: u32,
}

/// Pure: `j/workspaces` document. Entries without an id or monitor are dropped.
pub fn parse_workspaces(doc: &Value) -> Vec<Workspace> {
    let mut out: Vec<Workspace> = doc.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().filter_map(|w| {
        let id = w["id"].as_i64()?;
        let monitor = w["monitor"].as_str()?.to_string();
        Some(Workspace { id, name: w["name"].as_str().unwrap_or("").to_string(), monitor, windows: w["windows"].as_u64().unwrap_or(0) as u32 })
    }).collect();
    out.sort_by_key(|w| w.id);
    out
}

/// A workspace button of one output's pill.
#[derive(Clone, Debug, PartialEq)]
pub struct WsButton {
    pub id: i64,
    pub active: bool,
}

/// What the compositor reports; replaced wholesale by the watcher whenever anything changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desk {
    pub snap: DesktopSnapshot,
    /// Connector of the focused monitor.
    pub focused: Option<String>,
    pub workspaces: Vec<Workspace>,
    /// A screenshot freeze/selection surface (`hyprpicker`, `selection`) is on screen.
    pub capture_active: bool,
}

impl Desk {
    pub fn is_fullscreen(&self, output: &str) -> bool { self.snap.fullscreen_outputs.contains(output) }

    /// Numbered workspaces of `output` (special workspaces have negative ids) by id; the visible one is `active`
    /// and is listed even while empty.
    pub fn workspace_buttons(&self, output: &str) -> Vec<WsButton> {
        let active = self.snap.monitor(output).map(|m| m.active_workspace);
        let mut v: Vec<WsButton> = self.workspaces.iter()
            .filter(|w| w.monitor == output && w.id > 0 && (w.windows > 0 || Some(w.id) == active))
            .map(|w| WsButton { id: w.id, active: Some(w.id) == active })
            .collect();
        if let Some(a) = active.filter(|a| *a > 0 && !v.iter().any(|b| b.id == *a)) {
            v.push(WsButton { id: a, active: true });
            v.sort_by_key(|b| b.id);
        }
        v
    }
}

fn focused_name(snap: &DesktopSnapshot) -> Option<String> {
    snap.monitors.iter().find(|m| m.focused).map(|m| m.name.clone())
}

/// Blocking (call off the UI thread). `clients` is returned too so the app tracker can reuse it.
pub fn query_desk() -> Result<(Desk, Value), String> {
    let clients = hypr::json("j/clients")?;
    let snap = parse_snapshot(&hypr::json("j/monitors")?, &clients);
    let workspaces = parse_workspaces(&hypr::json("j/workspaces")?);
    // a malformed layer dump keeps the screenshot flag off rather than failing the whole snapshot
    let capture_active = hypr::json("j/layers").ok().as_ref().and_then(capture_active).unwrap_or(false);
    Ok((Desk { focused: focused_name(&snap), snap, workspaces, capture_active }, clients))
}

/// Which output hosts the notch and the side pills. Precedence: a live interaction pin, a connected fixed output,
/// the focused output, the previous output while still connected, the first output by connector name.
pub fn select_output(snap: &DesktopSnapshot, prefs: &NotchSettings, pin: Option<&str>, previous: Option<&str>) -> Option<String> {
    let has = |n: &str| snap.monitors.iter().any(|m| m.name == n);
    if let Some(p) = pin.filter(|p| has(p)) { return Some(p.to_string()); }
    if prefs.monitor.mode == MonitorMode::Fixed {
        if let Some(o) = prefs.monitor.output.as_deref().filter(|o| has(o)) { return Some(o.to_string()); }
    }
    if let Some(m) = snap.monitors.iter().find(|m| m.focused) { return Some(m.name.clone()); }
    if let Some(p) = previous.filter(|p| has(p)) { return Some(p.to_string()); }
    snap.monitors.iter().map(|m| &m.name).min().cloned()
}

// ------------------------------------------------------------------ event worker

/// Some(true) when a screenshot freeze/selection surface exists in a `j/layers` dump; None when malformed.
fn capture_active(layers: &Value) -> Option<bool> {
    let mut found = false;
    for out in layers.as_object()?.values() {
        for level in out.as_object()?.get("levels")?.as_object()?.values() {
            for l in level.as_array()? {
                let ns = l.as_object()?.get("namespace")?.as_str()?;
                found |= ns == "hyprpicker" || ns == "selection";
            }
        }
    }
    Some(found)
}

fn relevant_event(line: &str) -> bool {
    const EVENTS: [&str; 22] = [
        "focusedmon", "focusedmonv2", "monitoradded", "monitoraddedv2", "monitorremoved", "monitorremovedv2",
        "workspace", "workspacev2", "activespecial", "activespecialv2", "movewindow", "movewindowv2",
        "moveworkspace", "moveworkspacev2", "fullscreen", "activewindowv2", "openwindow", "closewindow",
        "configreloaded", "changefloatingmode", "openlayer", "closelayer",
    ];
    line.split(">>").next().is_some_and(|name| EVENTS.contains(&name))
}

/// Workspaces also change on create/destroy, which the desktop list above does not cover.
fn watched_event(line: &str) -> bool {
    relevant_event(line) || crate::app_status::title_event(line)
        || line.split(">>").next().is_some_and(|n| matches!(n, "createworkspace" | "createworkspacev2" | "destroyworkspace" | "destroyworkspacev2" | "renameworkspace"))
}

fn layer_event(line: &str) -> bool { line.starts_with("openlayer>>") || line.starts_with("closelayer>>") }

/// Title-only events (`windowtitle`) concern the app status, everything else the whole desktop snapshot.
fn needs_desk(line: &str) -> bool { relevant_event(line) || !crate::app_status::title_event(line) }

/// Publishes the compositor and app state to the UI thread whenever it changes. Subscribes to the event socket
/// before the first query so no startup event is lost; a failed query keeps the last good state.
pub fn start(poster: &Poster) {
    let poster = poster.clone();
    std::thread::Builder::new().name("desktop-watch".into()).spawn(move || watch(poster)).expect("spawn desktop-watch");
}

struct Publisher {
    poster: Poster,
    tracker: Option<crate::app_status::Tracker>,
    last_desk: Option<Desk>,
    last_buttons: Option<Vec<crate::apps::AppButton>>,
}

impl Publisher {
    fn apps(&mut self) {
        let Some(t) = &self.tracker else { return };
        let buttons = t.buttons();
        if self.last_buttons.as_ref() != Some(&buttons) {
            self.last_buttons = Some(buttons.clone());
            self.poster.post(move |st, _| st.apps.buttons = buttons);
        }
    }

    /// Everything: monitors, workspaces, fullscreen, screenshot layers, client-based app status.
    fn all(&mut self) {
        let Ok((desk, clients)) = query_desk() else { return };
        if let Some(t) = &mut self.tracker { let _ = t.update_clients(clients); }
        if self.last_desk.as_ref() != Some(&desk) {
            self.last_desk = Some(desk.clone());
            self.poster.post(move |st, _| st.desk = desk);
        }
        self.apps();
    }

    /// Window titles/classes only: app status without the monitor queries.
    fn clients(&mut self) {
        let Some(t) = &mut self.tracker else { return };
        if let Ok(clients) = hypr::json("j/clients") { let _ = t.update_clients(clients); }
        self.apps();
    }

    fn tick(&mut self) {
        if let Some(t) = &mut self.tracker { t.refresh(); }
        self.all();
    }
}

fn watch(poster: Poster) {
    let tracker = crate::config::read_document().map_err(|e| e.to_string()).and_then(|d| crate::app_status::Tracker::new(&d))
        .map_err(|e| eprintln!("waybar-notch: app buttons disabled: {e}")).ok();
    let wa = tracker.is_some() && crate::app_status::whatsapp_plugin_enabled();
    if wa {
        poster.post(|st, _| st.apps.wa_plugin = true);
        crate::app_status::watch_whatsapp(poster.clone());
    }
    let mut p = Publisher { poster, tracker, last_desk: None, last_buttons: None };
    p.apps();
    let events = hypr::subscribe(watched_event);
    loop {
        match events.recv_timeout(Duration::from_secs(2)) {
            Ok(hypr::Ev::Connected) => p.tick(),
            Ok(hypr::Ev::Line(first)) => {
                if layer_event(&first) { p.all(); }
                // legacy + v2 events arrive in bursts: wait for the burst to end, then ask once
                let mut desk = needs_desk(&first);
                while let Ok(ev) = events.recv_timeout(Duration::from_millis(50)) {
                    if let hypr::Ev::Line(l) = ev { desk |= needs_desk(&l); }
                }
                if desk { p.all(); } else { p.clients(); }
            }
            // scale/DPMS changes emit no usable event; the same tick scans processes for apps without a window
            Err(RecvTimeoutError::Timeout) => p.tick(),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LayoutSettings, MonitorSettings};
    use serde_json::json;

    fn mon(id: i64, name: &str, w: i64, h: i64, scale: f64, transform: i64, focused: bool, ws: i64, special: i64) -> Value {
        json!({"id": id, "name": name, "width": w, "height": h, "x": 0, "y": 0, "scale": scale, "transform": transform,
               "focused": focused, "disabled": false, "activeWorkspace": {"id": ws}, "specialWorkspace": {"id": special}})
    }
    fn client(monitor: i64, ws: i64, fullscreen: i64) -> Value {
        json!({"monitor": monitor, "workspace": {"id": ws}, "mapped": true, "hidden": false, "visible": true, "fullscreen": fullscreen})
    }
    fn two(focus_b: bool) -> DesktopSnapshot {
        parse_snapshot(
            &json!([mon(0, "A", 1920, 1200, 1.6, 0, !focus_b, 1, 0), mon(1, "B", 1920, 1080, 1.6, 0, focus_b, 2, 0)]),
            &json!([]),
        )
    }
    fn prefs() -> NotchSettings { NotchSettings::default() }

    #[test]
    fn focus_change_during_interaction_waits_for_release() {
        let b_focused = two(true);
        assert_eq!(select_output(&b_focused, &prefs(), Some("A"), Some("A")).as_deref(), Some("A"));
        // the pin is released: the latest snapshot already holds B as focused, so B wins with no new event
        assert_eq!(select_output(&b_focused, &prefs(), None, Some("A")).as_deref(), Some("B"));
    }

    #[test]
    fn removed_pin_falls_back_immediately() {
        let only_b = parse_snapshot(&json!([mon(1, "B", 1920, 1080, 1.6, 0, true, 2, 0)]), &json!([]));
        assert_eq!(select_output(&only_b, &prefs(), Some("A"), Some("A")).as_deref(), Some("B"));
        let empty = parse_snapshot(&json!([]), &json!([]));
        assert_eq!(select_output(&empty, &prefs(), Some("A"), Some("A")), None);
    }

    #[test]
    fn fixed_output_falls_back_when_missing_and_returns_when_connected() {
        let mut p = prefs();
        p.monitor = MonitorSettings { mode: MonitorMode::Fixed, output: Some("HDMI-A-9".into()) };
        let s = two(false);
        assert_eq!(select_output(&s, &p, None, None).as_deref(), Some("A"));
        p.monitor.output = Some("B".into());
        assert_eq!(select_output(&s, &p, None, Some("A")).as_deref(), Some("B"));
        // an open interaction on A still wins over the fixed output
        assert_eq!(select_output(&s, &p, Some("A"), Some("A")).as_deref(), Some("A"));
    }

    #[test]
    fn unfocused_snapshot_keeps_previous_then_first_by_name() {
        let s = parse_snapshot(&json!([mon(1, "B", 100, 100, 1.0, 0, false, 1, 0), mon(0, "A", 100, 100, 1.0, 0, false, 2, 0)]), &json!([]));
        assert_eq!(select_output(&s, &prefs(), None, Some("B")).as_deref(), Some("B"));
        assert_eq!(select_output(&s, &prefs(), None, Some("gone")).as_deref(), Some("A"));
    }

    #[test]
    fn disabled_outputs_are_skipped() {
        let mut m = mon(0, "A", 1920, 1080, 1.0, 0, true, 1, 0);
        m["disabled"] = json!(true);
        let s = parse_snapshot(&json!([m, mon(1, "B", 1920, 1080, 1.0, 0, false, 2, 0)]), &json!([]));
        assert_eq!(select_output(&s, &prefs(), None, None).as_deref(), Some("B"));
    }

    #[test]
    fn fullscreen_is_per_output_and_maximized_does_not_count() {
        let mons = json!([mon(0, "A", 1920, 1200, 1.0, 0, true, 1, 0), mon(1, "B", 1920, 1080, 1.0, 0, false, 2, 0)]);
        // true fullscreen on B only
        let s = parse_snapshot(&mons, &json!([client(1, 2, 2), client(0, 1, 0)]));
        assert!(s.fullscreen_outputs.contains("B") && !s.fullscreen_outputs.contains("A"));
        // maximized (1) never hides
        let s = parse_snapshot(&mons, &json!([client(0, 1, 1), client(1, 2, 1)]));
        assert!(s.fullscreen_outputs.is_empty());
        // fullscreen window on a workspace that is not the visible one
        let s = parse_snapshot(&mons, &json!([client(1, 5, 2)]));
        assert!(s.fullscreen_outputs.is_empty());
        // an open special workspace covers the active workspace's fullscreen window
        let mons = json!([mon(0, "A", 1920, 1200, 1.0, 0, true, 1, -98)]);
        let s = parse_snapshot(&mons, &json!([client(0, 1, 2)]));
        assert!(s.fullscreen_outputs.is_empty());
        let s = parse_snapshot(&mons, &json!([client(0, -98, 2)]));
        assert!(s.fullscreen_outputs.contains("A"));
        // unmapped / hidden clients are ignored; bool-true from older Hyprland counts
        let mons = json!([mon(0, "A", 1920, 1200, 1.0, 0, true, 1, 0)]);
        let mut c = client(0, 1, 2);
        c["hidden"] = json!(true);
        assert!(parse_snapshot(&mons, &json!([c])).fullscreen_outputs.is_empty());
        let mut c = client(0, 1, 0);
        c["fullscreen"] = json!(true);
        assert!(parse_snapshot(&mons, &json!([c])).fullscreen_outputs.contains("A"));
    }

    #[test]
    fn rotated_fractional_monitor_logical_size() {
        let s = parse_snapshot(&json!([mon(0, "A", 1920, 1080, 1.5, 1, true, 1, 0)]), &json!([]));
        let m = &s.monitors[0];
        assert_eq!((m.logical_width, m.logical_height), (720.0, 1280.0));
    }

    #[test]
    fn same_connector_scale_change_changes_snapshot() {
        let a = parse_snapshot(&json!([mon(0, "A", 1920, 1200, 1.6, 0, true, 1, 0)]), &json!([]));
        let b = parse_snapshot(&json!([mon(0, "A", 1920, 1200, 2.0, 0, true, 1, 0)]), &json!([]));
        assert_ne!(a, b);
    }

    #[test]
    fn capture_detection() {
        let l = |ns: &[&str]| ns.iter().map(|n| json!({"namespace": n})).collect::<Vec<_>>();
        let dump = |a: &[&str], b: &[&str]| json!({
            "DP-1": {"levels": {"0": l(&["waybar"]), "3": l(a)}},
            "DP-2": {"levels": {"0": [], "2": l(b)}},
        });
        assert_eq!(capture_active(&json!({})), Some(false));
        assert_eq!(capture_active(&dump(&["waybar-notch"], &["hyprpicker-extra"])), Some(false));
        assert_eq!(capture_active(&dump(&["hyprpicker"], &["selection"])), Some(true));
        assert_eq!(capture_active(&dump(&[], &["selection"])), Some(true));
        assert_eq!(capture_active(&dump(&[], &["hyprpicker"])), Some(true));
        assert_eq!(capture_active(&dump(&[], &[])), Some(false));
        assert_eq!(capture_active(&json!({"DP-1": {"levels": null}})), None);
        assert_eq!(capture_active(&json!({"DP-1": {"levels": {"0": [{}]}}})), None);
        assert_eq!(capture_active(&json!({"DP-1": {"levels": {"0": [1]}}})), None);
        assert_eq!(capture_active(&json!({"DP-1": {"levels": {"0": null}}})), None);
    }

    #[test]
    fn only_relevant_events_trigger_queries() {
        assert!(relevant_event("focusedmonv2>>eDP-1,1"));
        assert!(relevant_event("fullscreen>>1"));
        assert!(!relevant_event("windowtitle>>abc"));
        assert!(!relevant_event("fullscreenfoo>>x"));
        // the watcher also wakes for app windows and workspace lifecycle
        assert!(watched_event("windowtitlev2>>0x1,x"));
        assert!(watched_event("createworkspacev2>>3,3"));
        assert!(watched_event("destroyworkspace>>3"));
        assert!(!watched_event("activelayout>>kb,us"));
    }

    fn ws(id: i64, monitor: &str, windows: u64) -> Value {
        json!({"id": id, "name": id.to_string(), "monitor": monitor, "windows": windows})
    }

    #[test]
    fn workspace_buttons_follow_the_output_and_mark_the_visible_one() {
        let snap = parse_snapshot(
            &json!([mon(0, "A", 1920, 1200, 1.0, 0, true, 3, 0), mon(1, "B", 1920, 1080, 1.0, 0, false, 9, 0)]),
            &json!([]),
        );
        let workspaces = parse_workspaces(&json!([
            ws(9, "B", 0), ws(2, "A", 1), ws(1, "A", 0), ws(3, "A", 0), ws(-98, "A", 1), ws(7, "B", 2),
            {"id": "x"}, {"id": 4}
        ]));
        assert_eq!(workspaces.iter().map(|w| w.id).collect::<Vec<_>>(), [-98, 1, 2, 3, 7, 9], "sorted, malformed dropped");
        let desk = Desk { snap, workspaces, ..Default::default() };
        let a = desk.workspace_buttons("A");
        // empty workspace 1 hidden; special workspace hidden; the active (empty) 3 shown
        assert_eq!(a, [WsButton { id: 2, active: false }, WsButton { id: 3, active: true }]);
        assert_eq!(desk.workspace_buttons("B"), [WsButton { id: 7, active: false }, WsButton { id: 9, active: true }]);
        assert!(desk.workspace_buttons("C").is_empty());
    }

    #[test]
    fn active_workspace_missing_from_the_list_is_still_shown() {
        let snap = parse_snapshot(&json!([mon(0, "A", 1920, 1200, 1.0, 0, true, 2, 0)]), &json!([]));
        let desk = Desk { snap, workspaces: parse_workspaces(&json!([ws(5, "A", 1)])), ..Default::default() };
        assert_eq!(desk.workspace_buttons("A"), [WsButton { id: 2, active: true }, WsButton { id: 5, active: false }]);
    }
}
