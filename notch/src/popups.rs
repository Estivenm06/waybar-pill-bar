//! Shared Audio / Display / Battery controls embedded in the notch's utility page, plus the bar-click FIFO reader
//! (`waybar-popupctl`). Wi-Fi and Bluetooth have their own notch pages.
use crate::desktop::hypr_req;
use crate::{runtime_dir, sh_out, spawn_vec, Msg};
use gtk::{glib, prelude::*};
use gtk4 as gtk;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::rc::Rc;
use async_channel::Sender;
use std::time::{Duration, Instant};

const ICON_VOL: &str = "\u{f057e}";
const ICON_MUTE: &str = "\u{f075f}";
const VOL_MAX: f64 = 150.0;   // volume sliders go past 100% (software boost); brightness stays 0-100
const SCALES: [&str; 4] = ["1", "1.25", "1.6", "2"];
const TEXT_STOPS: [i64; 7] = [9, 10, 11, 12, 14, 16, 20];
/// Reusable control-style rules; `$R` is replaced by the root selector that hosts the content.
const POPUP_CONTENT_CSS: &str = "\
$R .popup-title { font-size: @14@; font-weight: 700; color: {{fg-hi}}; margin-bottom: @6@; }
$R .popup-item { font-size: @12@; color: {{fg}}; }
$R .popup-subtext { font-size: @11@; color: alpha({{fg}}, 0.62); }
$R button { background: alpha({{fg}}, 0.09); border: none; border-radius: @11@; box-shadow: none;
  color: {{fg}}; font-size: @11@; font-weight: 600; padding: @5@ @12@; min-height: 0; min-width: 0;
  transition: background 150ms ease, color 150ms ease; }
$R button:hover { background: alpha({{fg}}, 0.18); color: {{fg-hi}}; }
$R button.active { background: alpha({{accent}}, 0.30); color: {{accent-hi}}; font-weight: 700; }
$R button.btn-danger { background: alpha({{alert}}, 0.18); color: {{alert-hi}}; }
$R button.btn-danger:hover { background: alpha({{alert}}, 0.34); color: {{fg-hi}}; }
$R button.icon-btn { padding: @4@ @8@; min-width: @24@; font-size: @14@; border-radius: @12@; }
$R button.icon-btn.muted { color: alpha({{fg}}, 0.4); }
$R entry, $R text { background: alpha({{fg}}, 0.08); color: {{fg-hi}}; border-radius: @10@; font-size: @11@; padding: @3@ @8@; }
$R .vol-scale trough { background: alpha({{fg}}, 0.14); border-radius: @3@; min-height: @6@; }
$R .vol-scale highlight { background-image: linear-gradient(to right, {{accent-lo}}, {{accent-top}}); border-radius: @3@; min-height: @6@; }
$R .vol-scale slider { background: {{fg-hi}}; border-radius: 50%; min-height: @12@; min-width: @12@; margin: -@3@ 0; box-shadow: 0 0 0 @3@ alpha({{accent}}, 0.25), 0 2px 6px rgba(0,0,0,.7); }
$R .tile { background: alpha({{fg}}, 0.06); border-radius: @11@; padding: @4@ @10@; }
$R .pct-big { font-size: @12@; font-weight: 700; color: {{fg-hi}}; }
$R .tile-icon { font-size: @14@; color: {{accent-hi}}; min-width: @22@; }
$R .tile button.icon-btn { padding: @2@ @6@; min-width: @22@; font-size: @13@; border-radius: @9@; }
$R .battery-hero { padding: @10@ @12@; border-radius: @14@; }
$R .battery-pct { font-size: @28@; font-weight: 800; color: {{fg-hi}}; }
$R .battery-level trough { background: alpha({{fg}}, 0.14); border-radius: @4@; min-height: @8@; }
$R .battery-level progress { background-image: linear-gradient(to right, {{accent-lo}}, {{accent-top}}); border-radius: @4@; min-height: @8@; }
$R .battery-metric { padding: @8@ @10@; border-radius: @12@; }
$R .battery-value { font-size: @13@; font-weight: 700; color: {{fg-hi}}; }
$R flowboxchild { padding: 0; background: none; }
$R separator { background: alpha({{fg}}, 0.10); min-height: 1px; margin: @6@ 0; }
";

/// Substitute `@N@` px tokens (scaled by `s`) and the `$R` root selector. Output is not yet theme-painted.
fn content_rules(root: &str, s: f64) -> String {
    let src = POPUP_CONTENT_CSS.replace("$R", root);
    scale_tokens(&src, s)
}

fn scale_tokens(src: &str, s: f64) -> String {
    let mut out = String::with_capacity(src.len());
    let mut parts = src.split('@');
    out.push_str(parts.next().unwrap_or(""));
    while let Some(n) = parts.next() {
        let px: f64 = n.parse().expect("popup css token must be a number");
        out.push_str(&format!("{:.2}px", px * s));
        out.push_str(parts.next().unwrap_or(""));
    }
    out
}

/// Control rules for the notch's utility page (scale 1), unpainted; appended to the notch stylesheet so font prefs apply.
pub fn embedded_css() -> String { content_rules(".utility-page .control-content", 1.0) }

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presentation { Hidden, Embedded }

// ------------------------------------------------------------------ small widget helpers
/// Wrapping label that never widens its parent.
fn wlbl(t: &str, cls: &str) -> gtk::Label {
    let l = lbl(t, cls);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_width_chars(1);
    l.set_hexpand(true);
    l
}

fn lbl(t: &str, cls: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.set_xalign(0.0);
    l.add_css_class(cls);
    l
}

fn btn(t: &str, cls: &[&str]) -> gtk::Button {
    let b = gtk::Button::with_label(t);
    for c in cls { b.add_css_class(c); }
    b
}

fn hbox(sp: i32) -> gtk::Box { gtk::Box::new(gtk::Orientation::Horizontal, sp) }

fn sep() -> gtk::Separator { gtk::Separator::new(gtk::Orientation::Horizontal) }

