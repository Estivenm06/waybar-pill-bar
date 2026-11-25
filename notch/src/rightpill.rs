//! The right pill: Bluetooth, Wi-Fi, volume and battery icons plus the clock, hanging from the top-right corner.
//! Native replacement of Waybar's `group/right` (see the legacy `files/config.jsonc` / `files/style.css`).
//!
//! * Bluetooth / Wi-Fi / volume / battery / clock click -> the matching page of the notch (`NavTo::Page`).
//! * Volume scroll -> +-5% through `sysinfo::volume_step`.
//! * Clock click -> the notch's calendar page (`state.nav`).
//! * Hovering an icon for a moment shows its tooltip (Bluetooth devices, SSID, sink and level).
//!
//! The pill owns no data: everything comes from `state.status`, `state.net` and `state.settings`.
use crate::layout::{Rect, pt, rect};
use crate::power::{BatteryState, PowerSnapshot};
use crate::render::{Canvas, HAlign, TextStyle, VAlign, mix, r_all, r_bottom, rgba, with_a};
use crate::state::{NavTo, State};
use crate::ui::{Hits, Palette};
use crate::wayland::{Button, Cmd, Ctx, Cursor, DrawCtx, Event, KbMode, PanelId, PanelSpec, View};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HEIGHT: f32 = 26.0;
const MARGIN_RIGHT: i32 = 6;
const ID_BT: u32 = 1;
const ID_WIFI: u32 = 2;
const ID_VOL: u32 = 3;
const ID_BAT: u32 = 4;
const ID_CLOCK: u32 = 5;
const TOOLTIP_DELAY: Duration = Duration::from_millis(450);

pub const GLYPH_BT: &str = "\u{f293}";
pub const GLYPH_BT_OFF: &str = "\u{f00b2}";
pub const GLYPH_WIFI: &str = "\u{f1eb}";
pub const GLYPH_WIFI_OFF: &str = "\u{f092e}";
pub const GLYPH_VOL_MUTE: &str = "\u{f075f}";
pub const GLYPH_BT_AUDIO: &str = "\u{f025}";

/// The right pill for `output`.
pub fn right(output: &str) -> Box<dyn View> { Box::new(RightPill::new(output)) }

// ------------------------------------------------------------------ pure state -> item mapping

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatClass { Normal, Warning, Critical, Charging }

#[derive(Clone, Debug, PartialEq)]
pub struct Bat {
    pub glyph: &'static str,
    pub pct: u8,
    pub class: BatClass,
}

/// Waybar's battery module: `{icon} {capacity}%`; bolt while charging, plug when full/not charging on AC.
pub fn battery(p: &PowerSnapshot) -> Option<Bat> {
    if p.batteries.is_empty() { return None; }
    let pct = p.percentage.or_else(|| p.batteries.iter().find_map(|b| b.percentage))?.round().clamp(0.0, 100.0) as u8;
    let charging = p.batteries.iter().any(|b| b.state == BatteryState::Charging);
    let plugged = !charging && p.ac_online != Some(false)
        && p.batteries.iter().all(|b| matches!(b.state, BatteryState::Full | BatteryState::NotCharging));
    const LEVELS: [&str; 5] = ["\u{f244}", "\u{f243}", "\u{f242}", "\u{f241}", "\u{f240}"];
    let glyph = if charging { "\u{f0e7}" } else if plugged { "\u{f1e6}" } else { LEVELS[(pct as usize * 5 / 100).min(4)] };
    let class = if charging { BatClass::Charging } else if pct <= 15 { BatClass::Critical } else if pct <= 25 { BatClass::Warning } else { BatClass::Normal };
    Some(Bat { glyph, pct, class })
}

/// Waybar `format-icons` for three levels, or the muted glyph.
pub fn volume_glyph(percent: u8, muted: bool) -> &'static str {
    if muted { return GLYPH_VOL_MUTE; }
    ["\u{f026}", "\u{f027}", "\u{f028}"][(percent as usize * 3 / 100).min(2)]
}

/// One pill item: icon, optional text, and how it is coloured.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    id: u32,
    icon: &'static str,
    text: String,
    /// Extra text without an icon font (Bluetooth sink name).
    tint: Tint,
    dim: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tint { Normal, Dim, Accent, Good, Warn, Crit }

fn bt_sink_name(st: &State) -> Option<String> {
    st.net.bt.devs.iter().find(|d| d.connected && d.active).map(|d| d.name.clone())
}

/// Compact text: `87%` for one device reporting a battery, else the count, plus the lowest reported battery (`2 · 42%`).
fn bt_summary(bt: &crate::state::BtStatus) -> String {
    let low = bt.connected.iter().filter_map(|d| d.battery).min();
    match (bt.connected.len(), low) {
        (1, Some(b)) => format!("{b}%"),
        (n, Some(b)) => format!("{n} · {b}%"),
        (n, None) => n.to_string(),
    }
}

