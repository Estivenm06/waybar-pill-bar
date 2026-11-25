//! Settings page (reached from the Controls tab). A draft copy of the
//! preferences is edited with immediate-mode controls; Apply writes a config
//! patch and restyles the whole UI, Cancel/Back discards the draft.

use crate::config::{self, ClockFormat, MonitorMode, NotchSettings};
use crate::layout::{Rect, Scroll, Size, pt, rect};
use crate::page::Page;
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{ButtonKind, FieldResult, Hits, Palette, TextField, button, scrollbar, toggle};
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event, Poster};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

const CATALOG: &str = include_str!("../../files/setup/apps-catalog.json");

pub const TAB_LABELS: [(&str, &str); 4] = [("media", "Media"), ("calendar", "Calendar"), ("clipboard", "Clipboard"), ("ai", "AI usage")];
pub const UTILITY_LABELS: [(&str, &str); 11] = [
    ("screenshot", "Screenshot"), ("record", "Record"), ("nightlight", "Reading Mode"), ("dnd", "Do Not Disturb"),
    ("notification-sounds", "Notification sounds"),
    ("wifi", "Wi-Fi"), ("bluetooth", "Bluetooth"), ("lock", "Lock"), ("clear-clipboard", "Clear clipboard"),
    ("audio", "Audio"), ("display", "Displays"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct AppSel { pub id: String, pub background: bool }

#[derive(Default)]
pub struct DocInfo {
    pub err: Option<String>,
    pub ai: Option<Vec<String>>,
    pub apps: Vec<AppSel>,
    pub detected: Vec<(String, String)>,
    pub installed: HashSet<String>,
}

fn catalog() -> Vec<(String, String, Vec<String>, bool)> {
    let v: Value = serde_json::from_str(CATALOG).unwrap_or(Value::Null);
    v.as_array().map(|a| a.iter().filter_map(|e| Some((
        e["id"].as_str()?.to_string(),
        e["name"].as_str()?.to_string(),
        e["exe"].as_array().map(|x| x.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        e.get("web").is_some(),
    ))).collect()).unwrap_or_default()
}

fn load_doc_info() -> DocInfo {
    let (doc, err) = match config::read_document() { Ok(d) => (d, None), Err(e) => (Value::Null, Some(e)) };
    let ai = doc.get("ai").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
    let apps = doc.get("apps").and_then(Value::as_array).map(|a| a.iter().filter_map(|e| Some(AppSel { id: e["id"].as_str()?.to_string(), background: e["background"].as_bool().unwrap_or(false) })).collect()).unwrap_or_default();
    let installed = catalog().into_iter()
        .filter(|(_, _, exes, web)| if *web { crate::sysinfo::on_path("omarchy-launch-webapp") } else { exes.iter().any(|e| crate::sysinfo::on_path(e)) })
        .map(|(id, ..)| id).collect();
    DocInfo { err, ai, apps, detected: crate::usage::detected_providers().into_iter().map(|(i, l)| (i.to_string(), l.to_string())).collect(), installed }
}

/// Re-render app autostart from the saved `apps` with the installed helper.
fn run_apply() -> Result<(), String> {
    let script = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/waybar-notch/setup/waybar-setup.py");
    if !script.is_file() { return Err(format!("{} is missing; run ./install.sh once", script.display())); }
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script).arg("apply");
    if let Some(sig) = crate::hypr::sig() { cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig); }
    let out = cmd.output().map_err(|e| format!("cannot run python3: {e}"))?;
    if out.status.success() { return Ok(()); }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(if err.is_empty() { format!("helper exited with {}", out.status) } else { err.to_string() })
}

// ---------------------------------------------------------------- draft editing (pure)

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num { CollapsedW, CollapsedH, ExpandedW, TopMargin, CornerRadius, FontSize, CollapseDelay, HomeReset }

impl Num {
    fn range(self) -> (u32, u32, u32) {
        match self {
            Num::CollapsedW => (180, 420, 10), Num::CollapsedH => (24, 48, 1), Num::ExpandedW => (480, 960, 10), Num::TopMargin => (0, 24, 1),
            Num::CornerRadius => (0, 40, 1), Num::FontSize => (10, 18, 1), Num::CollapseDelay => (100, 2000, 50), Num::HomeReset => (0, 120, 5),
        }
    }
    fn get(self, d: &NotchSettings) -> u32 {
        match self {
            Num::CollapsedW => d.layout.collapsed_width, Num::CollapsedH => d.layout.collapsed_height, Num::ExpandedW => d.layout.expanded_width,
            Num::TopMargin => d.layout.top_margin, Num::CornerRadius => d.appearance.corner_radius, Num::FontSize => d.appearance.font_size,
            Num::CollapseDelay => d.behavior.collapse_delay_ms, Num::HomeReset => d.behavior.home_reset_secs,
        }
    }
    fn set(self, d: &mut NotchSettings, v: u32) {
        match self {
            Num::CollapsedW => d.layout.collapsed_width = v, Num::CollapsedH => d.layout.collapsed_height = v, Num::ExpandedW => d.layout.expanded_width = v,
            Num::TopMargin => d.layout.top_margin = v, Num::CornerRadius => d.appearance.corner_radius = v, Num::FontSize => d.appearance.font_size = v,
            Num::CollapseDelay => d.behavior.collapse_delay_ms = v, Num::HomeReset => d.behavior.home_reset_secs = v,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Flag { Hover, Animations, HideFullscreen }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ListKind { Tabs, Utilities }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Which { Background, Foreground, Accent }

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Step(Num, i32),
    Flag(Flag),
    Clock(ClockFormat),
    MonitorMode(MonitorMode),
    CycleOutput,
    ToggleList(ListKind, String),
    MoveList(ListKind, String, i32),
    CustomColor(Which, bool),
    FocusColor(Which),
    FocusFont,
    SystemFont,
    ToggleApp(String),
    BgApp(String),
    MoveApp(String, i32),
    ToggleAi(String),
    OpenApplications,
    Apply,
    Reset,
    Cancel,
}

fn list_ref(d: &mut NotchSettings, k: ListKind) -> &mut Vec<String> { match k { ListKind::Tabs => &mut d.tabs, ListKind::Utilities => &mut d.utilities } }

pub fn apply_to_draft(d: &mut NotchSettings, a: &Action, outputs: &[String]) {
    match a {
        Action::Step(n, dir) => {
            let (lo, hi, step) = n.range();
            let v = (n.get(d) as i64 + *dir as i64 * step as i64).clamp(lo as i64, hi as i64) as u32;
            n.set(d, v);
        }
        Action::Flag(f) => match f {
            Flag::Hover => d.behavior.open_on_hover = !d.behavior.open_on_hover,
            Flag::Animations => d.behavior.animations = !d.behavior.animations,
            Flag::HideFullscreen => d.behavior.hide_on_fullscreen = !d.behavior.hide_on_fullscreen,
        },
        Action::Clock(c) => d.behavior.clock_format = *c,
        Action::MonitorMode(m) => d.monitor.mode = *m,
        Action::CycleOutput => {
            if outputs.is_empty() { return; }
            let cur = d.monitor.output.as_ref().and_then(|o| outputs.iter().position(|x| x == o));
            let next = cur.map_or(0, |i| (i + 1) % outputs.len());
            d.monitor.output = Some(outputs[next].clone());
        }
        Action::ToggleList(k, id) => {
            let all: Vec<&str> = match k { ListKind::Tabs => TAB_LABELS.iter().map(|t| t.0).collect(), ListKind::Utilities => UTILITY_LABELS.iter().map(|t| t.0).collect() };
            let v = list_ref(d, *k);
            if let Some(p) = v.iter().position(|x| x == id) { v.remove(p); } else if all.contains(&id.as_str()) { v.push(id.clone()); }
            if *k == ListKind::Tabs { d.tabs = config::normalize_tabs(&d.tabs); }
        }
        Action::MoveList(k, id, delta) => {
            let v = list_ref(d, *k);
            if let Some(p) = v.iter().position(|x| x == id) {
                // tabs: home stays first and controls last; only optional pages are reorderable
                let (lo, hi) = if *k == ListKind::Tabs { (1usize, v.len().saturating_sub(2)) } else { (0, v.len().saturating_sub(1)) };
                if p >= lo && p <= hi {
                    let to = (p as i32 + delta).clamp(lo as i32, hi as i32) as usize;
                    v.swap(p, to);
                }
            }
        }
        Action::CustomColor(w, on) => {
            let slot = color_slot(d, *w);
            if !*on { *slot = None; } else if slot.is_none() { *slot = Some(default_color(*w)); }
        }
        _ => {}
    }
}

fn color_slot(d: &mut NotchSettings, w: Which) -> &mut Option<String> {
    match w { Which::Background => &mut d.appearance.background, Which::Foreground => &mut d.appearance.foreground, Which::Accent => &mut d.appearance.accent }
}

fn default_color(w: Which) -> String {
    let t = crate::theme::current();
    let c = match w { Which::Background => t.bg, Which::Foreground => t.fg, Which::Accent => t.accent };
    crate::theme::hex(c)
}

// ---------------------------------------------------------------- page

const ID_ACT0: u32 = 1000;
const ID_COLOR0: u32 = 100;
const ID_FONT: u32 = 110;

pub struct SettingsPage {
    hits: Hits,
    scroll: Scroll,
    draft: NotchSettings,
    active: NotchSettings,
    doc: Option<DocInfo>,
    ai_draft: Option<Vec<String>>,
    apps_draft: Vec<AppSel>,
    apps_base: Vec<AppSel>,
    apps_pending: bool,
    colors: [TextField; 3],
    font: TextField,
    bad: HashSet<&'static str>,
    status: String,
    busy: bool,
    actions: Vec<(u32, Action)>,
    focus: Option<usize>,
    loaded: bool,
    view_h: f32,
}

impl SettingsPage {
    pub fn new() -> Self {
        let mut p = SettingsPage {
            hits: Hits::default(), scroll: Scroll::default(), draft: NotchSettings::default(), active: NotchSettings::default(), doc: None,
            ai_draft: None, apps_draft: vec![], apps_base: vec![], apps_pending: false,
            colors: [TextField::new("#RRGGBB"), TextField::new("#RRGGBB"), TextField::new("#RRGGBB")], font: TextField::new("System default"),
            bad: HashSet::new(), status: String::new(), busy: false, actions: vec![], focus: None, loaded: false, view_h: 0.0,
        };
        p.font.placeholder = "System default".into();
        p
    }

    fn sync_fields(&mut self) {
        for (i, w) in [Which::Background, Which::Foreground, Which::Accent].into_iter().enumerate() {
            let cur = color_slot(&mut self.draft.clone(), w).clone();
            self.colors[i].set_text(cur.as_deref().unwrap_or(""));
            self.colors[i].focused = false;
        }
        self.font.set_text(self.draft.appearance.font_family.as_deref().unwrap_or(""));
        self.font.focused = false;
        self.bad.clear();
        self.focus = None;
    }

    fn dirty_notes(&self) -> bool { !self.bad.is_empty() || self.doc.as_ref().is_some_and(|d| d.err.is_some()) }

    fn outputs(st: &State) -> Vec<String> {
        let mut v: Vec<String> = st.outputs.iter().map(|o| o.name.clone()).collect();
        v.sort();
        v
    }

    fn do_apply(&mut self, ctx: &mut Ctx) {
        if self.busy || self.dirty_notes() { return; }
        // pins are edited only on the Applications page, under the config lock; Apply never writes them
        let mut draft = self.draft.clone();
        draft.pinned_apps = self.active.pinned_apps.clone();
        let mut patch: Map<String, Value> = Map::new();
        let np = config::diff_patch(&self.active.to_json(), &draft.to_json());
        if !np.is_empty() { patch.insert("notch".into(), Value::Object(np)); }
        let ai_changed = self.doc.as_ref().is_some_and(|d| d.ai != self.ai_draft) && self.ai_draft.is_some();
        if ai_changed { patch.insert("ai".into(), json!(self.ai_draft.clone().unwrap_or_default())); }
        let apps = self.apps_draft.clone();
        let apps_changed = apps != self.apps_base;
        if apps_changed { patch.insert("apps".into(), Value::Array(apps.iter().map(|a| json!({"id": a.id, "background": a.background})).collect())); }
        let run_apps = apps_changed || self.apps_pending;
        self.busy = true;
        self.status = if run_apps { "Saving and applying…".into() } else { "Saving…".into() };
        let poster = ctx.poster.clone();
        std::thread::spawn(move || {
            let saved = if patch.is_empty() { Ok(()) } else { config::save_patch(&patch) };
            let out: Result<(NotchSettings, Option<String>), String> = match saved {
                Err(e) => Err(e),
                Ok(()) if run_apps => match run_apply() { Ok(()) => Ok((draft, None)), Err(e) => Ok((draft, Some(e))) },
                Ok(()) => Ok((draft, None)),
            };
            poster.post(move |st, _| { st.pending_settings_result = Some(out); });
        });
    }

    /// Called every tick: consume a finished save.
    fn take_result(&mut self, st: &mut State) {
        let Some(res) = st.pending_settings_result.take() else { return };
        self.busy = false;
        match res {
            Ok((mut n, apps_err)) => {
                n.pinned_apps = st.settings.pinned_apps.clone();
                self.active = n.clone();
                st.settings = n;
                st.refresh_palette();
                self.apps_base = self.apps_draft.clone();
                if let Some(d) = self.doc.as_mut() { d.ai = self.ai_draft.clone(); }
                match apps_err {
                    None => { self.apps_pending = false; self.status = "Saved and applied".into(); }
                    Some(e) => { self.apps_pending = true; self.status = format!("Preferences saved; app changes not applied: {e}"); }
                }
            }
            Err(e) => self.status = format!("Not saved: {e}"),
        }
    }

    fn act(&mut self, a: Action, ctx: &mut Ctx) {
        match &a {
            Action::Apply => self.do_apply(ctx),
            Action::OpenApplications => ctx.state.nav = Some(NavTo::Page("apps")),
            Action::Cancel => { self.draft = self.active.clone(); self.sync_fields(); ctx.state.nav = Some(NavTo::Page("controls")); }
            Action::Reset => {
                let d = NotchSettings::default();
                self.draft.appearance = d.appearance; self.draft.layout = d.layout; self.draft.behavior = d.behavior;
                self.draft.monitor = d.monitor; self.draft.tabs = d.tabs; self.draft.utilities = d.utilities;
                self.sync_fields();
            }
            Action::CustomColor(w, on) => {
                apply_to_draft(&mut self.draft, &a, &[]);
                let i = *w as usize;
                self.colors[i].set_text(color_slot(&mut self.draft, *w).as_deref().unwrap_or(""));
                if !*on { self.bad.remove(color_label(*w)); }
            }
            Action::FocusColor(w) => { self.focus = Some(*w as usize); for (i, f) in self.colors.iter_mut().enumerate() { f.focused = i == *w as usize; } self.font.focused = false; }
            Action::FocusFont => { self.focus = Some(10); self.font.focused = true; for f in self.colors.iter_mut() { f.focused = false; } }
            Action::SystemFont => { self.draft.appearance.font_family = None; self.font.clear(); self.bad.remove("font"); }
            Action::ToggleApp(id) => {
                if let Some(p) = self.apps_draft.iter().position(|x| &x.id == id) { self.apps_draft.remove(p); } else { self.apps_draft.push(AppSel { id: id.clone(), background: false }); }
            }
            Action::BgApp(id) => { if let Some(a) = self.apps_draft.iter_mut().find(|x| &x.id == id) { a.background = !a.background; } }
            Action::MoveApp(id, delta) => {
                if let Some(p) = self.apps_draft.iter().position(|x| &x.id == id) {
                    let to = (p as i32 + delta).clamp(0, self.apps_draft.len() as i32 - 1) as usize;
                    self.apps_draft.swap(p, to);
                }
            }
            Action::ToggleAi(id) => {
                let detected: Vec<String> = self.doc.as_ref().map(|d| d.detected.iter().map(|(i, _)| i.clone()).collect()).unwrap_or_default();
                let mut cur = self.ai_draft.clone().unwrap_or(detected);
                if let Some(p) = cur.iter().position(|x| x == id) { cur.remove(p); } else { cur.push(id.clone()); }
                self.ai_draft = Some(cur);
            }
            _ => apply_to_draft(&mut self.draft, &a, &Self::outputs(ctx.state)),
        }
    }

    fn commit_color(&mut self, w: Which) {
        let i = w as usize;
        let text = self.colors[i].text.clone();
        if config::valid_hex_color(&text) { self.bad.remove(color_label(w)); *color_slot(&mut self.draft, w) = Some(text); } else { self.bad.insert(color_label(w)); }
    }

    fn commit_font(&mut self) {
        let t = self.font.text.trim().to_string();
        if t.is_empty() { self.bad.remove("font"); self.draft.appearance.font_family = None; }
        else if config::valid_font_family(&t) { self.bad.remove("font"); self.draft.appearance.font_family = Some(t); }
        else { self.bad.insert("font"); }
    }
}

fn color_label(w: Which) -> &'static str { match w { Which::Background => "bg", Which::Foreground => "fg", Which::Accent => "accent" } }

const ROW: f32 = 32.0;

enum Item {
    Section(&'static str),
    Hint(String),
    Flag(&'static str, Flag),
    Num(&'static str, Num),
    Color(&'static str, Which),
    Font,
    Clock,
    Mode,
    Output,
    ListRow(ListKind, &'static str, String),
    AppRow(String, String, bool, bool, bool, bool, bool),
    AiRow(String, String, bool),
    Link(&'static str),
}

impl Page for SettingsPage {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        let _ = st;
        Size { w: avail.w, h: 440.0_f32.min(avail.h) }
    }

    fn keeps_open(&self) -> bool { true }
    fn wants_keyboard(&self) -> bool { self.focus.is_some() }
    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn shown(&mut self, ctx: &mut Ctx) {
        self.active = ctx.state.settings.clone();
        self.draft = self.active.clone();
        self.sync_fields();
        self.status.clear();
        self.scroll.offset = 0.0;
        self.loaded = false;
        let p = ctx.poster.clone();
        std::thread::spawn(move || { let d = load_doc_info(); p.post(move |st, _| { st.settings_doc = Some(Box::new(d)); }); });
    }

    fn tick(&mut self, _: f32, ctx: &mut Ctx) -> bool {
        self.take_result(ctx.state);
        if let Some(d) = ctx.state.settings_doc.take() {
            let d: DocInfo = *d;
            self.ai_draft = d.ai.clone();
            self.apps_draft = d.apps.clone();
            self.apps_base = d.apps.clone();
            self.doc = Some(d);
            self.loaded = true;
        }
        false
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal: &Palette = &st.pal;
        self.hits.clear();
        self.actions.clear();
        self.view_h = area.h - 76.0;
        // header
        let back = rect(area.x, area.y, 60.0, 26.0);
        let hb = self.hits.add(back, ID_ACT0);
        self.actions.push((ID_ACT0, Action::Cancel));
        button(cv, pal, back, "\u{f053} Back", ButtonKind::Plain, hb, false);
        cv.text("Settings", &TextStyle::new(pal.fs(15.0), pal.fg_hi).bold(), pt(area.x + 72.0, area.y + 13.0), HAlign::Left, VAlign::Middle);

        let list = rect(area.x, area.y + 34.0, area.w, area.h - 34.0 - 40.0);
        let items = self.items(st);
        let mut y = list.y - self.scroll.offset;
        let mut next_id = ID_ACT0 + 1;
        let mut total = 0.0;
        cv.with_clip(list, [0.0; 4], |cv| {
            for it in &items {
                let h = match it { Item::Section(_) => 30.0, Item::Hint(_) => 20.0, Item::AppRow(..) => 40.0, _ => ROW };
                let r = rect(list.x, y, list.w - 8.0, h);
                let vis = r.bottom() > list.y && r.y < list.bottom();
                if vis { self.draw_item(cv, pal, it, r, list, &mut next_id); } else { next_id += 3; }
                y += h; total += h;
            }
        });
        self.scroll.content_h = total;
        self.scroll.clamp(list.h);
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);

        // footer
        let foot = rect(area.x, area.bottom() - 32.0, area.w, 28.0);
        let reset = rect(foot.x, foot.y, 150.0, 28.0);
        let cancel = rect(foot.right() - 170.0, foot.y, 76.0, 28.0);
        let apply = rect(foot.right() - 88.0, foot.y, 88.0, 28.0);
        for (r, label, act, kind) in [(reset, "Reset to defaults", Action::Reset, ButtonKind::Plain), (cancel, "Cancel", Action::Cancel, ButtonKind::Plain), (apply, "Apply", Action::Apply, ButtonKind::Accent)] {
            let id = next_id; next_id += 1;
            let hov = self.hits.add(r, id);
            self.actions.push((id, act.clone()));
            let can = !(act == Action::Apply && (self.busy || self.dirty_notes()));
            cv.with_alpha(if can { 1.0 } else { 0.45 }, |cv| button(cv, pal, r, label, kind, hov && can, false));
        }
        cv.text_in(&self.status, &pal.text_dim(10.5), rect(reset.right() + 8.0, foot.y, cancel.x - reset.right() - 16.0, foot.h), HAlign::Left);
        let _ = next_id;
    }

    fn event(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx) {
        match ev {
            Event::Scroll { dy, .. } => { self.scroll.by(*dy, self.view_h.max(area.h - 76.0)); return; }
            Event::Key(k) if self.focus.is_some() => {
                use smithay_client_toolkit::seat::keyboard::Keysym;
                let idx = self.focus.unwrap();
                if idx == 10 {
                    match self.font.key(k) { FieldResult::Submit | FieldResult::Cancel => { self.font.focused = false; self.focus = None; } _ => {} }
                    self.commit_font();
                } else {
                    let w = [Which::Background, Which::Foreground, Which::Accent][idx];
                    match self.colors[idx].key(k) { FieldResult::Submit | FieldResult::Cancel => { self.colors[idx].focused = false; self.focus = None; } _ => {} }
                    self.commit_color(w);
                }
                let _ = Keysym::Escape;
                return;
            }
            _ => {}
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        if let Some(a) = self.actions.iter().find(|(id, _)| *id == c.id).map(|(_, a)| a.clone()) { self.act(a, ctx); }
    }
}

impl SettingsPage {
    fn items(&self, st: &State) -> Vec<Item> {
        let mut v = vec![Item::Section("Appearance"), Item::Color("Background", Which::Background), Item::Color("Text", Which::Foreground), Item::Color("Accent", Which::Accent), Item::Font,
            Item::Num("Notch font size (px)", Num::FontSize), Item::Num("Corner radius", Num::CornerRadius),
            Item::Section("Layout"), Item::Num("Collapsed width", Num::CollapsedW), Item::Num("Collapsed height", Num::CollapsedH), Item::Num("Maximum expanded width", Num::ExpandedW), Item::Num("Top margin", Num::TopMargin),
            Item::Section("Behavior"), Item::Flag("Open on hover", Flag::Hover), Item::Num("Collapse delay (ms)", Num::CollapseDelay), Item::Num("Return to Home after (s, 0 = never)", Num::HomeReset),
            Item::Flag("Animations", Flag::Animations), Item::Flag("Hide on fullscreen", Flag::HideFullscreen), Item::Clock,
            Item::Section("Monitors"), Item::Mode];
        if self.draft.monitor.mode == MonitorMode::Fixed { v.push(Item::Output); }
        v.push(Item::Section("Tabs"));
        for id in self.draft.tabs.iter().filter(|t| *t != "home" && *t != "controls") {
            if let Some((i, l)) = TAB_LABELS.iter().find(|t| t.0 == id) { v.push(Item::ListRow(ListKind::Tabs, l, i.to_string())); }
        }
        for (i, l) in TAB_LABELS { if !self.draft.tabs.iter().any(|t| t == i) { v.push(Item::ListRow(ListKind::Tabs, l, i.to_string())); } }
        v.push(Item::Section("Utilities"));
        for id in &self.draft.utilities { if let Some((i, l)) = UTILITY_LABELS.iter().find(|t| t.0 == id) { v.push(Item::ListRow(ListKind::Utilities, l, i.to_string())); } }
        for (i, l) in UTILITY_LABELS { if !self.draft.utilities.iter().any(|t| t == i) { v.push(Item::ListRow(ListKind::Utilities, l, i.to_string())); } }
        v.push(Item::Section("AI usage"));
        match &self.doc {
            None => v.push(Item::Hint("Loading…".into())),
            Some(d) if d.detected.is_empty() => v.push(Item::Hint("No AI tool logs detected on this machine".into())),
            Some(d) => for (id, label) in &d.detected {
                let on = self.ai_draft.as_ref().is_none_or(|s| s.iter().any(|x| x == id));
                v.push(Item::AiRow(id.clone(), label.clone(), on));
            },
        }
        v.push(Item::Section("Apps"));
        v.push(Item::Link("Applications"));
        if let Some(d) = &self.doc {
            let names: HashMap<String, String> = catalog().into_iter().map(|a| (a.0, a.1)).collect();
            let mut order: Vec<String> = self.apps_draft.iter().map(|a| a.id.clone()).collect();
            for (id, ..) in catalog() { if !order.contains(&id) { order.push(id); } }
            let n = order.len();
            for (i, id) in order.iter().enumerate() {
                let Some(name) = names.get(id) else { continue };
                let sel = self.apps_draft.iter().find(|a| &a.id == id);
                let label = format!("{name}{}", if d.installed.contains(id) { "" } else { " (not installed)" });
                v.push(Item::AppRow(id.clone(), label, sel.is_some(), sel.is_some_and(|a| a.background), i > 0 && sel.is_some(), i + 1 < n && sel.is_some(), true));
            }
        } else { v.push(Item::Hint("Loading…".into())); }
        let _ = st;
        v
    }

    fn reg(&mut self, hits_rect: Rect, list: Rect, next_id: &mut u32, a: Action, cv_hover: &mut bool) {
        let id = *next_id;
        *next_id += 1;
        *cv_hover = self.hits.add_in(hits_rect, list, id);
        self.actions.push((id, a));
    }

    fn draw_item(&mut self, cv: &mut Canvas, pal: &Palette, it: &Item, r: Rect, list: Rect, next_id: &mut u32) {
        let lab = |cv: &mut Canvas, t: &str| { cv.text_in(t, &pal.text(12.0), rect(r.x + 4.0, r.y, r.w * 0.5 - 8.0, r.h), HAlign::Left); };
        let right = rect(r.x + r.w * 0.5, r.y, r.w * 0.5, r.h);
        match it {
            Item::Section(t) => { cv.text(t, &TextStyle::new(pal.fs(13.0), pal.accent_hi).bold(), pt(r.x + 4.0, r.y + 18.0), HAlign::Left, VAlign::Middle); cv.fill_rect(rect(r.x, r.bottom() - 2.0, r.w, 1.0), with_a(pal.fg, 0.1)); *next_id += 3; }
            Item::Hint(t) => { cv.text_in(t, &pal.text_dim(11.0), rect(r.x + 4.0, r.y, r.w, r.h), HAlign::Left); *next_id += 3; }
            Item::Flag(l, f) => {
                lab(cv, l);
                let on = match f { Flag::Hover => self.draft.behavior.open_on_hover, Flag::Animations => self.draft.behavior.animations, Flag::HideFullscreen => self.draft.behavior.hide_on_fullscreen };
                let sw = rect(right.right() - 44.0, r.y + 4.0, 44.0, 24.0);
                let mut h = false;
                self.reg(sw, list, next_id, Action::Flag(*f), &mut h);
                toggle(cv, pal, sw, if on { 1.0 } else { 0.0 });
                *next_id += 2;
            }
            Item::Num(l, n) => {
                lab(cv, l);
                let v = n.get(&self.draft);
                let minus = rect(right.right() - 110.0, r.y + 3.0, 26.0, 26.0);
                let plus = rect(right.right() - 26.0, r.y + 3.0, 26.0, 26.0);
                for (rr, g, d) in [(minus, "−", -1), (plus, "+", 1)] {
                    let mut h = false;
                    self.reg(rr, list, next_id, Action::Step(*n, d), &mut h);
                    cv.fill_rrect(rr, r_all(9.0), with_a(pal.fg, if h { 0.18 } else { 0.08 }));
                    cv.text(g, &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), rr.center(), HAlign::Center, VAlign::Middle);
                }
                cv.text(&v.to_string(), &TextStyle::new(pal.fs(12.0), pal.fg_hi).bold(), pt((minus.right() + plus.x) / 2.0, r.y + 16.0), HAlign::Center, VAlign::Middle);
                *next_id += 1;
            }
            Item::Color(l, w) => {
                lab(cv, l);
                let custom = color_slot(&mut self.draft.clone(), *w).is_some();
                let sw = rect(right.x, r.y + 4.0, 44.0, 24.0);
                let mut h = false;
                self.reg(sw, list, next_id, Action::CustomColor(*w, !custom), &mut h);
                toggle(cv, pal, sw, if custom { 1.0 } else { 0.0 });
                let f = rect(sw.right() + 8.0, r.y + 2.0, right.w - 52.0, 28.0);
                let i = *w as usize;
                if custom {
                    let id = *next_id; *next_id += 1;
                    self.hits.add_in(f, list, id);
                    self.actions.push((id, Action::FocusColor(*w)));
                    self.colors[i].draw(cv, pal, f);
                    if self.bad.contains(color_label(*w)) { cv.stroke_rrect(f, r_all(10.0), 1.5, pal.red); }
                    if let Some(c) = crate::render::parse_color(&self.colors[i].text) { cv.fill_circle(pt(f.right() - 14.0, f.center().y), 6.0, c); }
                } else { cv.text_in("Follows the Omarchy theme", &pal.text_dim(11.0), f, HAlign::Left); *next_id += 1; }
                *next_id += 1;
            }
            Item::Font => {
                lab(cv, "Font family (notch and pills)");
                let f = rect(right.x, r.y + 2.0, right.w - 70.0, 28.0);
                let id = *next_id; *next_id += 1;
                self.hits.add_in(f, list, id);
                self.actions.push((id, Action::FocusFont));
                self.font.draw(cv, pal, f);
                if self.bad.contains("font") { cv.stroke_rrect(f, r_all(10.0), 1.5, pal.red); }
                let b = rect(f.right() + 4.0, r.y + 4.0, 62.0, 24.0);
                let mut h = false;
                self.reg(b, list, next_id, Action::SystemFont, &mut h);
                button(cv, pal, b, "System", ButtonKind::Plain, h, false);
                *next_id += 1;
            }
            Item::Clock => {
                lab(cv, "Clock format");
                let opts = [("Locale", ClockFormat::Locale), ("12-hour", ClockFormat::H12), ("24-hour", ClockFormat::H24)];
                let w = right.w / 3.0;
                for (i, (t, f)) in opts.iter().enumerate() {
                    let rr = rect(right.x + i as f32 * w, r.y + 3.0, w - 4.0, 26.0);
                    let mut h = false;
                    self.reg(rr, list, next_id, Action::Clock(*f), &mut h);
                    seg(cv, pal, rr, t, self.draft.behavior.clock_format == *f, h);
                }
            }
            Item::Link(l) => {
                lab(cv, l);
                let b = rect(right.right() - 70.0, r.y + 4.0, 70.0, 24.0);
                let mut h = false;
                self.reg(b, list, next_id, Action::OpenApplications, &mut h);
                button(cv, pal, b, "Open", ButtonKind::Plain, h, false);
                *next_id += 2;
            }
            Item::Mode => {
                lab(cv, "Notch and pills");
                let opts = [("Follow focus", MonitorMode::Follow), ("One screen", MonitorMode::Fixed)];
                let w = right.w / 2.0;
                for (i, (t, m)) in opts.iter().enumerate() {
                    let rr = rect(right.x + i as f32 * w, r.y + 3.0, w - 4.0, 26.0);
                    let mut h = false;
                    self.reg(rr, list, next_id, Action::MonitorMode(*m), &mut h);
                    seg(cv, pal, rr, t, self.draft.monitor.mode == *m, h);
                }
                *next_id += 1;
            }
            Item::Output => {
                lab(cv, "Screen");
                let name = self.draft.monitor.output.clone().unwrap_or_else(|| "Choose…".into());
                let rr = rect(right.x, r.y + 3.0, right.w - 4.0, 26.0);
                let mut h = false;
                self.reg(rr, list, next_id, Action::CycleOutput, &mut h);
                seg(cv, pal, rr, &format!("{name}  \u{f0d7}"), false, h);
                *next_id += 2;
            }
            Item::ListRow(k, label, id) => {
                let (enabled, can_move, pos_ok) = {
                    let v = match k { ListKind::Tabs => &self.draft.tabs, ListKind::Utilities => &self.draft.utilities };
                    let p = v.iter().position(|x| x == id);
                    let n = v.len();
                    let (lo, hi) = if *k == ListKind::Tabs { (1, n.saturating_sub(2)) } else { (0, n.saturating_sub(1)) };
                    (p.is_some(), p, p.filter(|p| *p >= lo && *p <= hi).map(|p| (p > lo, p < hi)))
                };
                let _ = can_move;
                let chk = rect(r.x + 4.0, r.y + 7.0, 18.0, 18.0);
                let mut h = false;
                self.reg(rect(r.x, r.y, r.w - 70.0, r.h), list, next_id, Action::ToggleList(*k, id.clone()), &mut h);
                cv.fill_rrect(chk, r_all(5.0), if enabled { pal.accent } else { with_a(pal.fg, if h { 0.24 } else { 0.14 }) });
                if enabled { cv.text("\u{f00c}", &TextStyle::icon(11.0, pal.bg), chk.center(), HAlign::Center, VAlign::Middle); }
                cv.text_in(label, &pal.text(12.0), rect(r.x + 30.0, r.y, r.w - 110.0, r.h), HAlign::Left);
                if let Some((up, down)) = pos_ok {
                    for (i, (g, d, ok)) in [("\u{f077}", -1, up), ("\u{f078}", 1, down)].into_iter().enumerate() {
                        let rr = rect(r.right() - 60.0 + i as f32 * 30.0, r.y + 3.0, 26.0, 26.0);
                        if ok {
                            let mut h = false;
                            self.reg(rr, list, next_id, Action::MoveList(*k, id.clone(), d), &mut h);
                            cv.fill_rrect(rr, r_all(9.0), with_a(pal.fg, if h { 0.18 } else { 0.08 }));
                            cv.text(g, &TextStyle::icon(10.0, pal.fg), rr.center(), HAlign::Center, VAlign::Middle);
                        } else { *next_id += 1; }
                    }
                } else { *next_id += 2; }
            }
            Item::AiRow(id, label, on) => {
                let chk = rect(r.x + 4.0, r.y + 7.0, 18.0, 18.0);
                let mut h = false;
                self.reg(rect(r.x, r.y, r.w, r.h), list, next_id, Action::ToggleAi(id.clone()), &mut h);
                cv.fill_rrect(chk, r_all(5.0), if *on { pal.accent } else { with_a(pal.fg, if h { 0.24 } else { 0.14 }) });
                if *on { cv.text("\u{f00c}", &TextStyle::icon(11.0, pal.bg), chk.center(), HAlign::Center, VAlign::Middle); }
                cv.text_in(label, &pal.text(12.0), rect(r.x + 30.0, r.y, r.w - 34.0, r.h), HAlign::Left);
                *next_id += 2;
            }
            Item::AppRow(id, label, shown, bg, up, down, _) => {
                let chk = rect(r.x + 4.0, r.y + 4.0, 18.0, 18.0);
                let mut h = false;
                self.reg(rect(r.x, r.y, r.w - 70.0, 28.0), list, next_id, Action::ToggleApp(id.clone()), &mut h);
                cv.fill_rrect(chk, r_all(5.0), if *shown { pal.accent } else { with_a(pal.fg, if h { 0.24 } else { 0.14 }) });
                if *shown { cv.text("\u{f00c}", &TextStyle::icon(11.0, pal.bg), chk.center(), HAlign::Center, VAlign::Middle); }
                cv.text_in(label, &pal.text(12.0), rect(r.x + 30.0, r.y, r.w - 110.0, 26.0), HAlign::Left);
                for (i, (g, d, ok)) in [("\u{f077}", -1, *up), ("\u{f078}", 1, *down)].into_iter().enumerate() {
                    let rr = rect(r.right() - 60.0 + i as f32 * 30.0, r.y + 1.0, 26.0, 26.0);
                    if ok {
                        let mut h = false;
                        self.reg(rr, list, next_id, Action::MoveApp(id.clone(), d), &mut h);
                        cv.fill_rrect(rr, r_all(9.0), with_a(pal.fg, if h { 0.18 } else { 0.08 }));
                        cv.text(g, &TextStyle::icon(10.0, pal.fg), rr.center(), HAlign::Center, VAlign::Middle);
                    } else { *next_id += 1; }
                }
                if *shown {
                    let c2 = rect(r.x + 30.0, r.y + 22.0, 16.0, 16.0);
                    let mut h2 = false;
                    self.reg(rect(r.x + 26.0, r.y + 20.0, 120.0, 20.0), list, next_id, Action::BgApp(id.clone()), &mut h2);
                    cv.fill_rrect(c2, r_all(4.0), if *bg { pal.accent } else { with_a(pal.fg, if h2 { 0.24 } else { 0.14 }) });
                    if *bg { cv.text("\u{f00c}", &TextStyle::icon(9.0, pal.bg), c2.center(), HAlign::Center, VAlign::Middle); }
                    cv.text("Background", &pal.text_dim(11.0), pt(c2.right() + 6.0, c2.center().y), HAlign::Left, VAlign::Middle);
                }
            }
        }
    }
}

fn seg(cv: &mut Canvas, pal: &Palette, r: Rect, label: &str, on: bool, hov: bool) {
    cv.fill_rrect(r, r_all(10.0), if on { with_a(pal.accent, 0.28) } else { with_a(pal.fg, if hov { 0.12 } else { 0.06 }) });
    cv.text_in(label, &TextStyle::new(pal.fs(11.0), if on { pal.accent_hi } else { pal.fg }).weight(600), r.pad_xy(4.0, 0.0), HAlign::Center);
}

/// Reload preferences from disk when the config file changes outside the app.
pub fn watch_config(poster: &Poster) {
    let p = poster.clone();
    std::thread::spawn(move || {
        let path = config::path();
        let mtime = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last = mtime(&path);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let now = mtime(&path);
            if now != last {
                last = now;
                let (s, _) = config::load_settings();
                p.post(move |st, _| { if st.settings != s { st.settings = s; st.refresh_palette(); } });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_clamp_to_range() {
        let mut d = NotchSettings::default();
        for _ in 0..100 { apply_to_draft(&mut d, &Action::Step(Num::CornerRadius, 1), &[]); }
        assert_eq!(d.appearance.corner_radius, 40);
        for _ in 0..100 { apply_to_draft(&mut d, &Action::Step(Num::CollapsedH, -1), &[]); }
        assert_eq!(d.layout.collapsed_height, 24);
    }

    #[test]
    fn toggles_and_lists() {
        let mut d = NotchSettings::default();
        apply_to_draft(&mut d, &Action::Flag(Flag::Hover), &[]);
        assert!(!d.behavior.open_on_hover);
        apply_to_draft(&mut d, &Action::ToggleList(ListKind::Tabs, "media".into()), &[]);
        assert!(!d.tabs.contains(&"media".to_string()));
        assert_eq!(d.tabs.first().map(String::as_str), Some("home"));
        assert_eq!(d.tabs.last().map(String::as_str), Some("controls"));
        apply_to_draft(&mut d, &Action::ToggleList(ListKind::Tabs, "media".into()), &[]);
        assert!(d.tabs.contains(&"media".to_string()));
        // home/controls never move
        let before = d.tabs.clone();
        apply_to_draft(&mut d, &Action::MoveList(ListKind::Tabs, "home".into(), 1), &[]);
        assert_eq!(d.tabs, before);
        apply_to_draft(&mut d, &Action::MoveList(ListKind::Utilities, "screenshot".into(), 1), &[]);
        assert_eq!(d.utilities[1], "screenshot");
    }

    #[test]
    fn output_cycles_and_custom_color_toggles() {
        let mut d = NotchSettings::default();
        let outs = vec!["A".to_string(), "B".to_string()];
        apply_to_draft(&mut d, &Action::CycleOutput, &outs);
        assert_eq!(d.monitor.output.as_deref(), Some("A"));
        apply_to_draft(&mut d, &Action::CycleOutput, &outs);
        apply_to_draft(&mut d, &Action::CycleOutput, &outs);
        assert_eq!(d.monitor.output.as_deref(), Some("A"));
        apply_to_draft(&mut d, &Action::CustomColor(Which::Accent, true), &[]);
        assert!(d.appearance.accent.as_deref().is_some_and(config::valid_hex_color));
        apply_to_draft(&mut d, &Action::CustomColor(Which::Accent, false), &[]);
        assert!(d.appearance.accent.is_none());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::page::testutil::Harness;

    #[test]
    fn render_settings() {
        let mut st = State::new();
        let mut h = Harness::new();
        let mut p = SettingsPage::new();
        p.active = st.settings.clone();
        p.draft = p.active.clone();
        p.doc = Some(DocInfo { installed: ["discord".to_string()].into_iter().collect(), ..Default::default() });
        p.apps_draft = vec![AppSel { id: "discord".into(), background: false }];
        p.sync_fields();
        h.render(&mut p, &mut st, "/tmp/settings_top.png");
        p.scroll.offset = 400.0;
        h.render(&mut p, &mut st, "/tmp/settings_mid.png");
    }
}