fn strv(a: &[&str]) -> Vec<String> { a.iter().map(|s| s.to_string()).collect() }

// ------------------------------------------------------------------ the shared utility controls
pub struct Popups {
    content: gtk::Box,
    kind: RefCell<String>,
    presentation: Cell<Presentation>,
    embed_host: RefCell<Option<gtk::Box>>,
    render_pending: Cell<bool>,
    dragging: Cell<bool>,
    data: RefCell<HashMap<String, Value>>,
    lock_until: Cell<Option<Instant>>,
    poll_id: RefCell<Option<glib::SourceId>>,
    pending: RefCell<HashMap<String, Vec<String>>>,
    pend_id: RefCell<Option<glib::SourceId>>,
    tx: Sender<Msg>,
}

impl Popups {
    pub fn new(tx: Sender<Msg>) -> Rc<Self> {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
        content.add_css_class("control-content");
        let p = Rc::new(Popups {
            content, kind: RefCell::new(String::new()), presentation: Cell::new(Presentation::Hidden),
            embed_host: RefCell::new(None), render_pending: Cell::new(false), dragging: Cell::new(false),
            data: RefCell::new(HashMap::new()), lock_until: Cell::new(None), poll_id: RefCell::new(None),
            pending: RefCell::new(HashMap::new()), pend_id: RefCell::new(None), tx,
        });
        // Tracks a slider drag without claiming the sequence, so data refreshes never rebuild a widget under the pointer.
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let pc = p.clone();
        drag.connect_drag_begin(move |_, _, _| pc.dragging.set(true));
        let pc = p.clone();
        drag.connect_drag_end(move |_, _, _| pc.dragging.set(false));
        let pc = p.clone();
        drag.connect_cancel(move |_, _| pc.dragging.set(false));
        p.content.add_controller(drag);
        p
    }

