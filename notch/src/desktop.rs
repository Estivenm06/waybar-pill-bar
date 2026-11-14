//! Compositor state shared by every notch surface: Hyprland IPC, the monitor/fullscreen snapshot, the single
//! output-selection policy and the layer-window geometry derived from a monitor's logical size.
use crate::config::{MonitorMode, NotchSettings};
use crate::{runtime_dir, Msg};
use gtk4::{gdk, prelude::*};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// ------------------------------------------------------------------ hyprland sockets

const REQ_TIMEOUT: Duration = Duration::from_millis(800);

/// Connects to the live instance's socket (`.socket.sock` / `.socket2.sock`). `$HYPRLAND_INSTANCE_SIGNATURE` is tried
/// first, then every instance directory newest-first; a directory only counts if its socket accepts a connection
/// (a crashed compositor leaves its directory behind).
pub(crate) fn hypr_open(sock: &str) -> Option<(String, UnixStream)> {
    let rt = runtime_dir();
    let try_sig = |sig: &str| UnixStream::connect(format!("{rt}/hypr/{sig}/{sock}")).ok().map(|s| (sig.to_string(), s));
    if let Some(found) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty()).and_then(|s| try_sig(&s)) {
        return Some(found);
    }
    let mut ents: Vec<_> = std::fs::read_dir(format!("{rt}/hypr")).ok()?.flatten().collect();
    ents.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    ents.iter().find_map(|e| try_sig(&e.file_name().to_string_lossy()))
}

/// Signature of the live Hyprland instance, if one answers.
pub fn hypr_sig() -> Option<String> {
    hypr_open(".socket.sock").map(|(s, _)| s)
}

