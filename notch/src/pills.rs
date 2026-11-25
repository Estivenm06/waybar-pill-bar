//! Side pills: the LEFT pill (launcher, Hyprland workspaces, app buttons) and the per-output shell that keeps one
//! left and one right pill (`rightpill::right`) on every connected output.
//!
//! Geometry follows the former Waybar bar: bar height 26, 6px side margin, the pill hangs from the top edge with
//! `border-radius: 0 0 14px 14px` and 2px of margin. The left pill's surface spans the output width (anchored
//! left+right) because the compositor honours an exclusive zone only for a surface anchored to one edge or to an
//! edge plus both perpendicular ones; the input region is just the pill.
use crate::apps::{AppState, run_detached};
use crate::desktop::WsButton;
use crate::hypr;
use crate::layout::{Rect, pt, rect};
use crate::render::{Canvas, Color, HAlign, TextStyle, VAlign, mix, r_all, r_bottom, rgb, with_a};
use crate::state::State;
use crate::ui::{Hits, Palette};
use crate::render::TextSys;
use crate::wayland::{Button, Cmd, Ctx, Cursor, DrawCtx, Event, KbMode, PanelId, PanelSpec, Shell, View};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Bar height; also the exclusive zone the left pill reserves.
const BAR_H: f32 = 26.0;
const BAR_MARGIN: i32 = 6;
/// `#left { margin: 0 2px }`
const PILL_X: f32 = 2.0;
const PILL_RADIUS: f32 = 14.0;
const MODULE_RADIUS: f32 = 8.0;
/// `#left { padding: 0 6px }` plus the 1px border.
const PILL_PAD: f32 = 7.0;
/// Content row: 1px under the top, 1px above the bottom border.
const ROW_Y: f32 = 1.0;
const ROW_H: f32 = BAR_H - 3.0;
const ICON_PX: f32 = 14.0;
const WS_PX: f32 = 11.5;
/// Seconds for a hover highlight to fade in or out.
const HOVER_FADE: f32 = 0.18;

const ID_LAUNCHER: u32 = 1;
/// Workspace hit ids are `ID_WS + workspace id`.
const ID_WS: u32 = 1000;
/// The whole left pill, outer margin included, takes at most this share of the output width.
const LEFT_SHARE: f32 = 0.25;
/// Keyboard label: separator width, text padding and maximum label width.
const KB_SEP_W: f32 = 7.0;
const KB_PAD: f32 = 12.0;
const KB_MAX_W: f32 = 48.0;

/// Air kept between the collapsed notch and the windows below it.
const NOTCH_CLEARANCE: f32 = 4.0;
const LAUNCHER_GLYPH: &str = "\u{f303}";

/// Brand colours of the catalog apps.
pub(crate) fn brand(id: &str, pal: &Palette) -> Color {
    let c = match id {
        "discord" => (0x58, 0x65, 0xf2),
        "steam" => (0x66, 0xc0, 0xf4),
        "spotify" => (0x1e, 0xd7, 0x60),
        "whatsapp" => (0x25, 0xd3, 0x66),
        _ => return pal.accent,
    };
    let c = rgb(c.0, c.1, c.2);
    if pal.is_light { mix(c, rgb(0, 0, 0), 0.25) } else { c }
}

// ------------------------------------------------------------------ layout

struct Layout {
    pill: Rect,
    launcher: Rect,
    ws: Vec<(Rect, WsButton, String)>,
    /// Clip of the workspace strip when the workspaces exhaust the budget, with its maximum scroll.
    strip: Option<(Rect, f32)>,
    /// Keyboard-layout label rect, its text, and the x of the separator before it.
    kb: Option<(Rect, String, f32)>,
}

fn output_width(st: &State, output: &str) -> f32 {
    st.outputs.iter().find(|o| o.name == output).map_or(1200.0, |o| o.size.0 as f32)
}

/// Right edge (pill coordinates) the pill must not pass: the whole pill with its outer margin takes at most a
/// quarter of the logical output width.
fn limit(st: &State, output: &str) -> f32 {
    (output_width(st, output) * LEFT_SHARE - BAR_MARGIN as f32 - PILL_X).max(0.0)
}

fn ws_style(color: Color) -> TextStyle { TextStyle::new(WS_PX, color).weight(500) }
fn icon_style(color: Color) -> TextStyle { TextStyle::icon(ICON_PX, color) }