    // ---- FIFO (`waybar-popupctl kind <nanos>`)
    pub fn start_fifo(tx: Sender<Msg>) {
        let path = format!("{}/waybar-popupd.fifo", runtime_dir());
        let _ = std::fs::remove_file(&path);
        let _ = Command::new("mkfifo").args(["-m", "600", &path]).status();
        std::thread::spawn(move || {
            // read+write keeps the FIFO open when a writer disconnects (no EOF spin)
            let f = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
                Ok(f) => f,
                Err(_) => return,
            };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some(k) = line.split_whitespace().next() {
                    let _ = tx.try_send(Msg::PopupReq(k.to_string()));
                }
            }
        });
    }

    /// Show the real volume/display/battery controls inside `host` (the notch's utility page).
    pub fn embed(self: &Rc<Self>, kind: &str, host: &gtk::Box) {
        if !matches!(kind, "volume" | "display" | "battery") { return; }
        if self.presentation.get() == Presentation::Embedded && *self.kind.borrow() == kind
            && self.embed_host.borrow().as_ref() == Some(host) { return; }
        self.close();
        if let Some(p) = self.content.parent().and_then(|p| p.downcast::<gtk::Box>().ok()) { p.remove(&self.content); }
        host.append(&self.content);
        *self.embed_host.borrow_mut() = Some(host.clone());
        *self.kind.borrow_mut() = kind.to_string();
        self.lock_until.set(None);
        self.render_pending.set(false);
        self.presentation.set(Presentation::Embedded);
        self.render();
        self.refresh(kind);
        self.start_poll();
    }

    pub fn close_embedded(self: &Rc<Self>) { self.close(); }

    /// A slider drag is in progress inside the shared content.
    pub fn is_interacting(&self) -> bool { self.dragging.get() }

    /// Deactivate the view: stop polling and drag/render state. Late data may still update the cache but never repaints.
    pub fn close(self: &Rc<Self>) {
        self.presentation.set(Presentation::Hidden);
        self.dragging.set(false);
        self.render_pending.set(false);
        *self.embed_host.borrow_mut() = None;
        if let Some(id) = self.poll_id.borrow_mut().take() { id.remove(); }
    }

    fn render(self: &Rc<Self>) {
        while let Some(c) = self.content.first_child() { self.content.remove(&c); }
        let kind = self.kind.borrow().clone();
        let data = self.data.borrow().get(&kind).cloned();
        let Some(d) = data else {
            self.content.append(&lbl("Loading…", "popup-subtext"));
            let _ = self.tx.try_send(Msg::Relayout);
            return;
        };
        match kind.as_str() {
            "volume" => self.render_volume(&d),
            "display" => self.render_display(&d),
            "battery" => self.render_battery(&d),
            _ => {}
        }
        let _ = self.tx.try_send(Msg::Relayout);
    }

    /// Refresh cadence of the active view.
    fn poll_interval(&self) -> Option<Duration> {
        match (self.presentation.get(), self.kind.borrow().as_str()) {
            (Presentation::Hidden, _) => None,
            (_, "volume") => Some(Duration::from_millis(800)),
            (_, "display") => Some(Duration::from_secs(2)),
            (_, "battery") => Some(Duration::from_secs(5)),
            _ => None,
        }
    }

    fn start_poll(self: &Rc<Self>) {
        if let Some(id) = self.poll_id.borrow_mut().take() { id.remove(); }
        let Some(every) = self.poll_interval() else { return };
        let p = self.clone();
        *self.poll_id.borrow_mut() = Some(glib::timeout_add_local(every, move || {
            if p.poll_interval() == Some(every) {
                let k = p.kind.borrow().clone();
                p.refresh(&k);
                glib::ControlFlow::Continue
            } else {
                p.poll_id.borrow_mut().take();
                glib::ControlFlow::Break
            }
        }));
    }

    // ---- data
    pub fn refresh(&self, kind: &str) {
        let (tx, kind) = (self.tx.clone(), kind.to_string());
        std::thread::spawn(move || {
            let v = match kind.as_str() {
                "volume" => gather_volume(),
                "display" => gather_display(),
                "battery" => gather_battery(),
                _ => return,
            };
            let _ = tx.try_send(Msg::PopupData(kind, v));
        });
    }

    pub fn on_data(self: &Rc<Self>, kind: String, v: Value) {
        let changed = self.data.borrow().get(&kind) != Some(&v);
        self.data.borrow_mut().insert(kind.clone(), v);
        let locked = self.lock_until.get().map(|t| Instant::now() < t).unwrap_or(false);
        let active = self.presentation.get() != Presentation::Hidden && *self.kind.borrow() == kind;
        if active && changed { self.render_pending.set(true); }
        if active && !locked && !self.dragging.get() && self.render_pending.get() {
            self.render();
            self.render_pending.set(false);
        }
    }

    pub fn warm_up(&self) {
        for k in ["volume", "battery", "display"] { self.refresh(k); }
    }

    // ---- debounced command runner for sliders
    fn queue_cmd(self: &Rc<Self>, key: String, cmd: Vec<String>) {
        self.lock_until.set(Some(Instant::now() + Duration::from_millis(1500)));
        self.pending.borrow_mut().insert(key, cmd);
        if self.pend_id.borrow().is_none() {
            let p = self.clone();
            *self.pend_id.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(40), move || {
                p.pend_id.borrow_mut().take();
                for (_, c) in p.pending.borrow_mut().drain() { spawn_vec(&c); }
            }));
        }
    }

    fn act(self: &Rc<Self>, b: &gtk::Button, cmd: Vec<String>) {
        let p = self.clone();
        b.connect_clicked(move |_| {
            let (tx, kind, cmd) = (p.tx.clone(), p.kind.borrow().clone(), cmd.clone());
            std::thread::spawn(move || {
                if let Some((prog, args)) = cmd.split_first() {
                    let _ = Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit()).status();
                }
                let _ = tx.try_send(Msg::PopupRefresh(kind));
            });
        });
    }

    /// Slider tile. Embedded: two rows (mute/icon, name, percentage over a full-width slider); detached: one compact row.
    /// `vol == None` shows `—` with a disabled slider; `muted == None` omits the mute button.
    fn vol_row(self: &Rc<Self>, title: &str, vol: Option<f64>, muted: Option<bool>, dim: bool, key: String,
               mk: Rc<dyn Fn(i64) -> Vec<String>>, mute_cmd: Option<Vec<String>>, icon: &str, max: f64) -> gtk::Box {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 6);
        tile.add_css_class("tile");
        let head = hbox(8);
        let glyph = |muted: bool, v: Option<f64>| match (muted, v) {
            (true, _) => ICON_MUTE,
            (_, Some(v)) if v < 34.0 => "\u{f057f}",
            (_, Some(v)) if v < 67.0 => "\u{f0580}",
            _ => ICON_VOL,
        };
        if let (Some(m), Some(cmd)) = (muted, mute_cmd) {
            let b = btn(glyph(m, vol), &["icon-btn"]);
            if m { b.add_css_class("muted"); }
            b.connect_clicked(move |_| spawn_vec(&cmd));
            head.append(&b);
        } else {
            head.append(&lbl(icon, "tile-icon"));
        }
        let name = lbl(title, "popup-item");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_xalign(0.0);
        name.set_tooltip_text(Some(title));
        name.set_width_chars(1);
        name.set_hexpand(true);
        head.append(&name);
        let pct = lbl(&vol.map_or("\u{2014}".to_string(), |v| format!("{}%", v as i64)), "pct-big");
        pct.set_width_chars(4);
        pct.set_xalign(1.0);
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, max, 1.0);
        scale.add_css_class("vol-scale");
        scale.set_draw_value(false);
        scale.set_hexpand(true);
        scale.set_valign(gtk::Align::Center);
        scale.set_size_request(0, -1);
        if let Some(v) = vol {
            scale.set_value(v);
            let (p, pl) = (self.clone(), pct.clone());
            scale.connect_value_changed(move |s| {
                let v = s.value() as i64;
                pl.set_text(&format!("{v}%"));
                p.queue_cmd(key.clone(), mk(v));
            });
        } else {
            scale.set_sensitive(false);
        }
        head.append(&pct);
        tile.append(&head);
        tile.append(&scale);
        if dim { tile.add_css_class("dim"); }
        tile
    }

    fn render_volume(self: &Rc<Self>, d: &Value) {
        let c = &self.content;
        if !d["available"].as_bool().unwrap_or(false) {
            c.append(&wlbl("Audio unavailable", "popup-subtext"));
            return;
        }
        let outputs = d["outputs"].as_array().cloned().unwrap_or_default();
        let m = &d["master"];
        if let Some(mname) = m["name"].as_str() {
            let mname = mname.to_string();
            let target = mname.clone();
            let master = self.vol_row("Master", m["vol"].as_f64(), m["mute"].as_bool(),
                false, "master".into(), Rc::new(move |v| strv(&["pactl", "set-sink-volume", &target, &format!("{v}%")])),
                Some(strv(&["pactl", "set-sink-mute", &mname, "toggle"])), ICON_VOL, VOL_MAX);
            master.set_tooltip_text(m["desc"].as_str());
            c.append(&master);
        } else if !outputs.is_empty() {
            c.append(&wlbl("No default output selected", "popup-subtext"));
        }
        c.append(&sep());
        c.append(&lbl("Output", "popup-subtext"));
        if outputs.is_empty() { c.append(&wlbl("No audio outputs available", "popup-subtext")); }
        for o in outputs {
            let (name, is_def) = (o["name"].as_str().unwrap_or("").to_string(), o["default"].as_bool().unwrap_or(false));
            let text = format!("{}  {}", if is_def { "\u{f00c}" } else { "  " }, o["desc"].as_str().unwrap_or(&name));
            let b = btn(&text, &["popup-item"]);
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                l.set_width_chars(1);
                l.set_xalign(0.0);
            }
            b.set_hexpand(true);
            b.set_tooltip_text(Some(&text));
            if is_def { b.add_css_class("ctl-active"); }
            // make it the default and move already-playing streams over (checked, off the GTK thread)
            let (p, nm) = (self.clone(), name.clone());
            b.connect_clicked(move |_| {
                let (tx, name) = (p.tx.clone(), nm.clone());
                std::thread::spawn(move || {
                    if let Err(e) = select_audio_output(&name) { eprintln!("audio output {name}: {e}"); }
                    let _ = tx.try_send(Msg::PopupRefresh("volume".into()));
                });
            });
            c.append(&b);
        }
        c.append(&sep());
        c.append(&lbl("Applications", "popup-subtext"));
        let streams = d["streams"].as_array().cloned().unwrap_or_default();
        if !d["streams_available"].as_bool().unwrap_or(false) {
            c.append(&wlbl("Application audio unavailable", "popup-subtext"));
        } else if streams.is_empty() {
            c.append(&wlbl("No applications playing audio", "popup-subtext"));
        }
        for s in streams {
            let Some(id) = s["id"].as_u64().map(|i| i.to_string()) else { continue };
            let idc = id.clone();
            c.append(&self.vol_row(s["name"].as_str().unwrap_or("Unknown"), s["vol"].as_f64(),
                s["mute"].as_bool(), s["corked"].as_bool().unwrap_or(false), format!("in{id}"),
                Rc::new(move |v| strv(&["pactl", "set-sink-input-volume", &idc, &format!("{v}%")])),
                Some(strv(&["pactl", "set-sink-input-mute", &id, "toggle"])), ICON_VOL, VOL_MAX));
        }
    }

    fn render_display(self: &Rc<Self>, d: &Value) {
        let c = &self.content;
        let mons = d["mons"].as_array().cloned().unwrap_or_default();
        if mons.is_empty() { c.append(&lbl("No displays available", "popup-subtext")); }
        let enabled = mons.iter().filter(|m| m["enabled"].as_bool().unwrap_or(false)).count();
        for m in &mons {
            let on = m["enabled"].as_bool().unwrap_or(false);
            let name = m["name"].as_str().unwrap_or("").to_string();
            let row = hbox(8);
            let l = lbl(&format!("{name}  {}x{}{}", m["w"], m["h"],
                if m["focused"].as_bool().unwrap_or(false) { "  \u{b7} focused" } else { "" }), "popup-item");
            l.set_hexpand(true);
            row.append(&l);
            let b = btn(if on { "On" } else { "Off" }, &[]);
            if on { b.add_css_class("active"); }
            if on && enabled <= 1 { b.set_sensitive(false); } // never disable the last display
            let expr = if on { format!("hl.monitor({{ output = \"{name}\", disabled = true }})") } else {
                format!("hl.monitor({{ output = \"{name}\", disabled = false, mode = \"preferred\", position = \"auto\", scale = \"auto\" }})") };
            self.act(&b, strv(&["hyprctl", "eval", &expr]));
            row.append(&b);
            c.append(&row);
        }
        let shown: Vec<&Value> = mons.iter()
            .filter(|m| m["enabled"].as_bool().unwrap_or(false) && m["brightness"].is_number()).collect();
        if !shown.is_empty() {
            c.append(&sep());
            c.append(&lbl("Brightness", "popup-subtext"));
        }
        for m in shown {
            let name = m["name"].as_str().unwrap_or("").to_string();
            let n2 = name.clone();
            c.append(&self.vol_row(&name, m["brightness"].as_f64(), None, false, format!("br{name}"),
                Rc::new(move |v| strv(&["omarchy-brightness-display", "--no-osd", "--monitor", &n2, &format!("{v}%")])), None, "\u{f00df}", 100.0));
        }
        let foc = mons.iter().find(|m| m["focused"].as_bool().unwrap_or(false));
        let foc_on = foc.map_or(false, |m| m["enabled"].as_bool().unwrap_or(false));
        c.append(&sep());
        c.append(&lbl(&format!("Scale{}", foc.map(|m| format!(" ({})", m["name"].as_str().unwrap_or(""))).unwrap_or_default()), "popup-subtext"));
        let sr = hbox(6);
        for s in SCALES {
            let b = btn(&format!("{s}x"), &[]);
            b.set_hexpand(true);
            if foc.map(|m| (m["scale"].as_f64().unwrap_or(0.0) - s.parse::<f64>().unwrap_or(0.0)).abs() < 0.01).unwrap_or(false) {
                b.add_css_class("active");
            }
            self.act(&b, strv(&["omarchy-hyprland-monitor-scaling", s]));
            sr.append(&b);
        }
        sr.set_sensitive(foc_on);
        c.append(&sr);
        c.append(&lbl("Text size", "popup-subtext"));
        let tr = hbox(4);
        let cur = d["text"].as_i64().unwrap_or(12);
        let near = *TEXT_STOPS.iter().min_by_key(|s| (**s - cur).abs()).unwrap();
        for s in TEXT_STOPS {
            let b = btn(&s.to_string(), &[]);
            b.set_hexpand(true);
            if s == near { b.add_css_class("active"); }
            self.act(&b, strv(&["omarchy-display-text-size", &s.to_string()]));
            tr.append(&b);
        }
        c.append(&tr);
    }

    fn state_text(state: &str) -> &'static str {
        match state {
            "charging" => "Charging",
            "discharging" => "Discharging",
            "full" => "Fully charged",
            "not_charging" => "Not charging",
            _ => "Status unavailable",
        }
    }

    /// Summary tile: percentage, state, source, gauge and estimate for one pack.
    fn battery_hero(b: &Value, ac: Option<bool>) -> gtk::Box {
        let state = b["state"].as_str().unwrap_or("unknown");
        let hero = gtk::Box::new(gtk::Orientation::Vertical, 4);
        hero.add_css_class("tile");
        hero.add_css_class("battery-hero");
        let pct = b["percentage"].as_f64();
        let top = hbox(10);
        let big = lbl(&pct.map_or("\u{2014}".to_string(), |p| format!("{}%", p.round() as i64)), "battery-pct");
        big.set_valign(gtk::Align::Center);
        top.append(&big);
        let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
        info.set_valign(gtk::Align::Center);
        info.set_hexpand(true);
        info.append(&wlbl(Self::state_text(state), "popup-item"));
        info.append(&wlbl(Self::source_text(ac, state == "discharging"), "popup-subtext"));
        top.append(&info);
        hero.append(&top);
        let gauge = gtk::ProgressBar::new();
        gauge.add_css_class("battery-level");
        gauge.set_fraction(pct.map_or(0.0, |p| (p / 100.0).clamp(0.0, 1.0)));
        gauge.set_visible(pct.is_some());
        hero.append(&gauge);
        let est = match (state, b["time_to_empty_secs"].as_u64(), b["time_to_full_secs"].as_u64()) {
            ("discharging", Some(s), _) => format!("Estimated time to empty: {}", fmt_duration(s)),
            ("charging", _, Some(s)) => format!("Estimated time to full: {}", fmt_duration(s)),
            _ => "Time estimate unavailable".to_string(),
        };
        hero.append(&wlbl(&est, "popup-subtext"));
        hero
    }

    fn source_text(ac: Option<bool>, discharging: bool) -> &'static str {
        match ac {
            Some(true) => "External power connected",
            Some(false) => "External power disconnected",
            None if discharging => "On battery",
            None => "Power source unavailable",
        }
    }

    /// Metric tiles and identity for one pack.
    fn battery_details(c: &gtk::Box, b: &Value) {
        let state = b["state"].as_str().unwrap_or("unknown");
        let na = || "Not available".to_string();
        let f1 = |k: &str, unit: &str| b[k].as_f64().map_or_else(na, |v| format!("{v:.1} {unit}"));
        let rate_title = match state { "discharging" => "Power draw", "charging" => "Charging rate", _ => "Power rate" };
        let metrics = [
            (rate_title, f1("power_w", "W")),
            ("Energy remaining", f1("energy_now_wh", "Wh")),
            ("Full capacity", f1("energy_full_wh", "Wh")),
            ("Design capacity", f1("energy_design_wh", "Wh")),
            ("Battery health", f1("health_pct", "%")),
            ("Voltage", f1("voltage_v", "V")),
            ("Charge cycles", b["cycles"].as_u64().map_or_else(na, |v| v.to_string())),
        ];
        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_homogeneous(true);
        flow.set_min_children_per_line(1);
        flow.set_max_children_per_line(2);
        flow.set_row_spacing(8);
        flow.set_column_spacing(8);
        flow.set_focusable(false);
        for (t, v) in metrics.iter() {
            let m = gtk::Box::new(gtk::Orientation::Vertical, 1);
            m.add_css_class("tile");
            m.add_css_class("battery-metric");
            m.set_width_request(160);
            m.append(&wlbl(t, "popup-subtext"));
            let val = lbl(v, "battery-value");
            val.set_ellipsize(gtk::pango::EllipsizeMode::End);
            val.set_width_chars(1);
            val.set_hexpand(true);
            val.set_tooltip_text(Some(v));
            m.append(&val);
            flow.insert(&m, -1);
        }
        c.append(&flow);
        let ident: Vec<&str> = ["manufacturer", "model", "technology"].iter().filter_map(|k| b[*k].as_str()).collect();
        if !ident.is_empty() { c.append(&wlbl(&ident.join(" \u{b7} "), "popup-subtext")); }
    }

    fn render_battery(self: &Rc<Self>, d: &Value) {
        let c = &self.content;
        let ac = d["ac_online"].as_bool();
        let packs: Vec<Value> = d["batteries"].as_array().cloned().unwrap_or_default();
        if !d["available"].as_bool().unwrap_or(false) {
            c.append(&wlbl("Power information unavailable", "battery-value"));
        } else if packs.is_empty() {
            c.append(&wlbl("No battery detected", "battery-value"));
            c.append(&wlbl(match ac {
                Some(true) => "External power connected",
                Some(false) => "External power disconnected",
                None => "Power source unavailable",
            }, "popup-subtext"));
        } else if packs.len() == 1 {
            c.append(&Self::battery_hero(&packs[0], ac));
        } else {
            let hero = gtk::Box::new(gtk::Orientation::Vertical, 4);
            hero.add_css_class("tile");
            hero.add_css_class("battery-hero");
            let pct = d["percentage"].as_f64();
            let top = hbox(10);
            let big = lbl(&pct.map_or("\u{2014}".to_string(), |p| format!("{}%", p.round() as i64)), "battery-pct");
            big.set_valign(gtk::Align::Center);
            top.append(&big);
            let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
            info.set_valign(gtk::Align::Center);
            info.set_hexpand(true);
            info.append(&wlbl(&format!("{} batteries", packs.len()), "popup-item"));
            info.append(&wlbl(Self::source_text(ac, false), "popup-subtext"));
            top.append(&info);
            hero.append(&top);
            let gauge = gtk::ProgressBar::new();
            gauge.add_css_class("battery-level");
            gauge.set_fraction(pct.map_or(0.0, |p| (p / 100.0).clamp(0.0, 1.0)));
            gauge.set_visible(pct.is_some());
            hero.append(&gauge);
            c.append(&hero);
        }

        c.append(&sep());
        c.append(&lbl("Power Profile:", "popup-item"));
        match d["profiles"].as_array() {
            None => c.append(&wlbl("Power profiles unavailable", "popup-subtext")),
            Some(list) => {
                let cur = d["profile"].as_str();
                let pb = gtk::FlowBox::new();
                pb.set_selection_mode(gtk::SelectionMode::None);
                pb.set_homogeneous(true);
                pb.set_min_children_per_line(1);
                pb.set_max_children_per_line(3);
                pb.set_row_spacing(6);
                pb.set_column_spacing(6);
                pb.set_focusable(false);
                for id in list.iter().filter_map(|v| v.as_str()) {
                    let text = match id {
                        "performance" => "⚡ Performance",
                        "balanced" => "⚖ Balanced",
                        "power-saver" => "\u{f06c} Power Saver",
                        _ => continue,
                    };
                    let b = btn(text, &[]);
                    if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        l.set_width_chars(1);
                    }
                    b.set_tooltip_text(Some(text));
                    b.set_hexpand(true);
                    if Some(id) == cur { b.add_css_class("active"); }
                    self.act(&b, strv(&["powerprofilesctl", "set", id]));
                    pb.insert(&b, -1);
                }
                c.append(&pb);
                if cur.is_none() { c.append(&wlbl("Current profile unavailable", "popup-subtext")); }
            }
        }

        if !packs.is_empty() {
            let multi = packs.len() > 1;
            for b in &packs {
                c.append(&sep());
                if multi {
                    c.append(&wlbl(b["name"].as_str().unwrap_or("Battery"), "popup-item"));
                    c.append(&Self::battery_hero(b, ac));
                }
                Self::battery_details(c, b);
            }
            c.append(&wlbl("Live battery power; time is an estimate.", "popup-subtext"));
        }
    }
}