fn items(st: &State) -> Vec<Item> {
    let mut v = Vec::new();
    let bt = &st.status.bt;
    if bt.available {
        let (icon, text, tint, dim) = if !bt.on { (GLYPH_BT_OFF, String::new(), Tint::Dim, true) }
            else if bt.connected.is_empty() { (GLYPH_BT, String::new(), Tint::Normal, false) }
            else { (GLYPH_BT, bt_summary(bt), Tint::Accent, false) };
        v.push(Item { id: ID_BT, icon, text, tint, dim });
    }
    let w = &st.status.wifi;
    if w.available {
        v.push(Item { id: ID_WIFI, icon: if w.on { GLYPH_WIFI } else { GLYPH_WIFI_OFF }, text: String::new(), tint: Tint::Normal, dim: false });
    }
    let vol = &st.status.volume;
    if vol.available {
        let sink = if vol.muted { None } else { bt_sink_name(st) };
        let (icon, text, tint) = match sink {
            // `format-bluetooth`: headphones glyph and the device name, at most 24 characters in total
            Some(name) => (GLYPH_BT_AUDIO, name.chars().take(22).collect::<String>(), Tint::Accent),
            None => (volume_glyph(vol.percent, vol.muted), String::new(), if vol.muted { Tint::Dim } else { Tint::Normal }),
        };
        v.push(Item { id: ID_VOL, icon, text, tint, dim: false });
    }
    if let Some(b) = st.status.power.as_ref().and_then(battery) {
        let tint = match b.class { BatClass::Charging => Tint::Good, BatClass::Critical => Tint::Crit, BatClass::Warning => Tint::Warn, BatClass::Normal => Tint::Normal };
        v.push(Item { id: ID_BAT, icon: b.glyph, text: format!("{}%", b.pct), tint, dim: false });
    }
    v
}