/// Content-driven widths, the `#left` CSS box model: launcher 5px padding, workspace buttons min 18/22px with 4px
/// padding. Measures only; painting and `occupied_width` share it.
fn layout(text: &mut TextSys, st: &State, output: &str, ws_scroll: f32) -> Layout {
    let pal = &st.pal;
    let limit = limit(st, output);
    let mut x = PILL_X + PILL_PAD;
    let launcher_w = text.measure(LAUNCHER_GLYPH, &icon_style(pal.fg), None).0 + 10.0;
    x += 1.0;
    let launcher = rect(x, ROW_Y, launcher_w, ROW_H);
    x += launcher_w + 1.0;
    let after_launcher = x;

    // the layout label goes first; it is dropped before anything else is clipped
    let mut kb = st.desk.keyboard.as_ref().map(|k| (k.clone(), (text.measure(k, &ws_style(pal.fg), None).0 + KB_PAD).min(KB_MAX_W)));
    if kb.as_ref().is_some_and(|(_, w)| after_launcher + KB_SEP_W + w + PILL_PAD > limit) { kb = None; }
    let kb_total = kb.as_ref().map_or(0.0, |(_, w)| KB_SEP_W + w);

    let buttons = st.desk.workspace_buttons(output);
    let mut ws = Vec::new();
    let mut ws_end = x;
    if !buttons.is_empty() {
        ws_end += 1.0 + 2.0; // #workspaces padding
        for b in buttons {
            let label = b.id.to_string();
            let min = if b.active { 22.0 } else { 18.0 };
            let w = (text.measure(&label, &ws_style(pal.fg), None).0 + 8.0).max(min);
            ws_end += 1.0;
            ws.push((rect(ws_end, ROW_Y + 2.0, w, ROW_H - 4.0), b, label));
            ws_end += w + 1.0;
        }
        ws_end += 2.0 + 1.0;
    }

    let room = (limit - PILL_PAD - after_launcher - kb_total).max(0.0);
    let total = ws_end - after_launcher;
    let mut strip = None;
    if total > room {
        let max = (total - room).max(0.0);
        let shift = -ws_scroll.clamp(0.0, max);
        for (r, ..) in ws.iter_mut() { r.x += shift; }
        strip = Some((rect(after_launcher, 0.0, room, BAR_H), max));
        x = after_launcher + room;
    } else {
        x = ws_end;
    }

    let kb = kb.map(|(label, w)| {
        let sep_x = x;
        let r = rect(x + KB_SEP_W, ROW_Y, w, ROW_H);
        x += KB_SEP_W + w;
        (r, label, sep_x)
    });
    x += PILL_PAD;
    Layout { pill: rect(PILL_X, 0.0, x - PILL_X, BAR_H), launcher, ws, strip, kb }
}

/// Right edge of the left pill measured from the output's left edge, outer margins included.
pub(crate) fn occupied_width(st: &State, text: &mut TextSys, output: &str) -> f32 {
    BAR_MARGIN as f32 + layout(text, st, output, 0.0).pill.right() + PILL_X
}

// ------------------------------------------------------------------ drawing

fn lerp_color(a: Color, b: Color, t: f32) -> Color { mix(a, b, t.clamp(0.0, 1.0)) }

fn level(hover: &[(u32, f32)], id: u32) -> f32 { hover.iter().find(|(i, _)| *i == id).map_or(0.0, |(_, l)| *l) }

/// Row-tall hit region for a visual rect (the pill is thin; the whole column reacts).
fn hit_rect(r: Rect) -> Rect { rect(r.x, 0.0, r.w, BAR_H - 1.0) }

/// A pill shows only on the output the notch actually occupies (`State.notch.output`, `None` before the notch
/// reports one) and not while the bar is toggled off or a fullscreen window covers that output.
pub(crate) fn hidden(st: &State, output: &str) -> bool {
    st.notch.output.as_deref() != Some(output)
        || st.bar_hidden
        || (st.settings.behavior.hide_on_fullscreen && st.desk.is_fullscreen(output))
}

/// Paint the left pill of `output`; returns its rect (None while hidden by a fullscreen window).
#[cfg(test)]
fn paint(cv: &mut Canvas, st: &State, output: &str, hits: &mut Hits, hover: &[(u32, f32)]) -> Option<Rect> {
    paint_with(cv, st, output, hits, hover, 0.0).0
}