/// `H h M min` from seconds.
fn fmt_duration(secs: u64) -> String { format!("{} h {} min", secs / 3600, secs % 3600 / 60) }

// ------------------------------------------------------------------ data gatherers (worker threads)
/// `pactl --format=json <args>` as an array; `Err` on spawn failure, non-zero exit, invalid JSON or non-array output.
fn pactl_json(args: &[&str]) -> Result<Vec<Value>, ()> {
    let o = Command::new("pactl").arg("--format=json").args(args).stderr(Stdio::null()).output().map_err(|_| ())?;
    if !o.status.success() { return Err(()); }
    serde_json::from_slice::<Value>(&o.stdout).ok().and_then(|v| v.as_array().cloned()).ok_or(())
}

/// Highest channel volume in percent; `None` when the channels are missing or any is malformed.
fn max_pct(v: &Value) -> Option<f64> {
    let mut best: Option<f64> = None;
    for c in v.as_object()?.values() {
        let p = c["value_percent"].as_str().and_then(|s| s.trim().trim_end_matches('%').parse::<f64>().ok())
            .or_else(|| c["value"].as_f64().map(|x| x / 65536.0 * 100.0))
            .filter(|p| p.is_finite() && *p >= 0.0)?;
        best = Some(best.map_or(p, |b| b.max(p)));
    }
    best
}