pub fn hypr_req(cmd: &str) -> Option<String> {
    let (_, mut s) = hypr_open(".socket.sock")?;
    s.set_read_timeout(Some(REQ_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(REQ_TIMEOUT)).ok()?;
    s.write_all(cmd.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

fn hypr_json(cmd: &str) -> Result<Value, String> {
    let text = hypr_req(cmd).ok_or_else(|| format!("hyprland request {cmd} failed"))?;
    serde_json::from_str(&text).map_err(|e| format!("hyprland {cmd}: {e}"))
}

pub fn gdk_monitor_by_name(name: &str) -> Option<gdk::Monitor> {
    let mons = gdk::Display::default()?.monitors();
    (0..mons.n_items())
        .filter_map(|i| mons.item(i).and_then(|o| o.downcast::<gdk::Monitor>().ok()))
        .find(|m| m.connector().as_deref() == Some(name))
}

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

/// Blocking (call off the GTK thread).
pub fn query_snapshot() -> Result<DesktopSnapshot, String> {
    Ok(parse_snapshot(&hypr_json("j/monitors")?, &hypr_json("j/clients")?))
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

// ------------------------------------------------------------------ geometry

/// Room around the expanded card for the open spring.
pub const HEADROOM: f64 = 1.12;
const SCREEN_MARGIN: f64 = 16.0;

/// Notch sizes for one monitor: the user's request capped to what the output can show.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub coll_w: f64,
    pub coll_h: f64,
    pub exp_w: f64,
    pub exp_h_max: f64,
    pub top_margin: i32,
}

impl Geometry {
    pub fn compute(l: &crate::config::LayoutSettings, logical: Option<(f64, f64)>) -> Geometry {
        let (lw, lh) = logical.unwrap_or((1.0e6, 1.0e6));
        let top = l.top_margin as f64;
        let avail_w = (lw - SCREEN_MARGIN).max(1.0);
        let avail_h = (lh - top - SCREEN_MARGIN).max(1.0);
        Geometry {
            coll_w: (l.collapsed_width as f64).min(avail_w),
            coll_h: (l.collapsed_height as f64).min(avail_h),
            exp_w: (l.expanded_width as f64).min(avail_w / HEADROOM),
            exp_h_max: avail_h / HEADROOM,
            top_margin: l.top_margin as i32,
        }
    }

    /// Layer-window size for an expanded card of natural height `th`.
    pub fn host(&self, th: f64) -> (i32, i32) {
        ((self.exp_w * HEADROOM).floor() as i32, (th.min(self.exp_h_max) * HEADROOM).floor() as i32)
    }
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

/// Publishes `Msg::Desktop` whenever the authoritative snapshot changes. Subscribes to the event socket before the
/// initial query so no startup event is lost; a failed query keeps the last good snapshot.
pub fn start_watcher(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        let mut last: Option<DesktopSnapshot> = None;
        let mut last_capture: Option<bool> = None;
        let publish = |last: &mut Option<DesktopSnapshot>| {
            if let Ok(s) = query_snapshot() {
                if last.as_ref() != Some(&s) {
                    *last = Some(s.clone());
                    let _ = tx.try_send(Msg::Desktop(s));
                }
            }
        };
        let publish_capture = |last: &mut Option<bool>| {
            if let Some(c) = hypr_json("j/layers").ok().as_ref().and_then(capture_active) {
                if *last != Some(c) {
                    *last = Some(c);
                    let _ = tx.try_send(Msg::CaptureActive(c));
                }
            }
        };
        loop {
            if let Some((_, stream)) = hypr_open(".socket2.sock") {
                let (etx, erx) = std::sync::mpsc::channel::<String>();
                std::thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        if relevant_event(&line) && etx.send(line).is_err() { break; }
                    }
                });
                publish_capture(&mut last_capture);
                publish(&mut last);
                loop {
                    match erx.recv_timeout(Duration::from_secs(1)) {
                        Ok(first) => {
                            let layer = |l: &str| l.starts_with("openlayer>>") || l.starts_with("closelayer>>");
                            if layer(&first) { publish_capture(&mut last_capture); }
                            // legacy + v2 events arrive in bursts: wait for the burst to end, then ask once
                            while erx.recv_timeout(Duration::from_millis(50)).is_ok() {}
                            publish_capture(&mut last_capture);
                            publish(&mut last);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => { publish_capture(&mut last_capture); publish(&mut last) } // scale/DPMS changes emit no usable event
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
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
    fn same_connector_scale_change_changes_snapshot_and_geometry() {
        let l = LayoutSettings { collapsed_width: 210, collapsed_height: 30, expanded_width: 960, top_margin: 0 };
        let a = parse_snapshot(&json!([mon(0, "A", 1920, 1200, 1.6, 0, true, 1, 0)]), &json!([]));
        let b = parse_snapshot(&json!([mon(0, "A", 1920, 1200, 2.0, 0, true, 1, 0)]), &json!([]));
        assert_ne!(a, b);
        let ga = Geometry::compute(&l, Some((a.monitors[0].logical_width, a.monitors[0].logical_height)));
        let gb = Geometry::compute(&l, Some((b.monitors[0].logical_width, b.monitors[0].logical_height)));
        assert_ne!(ga, gb);
    }

    #[test]
    fn geometry_never_exceeds_the_output() {
        let l = LayoutSettings { collapsed_width: 420, collapsed_height: 48, expanded_width: 960, top_margin: 24 };
        let g = Geometry::compute(&l, Some((600.0, 400.0)));
        assert_eq!(g.coll_w, 420.0);
        assert!(g.exp_w * HEADROOM <= 600.0 - 16.0 + 1e-9);
        let (hw, hh) = g.host(10_000.0);
        assert!(hw as f64 <= 584.0 && hh as f64 <= 400.0 - 24.0 - 16.0);
        let tiny = Geometry::compute(&l, Some((300.0, 100.0)));
        assert!(tiny.coll_w <= 284.0 && tiny.coll_h <= 60.0);
        // roomy output: the requested size is kept
        let big = Geometry::compute(&l, Some((3000.0, 2000.0)));
        assert_eq!((big.exp_w, big.coll_w), (960.0, 420.0));
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
    }
}