/// The second result is the maximum workspace-strip scroll (0 when every workspace fits).
fn paint_with(cv: &mut Canvas, st: &State, output: &str, hits: &mut Hits, hover: &[(u32, f32)], ws_scroll: f32) -> (Option<Rect>, f32) {
    hits.clear();
    if hidden(st, output) { return (None, 0.0); }
    let pal = &st.pal;
    let l = layout(&mut *cv.text, st, output, ws_scroll);
    let p = l.pill;

    // Glass body: no top edge (it hangs from the screen edge), 1px border plus the 1px inset line of the CSS.
    cv.fill_rrect(p, r_bottom(PILL_RADIUS), pal.glass);
    cv.stroke_rrect(rect(p.x, -1.0, p.w, p.h + 1.0), r_bottom(PILL_RADIUS), 1.0, pal.edge);
    cv.stroke_rrect(rect(p.x + 1.0, 0.0, p.w - 2.0, p.h - 1.0), r_bottom(PILL_RADIUS - 1.0), 1.0, pal.edge);

    // Launcher
    hits.add(hit_rect(l.launcher), ID_LAUNCHER);
    let h = level(hover, ID_LAUNCHER);
    if h > 0.0 { cv.fill_rrect(l.launcher, r_all(MODULE_RADIUS), with_a(pal.fg, 0.12 * h)); }
    cv.text(LAUNCHER_GLYPH, &icon_style(pal.fg), l.launcher.center(), HAlign::Center, VAlign::Middle);

    // Workspaces (clipped to the strip when scrolling)
    let strip = l.strip.map(|(r, _)| r);
    let paint_ws = |cv: &mut Canvas, hits: &mut Hits| {
        for (r, b, label) in &l.ws {
            let id = if b.id < 1_000_000 { ID_WS + b.id as u32 } else { 0 };
            let h = if id != 0 {
                match strip { Some(s) => { hits.add_in(hit_rect(*r), s, id); } None => { hits.add(hit_rect(*r), id); } }
                level(hover, id)
            } else { 0.0 };
            let text = if b.active { pal.fg } else { lerp_color(with_a(pal.fg, 0.7), pal.fg, h) };
            if b.active { cv.fill_rrect(*r, r_all(MODULE_RADIUS), pal.active); }
            else if h > 0.0 { cv.fill_rrect(*r, r_all(MODULE_RADIUS), with_a(pal.fg, 0.12 * h)); }
            cv.text(label, &ws_style(text), r.center(), HAlign::Center, VAlign::Middle);
        }
    };
    match strip { Some(s) => cv.with_clip(s, [0.0; 4], |cv| paint_ws(cv, hits)), None => paint_ws(cv, hits) }

    // Keyboard layout: display only, outside the workspace clip
    if let Some((r, label, sep_x)) = &l.kb {
        cv.fill_rrect(rect(*sep_x + KB_SEP_W / 2.0, ROW_Y + 5.0, 1.0, ROW_H - 10.0), r_all(0.5), with_a(pal.fg, 0.25));
        cv.text_in(label, &ws_style(with_a(pal.fg, 0.85)), r.pad_xy(KB_PAD / 2.0 - 1.0, 0.0), HAlign::Center);
    }
    (Some(p), l.strip.map_or(0.0, |(_, m)| m))
}

// ------------------------------------------------------------------ left pill view

pub struct LeftPill {
    output: String,
    hits: Hits,
    /// Fading hover highlight per hit id.
    hover: Vec<(u32, f32)>,
    pill: Option<Rect>,
    ws_scroll: f32,
    ws_max: f32,
}

/// The left pill of `output`.
pub fn left(output: &str) -> Box<dyn View> {
    Box::new(LeftPill { output: output.to_string(), hits: Hits::default(), hover: Vec::new(), pill: None, ws_scroll: 0.0, ws_max: 0.0 })
}

impl View for LeftPill {
    fn spec(&self, state: &State) -> PanelSpec {
        let width = state.outputs.iter().find(|o| o.name == self.output)
            .map(|o| o.size.0 - 2 * BAR_MARGIN).filter(|w| *w > 0).unwrap_or(1200) as u32;
        PanelSpec {
            namespace: "waybar-notch",
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
            size: (width, BAR_H as u32),
            margin: (0, BAR_MARGIN, 0, BAR_MARGIN),
            exclusive: if hidden(state, &self.output) { -1 } else { (BAR_H as i32).max((state.notch.bottom + NOTCH_CLEARANCE).ceil() as i32) },
            kb: KbMode::None,
        }
    }