fn nonblank(v: &Value) -> Option<&str> { v.as_str().filter(|s| !s.trim().is_empty()) }

/// Master + output list from the default sink name and `pactl list sinks`. Never substitutes another sink for a missing default.
fn normalize_audio_outputs(default: Option<&str>, sinks: &[Value]) -> Value {
    let desc = |s: &Value| nonblank(&s["description"]).or(nonblank(&s["name"])).unwrap_or("").to_string();
    let default = default.filter(|d| !d.is_empty());
    let master = default.and_then(|d| sinks.iter().find(|s| nonblank(&s["name"]) == Some(d))).map(|s| json!({
        "name": s["name"], "desc": desc(s), "vol": max_pct(&s["volume"]), "mute": s["mute"].as_bool(),
    }));
    // skip sinks whose every port is unplugged (e.g. HDMI with nothing attached), but keep the current default
    let outputs: Vec<Value> = sinks.iter().filter_map(|s| {
        let name = nonblank(&s["name"])?;
        let is_def = default == Some(name);
        let ports = s["ports"].as_array().cloned().unwrap_or_default();
        let plugged = ports.is_empty() || ports.iter().any(|p| p["availability"].as_str() != Some("not available"));
        (is_def || plugged).then(|| json!({"name": name, "desc": desc(s), "default": is_def, "bt_mac": bluetooth_sink_mac(s), "mute": s["mute"].as_bool()}))
    }).collect();
    json!({"master": master, "outputs": outputs})
}