/// Tooltip text of an item, if it has one (`volume_desc` is the default sink description when known).
fn tooltip(st: &State, id: u32, volume_desc: Option<&str>) -> Option<String> {
    match id {
        ID_BT => {
            let bt = &st.status.bt;
            Some(if !bt.on { "Bluetooth: off".to_string() } else if bt.connected.is_empty() { "Bluetooth: on".to_string() } else {
                let mut s = format!("{} connected", bt.connected.len());
                for d in &bt.connected {
                    s.push('\n');
                    s.push_str(&d.name);
                    if let Some(b) = d.battery { s.push_str(&format!(" · {b}%")); }
                }
                s
            })
        }
        ID_WIFI => {
            let w = &st.status.wifi;
            Some(if !w.on { "Wi-Fi off".to_string() } else { w.ssid.clone().unwrap_or_else(|| "Wi-Fi offline".to_string()) })
        }
        ID_VOL => {
            let v = &st.status.volume;
            let desc = bt_sink_name(st).or_else(|| volume_desc.map(String::from)).unwrap_or_else(|| "Volume".into());
            Some(format!("{desc} · {}%", v.percent))
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ view

struct Placed {
    item: Item,
    icon_w: f32,
    text_w: f32,
    rect: Rect,
}

struct Geometry {
    placed: Vec<Placed>,
    clock: String,
    clock_rect: Rect,
    width: f32,
}

/// Item and clock placement; shared by `RightPill::layout` and `occupied_width`.
fn geometry(ts: &mut crate::render::TextSys, st: &State) -> Geometry {
    let pal = &st.pal;
    let mut placed = Vec::new();
    // pill border 1 + padding 6, then icons with 1px margins and 4px padding (min 18)
    let mut x = 1.0 + 6.0;
    for item in items(st) {
        let icon_w = ts.measure(item.icon, &RightPill::icon_style(pal, pal.fg), None).0;
        let text_w = if item.text.is_empty() { 0.0 } else { ts.measure(&item.text, &RightPill::text_style(pal, pal.fg), None).0 };
        let content = icon_w + if text_w > 0.0 { 5.0 + text_w } else { 0.0 };
        let w = (content + 8.0).max(18.0);
        placed.push(Placed { item, icon_w, text_w, rect: rect(x + 1.0, 1.0, w, HEIGHT - 3.0) });
        x += w + 2.0;
    }
    let clock = chrono::Local::now().format(st.settings.behavior.clock_format.chrono_pattern()).to_string();
    let cw = ts.measure(&clock, &RightPill::clock_style(pal), None).0;
    // clock: 4px margin, 1px left divider, 7px padding each side
    let cx = x + 4.0;
    let clock_rect = rect(cx, 1.0, 1.0 + 7.0 + cw + 7.0, HEIGHT - 3.0);
    let width = (clock_rect.right() + 6.0 + 1.0).ceil();
    Geometry { placed, clock, clock_rect, width }
}

/// Width the right pill takes from the output's right edge, outer margin included.
pub(crate) fn occupied_width(st: &State, ts: &mut crate::render::TextSys) -> f32 {
    geometry(ts, st).width + MARGIN_RIGHT as f32
}

pub struct RightPill {
    output: String,
    hidden: bool,
    hits: Hits,
    placed: Vec<Placed>,
    clock: String,
    clock_rect: Rect,
    width: f32,
    /// Blink clock of the critical battery.
    t: f32,
    animate: bool,
    hover: Option<u32>,
    hover_since: Instant,
    tip: Option<Tip>,
    /// Default sink description fetched when the pointer enters the volume icon.
    volume_desc: Arc<Mutex<Option<String>>>,
}

struct Tip {
    id: Rc<Cell<PanelId>>,
    alive: Rc<Cell<bool>>,
}

impl RightPill {
    fn new(output: &str) -> RightPill {
        RightPill {
            hidden: false,
            output: output.to_string(), hits: Hits::default(), placed: Vec::new(), clock: String::new(), clock_rect: rect(0.0, 0.0, 0.0, 0.0),
            width: 80.0, t: 0.0, animate: true, hover: None, hover_since: Instant::now(), tip: None, volume_desc: Arc::new(Mutex::new(None)),
        }
    }

    fn icon_style(pal: &Palette, color: crate::render::Color) -> TextStyle { TextStyle::icon(13.0, color).size(13.0 * pal.font_scale.max(0.5)) }
    fn text_style(pal: &Palette, color: crate::render::Color) -> TextStyle { TextStyle::new(pal.fs(13.0), color) }
    fn clock_style(pal: &Palette) -> TextStyle { TextStyle::new(pal.fs(11.5), pal.fg).bold() }

    fn layout(&mut self, ts: &mut crate::render::TextSys, st: &State) {
        let g = geometry(ts, st);
        self.placed = g.placed;
        self.clock = g.clock;
        self.clock_rect = g.clock_rect;
        self.width = g.width;
    }

    fn close_tip(&mut self, cmds: &mut Vec<Cmd>) {
        if let Some(t) = self.tip.take() {
            t.alive.set(false);
            let id = t.id.get();
            if id != 0 { cmds.push(Cmd::Close(id)); }
        }
    }

    fn rect_of(&self, id: u32) -> Option<Rect> { self.placed.iter().find(|p| p.item.id == id).map(|p| p.rect) }
}

impl View for RightPill {
    fn spec(&self, _st: &State) -> PanelSpec {
        PanelSpec {
            namespace: "waybar-notch",
            layer: Layer::Top,
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (self.width as u32, HEIGHT as u32),
            margin: (0, MARGIN_RIGHT, 0, 0),
            exclusive: -1,
            kb: KbMode::None,
        }
    }

    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        self.animate = ctx.state.settings.behavior.animations;
        let was_hidden = self.hidden;
        self.hidden = crate::pills::hidden(ctx.state, &self.output);
        if self.hidden && !was_hidden {
            self.hover = None;
            self.hits.clear();
            self.close_tip(ctx.cmds);
        }
        self.t += dt;
        self.layout(ctx.text, ctx.state);
        let critical = self.placed.iter().any(|p| p.item.id == ID_BAT && p.item.tint == Tint::Crit);
        // tooltip after the pointer rests on an icon
        let mut waiting = false;
        if self.hidden { return false; }
        if let (Some(id), None) = (self.hover, &self.tip) {
            let desc = self.volume_desc.lock().ok().and_then(|d| d.clone());
            if let Some(text) = tooltip(ctx.state, id, desc.as_deref()) {
                if self.hover_since.elapsed() >= TOOLTIP_DELAY {
                    if let Some(r) = self.rect_of(id) {
                        let (view, tip) = Tooltip::open(&text, ctx, &ctx.state.pal.clone(), self.width, r);
                        self.tip = Some(tip);
                        ctx.cmds.push(Cmd::Open { view: Box::new(view), output: Some(self.output.clone()).filter(|o| !o.is_empty()) });
                    }
                } else {
                    waiting = true;
                }
            }
        }
        (critical && self.animate) || waiting
    }

    fn ticks_seconds(&self) -> bool { true }

    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn input_region(&self) -> Option<Vec<Rect>> { if self.hidden { Some(Vec::new()) } else { None } }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx) {
        self.hits.clear();
        if self.hidden { return; }
        let pal = &dc.state.pal;
        let w = self.width;
        let pill = rect(0.0, 0.0, w, HEIGHT);
        // glass hanging from the top edge: rounded at the bottom only, no top border
        cv.fill_rrect(pill, r_bottom(14.0), pal.glass);
        cv.stroke_rrect(rect(0.0, -1.0, w, HEIGHT + 1.0), r_bottom(14.0), 1.0, pal.edge);

        let blink = {
            // linear alternate between 0 and 1 every 0.8 s
            let ph = (self.t / 0.8) % 2.0;
            if self.animate { if ph < 1.0 { ph } else { 2.0 - ph } } else { 0.0 }
        };
        for p in &self.placed {
            let hovered = self.hits.add(p.rect, p.item.id);
            let pressed = self.hits.is_pressed(p.item.id);
            let mut color = match p.item.tint {
                Tint::Normal => pal.fg,
                Tint::Dim => pal.dim,
                Tint::Accent => pal.accent,
                Tint::Good => pal.green,
                Tint::Warn => pal.yellow,
                Tint::Crit => pal.red,
            };
            if p.item.tint == Tint::Crit {
                cv.fill_rrect(p.rect, r_all(10.0), with_a(pal.red, 0.55 * blink));
                color = mix(pal.red, pal.fg, blink);
            } else if pressed {
                cv.fill_rrect(p.rect, r_all(10.0), with_a(pal.accent, 0.28));
            } else if hovered {
                cv.fill_rrect(p.rect, r_all(10.0), with_a(pal.fg, 0.14));
            }
            let alpha = if p.item.dim { 0.6 } else { 1.0 };
            cv.with_alpha(alpha, |cv| {
                let content = p.icon_w + if p.text_w > 0.0 { 5.0 + p.text_w } else { 0.0 };
                let x = p.rect.x + (p.rect.w - content) / 2.0;
                let cy = p.rect.y + p.rect.h / 2.0;
                cv.text(p.item.icon, &Self::icon_style(pal, color), pt(x, cy), HAlign::Left, VAlign::Middle);
                if p.text_w > 0.0 {
                    cv.text(&p.item.text, &Self::text_style(pal, color), pt(x + p.icon_w + 5.0, cy), HAlign::Left, VAlign::Middle);
                }
            });
        }

        // clock: divider + bold time, rounded on the right
        let c = self.clock_rect;
        let hovered = self.hits.add(c, ID_CLOCK);
        if self.hits.is_pressed(ID_CLOCK) {
            cv.fill_rrect(c, [0.0, 11.0, 11.0, 0.0], with_a(pal.accent, 0.28));
        } else if hovered {
            cv.fill_rrect(c, [0.0, 11.0, 11.0, 0.0], with_a(pal.fg, 0.14));
        }
        cv.fill_rect(rect(c.x, c.y + 1.0, 1.0, c.h - 2.0), with_a(pal.fg, 0.18));
        cv.text(&self.clock, &Self::clock_style(pal), pt(c.x + 1.0 + 7.0, c.y + c.h / 2.0), HAlign::Left, VAlign::Middle);
        let _ = (&dc.output, rgba(0, 0, 0, 0.0));
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) {
        // a stale hit region of a pill that just became hidden must not act
        if crate::pills::hidden(ctx.state, &self.output) {
            self.hover = None;
            self.close_tip(ctx.cmds);
            return;
        }
        if let Event::Scroll { steps, .. } = &ev {
            if self.hits.hovered() == Some(ID_VOL) && *steps != 0 {
                // wheel up (negative steps) raises the volume
                crate::sysinfo::volume_step(ctx.poster, (-*steps * 5).clamp(-100, 100) as i8);
                ctx.state.nav = Some(NavTo::Page("audio"));
            }
            return;
        }
        let click = self.hits.event(&ev);
        let hovered = self.hits.hovered();
        if hovered != self.hover {
            self.hover = hovered;
            self.hover_since = Instant::now();
            self.close_tip(ctx.cmds);
            if hovered == Some(ID_VOL) { prefetch_volume_desc(&self.volume_desc, ctx.poster); }
        }
        let Some(click) = click else { return };
        if click.button != Button::Left { return; }
        self.close_tip(ctx.cmds);
        let page = match click.id {
            ID_BT => "bt",
            ID_WIFI => "wifi",
            ID_VOL => "audio",
            ID_BAT => "battery",
            ID_CLOCK => "calendar",
            _ => return,
        };
        ctx.state.nav = Some(NavTo::Page(page));
    }
}

/// Look up the default sink's description off the UI thread so the volume tooltip can show it.
fn prefetch_volume_desc(slot: &Arc<Mutex<Option<String>>>, poster: &crate::wayland::Poster) {
    let (slot, poster) = (slot.clone(), poster.clone());
    std::thread::spawn(move || {
        let v = crate::audio::gather_audio_outputs();
        let desc = v["master"]["desc"].as_str().map(String::from);
        if let Ok(mut s) = slot.lock() { *s = desc; }
        poster.post(|_, _| {});
    });
}

// ------------------------------------------------------------------ tooltip

/// A small click-through surface under the pill, like a Waybar tooltip.
struct Tooltip {
    lines: Vec<String>,
    size: (f32, f32),
    margin_right: i32,
    id: Rc<Cell<PanelId>>,
    alive: Rc<Cell<bool>>,
}

impl Tooltip {
    /// Build a tooltip for `text` centred under `item` (surface coordinates of a pill `pill_w` wide).
    fn open(text: &str, ctx: &mut Ctx, pal: &Palette, pill_w: f32, item: Rect) -> (Tooltip, Tip) {
        let style = Self::style(pal);
        let lines: Vec<String> = text.lines().map(String::from).collect();
        let w = lines.iter().map(|l| ctx.text.measure(l, &style, None).0).fold(0.0f32, f32::max) + 16.0 + 2.0;
        let lh = (style.size * 1.3).ceil();
        let h = lh * lines.len() as f32 + 8.0 + 2.0;
        // centre under the icon, never past the screen edge
        let from_right = pill_w - item.center().x;
        let margin_right = (MARGIN_RIGHT as f32 + from_right - w / 2.0).max(MARGIN_RIGHT as f32).round() as i32;
        let id = Rc::new(Cell::new(0));
        let alive = Rc::new(Cell::new(true));
        (Tooltip { lines, size: (w.ceil(), h.ceil()), margin_right, id: id.clone(), alive: alive.clone() }, Tip { id, alive })
    }

    fn style(pal: &Palette) -> TextStyle { TextStyle::new(pal.fs(10.0), pal.fg) }
}

impl View for Tooltip {
    fn spec(&self, _st: &State) -> PanelSpec {
        PanelSpec {
            namespace: "waybar-notch",
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (self.size.0 as u32, self.size.1 as u32),
            margin: (HEIGHT as i32 + 3, self.margin_right, 0, 0),
            exclusive: -1,
            kb: KbMode::None,
        }
    }
    fn attached(&mut self, id: PanelId) { self.id.set(id); }
    fn tick(&mut self, _dt: f32, ctx: &mut Ctx) -> bool {
        if !self.alive.get() { ctx.cmds.push(Cmd::Close(ctx.id)); }
        false
    }
    fn input_region(&self) -> Option<Vec<Rect>> { Some(Vec::new()) }
    fn event(&mut self, _ev: Event, _ctx: &mut Ctx) {}
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx) {
        let pal = &dc.state.pal;
        let r = rect(0.0, 0.0, self.size.0, self.size.1);
        cv.fill_rrect(r, r_all(10.0), with_a(mix(pal.bg, pal.fg, 0.04), 1.0));
        cv.stroke_rrect(r, r_all(10.0), 1.0, pal.edge);
        let style = Self::style(pal);
        let lh = (style.size * 1.3).ceil();
        for (i, l) in self.lines.iter().enumerate() {
            cv.text(l, &style, pt(r.x + 9.0, 5.0 + lh * i as f32 + lh / 2.0), HAlign::Left, VAlign::Middle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::{BatterySnapshot, PowerSnapshot};
    use crate::state::{BtStatus, Volume, WifiStatus};

    fn pack(state: BatteryState, pct: f64) -> BatterySnapshot {
        BatterySnapshot {
            name: "BAT0".into(), state, percentage: Some(pct), power_w: None, energy_now_wh: None, energy_full_wh: None, energy_design_wh: None,
            health_pct: None, voltage_v: None, cycles: None, technology: None, manufacturer: None, model: None, time_to_empty_secs: None, time_to_full_secs: None,
        }
    }

    fn power(state: BatteryState, pct: f64, ac: Option<bool>) -> PowerSnapshot {
        PowerSnapshot { available: true, ac_online: ac, batteries: vec![pack(state, pct)], percentage: Some(pct) }
    }

    #[test]
    fn battery_glyph_and_class_follow_waybar_states() {
        let b = |s, p, ac| battery(&power(s, p, ac)).unwrap();
        assert_eq!((b(BatteryState::Discharging, 80.0, Some(false)).glyph, b(BatteryState::Discharging, 80.0, Some(false)).class), ("\u{f240}", BatClass::Normal));
        assert_eq!(b(BatteryState::Discharging, 19.0, Some(false)).glyph, "\u{f244}");
        assert_eq!(b(BatteryState::Discharging, 20.0, Some(false)).glyph, "\u{f243}");
        assert_eq!(b(BatteryState::Discharging, 25.0, Some(false)).class, BatClass::Warning);
        assert_eq!(b(BatteryState::Discharging, 26.0, Some(false)).class, BatClass::Normal);
        assert_eq!(b(BatteryState::Discharging, 15.0, Some(false)).class, BatClass::Critical);
        let ch = b(BatteryState::Charging, 10.0, Some(true));
        assert_eq!((ch.glyph, ch.class), ("\u{f0e7}", BatClass::Charging), "charging wins over critical");
        assert_eq!(b(BatteryState::Full, 100.0, Some(true)).glyph, "\u{f1e6}");
        assert_eq!(b(BatteryState::NotCharging, 80.0, None).glyph, "\u{f1e6}");
        assert!(battery(&PowerSnapshot { available: true, ac_online: Some(true), batteries: vec![], percentage: None }).is_none(), "no pack, no module");
    }

    #[test]
    fn volume_glyph_levels() {
        assert_eq!(volume_glyph(0, false), "\u{f026}");
        assert_eq!(volume_glyph(33, false), "\u{f026}");
        assert_eq!(volume_glyph(34, false), "\u{f027}");
        assert_eq!(volume_glyph(66, false), "\u{f027}");
        assert_eq!(volume_glyph(67, false), "\u{f028}");
        assert_eq!(volume_glyph(150, false), "\u{f028}");
        assert_eq!(volume_glyph(80, true), GLYPH_VOL_MUTE);
    }

    fn st_with(bt: BtStatus, wifi: WifiStatus, vol: Volume) -> State {
        let mut st = State::new();
        st.status.bt = bt;
        st.status.wifi = wifi;
        st.status.volume = vol;
        st
    }

    fn cbt(mac: &str, name: &str, battery: Option<u8>) -> crate::state::ConnectedBtDevice {
        crate::state::ConnectedBtDevice { mac: mac.into(), name: name.into(), battery }
    }

    #[test]
    fn battery_summary_and_tooltip_use_live_status() {
        let one = BtStatus { available: true, on: true, connected: vec![cbt("A", "Buds", Some(87))] };
        assert_eq!(bt_summary(&one), "87%");
        let zero = BtStatus { available: true, on: true, connected: vec![cbt("A", "Buds", Some(0))] };
        assert_eq!(bt_summary(&zero), "0%");
        let two = BtStatus { available: true, on: true, connected: vec![cbt("A", "Same", Some(42)), cbt("B", "Same", Some(90))] };
        assert_eq!(bt_summary(&two), "2 · 42%");
        let mixed = BtStatus { available: true, on: true, connected: vec![cbt("A", "Same", None), cbt("B", "Same", Some(90))] };
        assert_eq!(bt_summary(&mixed), "2 · 90%");
        let none = BtStatus { available: true, on: true, connected: vec![cbt("A", "Mouse", None)] };
        assert_eq!(bt_summary(&none), "1");
        let mut st = st_with(two, WifiStatus::default(), Volume::default());
        assert_eq!(tooltip(&st, ID_BT, None).unwrap(), "2 connected\nSame · 42%\nSame · 90%");
        st.status.bt.connected.clear();
        assert_eq!(tooltip(&st, ID_BT, None).unwrap(), "Bluetooth: on");
    }

    #[test]
    fn items_follow_status() {
        let st = st_with(
            BtStatus { available: true, on: true, connected: vec![cbt("A", "Buds", None), cbt("B", "Mouse", None)] },
            WifiStatus { available: true, on: false, ssid: None, signal: 0 },
            Volume { available: true, percent: 50, muted: false },
        );
        let it = items(&st);
        assert_eq!(it.iter().map(|i| i.id).collect::<Vec<_>>(), [ID_BT, ID_WIFI, ID_VOL]);
        assert_eq!((it[0].icon, it[0].text.as_str(), it[0].tint), (GLYPH_BT, "2", Tint::Accent));
        assert_eq!(it[1].icon, GLYPH_WIFI_OFF);
        assert_eq!(it[2].icon, "\u{f027}");
        let off = st_with(BtStatus { available: true, on: false, connected: vec![] }, WifiStatus::default(), Volume::default());
        let it = items(&off);
        assert_eq!(it.len(), 1, "unavailable modules are hidden");
        assert!(it[0].dim && it[0].icon == GLYPH_BT_OFF);
    }

    #[test]
    fn active_bluetooth_sink_shows_headphones_and_name() {
        let mut st = st_with(BtStatus::default(), WifiStatus::default(), Volume { available: true, percent: 30, muted: false });
        st.net.bt.devs.push(crate::net::BtDev { name: "Buds Pro".into(), connected: true, active: true, ..Default::default() });
        let it = items(&st);
        assert_eq!((it[0].icon, it[0].text.as_str(), it[0].tint), (GLYPH_BT_AUDIO, "Buds Pro", Tint::Accent));
        st.status.volume.muted = true;
        assert_eq!(items(&st)[0].icon, GLYPH_VOL_MUTE, "muted shows the mute glyph, not the device");
    }

    #[test]
    fn tooltips_match_waybar_formats() {
        let mut st = st_with(
            BtStatus { available: true, on: true, connected: vec![cbt("A", "Buds", None), cbt("B", "Mouse", None)] },
            WifiStatus { available: true, on: true, ssid: Some("Home".into()), signal: 70 },
            Volume { available: true, percent: 42, muted: false },
        );
        st.status.bt.connected[0].battery = Some(80);
        assert_eq!(tooltip(&st, ID_BT, None).unwrap(), "2 connected\nBuds · 80%\nMouse");
        assert_eq!(tooltip(&st, ID_WIFI, None).unwrap(), "Home");
        assert_eq!(tooltip(&st, ID_VOL, Some("Speakers")).unwrap(), "Speakers · 42%");
        st.status.wifi.ssid = None;
        assert_eq!(tooltip(&st, ID_WIFI, None).unwrap(), "Wi-Fi offline");
        st.status.wifi.on = false;
        assert_eq!(tooltip(&st, ID_WIFI, None).unwrap(), "Wi-Fi off");
        assert!(tooltip(&st, ID_BAT, None).is_none() && tooltip(&st, ID_CLOCK, None).is_none());
    }

    use crate::wayland::Event;

    /// A visible pill (the notch reports `eDP-1`) showing all five regions.
    fn visible_rig() -> (crate::testkit::Rig, RightPill) {
        let mut rig = crate::testkit::Rig::new();
        rig.st = st_with(
            BtStatus { available: true, on: true, connected: vec![] },
            WifiStatus { available: true, on: true, ssid: Some("Home".into()), signal: 70 },
            Volume { available: true, percent: 50, muted: false },
        );
        rig.st.status.power = Some(power(BatteryState::Discharging, 50.0, Some(false)));
        rig.st.notch.output = Some("eDP-1".into());
        let mut pill = RightPill::new("eDP-1");
        rig.run(&mut pill, 0.1);
        rig.draw(&mut pill);
        (rig, pill)
    }

    fn click(rig: &mut crate::testkit::Rig, pill: &mut RightPill, at: crate::layout::Point) -> Vec<Cmd> {
        let mut cmds = rig.event(pill, Event::PointerMove(at));
        cmds.extend(rig.event(pill, Event::Press(Button::Left, at)));
        cmds.extend(rig.event(pill, Event::Release(Button::Left, at)));
        cmds
    }

    #[test]
    fn every_region_opens_its_page_inside_the_notch_and_no_surface_of_its_own() {
        let (mut rig, mut pill) = visible_rig();
        let regions = [(ID_BT, "bt"), (ID_WIFI, "wifi"), (ID_VOL, "audio"), (ID_BAT, "battery")];
        for (id, page) in regions {
            rig.st.nav = None;
            let at = pill.rect_of(id).unwrap().center();
            let cmds = click(&mut rig, &mut pill, at);
            assert_eq!(rig.st.nav, Some(NavTo::Page(page)), "region {id}");
            assert!(!cmds.iter().any(|c| matches!(c, Cmd::Open { .. })), "region {id} opened a detached surface");
            // clicking again keeps the chosen page: a request, never a toggle that closes it
            rig.st.nav = None;
            click(&mut rig, &mut pill, at);
            assert_eq!(rig.st.nav, Some(NavTo::Page(page)), "repeat click on {id}");
        }
        rig.st.nav = None;
        let clock = pill.clock_rect.center();
        let cmds = click(&mut rig, &mut pill, clock);
        assert_eq!(rig.st.nav, Some(NavTo::Page("calendar")));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Open { .. })));
    }

    #[test]
    fn only_the_left_button_activates() {
        let (mut rig, mut pill) = visible_rig();
        let at = pill.rect_of(ID_WIFI).unwrap().center();
        rig.event(&mut pill, Event::PointerMove(at));
        rig.event(&mut pill, Event::Press(Button::Right, at));
        rig.event(&mut pill, Event::Release(Button::Right, at));
        assert_eq!(rig.st.nav, None);
    }

    #[test]
    fn volume_wheel_steps_five_points_and_opens_the_audio_page() {
        let p = crate::audio::stub::Pactl::install();
        let (mut rig, mut pill) = visible_rig();
        let vol = pill.rect_of(ID_VOL).unwrap().center();
        rig.event(&mut pill, Event::PointerMove(vol));
        rig.event(&mut pill, Event::Scroll { dx: 0.0, dy: -10.0, steps: -1, pos: vol });
        assert_eq!(rig.st.nav, Some(NavTo::Page("audio")));
        let end = Instant::now() + Duration::from_secs(5);
        while !p.log().iter().any(|l| l == "set-sink-volume @DEFAULT_SINK@ 55%") {
            assert!(Instant::now() < end, "no volume command: {:?}", p.log());
            std::thread::sleep(Duration::from_millis(20));
        }
        // the stub master is at 50 %: one step is +5 points
        rig.st.nav = None;
        rig.event(&mut pill, Event::Scroll { dx: 0.0, dy: 0.0, steps: 0, pos: vol });
        assert_eq!(rig.st.nav, None, "a zero step does nothing");
        // scrolling elsewhere on the pill never touches the audio page
        let bt = pill.rect_of(ID_BT).unwrap().center();
        rig.event(&mut pill, Event::PointerMove(bt));
        rig.event(&mut pill, Event::Scroll { dx: 0.0, dy: -10.0, steps: -1, pos: bt });
        assert_eq!(rig.st.nav, None);
    }

    #[test]
    fn pill_on_another_output_draws_nothing_takes_no_input_and_reserves_no_space() {
        let (mut rig, mut pill) = visible_rig();
        assert!(rig.draw(&mut pill).pixels().iter().any(|p| p.alpha() > 0));
        assert_eq!(pill.input_region(), None);
        let at = pill.rect_of(ID_BT).unwrap().center();
        rig.event(&mut pill, Event::PointerMove(at));
        assert_eq!(pill.hover, Some(ID_BT));
        // the notch moves to another output: this pill hides, forgets the hover and closes its tooltip
        pill.tip = Some(Tip { id: Rc::new(Cell::new(42)), alive: Rc::new(Cell::new(true)) });
        rig.st.notch.output = Some("HDMI-A-1".into());
        let cmds = rig.run(&mut pill, 0.05);
        assert!(pill.hover.is_none() && pill.tip.is_none());
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Close(42))), "the tooltip surface is closed");
        assert!(rig.draw(&mut pill).pixels().iter().all(|p| p.alpha() == 0), "hidden means no drawn pixels");
        assert_eq!(pill.input_region(), Some(Vec::new()));
        assert_eq!(pill.spec(&rig.st).exclusive, -1);
        // a stale hit region cannot act
        rig.st.nav = None;
        click(&mut rig, &mut pill, at);
        assert_eq!(rig.st.nav, None);
        // and no tooltip ever opens while hidden
        pill.hover = Some(ID_BT);
        pill.hover_since = Instant::now() - Duration::from_secs(2);
        let cmds = rig.run(&mut pill, 0.05);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Open { .. })));
        // startup: the notch has not reported an output
        rig.st.notch.output = None;
        rig.run(&mut pill, 0.05);
        assert!(pill.hidden);
        // back on its own output the pill shows again
        rig.st.notch.output = Some("eDP-1".into());
        rig.run(&mut pill, 0.05);
        assert!(!pill.hidden && rig.draw(&mut pill).pixels().iter().any(|p| p.alpha() > 0));
    }

    #[test]
    fn pill_and_tooltip_interiors_are_opaque_over_any_backdrop() {
        let (mut rig, mut pill) = visible_rig();
        crate::ui::assert_opaque_interior(&rig.draw(&mut pill), "right pill");
        let mut tip = Tooltip { lines: vec!["Speakers · 42%".into()], size: (150.0, 28.0), margin_right: 6, id: Rc::new(Cell::new(0)), alive: Rc::new(Cell::new(true)) };
        crate::ui::assert_opaque_interior(&rig.draw(&mut tip), "tooltip");
    }

    #[test]
    fn preview_right_pill_states() {
        use crate::testkit::{Rig, save_over_backdrop};
        let cases: Vec<(&str, BatteryState, f64, bool, bool, bool)> = vec![
            ("rp_pill_normal", BatteryState::Discharging, 62.0, false, false, true),
            ("rp_pill_warning", BatteryState::Discharging, 22.0, false, true, false),
            ("rp_pill_critical", BatteryState::Discharging, 9.0, false, true, true),
            ("rp_pill_charging", BatteryState::Charging, 54.0, true, false, true),
            ("rp_pill_muted", BatteryState::Full, 100.0, true, false, false),
        ];
        let mut tiles = Vec::new();
        for (name, bs, pct, charging_or_ac, muted_or_bt, connected) in cases {
            let mut rig = Rig::new();
            rig.st.status.power = Some(power(bs, pct, Some(charging_or_ac)));
            rig.st.status.bt = BtStatus { available: true, on: connected || muted_or_bt, connected: if connected { vec![cbt("A", "Buds", None)] } else { vec![] } };
            rig.st.status.wifi = WifiStatus { available: true, on: true, ssid: Some("Home".into()), signal: 60 };
            rig.st.status.volume = Volume { available: true, percent: 70, muted: name == "rp_pill_muted" };
            let mut pill = RightPill::new("eDP-1");
            rig.run(&mut pill, 0.5);
            let pm = rig.draw(&mut pill);
            save_over_backdrop(&pm, 8, &format!("/tmp/{name}.png"));
            tiles.push(format!("/tmp/{name}.png"));
        }
        assert_eq!(tiles.len(), 5);
    }
}