    fn tick(&mut self, dt: f32, _: &mut Ctx) -> bool {
        let hovered = self.hits.hovered();
        if let Some(h) = hovered { if !self.hover.iter().any(|(i, _)| *i == h) { self.hover.push((h, 0.0)); } }
        let step = dt / HOVER_FADE;
        let mut busy = false;
        for (id, l) in &mut self.hover {
            let target = if Some(*id) == hovered { 1.0 } else { 0.0 };
            *l = if *l < target { (*l + step).min(target) } else { (*l - step).max(target) };
            busy |= *l != target;
        }
        self.hover.retain(|(_, l)| *l > 0.0);
        busy
    }

    fn draw(&mut self, cv: &mut Canvas, ctx: &mut DrawCtx) {
        let (pill, max) = paint_with(cv, ctx.state, &self.output, &mut self.hits, &self.hover, self.ws_scroll);
        self.pill = pill;
        self.ws_max = max;
        self.ws_scroll = self.ws_scroll.min(max);
    }

    fn input_region(&self) -> Option<Vec<Rect>> {
        Some(self.pill.into_iter().collect())
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) {
        if let Event::Scroll { dx, dy, .. } = &ev {
            if self.ws_max > 0.0 {
                self.ws_scroll = (self.ws_scroll + if dx.abs() > dy.abs() { *dx } else { *dy }).clamp(0.0, self.ws_max);
                ctx.cmds.push(Cmd::Redraw(ctx.id));
            }
            return;
        }
        let Some(click) = self.hits.event(&ev) else { return };
        // a stale hit region of a pill that just became hidden must not launch anything
        if hidden(ctx.state, &self.output) { return; }
        if click.button != Button::Left { return; }
        match click.id {
            ID_LAUNCHER => run_detached("omarchy-menu", &[]),
            id if id >= ID_WS => {
                let ws = (id - ID_WS) as i64;
                std::thread::spawn(move || { if let Err(e) = hypr::focus_workspace(ws) { eprintln!("waybar-notch: {e}"); } });
            }
            _ => {}
        }
    }

    fn cursor(&self) -> Cursor { self.hits.cursor() }
}



// ------------------------------------------------------------------ per-output shell

/// Forwards to `inner` and publishes the panel id the app assigns on creation.
struct Tracked {
    inner: Box<dyn View>,
    id: Rc<Cell<Option<PanelId>>>,
}

impl View for Tracked {
    fn spec(&self, state: &State) -> PanelSpec { self.inner.spec(state) }
    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool { self.inner.tick(dt, ctx) }
    fn draw(&mut self, cv: &mut Canvas, ctx: &mut DrawCtx) { self.inner.draw(cv, ctx) }
    fn input_region(&self) -> Option<Vec<Rect>> { self.inner.input_region() }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) { self.inner.event(ev, ctx) }
    fn cursor(&self) -> Cursor { self.inner.cursor() }
    fn ticks_seconds(&self) -> bool { self.inner.ticks_seconds() }
    fn attached(&mut self, id: PanelId) { self.id.set(Some(id)); self.inner.attached(id); }
}

#[derive(Default)]
struct Slot {
    left: Rc<Cell<Option<PanelId>>>,
    right: Rc<Cell<Option<PanelId>>>,
}

/// One left and one right pill per connected output.
#[derive(Default)]
pub struct PillShell {
    slots: BTreeMap<String, Slot>,
}

impl PillShell {
    pub fn new() -> Self { Self::default() }
}