fn normalize_audio_streams(inputs: &[Value]) -> Vec<Value> {
    inputs.iter().filter_map(|i| {
        let id = i["index"].as_u64()?;
        let p = &i["properties"];
        let name = ["application.name", "application.process.binary", "media.name"].iter()
            .find_map(|k| nonblank(&p[*k])).unwrap_or("Unknown");
        Some(json!({"id": id, "name": name, "vol": max_pct(&i["volume"]), "mute": i["mute"].as_bool(),
                    "corked": i["corked"].as_bool().unwrap_or(false)}))
    }).collect()
}

pub(crate) fn gather_audio_outputs() -> Value {
    let Ok(sinks) = pactl_json(&["list", "sinks"]) else {
        return json!({"available": false, "master": null, "outputs": []});
    };
    let default = cmd_ok("pactl", &["get-default-sink"]);
    let mut v = normalize_audio_outputs(default.as_deref(), &sinks);
    v["available"] = json!(true);
    v
}

/// `AA:BB:CC:DD:EE:FF` (any case) → uppercase; exactly six two-digit hex octets, nothing else.
fn normalize_mac(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split(':').collect();
    (parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit())))
        .then(|| s.to_ascii_uppercase())
}

/// MAC of the Bluetooth device behind a playback sink record. Sources/monitors, non-Bluetooth sinks and malformed
/// addresses give `None`; aliases and arbitrary name substrings are never matched.
pub(crate) fn bluetooth_sink_mac(s: &Value) -> Option<String> {
    let name = nonblank(&s["name"])?;
    if name.ends_with(".monitor") { return None; }
    let p = &s["properties"];
    let by_name = name.starts_with("bluez_output.");
    if !(by_name || p["device.api"].as_str() == Some("bluez5") || p["device.bus"].as_str() == Some("bluetooth")) { return None; }
    nonblank(&p["api.bluez5.address"]).and_then(normalize_mac).or_else(|| {
        let rest = name.strip_prefix("bluez_output.")?;
        normalize_mac(&rest.split('.').next()?.replace('_', ":"))
    })
}

fn run_pactl(args: &[&str]) -> Result<(), String> {
    let o = Command::new("pactl").args(args).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).trim().to_string()) }
}

/// Indices of sink inputs not already playing on `target_index`.
fn sink_input_moves(inputs: &[Value], target_index: u64) -> Vec<u64> {
    inputs.iter().filter_map(|i| {
        let idx = i["index"].as_u64()?;
        (i["sink"].as_u64() != Some(target_index)).then_some(idx)
    }).collect()
}

/// Make `name` the default output and move playing streams to it. Verified; never touches volume or mute.
/// Streams are only moved after the default really changed; a stream that vanished meanwhile is not an error.
pub(crate) fn select_audio_output(name: &str) -> Result<(), String> {
    let sinks = pactl_json(&["list", "sinks"]).map_err(|_| "Audio server unavailable".to_string())?;
    let target = sinks.iter().find(|s| s["name"].as_str() == Some(name))
        .ok_or_else(|| format!("Audio output {name} is not available"))?;
    let tidx = target["index"].as_u64().unwrap_or(u64::MAX);
    run_pactl(&["set-default-sink", name]).map_err(|e| format!("Could not select {name}: {e}"))?;
    if cmd_ok("pactl", &["get-default-sink"]).as_deref() != Some(name) {
        return Err(format!("Could not make {name} the default output"));
    }
    let inputs = pactl_json(&["list", "sink-inputs"]).map_err(|_| "Could not list playing applications".to_string())?;
    let mut failed = 0;
    for idx in sink_input_moves(&inputs, tidx) {
        if run_pactl(&["move-sink-input", &idx.to_string(), name]).is_err() {
            let alive = pactl_json(&["list", "sink-inputs"]).map(|v| v.iter().any(|i| i["index"].as_u64() == Some(idx))).unwrap_or(true);
            if alive { failed += 1; }
        }
    }
    if failed == 0 { Ok(()) } else { Err(format!("Could not move {failed} playing application(s) to {name}")) }
}

/// One-line Home status for the default output.
pub(crate) fn audio_status() -> String {
    let v = gather_audio_outputs();
    if !v["available"].as_bool().unwrap_or(false) { return "Audio unavailable".into(); }
    let m = &v["master"];
    if m.is_null() { return "No output".into(); }
    if m["mute"].as_bool() == Some(true) { return format!("{ICON_MUTE} Muted"); }
    match m["vol"].as_f64() {
        Some(p) => format!("{ICON_VOL} {}%", p.round() as i64),
        None => format!("{ICON_VOL} \u{2014}"),
    }
}

fn gather_volume() -> Value {
    let mut v = gather_audio_outputs();
    let streams = if v["available"].as_bool().unwrap_or(false) { pactl_json(&["list", "sink-inputs"]).ok() } else { None };
    v["streams_available"] = json!(streams.is_some());
    v["streams"] = json!(streams.map(|s| normalize_audio_streams(&s)).unwrap_or_default());
    v
}

fn gather_display() -> Value {
    let mons: Vec<Value> = hypr_req("j/monitors all").and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().map(|m| {
        let enabled = !m["disabled"].as_bool().unwrap_or(false);
        let name = m["name"].as_str().unwrap_or("");
        let brightness = if enabled {
            sh_out("omarchy-brightness-display", &["--no-osd", "--monitor", name]).parse::<i64>().ok()
        } else { None };
        json!({"name": name, "w": m["width"], "h": m["height"], "scale": m["scale"],
               "focused": m["focused"].as_bool().unwrap_or(false), "enabled": enabled, "brightness": brightness})
    }).collect();
    let home = std::env::var("HOME").unwrap_or_default();
    let text = std::fs::read_to_string(format!("{home}/.config/omarchy/shell.toml")).ok()
        .and_then(|t| t.lines().find(|l| l.contains("base-size")).map(String::from))
        .and_then(|l| l.split('=').nth(1).map(|v| v.trim().to_string()))
        .and_then(|v| v.parse::<i64>().ok()).unwrap_or(12);
    json!({"mons": mons, "text": text})
}