impl Shell for PillShell {
    fn outputs_changed(&mut self, state: &mut State, cmds: &mut Vec<Cmd>) {
        let live: BTreeSet<&str> = state.outputs.iter().map(|o| o.name.as_str()).collect();
        self.slots.retain(|name, slot| {
            let keep = live.contains(name.as_str());
            if !keep {
                cmds.extend([slot.left.get(), slot.right.get()].into_iter().flatten().map(Cmd::Close));
            }
            keep
        });
        for name in live {
            if self.slots.contains_key(name) { continue; }
            let slot = Slot::default();
            let views = [(left(name), slot.left.clone()), (crate::rightpill::right(name), slot.right.clone())];
            for (inner, id) in views {
                cmds.push(Cmd::Open { view: Box::new(Tracked { inner, id }), output: Some(name.to_string()) });
            }
            self.slots.insert(name.to_string(), slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppButton;
    use crate::desktop::{Desk, parse_snapshot, parse_workspaces};
    use crate::render::TextSys;
    use crate::wayland::OutputInfo;
    use serde_json::json;
    use tiny_skia::Pixmap;

    fn desk(fullscreen: bool) -> Desk {
        let mon = |id: i64, name: &str, ws: i64| json!({"id": id, "name": name, "width": 1920, "height": 1200, "x": 0, "y": 0, "scale": 1.6,
            "transform": 0, "focused": id == 0, "disabled": false, "activeWorkspace": {"id": ws}, "specialWorkspace": {"id": 0}});
        let clients = if fullscreen { json!([{"monitor": 0, "workspace": {"id": 9}, "mapped": true, "hidden": false, "visible": true, "fullscreen": 2}]) } else { json!([]) };
        let snap = parse_snapshot(&json!([mon(0, "eDP-1", 9), mon(1, "HDMI-A-1", 2)]), &clients);
        let ws = |id: i64, m: &str, w: u64| json!({"id": id, "name": id.to_string(), "monitor": m, "windows": w});
        let workspaces = parse_workspaces(&json!([ws(1, "eDP-1", 2), ws(2, "HDMI-A-1", 1), ws(3, "eDP-1", 1), ws(9, "eDP-1", 1), ws(-98, "eDP-1", 1)]));
        Desk { snap, workspaces, focused: Some("eDP-1".into()), capture_active: false, keyboard: None }
    }

    fn btn(id: &str, glyph: &str, state: AppState) -> AppButton {
        AppButton { id: id.into(), name: id.into(), glyph: glyph.into(), class: String::new(), background: false, launch: vec![], state }
    }

    fn state() -> State {
        let mut st = State::new();
        st.settings.behavior.hide_on_fullscreen = true;
        st.desk = desk(false);
        st.apps.buttons = vec![
            btn("whatsapp", "\u{f232}", AppState::Running),
            btn("steam", "\u{f1b6}", AppState::Running),
            btn("discord", "\u{f066f}", AppState::Idle),
            btn("spotify", "\u{f1bc}", AppState::Offline),
        ];
        st.apps.wa_plugin = true;
        st.apps.wa = Some(crate::apps::WaView { state: AppState::Unread, badge: Some("26".into()) });
        st.outputs = vec![OutputInfo { name: "eDP-1".into(), pos: (0, 0), size: (1200, 750) }];
        st.notch.output = Some("eDP-1".into());
        st
    }

    fn render(st: &State, hits: &mut Hits, hover: &[(u32, f32)]) -> (Pixmap, Option<Rect>) {
        let mut pm = Pixmap::new((480.0 * 1.6) as u32, (BAR_H * 1.6) as u32).unwrap();
        let mut ts = TextSys::new();
        ts.set_ui_family(Some(crate::theme::current().font));
        let mut cv = Canvas::new(&mut pm, &mut ts, 1.6);
        let r = paint(&mut cv, st, "eDP-1", hits, hover);
        (pm, r)
    }

    #[test]
    fn left_pill_renders_launcher_workspaces_and_keyboard_only() {
        let mut st = state();
        st.desk.keyboard = Some("US".into());
        let mut hits = Hits::default();
        let (pm, pill) = render(&st, &mut hits, &[(ID_WS + 3, 1.0)]);
        pm.save_png("/tmp/leftpill.png").unwrap();
        let pill = pill.unwrap();
        assert_eq!((pill.x, pill.y, pill.h), (PILL_X, 0.0, BAR_H));
        assert!(pill.w > 60.0 && pill.w < 300.0, "{}", pill.w);
        // glass body inside, transparent outside, bottom corners rounded, top corners square
        let a = |x: f32, y: f32| pm.pixel((x * 1.6) as u32, (y * 1.6) as u32).unwrap().alpha();
        assert!(a(pill.x + pill.w / 2.0, 12.0) > 100);
        assert_eq!(a(pill.right() + 4.0, 12.0), 0);
        assert_eq!(a(pill.x + 1.0, 25.0), 0, "rounded bottom-left corner");
        assert!(a(pill.x + 1.0, 0.5) > 0, "square top-left corner");
        // launcher and workspaces 1 3 9 (special and other-output ones excluded); nothing else
        let ids: Vec<u32> = (0..pill.w as i32).filter_map(|x| hits.at(pt(pill.x + x as f32, 12.0))).fold(Vec::new(), |mut v, id| { if v.last() != Some(&id) { v.push(id); } v });
        assert_eq!(ids, [ID_LAUNCHER, ID_WS + 1, ID_WS + 3, ID_WS + 9]);
    }

    fn open_apps(st: &mut State, n: usize) {
        let open = (0..n).map(|i| crate::apps::OpenApp { key: format!("app:a{i:02}"), name: format!("A{i:02}"), configured_id: None, desktop_id: None,
            windows: vec![crate::apps::OpenWindow { address: format!("0x{i:x}"), class: format!("a{i:02}"), initial_class: format!("a{i:02}"), title: String::new(), workspace: 1, workspace_name: "1".into(), focus_order: i as i64 }],
            active: i == 0 }).collect();
        st.apps.set_open(open);
    }


    #[test]
    fn fullscreen_output_hides_the_pill_only_when_enabled() {
        let mut st = state();
        st.desk = desk(true);
        let mut hits = Hits::default();
        let (pm, pill) = render(&st, &mut hits, &[]);
        assert!(pill.is_none() && hits.at(pt(30.0, 12.0)).is_none());
        assert!(pm.pixels().iter().all(|p| p.alpha() == 0));
        st.settings.behavior.hide_on_fullscreen = false;
        assert!(render(&st, &mut hits, &[]).1.is_some());
    }

    #[test]
    fn apps_never_change_the_left_pill() {
        let mut st = state();
        let mut ts = TextSys::new();
        let with_apps = layout(&mut ts, &st, "eDP-1", 0.0).pill;
        st.apps.buttons.clear();
        open_apps(&mut st, 20);
        let many = layout(&mut ts, &st, "eDP-1", 0.0).pill;
        st.apps = Default::default();
        let none = layout(&mut ts, &st, "eDP-1", 0.0).pill;
        assert_eq!((with_apps.w, many.w), (none.w, none.w));
    }

    #[test]
    fn keyboard_label_stays_outside_a_scrolling_workspace_clip() {
        let mut st = state();
        st.outputs[0].size = (700, 500);
        st.desk.keyboard = Some("ES".into());
        let ws = |id: i64| json!({"id": id, "name": id.to_string(), "monitor": "eDP-1", "windows": 1});
        st.desk.workspaces = parse_workspaces(&json!((1..=12).map(ws).collect::<Vec<_>>()));
        let mut ts = TextSys::new();
        let l = layout(&mut ts, &st, "eDP-1", 0.0);
        let (strip, max) = l.strip.expect("workspaces scroll");
        let (kb, label, _) = l.kb.expect("keyboard label kept");
        assert!(max > 0.0 && label == "ES");
        assert!(kb.x >= strip.right() && kb.w <= KB_MAX_W);
        let occ = occupied_width(&st, &mut ts, "eDP-1");
        assert!(occ <= 700.0 * LEFT_SHARE + 0.01, "{occ}");
        // no layout: no label; long code: capped
        st.desk.keyboard = None;
        assert!(layout(&mut ts, &st, "eDP-1", 0.0).kb.is_none());
        st.desk.keyboard = Some("VERYLONGLAYOUT".into());
        assert_eq!(layout(&mut ts, &st, "eDP-1", 0.0).kb.unwrap().0.w, KB_MAX_W);
        // the label is display only
        st.desk.keyboard = Some("ES".into());
        let mut hits = Hits::default();
        let mut pm = Pixmap::new((700.0 * 1.6) as u32, (BAR_H * 1.6) as u32).unwrap();
        let pill = { let mut cv = Canvas::new(&mut pm, &mut ts, 1.6); paint(&mut cv, &st, "eDP-1", &mut hits, &[]).unwrap() };
        let kb = layout(&mut ts, &st, "eDP-1", 0.0).kb.unwrap().0;
        assert!(hits.at(pt(kb.center().x, 12.0)).is_none());
        assert!(hits.at(pt(pill.x + 12.0, 12.0)) == Some(ID_LAUNCHER));
    }

    #[test]
    fn tiny_budget_drops_the_label_before_the_launcher() {
        let mut st = state();
        st.outputs[0].size = (200, 500);
        st.desk.keyboard = Some("US".into());
        let mut ts = TextSys::new();
        let l = layout(&mut ts, &st, "eDP-1", 0.0);
        assert!(l.kb.is_none() && l.launcher.w > 0.0);
    }


    #[test]
    fn spec_spans_the_output_and_reserves_the_bar() {
        let st = state();
        let v = left("eDP-1");
        let s = v.spec(&st);
        assert_eq!((s.size, s.margin, s.exclusive), ((1188, 26), (0, 6, 0, 6), 26));
        assert_eq!(s.anchor, Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    }

    #[test]
    fn pill_is_drawn_and_reserves_space_only_on_the_notch_output() {
        let mut st = state();
        st.outputs.push(OutputInfo { name: "HDMI-A-1".into(), pos: (1200, 0), size: (1920, 1080) });
        let other = left("HDMI-A-1");
        assert_eq!(other.spec(&st).exclusive, -1, "inactive output reserves no work area");
        assert_eq!(left("eDP-1").spec(&st).exclusive, 26);
        let mut hits = Hits::default();
        let mut pm = Pixmap::new(768, 42).unwrap();
        let mut ts = TextSys::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut ts, 1.6);
            assert!(paint(&mut cv, &st, "HDMI-A-1", &mut hits, &[]).is_none());
        }
        assert!(pm.pixels().iter().all(|p| p.alpha() == 0) && hits.at(pt(30.0, 12.0)).is_none());
        // startup: the notch has not reported an output yet
        st.notch.output = None;
        assert!(hidden(&st, "eDP-1") && hidden(&st, "HDMI-A-1"));
        assert_eq!(left("eDP-1").spec(&st).exclusive, -1);
        // the notch hops: the pill follows without any other signal
        st.notch.output = Some("HDMI-A-1".into());
        assert!(!hidden(&st, "HDMI-A-1") && hidden(&st, "eDP-1"));
        assert_eq!(other.spec(&st).exclusive, 26);
        let mut view = left("eDP-1");
        let mut rig = crate::testkit::Rig::new();
        rig.st = st;
        let pm = rig.draw(view.as_mut());
        assert!(pm.pixels().iter().all(|p| p.alpha() == 0), "old output draws nothing");
        assert_eq!(view.input_region(), Some(Vec::new()), "and receives no input");
    }

    #[test]
    fn shell_opens_two_panels_per_new_output_and_closes_the_vanished_ones() {
        let mut st = state();
        st.outputs.push(OutputInfo { name: "HDMI-A-1".into(), pos: (1200, 0), size: (1920, 1080) });
        let mut shell = PillShell::new();
        let mut cmds = Vec::new();
        shell.outputs_changed(&mut st, &mut cmds);
        assert_eq!(cmds.len(), 4);
        assert!(cmds.iter().all(|c| matches!(c, Cmd::Open { output: Some(_), .. })));
        // the app assigns ids through `attached`
        let mut next = 10;
        for c in cmds.drain(..) {
            if let Cmd::Open { mut view, .. } = c { view.attached(next); next += 1; }
        }
        shell.outputs_changed(&mut st, &mut cmds);
        assert!(cmds.is_empty(), "known outputs are left alone");
        st.outputs.retain(|o| o.name == "eDP-1");
        shell.outputs_changed(&mut st, &mut cmds);
        let closed: Vec<PanelId> = cmds.iter().filter_map(|c| if let Cmd::Close(i) = c { Some(*i) } else { None }).collect();
        assert_eq!(closed, [10, 11], "HDMI-A-1 sorts before eDP-1, so its panels were opened first");
    }
}

#[cfg(test)]
mod reserve_tests {
    use super::*;

    #[test]
    fn windows_start_below_the_collapsed_notch() {
        let mut st = State::new();
        st.outputs = vec![crate::wayland::OutputInfo { name: "eDP-1".into(), pos: (0, 0), size: (1200, 750) }];
        st.notch.output = Some("eDP-1".into());
        st.settings.behavior.hide_on_fullscreen = false;
        assert_eq!(left("eDP-1").spec(&st).exclusive, 26, "no notch geometry yet: bar height");
        st.notch.bottom = 44.0;
        assert_eq!(left("eDP-1").spec(&st).exclusive, 48);
    }
}