/// Run a command and return trimmed stdout only when it spawned and exited successfully.
fn cmd_ok(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Profile IDs advertised by `powerprofilesctl list` (top-level `id:` headings, optionally starred), in canonical order.
fn supported_profiles(output: &str) -> Vec<String> {
    let mut found = [false; 3];
    for line in output.lines() {
        let t = line.trim();
        match t.strip_prefix('*').unwrap_or(t).trim() {
            "performance:" => found[0] = true,
            "balanced:" => found[1] = true,
            "power-saver:" => found[2] = true,
            _ => {}
        }
    }
    ["performance", "balanced", "power-saver"].iter().zip(found).filter(|(_, f)| *f).map(|(id, _)| id.to_string()).collect()
}

fn gather_battery() -> Value {
    let profiles = cmd_ok("powerprofilesctl", &["list"]).map(|o| supported_profiles(&o)).filter(|p| !p.is_empty());
    let profile = profiles.as_ref().and_then(|ps| cmd_ok("powerprofilesctl", &["get"]).filter(|c| ps.contains(c)));
    let p = crate::power::snapshot();
    let batteries: Vec<Value> = p.batteries.iter().map(|b| json!({
        "name": b.name, "state": b.state.as_str(), "percentage": b.percentage,
        "power_w": b.power_w, "energy_now_wh": b.energy_now_wh, "energy_full_wh": b.energy_full_wh,
        "energy_design_wh": b.energy_design_wh, "health_pct": b.health_pct, "voltage_v": b.voltage_v,
        "cycles": b.cycles, "technology": b.technology, "manufacturer": b.manufacturer, "model": b.model,
        "time_to_empty_secs": b.time_to_empty_secs, "time_to_full_secs": b.time_to_full_secs,
    })).collect();
    json!({"available": p.available, "ac_online": p.ac_online, "percentage": p.percentage,
           "batteries": batteries, "profile": profile, "profiles": profiles})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_profiles_supported_choices() {
        let out = "  performance-ignored: no\n* balanced:\n    CpuDriver:  amd_pstate\n    PlatformDriver: placeholder\n\n  power-saver:\n    Driver: x\n";
        assert_eq!(supported_profiles(out), ["balanced", "power-saver"]);
        assert!(supported_profiles("garbage").is_empty());
    }

    fn sink(name: &str, desc: &str, vol: Value, ports: Value) -> Value {
        json!({"name": name, "description": desc, "mute": false, "volume": vol, "ports": ports})
    }
    fn ch(p: &str) -> Value { json!({"front-left": {"value_percent": p}, "front-right": {"value_percent": "10%"}}) }

    #[test]
    fn audio_outputs_select_default_not_first() {
        let sinks = [sink("a", "A", ch("30%"), json!([])), sink("b", "B", ch("0%"), json!([]))];
        let v = normalize_audio_outputs(Some("b"), &sinks);
        assert_eq!(v["master"]["name"], "b");
        assert_eq!(v["master"]["vol"], 10.0);
        let v = normalize_audio_outputs(None, &sinks);
        assert!(v["master"].is_null());
        assert_eq!(v["outputs"].as_array().unwrap().len(), 2);
        assert!(normalize_audio_outputs(Some("gone"), &sinks)["master"].is_null());
    }

    #[test]
    fn audio_outputs_filter_unplugged_but_keep_default() {
        let off = json!([{"availability": "not available"}]);
        let sinks = [sink("hdmi", "", ch("0%"), off.clone()), sink("usb", "", ch("0%"), json!([])), sink("hdmi2", "H2", ch("0%"), off)];
        let v = normalize_audio_outputs(Some("hdmi2"), &sinks);
        let names: Vec<&str> = v["outputs"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["usb", "hdmi2"]);
        assert_eq!(v["outputs"][0]["desc"], "usb"); // blank description falls back to the name
        assert_eq!(v["outputs"][1]["default"], true);
    }

    #[test]
    fn audio_volume_preserves_zero_overdrive_and_missing() {
        assert_eq!(max_pct(&json!({"l": {"value_percent": "0%"}})), Some(0.0));
        assert_eq!(max_pct(&json!({"l": {"value_percent": "120%"}, "r": {"value": 32768}})), Some(120.0));
        assert_eq!(max_pct(&json!({"l": {"value": 32768}})), Some(50.0));
        assert_eq!(max_pct(&json!({"l": {"value_percent": "x"}})), None);
        assert_eq!(max_pct(&json!({})), None);
        assert_eq!(max_pct(&Value::Null), None);
        let s = json!({"name": "n", "volume": {}});
        let v = normalize_audio_outputs(Some("n"), &[s]);
        assert!(v["master"]["vol"].is_null() && v["master"]["mute"].is_null());
    }

    #[test]
    fn audio_streams_keep_names_and_drop_invalid_ids() {
        let inputs = [
            json!({"index": 3, "properties": {"application.name": "écho"}, "volume": {"l": {"value_percent": "40%"}}}),
            json!({"index": 4, "properties": {"application.name": "", "application.process.binary": "音楽"}}),
            json!({"index": "x", "properties": {}}),
            json!({"properties": {}}),
        ];
        let s = normalize_audio_streams(&inputs);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0]["name"].as_str(), s[0]["vol"].as_f64()), (Some("écho"), Some(40.0)));
        assert_eq!(s[1]["name"], "音楽");
        assert!(s[1]["vol"].is_null() && s[1]["mute"].is_null());
        let outs = normalize_audio_outputs(Some("a"), &[sink("", "x", ch("1%"), json!([]))]);
        assert!(outs["outputs"].as_array().unwrap().is_empty() && outs["master"].is_null());
    }

    fn bt_sink(name: &str, props: Value) -> Value { json!({"name": name, "properties": props}) }

    #[test]
    fn bluetooth_sink_mac_is_exact() {
        let p = |a: &str| bt_sink("alsa_output.x", json!({"device.api": "bluez5", "api.bluez5.address": a}));
        assert_eq!(bluetooth_sink_mac(&p("aa:bb:cc:dd:ee:ff")).as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC_DD_EE_0F.1", json!({}))).as_deref(), Some("AA:BB:CC:DD:EE:0F"));
        // invalid property falls back to the name
        let bad = bt_sink("bluez_output.11_22_33_44_55_66.1", json!({"api.bluez5.address": "zz"}));
        assert_eq!(bluetooth_sink_mac(&bad).as_deref(), Some("11:22:33:44:55:66"));
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC.1", json!({}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("bluez_output.AA_BB_CC_DD_EE_FF.1.monitor", json!({}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("alsa_output.pci", json!({"api.bluez5.address": "AA:BB:CC:DD:EE:FF"}))), None);
        assert_eq!(bluetooth_sink_mac(&bt_sink("alsa_output.AA_BB_CC_DD_EE_FF", json!({"device.bus": "pci"}))), None);
        assert_eq!(bluetooth_sink_mac(&p("AA:BB:CC:DD:EE")), None);
    }

    #[test]
    fn outputs_carry_exact_mac_and_mute_and_identical_aliases_stay_distinct() {
        let mk = |n: &str| json!({"name": n, "description": "Buds", "mute": true, "properties": {}});
        let sinks = [mk("bluez_output.AA_AA_AA_AA_AA_AA.1"), mk("bluez_output.BB_BB_BB_BB_BB_BB.1"), mk("alsa")];
        let v = normalize_audio_outputs(Some("bluez_output.BB_BB_BB_BB_BB_BB.1"), &sinks);
        assert_eq!(v["outputs"][0]["bt_mac"], "AA:AA:AA:AA:AA:AA");
        assert_eq!(v["outputs"][1]["bt_mac"], "BB:BB:BB:BB:BB:BB");
        assert!(v["outputs"][2]["bt_mac"].is_null());
        assert_eq!(v["outputs"][1]["default"], true);
        assert_eq!(v["outputs"][0]["mute"], true);
    }

    #[test]
    fn only_inputs_on_other_sinks_are_moved() {
        let inputs = [json!({"index": 3, "sink": 7}), json!({"index": 4, "sink": 1}), json!({"sink": 1})];
        assert_eq!(sink_input_moves(&inputs, 7), vec![4]);
    }
}
