//! The notch: one layer surface whose content *morphs* between the collapsed
//! pill and the expanded card instead of cross-fading.
//!
//! `prog` (0 collapsed .. 1 expanded) is a critically damped spring. The
//! clock, weather glyph and album art are drawn by the notch itself at
//! positions/sizes interpolated between their collapsed spot and the target
//! the current page reports through `Page::morph`. Everything else (tab bar,
//! page content) fades in once `prog` passes 0.5.

use crate::dock::{Dims, collapsed_layout, icon_for};
use crate::layout::{Point, Rect, Size, lerp, lerp_pt, lerp_rect, pt, rect, smooth, window};
use crate::page::{Morph, Page};
use crate::render::{Canvas, HAlign, Image, TextStyle, TextSys, VAlign, mix, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{Hits, Palette, glass};
use crate::wayland::{Button, Cmd, Cursor, Ctx, DrawCtx, Event, KbMode, PanelSpec, View, PanelId};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use std::time::{Duration, Instant};

const HEADROOM: f32 = 1.12;
const SCREEN_MARGIN: f32 = 16.0;
const RESERVE_H: f32 = 560.0;
const SPRING_K: f32 = 250.0;
const SPRING_C: f32 = 31.6; // 2*sqrt(K): critically damped, no overshoot
const TAB_H: f32 = 30.0;
const TABS_BAR_H: f32 = 36.0;
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 12.0;
const GAP: f32 = 6.0;
const EQ_MIN: f32 = 0.4;

pub const TABS: [(&str, &str); 6] = [
    ("home", "\u{f015}"),
    ("media", "\u{f001}"),
    ("calendar", "\u{f073}"),
    ("clipboard", "\u{f0ea}"),
    ("ai", "\u{f06a9}"),
    ("controls", "\u{f013}"),
];

/// One damped-spring step, integrated in <= 4 ms substeps (frame-rate independent).
pub fn spring(x: f32, v: f32, target: f32, dt: f32) -> (f32, f32) {
    let n = ((dt / 0.004).ceil() as u32).max(1);
    let h = dt / n as f32;
    let (mut x, mut v) = (x, v);
    for _ in 0..n {
        v += (SPRING_K * (target - x) - SPRING_C * v) * h;
        x += v * h;
    }
    (x, v)
}

/// Collapsed/expanded geometry derived from settings and the output size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    /// Target collapsed width (adaptive once the output is known).
    pub coll_w: f32,
    /// Widest the collapsed notch may be right now without touching a side pill.
    pub cap_w: f32,
    pub coll_h: f32,
    pub exp_w: f32,
    pub exp_h_max: f32,
    pub top_margin: i32,
}

/// Clearance kept between the collapsed notch and the side pills.
const SIDE_GAP: f32 = 12.0;
/// Below this much room between the pills the notch drops under them.
const MIN_COLL_W: f32 = 180.0;
/// Top margin that clears the 26px side-pill row.
const LOWER_ROW_MARGIN: i32 = 30;

/// Collapsed width and whether the notch must sit below the side-pill row.
/// `configured` is the saved width (a preferred minimum), `preferred` what the content needs, `capacity` the
/// room between the pills.
pub fn fit_collapsed(output_w: f32, left: f32, right: f32, configured: f32, preferred: f32, exp_w: f32) -> (f32, f32, bool) {
    let capacity = output_w - 2.0 * (left.max(right) + SIDE_GAP);
    let want = configured.max(preferred).min(exp_w);
    if capacity < MIN_COLL_W {
        let cap = (output_w - SCREEN_MARGIN).max(1.0).min(exp_w);
        (want.min(cap), cap, true)
    } else {
        let cap = capacity.min(exp_w);
        (want.min(cap), cap, false)
    }
}
/// What the collapsed notch has to fit: measured clock, playing media, app count and the room the side pills take.
#[derive(Clone, Copy, Debug)]
pub struct Measured {
    pub time_w: f32,
    pub media: bool,
    pub n_apps: usize,
    pub left: f32,
    pub right: f32,
}

impl Geom {
    pub fn compute(st: &State, output: Option<&str>) -> Geom {
        Self::compute_at(st, output, st.settings.layout.top_margin as i32)
    }

    fn compute_at(st: &State, output: Option<&str>, top_margin: i32) -> Geom {
        let l = &st.settings.layout;
        let logical = output.and_then(|n| st.outputs.iter().find(|o| o.name == n)).map(|o| (o.size.0 as f32, o.size.1 as f32));
        let (lw, lh) = logical.filter(|(w, h)| *w > 0.0 && *h > 0.0).unwrap_or((1.0e6, 1.0e6));
        let top = top_margin as f32;
        let avail_w = (lw - SCREEN_MARGIN).max(1.0);
        let avail_h = (lh - top - SCREEN_MARGIN).max(1.0);
        let coll_w = (l.collapsed_width as f32).min(avail_w);
        Geom {
            coll_w,
            cap_w: coll_w,
            coll_h: (l.collapsed_height as f32).min(avail_h),
            exp_w: (l.expanded_width as f32).min(avail_w / HEADROOM),
            exp_h_max: avail_h / HEADROOM,
            top_margin,
        }
    }

    /// Base geometry widened to fit the collapsed content between the side pills; `m` carries the measured
    /// content. Before the output is known the base geometry is returned unchanged.
    pub fn adaptive(st: &State, output: Option<&str>, m: Measured) -> Geom {
        let base = Self::compute(st, output);
        let Some(o) = output.and_then(|n| st.outputs.iter().find(|o| o.name == n)).filter(|o| o.size.0 > 0 && o.size.1 > 0) else { return base };
        let pref = crate::dock::preferred_width(m.time_w, base.coll_h, m.media, m.n_apps);
        let (width, cap, lower) = fit_collapsed(o.size.0 as f32, m.left, m.right, st.settings.layout.collapsed_width as f32, pref, base.exp_w);
        let mut g = if lower { Self::compute_at(st, output, (st.settings.layout.top_margin as i32).max(LOWER_ROW_MARGIN)) } else { base };
        g.coll_w = width.min(g.exp_w);
        g.cap_w = cap.min(g.exp_w);
        g
    }

    /// Layer-surface size for an expanded card of height `th`.
    pub fn host(&self, th: f32) -> (u32, u32) {
        ((self.exp_w * HEADROOM).floor() as u32, (th.min(self.exp_h_max).max(RESERVE_H.min(self.exp_h_max)) * HEADROOM).floor() as u32)
    }
}

/// What the collapsed pill shows while a player exists.
pub struct CollapsedMedia {
    pub art: Option<Image>,
    pub bars: [f32; 6],
    pub playing: bool,
}

pub struct Notch {
    id: PanelId,
    hits: Hits,
    pages: Vec<(&'static str, Box<dyn Page>)>,
    cur: &'static str,
    /// Tab highlighted for secondary pages (settings -> controls, weather -> home ...).
    origin: &'static str,
    page_fade: f32,
    prog: f32,
    vel: f32,
    target: f32,
    size: (f32, f32),
    size_v: (f32, f32),
    size_t: (f32, f32),
    geom: Geom,
    host: (u32, u32),
    hover: bool,
    collapse_at: Option<Instant>,
    reset_at: Option<Instant>,
    kb: bool,
    last_rect: Rect,
    area: Rect,
    tabs_rect: Rect,
    morph: Morph,
    output_seen: Option<String>,
    viz_on: bool,
    /// Fullscreen window on this output (or a capture freeze): draw nothing.
    hidden: bool,
    /// Hover-open dwell deadline; only armed while the pointer rests on the clock/background, never on a tile.
    dwell_at: Option<Instant>,
    /// Shortcut key behind each app tile hit id (`ID_TILE + index`) of the last collapsed paint.
    tile_keys: Vec<String>,
    /// Eased hover level per tile hit id (0..1).
    tile_hover: Vec<(u32, f32)>,
    /// What lay under the pointer at press time (`Some(id)` for a tile, `None` for the background).
    press_id: Option<Option<u32>>,
    press_key: Option<String>,
    /// Rect of the pressed tile at press time.
    press_rect: Option<Rect>,
    /// Hit id and rect of every tile of the last collapsed paint.
    tile_rects: Vec<(u32, Rect)>,
    /// Hover label: text, tile rect, eased hover level.
    tip: Option<(String, Rect, f32)>,
    /// Current (animated) collapsed width and its velocity; `geom.coll_w` is the target.
    coll_w: f32,
    coll_v: f32,
    coll_ready: bool,
    drag: Option<Drag>,
    /// Drawn x of each tile by shortcut key, so tiles slide to their new slots.
    tile_x: Vec<(String, f32)>,
    sliding: bool,
    /// Seconds the slide keeps easing after a drop.
    settle: f32,
    last_dt: f32,
}

/// Collapsed app tiles live far above the tab ids (1..=6); `+N` has its own id.
const ID_OVERFLOW: u32 = 1_900_000;
const ID_TILE: u32 = 2_000_000;
/// Pointer travel before a press on a tile turns into a drag.
const DRAG_START: f32 = 6.0;

/// A tile press that may become a drag: shortcut index and key, press point, grab offset inside the tile, the
/// drop slot computed by the last paint.
#[derive(Clone, Debug)]
struct Drag {
    key: String,
    idx: usize,
    start: Point,
    pos: Point,
    grab: f32,
    active: bool,
    drop: usize,
}
/// Pointer must rest this long on the clock/background before hover opens the notch.
const DWELL: Duration = Duration::from_millis(350);
/// Hover highlight / icon scale easing time.
const TILE_FADE: f32 = 0.14;
/// Longest frame step fed to the springs (a resume after suspend must not integrate a long catch-up).
const MAX_DT: f32 = 0.05;
/// Clock size in the collapsed notch.
const CLOCK_PX: f32 = 14.0;
/// Room the charging bolt takes before the clock.
const BOLT_W: f32 = 12.0;

impl Notch {
    pub fn new(pages: Vec<(&'static str, Box<dyn Page>)>) -> Notch {
        Notch {
            id: 0,
            hits: Hits::default(),
            pages,
            cur: "home",
            origin: "home",
            page_fade: 1.0,
            prog: 0.0,
            vel: 0.0,
            target: 0.0,
            size: (0.0, 0.0),
            size_v: (0.0, 0.0),
            size_t: (0.0, 0.0),
            geom: Geom { coll_w: 360.0, cap_w: 360.0, coll_h: 40.0, exp_w: 560.0, exp_h_max: 800.0, top_margin: 4 },
            host: (582, 627),
            hover: false,
            collapse_at: None,
            reset_at: None,
            kb: false,
            last_rect: Rect::default(),
            area: Rect::default(),
            tabs_rect: Rect::default(),
            morph: Morph::default(),
            output_seen: None,
            viz_on: false,
            hidden: false,
            dwell_at: None,
            tile_keys: Vec::new(),
            tile_hover: Vec::new(),
            press_id: None,
            press_key: None,
            press_rect: None,
            tile_rects: Vec::new(),
            tip: None,
            coll_w: 360.0,
            coll_v: 0.0,
            coll_ready: false,
            drag: None,
            tile_x: Vec::new(),
            sliding: false,
            settle: 0.0,
            last_dt: 0.016,
        }
    }

    fn page_idx(&self, id: &str) -> Option<usize> { self.pages.iter().position(|(n, _)| *n == id) }

    fn expanded(&self) -> bool { self.target > 0.5 }

    fn tab_for(&self, page: &'static str) -> &'static str {
        match page {
            "weather" => "home",
            "settings" | "apps" | "wifi" | "bt" | "audio" | "battery" | "display" => "controls",
            p => TABS.iter().map(|t| t.0).find(|t| *t == p).unwrap_or("home"),
        }
    }

    fn show(&mut self, id: &'static str, ctx: &mut Ctx) {
        if id == self.cur || self.page_idx(id).is_none() { return; }
        if self.cur == "whatsapp" && id != "whatsapp" {
            let mut o = Vec::new();
            crate::whatsapp::drop_session(ctx.state, "page changed", None, &mut o);
        }
        if let Some(i) = self.page_idx(self.cur) { self.pages[i].1.hidden(ctx); }
        self.cur = id;
        // Secondary pages keep the tab they were entered from; tab pages highlight themselves.
        self.origin = match id {
            "weather" => "home",
            "settings" | "apps" | "wifi" | "bt" | "audio" | "battery" | "display" => "controls",
            _ => self.tab_for(id),
        };
        self.page_fade = 0.0;
        // from a collapsed notch `set_expanded(true)` activates the page, so it is activated exactly once
        if self.target > 0.5 { if let Some(i) = self.page_idx(id) { self.pages[i].1.shown(ctx); } }
    }

    fn set_expanded(&mut self, expand: bool, ctx: &mut Ctx) {
        if expand == (self.target > 0.5) { return; }
        self.target = if expand { 1.0 } else { 0.0 };
        self.collapse_at = None;
        if expand {
            self.reset_at = None;
            if let Some(i) = self.page_idx(self.cur) { self.pages[i].1.shown(ctx); }
        } else {
            let mut o = Vec::new();
            crate::whatsapp::drop_session(ctx.state, "collapsed", Some("home"), &mut o);
            for x in o { if let crate::whatsapp::Out::Leave(p) = x { if self.cur == "whatsapp" { self.show(p, ctx); } } }
            let secs = ctx.state.settings.behavior.home_reset_secs;
            if secs > 0 { self.reset_at = Some(Instant::now() + Duration::from_secs(secs as u64)); }
            if let Some(i) = self.page_idx(self.cur) { self.pages[i].1.hidden(ctx); }
        }
        crate::glue::set_active(expand);
        if !ctx.state.settings.behavior.animations {
            self.prog = self.target;
            self.vel = 0.0;
            self.size = self.size_t;
        }
    }

    fn schedule_collapse(&mut self, ctx: &Ctx) {
        let ms = ctx.state.settings.behavior.collapse_delay_ms as u64;
        self.collapse_at = Some(Instant::now() + Duration::from_millis(ms));
    }

    fn cur_page(&mut self) -> Option<&mut Box<dyn Page>> {
        let i = self.page_idx(self.cur)?;
        Some(&mut self.pages[i].1)
    }

    fn visible_tabs(&self, st: &State) -> Vec<(&'static str, &'static str)> {
        st.settings.tabs.iter().filter_map(|t| TABS.iter().find(|d| d.0 == t.as_str()).copied()).collect()
    }

    /// Target card size for the visible page.
    fn compute_target(&mut self, ctx: &mut Ctx) {
        let g = self.geom;
        let avail = Size { w: g.exp_w - 2.0 * PAD_X, h: g.exp_h_max - 2.0 * PAD_Y - GAP - TABS_BAR_H };
        let (state, text) = (&*ctx.state, &mut *ctx.text);
        let nat = match self.cur_page() { Some(p) => p.natural(avail, state, text), None => Size::default() };
        let tw = (nat.w + 2.0 * PAD_X).clamp(g.coll_w.max(360.0).min(g.exp_w), g.exp_w);
        let chrome = 2.0 * PAD_Y + GAP + TABS_BAR_H;
        let th = (nat.h + chrome).clamp(chrome + 40.0, g.exp_h_max);
        self.size_t = (tw, th);
        let host = g.host(th);
        self.host = host;
    }

    fn collapsed_media(st: &State) -> Option<CollapsedMedia> { crate::glue::collapsed_media(st) }

    /// Base geometry widened to the collapsed content, measured with the exact styles the notch paints with.
    fn measure_geom(&self, ctx: &mut Ctx) -> Geom {
        let st = &*ctx.state;
        let Some(out) = ctx.output.filter(|n| st.outputs.iter().any(|o| o.name == *n)) else { return Geom::compute(st, ctx.output) };
        let (time, _) = Self::clock_strings(st);
        let bolt = if st.status.power.as_ref().is_some_and(crate::glue::actively_charging) { BOLT_W } else { 0.0 };
        let time_w = bolt + ctx.text.measure(&time, &TextStyle::new(st.pal.fs(CLOCK_PX), st.pal.fg_hi).bold(), None).0;
        let m = Measured {
            time_w,
            media: Self::collapsed_media(st).is_some_and(|m| m.playing),
            n_apps: st.apps.shortcuts.len(),
            left: crate::pills::occupied_width(st, ctx.text, out),
            right: crate::rightpill::occupied_width(st, ctx.text),
        };
        Geom::adaptive(st, Some(out), m)
    }

    fn tick_inner(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        let dt = dt.min(MAX_DT);
        self.last_dt = dt;
        if self.settle > 0.0 { self.settle = (self.settle - dt).max(0.0); }
        let new_geom = self.measure_geom(ctx);
        if new_geom != self.geom { self.geom = new_geom; }
        if !self.coll_ready { self.coll_w = self.geom.coll_w; self.coll_v = 0.0; self.coll_ready = true; }
        // a smaller safety capacity applies at once; growth and voluntary shrinking animate
        self.coll_w = self.coll_w.min(self.geom.cap_w);
        if self.size_t == (0.0, 0.0) { self.size = (self.coll_w, self.geom.coll_h); }
        {
            let out = ctx.output.and_then(|n| ctx.state.outputs.iter().find(|o| o.name == n));
            let origin = out.map_or((0.0, 0.0), |o| (((o.size.0 as f32 - self.host.0 as f32) / 2.0).floor(), self.geom.top_margin as f32));
            let info = crate::state::NotchInfo { output: ctx.output.map(str::to_string), origin, bottom: self.geom.top_margin as f32 + self.geom.coll_h, expanded: self.target > 0.5, hover: self.hover };
            if ctx.state.notch != info {
                // the pills follow `State.notch.output`: repaint every panel as soon as it changes
                let moved = ctx.state.notch.output != info.output;
                ctx.state.notch = info;
                if moved { ctx.cmds.push(Cmd::RedrawAll); }
            }
        }
        // external navigation
        if let Some(nav) = ctx.state.nav.take() {
            match nav {
                NavTo::Page(id) => {
                    if let Some(id) = TABS.iter().map(|t| t.0).chain(["weather", "settings", "apps", "wifi", "bt", "audio", "battery", "display", "whatsapp"]).find(|p| *p == id) {
                        self.show(id, ctx);
                        self.set_expanded(true, ctx);
                        // opened from outside (pill click, fifo): fold away again unless the pointer arrives
                        if !self.hover { self.collapse_at = Some(Instant::now() + Duration::from_millis(3000)); }
                    }
                }
                NavTo::Collapse => self.set_expanded(false, ctx),
            }
        }
        // follow the focused output (only while idle and not under the pointer), or hop to the output a
        // WhatsApp session was opened from
        {
            let pin = ctx.state.wa.pin().map(str::to_string);
            let must_move = pin.as_deref().is_some_and(|p| Some(p) != ctx.output);
            let idle = self.target < 0.5 && self.prog < 0.02 && !self.hover;
            if (idle || must_move) && ctx.output.is_some() {
                let connected = |n: &String| ctx.state.outputs.iter().any(|o| &o.name == n);
                let desired = crate::desktop::select_output(&ctx.state.desk.snap, &ctx.state.settings, pin.as_deref(), ctx.output).filter(connected);
                if let Some(out) = desired.filter(|d| Some(d.as_str()) != ctx.output) {
                    let mut n = Notch::new(std::mem::take(&mut self.pages));
                    n.cur = self.cur;
                    n.origin = self.origin;
                    n.target = self.target;
                    n.prog = self.prog;
                    n.size = self.size;
                    n.size_t = self.size_t;
                    n.collapse_at = self.collapse_at;
                    n.reset_at = self.reset_at;
                    // publish the new output first so both pills switch in the same frame the notch moves
                    ctx.state.notch.output = Some(out.clone());
                    ctx.cmds.push(Cmd::RedrawAll);
                    ctx.cmds.push(Cmd::Close(self.id));
                    ctx.cmds.push(Cmd::Open { view: Box::new(n), output: Some(out) });
                    return false;
                }
            }
        }
        {
            let settled = self.target > 0.5 && self.prog > 0.99 && (self.size.0 - self.size_t.0).abs() < 0.6 && (self.size.1 - self.size_t.1).abs() < 0.6;
            for o in crate::whatsapp::drive(ctx.state, settled) {
                use crate::whatsapp::Out;
                match o {
                    Out::Show(p) => self.show(p, ctx),
                    Out::Expand => self.set_expanded(true, ctx),
                    Out::Collapse => self.set_expanded(false, ctx),
                    Out::ScheduleCollapse(ms) => { self.collapse_at = Some(Instant::now() + Duration::from_millis(ms)); }
                    Out::CancelCollapse => self.collapse_at = None,
                    Out::Leave(p) => { if self.cur == "whatsapp" { self.show(p, ctx); } }
                }
            }
        }
        let now = Instant::now();
        if let Some(at) = self.collapse_at {
            if now >= at {
                self.collapse_at = None;
                let keep = self.cur_page().is_some_and(|p| p.keeps_open()) || ctx.state.wa.holds_open();
                if keep { self.schedule_collapse(ctx); }
                else if !self.hover && !crate::whatsapp::close(ctx.state, crate::whatsapp::CloseTo::Collapse) { self.set_expanded(false, ctx); }
            }
        }
        if let Some(at) = self.dwell_at {
            if now >= at {
                self.dwell_at = None;
                if self.hover && !self.expanded() { self.set_expanded(true, ctx); }
            }
        }
        if let Some(at) = self.reset_at {
            if now >= at {
                self.reset_at = None;
                if !self.expanded() && !matches!(self.cur, "settings" | "home") { self.show("home", ctx); }
            }
        }
        if self.expanded() || self.prog > 0.01 { self.compute_target(ctx); } else { self.size_t = (self.geom.coll_w, self.geom.coll_h); self.host = self.geom.host(0.0); }
        let anim = ctx.state.settings.behavior.animations;
        let mut active = false;
        let (np, nv) = spring(self.prog, self.vel, self.target, dt);
        let done = (np - self.target).abs() < 0.0015 && nv.abs() < 0.02;
        if anim && !done { self.prog = np; self.vel = nv; active = true; } else { self.prog = self.target; self.vel = 0.0; }
        // size springs towards the page's own size while expanded, else towards the collapsed size
        let (cw, cv) = spring(self.coll_w, self.coll_v, self.geom.coll_w, dt);
        if anim && ((cw - self.geom.coll_w).abs() >= 0.5 || cv.abs() >= 1.0) { self.coll_w = cw.min(self.geom.cap_w); self.coll_v = cv; active = true; } else { self.coll_w = self.geom.coll_w; self.coll_v = 0.0; }
        let (tw, th) = if self.target > 0.5 { self.size_t } else { (self.coll_w, self.geom.coll_h) };
        let (w, vw) = spring(self.size.0, self.size_v.0, tw, dt);
        let (h, vh) = spring(self.size.1, self.size_v.1, th, dt);
        let sdone = (w - tw).abs() < 0.5 && vw.abs() < 1.0 && (h - th).abs() < 0.5 && vh.abs() < 1.0;
        if anim && !sdone { self.size = (w, h); self.size_v = (vw, vh); active = true; } else { self.size = (tw, th); self.size_v = (0.0, 0.0); }
        if self.page_fade < 1.0 { self.page_fade = (self.page_fade + dt / 0.16).min(1.0); active = true; }
        // eased tile hover (snaps when animations are off)
        {
            let hovered = self.hits.hovered();
            if let Some(h) = hovered { if h >= ID_TILE || h == ID_OVERFLOW { if !self.tile_hover.iter().any(|(i, _)| *i == h) { self.tile_hover.push((h, 0.0)); } } }
            let step = if anim { dt / TILE_FADE } else { 1.0 };
            for (id, l) in &mut self.tile_hover {
                let target = if Some(*id) == hovered { 1.0 } else { 0.0 };
                *l = if *l < target { (*l + step).min(target) } else { (*l - step).max(target) };
                active |= *l != target;
            }
            self.tile_hover.retain(|(_, l)| *l > 0.0);
        }
        active |= self.dwell_at.is_some() || self.sliding || self.settle > 0.0 || self.drag.as_ref().is_some_and(|d| d.active);
        // equaliser feed only while something plays and the pill is what is visible
        let want_viz = ctx.state.media.playing && !self.expanded() && self.prog < 0.05;
        if want_viz != self.viz_on { self.viz_on = want_viz; crate::media::set_visualizer(ctx.poster, want_viz); }
        if let Some(p) = self.cur_page() { active |= p.tick(dt, ctx); }
        active |= self.collapse_at.is_some() || self.reset_at.is_some();
        active
    }
}

impl View for Notch {
    fn attached(&mut self, id: PanelId) { self.id = id; }

    fn spec(&self, st: &State) -> PanelSpec {
        // the adaptive geometry (lower-row margin) applies once the output is known
        let g = if self.output_seen.is_some() { self.geom } else { Geom::compute(st, None) };
        let host = if self.host.0 > 0 { self.host } else { g.host(0.0) };
        PanelSpec {
            namespace: "waybar-notch",
            layer: Layer::Top,
            anchor: Anchor::TOP,
            size: host,
            margin: (g.top_margin, 0, 0, 0),
            exclusive: -1,
            kb: if self.kb && self.expanded() { KbMode::OnDemand } else { KbMode::None },
        }
    }

    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        self.output_seen = ctx.output.map(str::to_string);
        ctx.state.osd.collapsed = Some((self.coll_w, self.geom.coll_h));
        ctx.state.osd.output = ctx.output.map(str::to_string);
        let b = &ctx.state.settings.behavior;
        let hide = ctx.state.bar_hidden || (b.hide_on_fullscreen && ctx.output.is_some_and(|o| ctx.state.desk.is_fullscreen(o))) || ctx.state.desk.capture_active;
        ctx.state.osd.suppressed = hide;
        crate::osd::sync(ctx.state, ctx.cmds);
        if hide != self.hidden {
            self.hidden = hide;
            if hide && self.target > 0.5 { self.set_expanded(false, ctx); self.prog = 0.0; self.vel = 0.0; self.size = (self.geom.coll_w, self.geom.coll_h); }
        }
        let active = self.tick_inner(dt, ctx);
        self.kb = self.cur_page().is_some_and(|p| p.wants_keyboard());
        active
    }

    fn input_region(&self) -> Option<Vec<Rect>> { if self.hidden { Some(vec![]) } else { Some(vec![self.last_rect.pad(-1.0)]) } }

    fn cursor(&self) -> Cursor {
        let c = self.hits.cursor();
        if c != Cursor::Default { return c; }
        self.pages.iter().find(|(n, _)| *n == self.cur).map_or(Cursor::Default, |(_, p)| p.cursor())
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) {
        let open_on_hover = ctx.state.settings.behavior.open_on_hover;
        match &ev {
            Event::PointerEnter(p) => {
                self.hover = true;
                self.collapse_at = None;
                if !self.expanded() { self.update_dwell(*p, open_on_hover); }
            }
            Event::PointerMove(p) if !self.expanded() && self.drag.is_none() => self.update_dwell(*p, open_on_hover),
            Event::PointerLeave => {
                self.hover = false;
                self.dwell_at = None;
                self.press_id = None;
                self.drag = None;
                if self.expanded() { self.schedule_collapse(ctx); }
            }
            _ => {}
        }
        if !self.expanded() {
            self.collapsed_event(ev, ctx);
            return;
        }
        // tab bar
        if let Some(click) = self.hits.event(&ev) {
            if let Some(i) = (click.id as usize).checked_sub(1).filter(|i| *i < TABS.len()) {
                if click.button == Button::Left {
                    let id = TABS[i].0;
                    if self.cur == "whatsapp" && crate::whatsapp::close(ctx.state, crate::whatsapp::CloseTo::Tab(id)) { return; }
                    self.show(id, ctx);
                }
                return;
            }
        }
        if matches!(ev, Event::PointerLeave) {
            let area = self.area;
            if let Some(p) = self.cur_page() { p.event(&ev, area, ctx); }
            return;
        }
        if let Event::Key(k) = &ev {
            use smithay_client_toolkit::seat::keyboard::Keysym;
            if k.sym == Keysym::Escape && self.cur == "whatsapp" && crate::whatsapp::close(ctx.state, crate::whatsapp::CloseTo::Home) { return; }
            if k.sym == Keysym::Escape && !self.cur_page().is_some_and(|p| p.keeps_open() || p.wants_keyboard()) {
                self.set_expanded(false, ctx);
                return;
            }
        }
        // pointer events over the tab bar must not reach the page
        let over_tabs = match &ev { Event::Press(_, p) | Event::Release(_, p) | Event::PointerMove(p) | Event::Scroll { pos: p, .. } => self.tabs_rect.contains(*p), _ => false };
        if over_tabs { return; }
        let area = self.area;
        if let Some(p) = self.cur_page() { p.event(&ev, area, ctx); }
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx) {
        if self.hidden { self.last_rect = Rect::default(); return; }
        let st = dc.state;
        let pal = st.pal.clone();
        let (host_w, _host_h) = cv.size();
        let g = self.geom;
        let t = self.prog;
        let tc = t.clamp(0.0, 1.0);
        let co = (1.0 - tc / 0.22).clamp(0.0, 1.0); // collapsed content
        let eo = ((tc - 0.5) / 0.4).clamp(0.0, 1.0); // expanded content

        let cw = self.coll_w;
        let w = lerp(cw, self.size.0, tc).max(cw);
        let h = lerp(g.coll_h, self.size.1, tc).max(g.coll_h);
        let w = if self.target > 0.5 || self.prog > 0.01 { self.size.0.max(cw).min(w.max(self.size.0 * tc)) } else { w };
        let nrect = rect((host_w - w) / 2.0, 0.0, w, h.max(self.size.1 * tc).max(g.coll_h));
        self.last_rect = nrect;
        let radius = pal.radius.min(nrect.h / 2.0);
        let radii = r_all(radius);

        // glassmorphic surface: translucent body with a soft top sheen, no outline
        cv.fill_rrect(nrect, radii, with_a(pal.glass, 0.58 + 0.24 * tc));
        cv.fill_rrect(nrect, radii, crate::render::vgradient(nrect, with_a(pal.fg_hi, 0.10), with_a(pal.fg_hi, 0.0)));

        // layout reference: the notch at its target size, centred on the surface
        let (tw, th) = if self.expanded() || self.prog > 0.01 { self.size_t } else { (self.coll_w, g.coll_h) };
        let card = rect((host_w - tw) / 2.0, 0.0, tw, th);
        let tabs = self.visible_tabs(st);
        let tabs_w = tabs.len() as f32 * 36.0 + (tabs.len().saturating_sub(1)) as f32 * 4.0 + 6.0;
        self.tabs_rect = rect(card.x + (card.w - tabs_w) / 2.0, card.bottom() - PAD_Y - TABS_BAR_H, tabs_w, TABS_BAR_H);
        self.area = rect(card.x + PAD_X, PAD_Y, card.w - 2.0 * PAD_X, (self.tabs_rect.y - GAP - PAD_Y).max(0.0));
        let area = self.area;
        self.morph = match self.page_idx(self.cur) { Some(i) if eo > 0.0 || tc > 0.05 => self.pages[i].1.morph(area, st), _ => Morph::default() };

        self.hits.clear();
        cv.with_clip(nrect, radii, |cv| {
            if eo > 0.0 {
                let a = eo * smooth(self.page_fade.max(0.001));
                if let Some(i) = self.page_idx(self.cur) {
                    let page = &mut self.pages[i].1;
                    cv.with_alpha(eo, |cv| page.draw_backdrop(cv, dc, card));
                }
                cv.with_alpha(eo, |cv| self.draw_tabs(cv, &pal, &tabs));
                if let Some(i) = self.page_idx(self.cur) {
                    let page = &mut self.pages[i].1;
                    cv.with_alpha(a, |cv| page.draw(cv, dc, area));
                }
            }
            self.draw_collapsed_and_morph(cv, dc, &pal, nrect, tc, co, eo);
        });
        // tile label: drawn outside the notch clip, fading and sliding in with the hover level
        if let Some((name, r, l)) = self.tip.take() {
            if tc < 0.05 {
                let style = TextStyle::new(pal.fs(11.0), pal.fg_hi).weight(600);
                let w = cv.measure(&name, &style).0 + 18.0;
                let (host_w, _) = cv.size();
                let x = (r.center().x - w / 2.0).clamp(4.0, (host_w - w - 4.0).max(4.0));
                let k = crate::layout::smooth(l);
                let tip = rect(x, nrect.bottom() + 6.0 + (1.0 - k) * -4.0, w, 22.0);
                cv.with_alpha(k, |cv| {
                    cv.fill_rrect(tip, r_all(11.0), pal.glass);
                    cv.fill_rrect(tip, r_all(11.0), crate::render::vgradient(tip, with_a(pal.fg_hi, 0.09), with_a(pal.fg_hi, 0.0)));
                    cv.text(&name, &style, tip.center(), HAlign::Center, VAlign::Middle);
                });
            }
        }
    }
}

impl Notch {
    /// Arm or cancel the hover dwell: armed only over the clock/background (never over a tile).
    fn update_dwell(&mut self, p: Point, open_on_hover: bool) {
        // hit rectangles exist only while the collapsed content is interactive
        let over_tile = self.hits.at(p).is_some();
        if over_tile || !open_on_hover { self.dwell_at = None; }
        else if self.dwell_at.is_none() { self.dwell_at = Some(Instant::now() + DWELL); }
    }

    /// Pointer input while collapsed: tile clicks never expand the notch; a click on the background opens Home.
    fn collapsed_event(&mut self, ev: Event, ctx: &mut Ctx) {
        match &ev {
            Event::Press(b, p) => {
                self.dwell_at = None;
                let hit = self.hits.at(*p);
                self.press_id = Some(hit);
                self.press_key = hit.and_then(|id| self.tile_key(id)).map(str::to_string);
                self.press_rect = hit.and_then(|id| self.tile_rects.iter().find(|(i, _)| *i == id).map(|(_, r)| *r));
                self.drag = None;
                if *b == Button::Left {
                    if let (Some(id), Some(key), Some(r)) = (hit, self.press_key.clone(), self.press_rect) {
                        let idx = (id - ID_TILE) as usize;
                        self.drag = Some(Drag { key, idx, start: *p, pos: *p, grab: p.x - r.x, active: false, drop: idx });
                    }
                }
            }
            Event::Release(b, p) => {
                self.dwell_at = None;
                if let Some(d) = self.drag.take() {
                    if d.active {
                        self.press_id = None;
                        self.press_key = None;
                        self.press_rect = None;
                        self.hits.event(&ev);
                        if *b == Button::Left { self.commit_drag(d, ctx); }
                        return;
                    }
                }
                let down = self.press_id.take();
                let click = self.hits.event(&ev);
                let pressed_key = self.press_key.take();
                match down {
                    // pressed on a tile: only a release on the same tile counts
                    Some(Some(_)) => {
                        let Some(c) = click else { return };
                        if c.id == ID_OVERFLOW {
                            if *b == Button::Left { ctx.state.nav = Some(NavTo::Page("apps")); }
                            return;
                        }
                        let Some(key) = self.tile_key(c.id).map(str::to_string).filter(|k| pressed_key.as_ref() == Some(k)) else { return };
                        let st = &mut *ctx.state;
                        let Some(sc) = st.apps.shortcut(&key) else { return };
                        match *b {
                            Button::Left => st.apps.activate_shortcut(&key, false, st.notch.output.as_deref()),
                            Button::Right if sc.configured_id.is_some() => st.apps.activate_shortcut(&key, true, st.notch.output.as_deref()),
                            Button::Right => { st.apps.page_focus = Some(key); st.nav = Some(NavTo::Page("apps")); }
                            _ => {}
                        }
                    }
                    _ => {
                        if *b == Button::Left && self.hits.at(*p).is_none() {
                            self.show("home", ctx);
                            self.set_expanded(true, ctx);
                        }
                    }
                }
                return;
            }
            Event::PointerMove(p) => {
                if let Some(d) = self.drag.as_mut() {
                    d.pos = *p;
                    if !d.active && (p.x - d.start.x).abs() > DRAG_START { d.active = true; self.dwell_at = None; self.tip = None; }
                    if d.active { ctx.cmds.push(Cmd::Redraw(ctx.id)); }
                }
            }
            _ => {}
        }
        self.hits.event(&ev);
    }

    /// Drop of a dragged tile: reorder the shortcut row, apply it now and save it in the background.
    fn commit_drag(&mut self, d: Drag, ctx: &mut Ctx) {
        let st = &mut *ctx.state;
        let mut keys: Vec<String> = st.apps.shortcuts.iter().map(|s| s.key.clone()).collect();
        if d.idx >= keys.len() || keys[d.idx] != d.key || d.drop == d.idx { return; }
        let k = keys.remove(d.idx);
        keys.insert(d.drop.min(keys.len()), k);
        st.settings.app_order = keys.clone();
        st.apps.set_order(&keys);
        self.settle = 0.35;
        #[cfg(not(test))]
        std::thread::spawn(move || { if let Err(e) = crate::config::save_app_order(&keys) { eprintln!("waybar-notch: app order not saved: {e}"); } });
    }

    fn tile_key(&self, id: u32) -> Option<&str> { id.checked_sub(ID_TILE).and_then(|i| self.tile_keys.get(i as usize)).map(String::as_str) }

    fn draw_tabs(&mut self, cv: &mut Canvas, pal: &Palette, tabs: &[(&'static str, &'static str)]) {
        let bar = self.tabs_rect;
        cv.fill_rrect(bar, r_all(15.0), pal.surface);
        cv.stroke_rrect(bar, r_all(15.0), 1.0, with_a(pal.fg, 0.08));
        for (i, (id, glyph)) in tabs.iter().enumerate() {
            let r = rect(bar.x + 3.0 + i as f32 * 40.0, bar.y + (bar.h - TAB_H) / 2.0, 36.0, TAB_H);
            let idx = TABS.iter().position(|t| t.0 == *id).unwrap_or(0) as u32 + 1;
            let hovered = self.hits.add(r, idx);
            let active = *id == self.origin;
            if active {
                cv.fill_rrect(r, r_all(13.0), with_a(pal.accent, 0.28));
                cv.fill_rrect(rect(r.x + 8.0, r.bottom() - 2.0, r.w - 16.0, 2.0), r_all(1.0), pal.accent);
            } else if hovered {
                cv.fill_rrect(r, r_all(13.0), with_a(pal.fg, 0.12));
            }
            let col = if active { pal.accent_hi } else if hovered { pal.fg_hi } else { with_a(pal.fg_hi, 0.72) };
            cv.text(glyph, &TextStyle::icon(15.0, col), r.center(), HAlign::Center, VAlign::Middle);
        }
    }

    fn clock_strings(st: &State) -> (String, String) {
        let now = chrono::Local::now();
        if st.settings.behavior.clock_format.uses_12h() {
            (now.format("%-I:%M").to_string(), now.format("%p").to_string())
        } else {
            (now.format("%H:%M").to_string(), String::new())
        }
    }

    fn weather_strings(st: &State) -> (String, String) {
        match &st.weather.report {
            Some(r) => (r.icon.clone(), format!("{:.0}°C", r.temperature_c)),
            None => ("\u{e33d}".to_string(), "—".to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_collapsed_and_morph(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, pal: &Palette, nrect: Rect, tc: f32, co: f32, eo: f32) {
        let st = dc.state;
        let g = self.geom;
        let (host_w, _) = cv.size();
        let coll = rect((host_w - self.coll_w) / 2.0, 0.0, self.coll_w, g.coll_h);
        let cy = coll.y + coll.h / 2.0;
        let media = Self::collapsed_media(st);
        let (time, period) = Self::clock_strings(st);
        let (w_icon, w_temp) = Self::weather_strings(st);
        let p = self.prog; // may overshoot a hair; positions follow it, alphas use the clamped value
        let m = self.morph;

        let day_st = TextStyle::new(pal.fs(10.0), with_a(pal.fg, 0.65)).weight(600);
        let time_st = TextStyle::new(pal.fs(CLOCK_PX), pal.fg_hi).bold();
        let temp_st = TextStyle::new(pal.fs(11.0), pal.fg_hi).weight(600);
        let icon_sz = pal.fs(12.0);

        // --- collapsed layout: clock block, media slot, app tiles, then whatever room is left
        let day = chrono::Local::now().format("%a").to_string();
        let bolt_w = if st.status.power.as_ref().is_some_and(crate::glue::actively_charging) { BOLT_W } else { 0.0 };
        let dims = Dims { day_w: cv.measure(&day, &day_st).0, time_w: cv.measure(&time, &time_st).0, temp_w: cv.measure(&w_temp, &temp_st).0, icon_w: icon_sz, bolt_w };
        let lay = collapsed_layout(coll, dims, media.as_ref().is_some_and(|m| m.playing), st.apps.shortcuts.len());
        let (clock_c0, icon_c0) = (lay.clock_c, lay.weather_anchor);
        let art0 = lay.art.unwrap_or_default();

        // tile hits exist only while the collapsed content is the visible, settled state
        self.tile_keys.clear();
        self.tile_rects.clear();
        let interactive = self.target < 0.5 && co > 0.5;
        let mut tiles: Vec<(u32, Rect, usize)> = Vec::new();
        for (r, i) in &lay.tiles {
            let id = ID_TILE + tiles.len() as u32;
            self.tile_keys.push(st.apps.shortcuts[*i].key.clone());
            self.tile_rects.push((id, *r));
            if interactive { self.hits.add(*r, id); }
            tiles.push((id, *r, *i));
        }
        if let Some((r, _)) = lay.overflow {
            self.tile_rects.push((ID_OVERFLOW, r));
            if interactive { self.hits.add(r, ID_OVERFLOW); }
        }
        // hover label under the notch for the tile the pointer eases into
        self.tip = None;
        if interactive {
            let mut best = 0.02;
            for (id, r, i) in &tiles {
                let l = self.tile_level(*id);
                if l > best { best = l; self.tip = Some((st.apps.shortcuts[*i].name.clone(), *r, l)); }
            }
            if let Some((r, n)) = lay.overflow {
                let l = self.tile_level(ID_OVERFLOW);
                if l > best { self.tip = Some((format!("{n} more apps"), r, l)); }
            }
        }
        // drag preview: the other tiles slide aside to open the slot the dragged tile would drop into
        let dragging = self.drag.as_ref().filter(|d| d.active && d.idx < tiles.len()).cloned();
        if dragging.is_some() { self.tip = None; }
        let mut shown_tiles: Vec<(u32, Rect, usize)> = Vec::new();
        let mut lifted: Option<(u32, Rect, usize)> = None;
        let slide_k = 1.0 - (-self.last_dt * 24.0).exp();
        let animating = dragging.is_some() || self.settle > 0.0;
        let mut moving = false;
        let drop = dragging.as_ref().map_or(0, |d| tiles.iter().enumerate().filter(|(n, (_, r, _))| *n != d.idx && r.center().x < d.pos.x).count());
        if let Some(d) = self.drag.as_mut() { d.drop = if dragging.is_some() { drop } else { d.idx }; }
        for (n, (id, r, i)) in tiles.iter().enumerate() {
            let mut target = *r;
            if let Some(d) = &dragging {
                if n == d.idx {
                    let (lo, hi) = (tiles[0].1.x, tiles[tiles.len() - 1].1.x);
                    lifted = Some((*id, rect((d.pos.x - d.grab).clamp(lo, hi), r.y, r.w, r.h), *i));
                    continue;
                }
                let o = n - (n > d.idx) as usize;
                target = tiles[o + (o >= drop) as usize].1;
            }
            let key = &st.apps.shortcuts[*i].key;
            let pos = match self.tile_x.iter().position(|(k, _)| k == key) {
                Some(p) => p,
                None => { self.tile_x.push((key.clone(), target.x)); self.tile_x.len() - 1 }
            };
            let cur = &mut self.tile_x[pos].1;
            if animating {
                *cur += (target.x - *cur) * slide_k;
                if (target.x - *cur).abs() > 0.3 { moving = true; } else { *cur = target.x; }
            } else {
                *cur = target.x;
            }
            shown_tiles.push((*id, rect(*cur, target.y, target.w, target.h), *i));
        }
        self.tile_x.retain(|(k, _)| st.apps.shortcuts.iter().any(|s| s.key == *k));
        self.sliding = moving;
        // a press whose tile moved, resized or changed identity meanwhile must not fire on release
        if let (Some(Some(id)), Some(r)) = (self.press_id, self.press_rect) {
            let same = self.tile_rects.iter().any(|(i, tr)| *i == id && *tr == r)
                && (id == ID_OVERFLOW || self.tile_key(id).map(str::to_string) == self.press_key);
            if !same { self.press_id = None; self.press_key = None; self.press_rect = None; }
        }

        // --- plain collapsed bits and tiles (fade out first)
        if co > 0.0 {
            cv.with_alpha(co, |cv| {
                if let Some(x) = lay.day_x { cv.text(&day, &day_st, pt(x, cy), HAlign::Left, VAlign::Middle); }
                if let Some((_, tx)) = lay.weather { cv.text(&w_temp, &temp_st, pt(tx, cy), HAlign::Left, VAlign::Middle); }
                if let (Some(mm), Some(eq)) = (&media, lay.eq) { draw_eq(cv, pal, eq, &mm.bars, mm.playing); }
                for (id, r, i) in &shown_tiles { self.paint_tile(cv, pal, st, &st.apps.shortcuts[*i], *r, *id); }
                if let Some((id, r, i)) = &lifted {
                    // the dragged tile floats above the row with a soft shadow
                    let (rr, sh) = (r.pad_xy(-2.0, -2.0), r.pad_xy(-2.0, -2.0));
                    cv.fill_rrect(rect(sh.x, sh.y + 3.0, sh.w, sh.h), r_all(10.0), with_a(crate::render::rgb(0, 0, 0), 0.35));
                    self.paint_tile(cv, pal, st, &st.apps.shortcuts[*i], rr, *id);
                }
                if let Some((r, n)) = lay.overflow { self.paint_overflow(cv, pal, r, n); }
                if let Some(b) = lay.bolt { cv.text("\u{f0e7}", &TextStyle::icon(11.0, pal.accent_hi), b, HAlign::Center, VAlign::Middle); }
            });
        }

        // --- morphing elements
        // clock
        {
            let (c, size) = match m.clock {
                Some((tp, ts)) => (lerp_pt(clock_c0, tp, p), lerp(pal.fs(CLOCK_PX), ts, p)),
                None => (clock_c0, pal.fs(CLOCK_PX)),
            };
            let a = match (m.clock.is_some(), lay.clock) { (true, true) => 1.0, (true, false) => tc, (false, true) => co, (false, false) => 0.0 };
            if a > 0.0 {
                let sty = TextStyle::new(size, pal.fg_hi).bold();
                cv.with_alpha(a, |cv| {
                    let w = cv.text(&time, &sty, c, HAlign::Center, VAlign::Middle);
                    if !period.is_empty() && eo > 0.0 {
                        let ps = TextStyle::new(pal.fs(11.0), with_a(pal.fg, 0.72));
                        cv.with_alpha(eo, |cv| cv.text(&period, &ps, pt(c.x + w / 2.0 + 6.0, c.y + size * 0.22), HAlign::Left, VAlign::Middle));
                    }
                });
            }
        }
        // weather glyph (collapsed only when the layout had room for it)
        {
            let shown = lay.weather.is_some();
            let (c, size, a) = match m.weather_icon {
                Some((tp, ts)) => (lerp_pt(icon_c0, tp, p), lerp(icon_sz, ts, p), if shown { 1.0 } else { tc }),
                None => (icon_c0, icon_sz, if shown { co } else { 0.0 }),
            };
            if a > 0.0 { cv.with_alpha(a, |cv| { cv.text(&w_icon, &TextStyle::icon(size, pal.fg_hi), c, HAlign::Center, VAlign::Middle); }); }
        }
        // album art: the media status slot
        if let Some(mm) = &media {
            let (r, a) = match (m.art, lay.art.is_some()) {
                (Some(tr), true) => (lerp_rect(art0, tr, p), 1.0),
                (Some(tr), false) => (tr, tc),
                (None, true) => (art0, co),
                (None, false) => (art0, 0.0),
            };
            if a > 0.0 {
                let rad = lerp(8.0, 10.0, p.clamp(0.0, 1.0));
                cv.with_alpha(a, |cv| match &mm.art {
                    Some(img) => cv.image_cover(img, r, r_all(rad)),
                    None => {
                        cv.fill_rrect(r, r_all(rad), pal.hover);
                        cv.text("\u{f001}", &TextStyle::icon(r.h * 0.55, pal.dim), r.center(), HAlign::Center, VAlign::Middle);
                    }
                });
            }
        }
        let _ = nrect;
    }

    fn tile_level(&self, id: u32) -> f32 { self.tile_hover.iter().find(|(i, _)| *i == id).map_or(0.0, |(_, l)| *l) }

    /// One app tile: fixed hit cell, icon scaling 1.0 -> 1.10 on hover (0.94 pressed), active underline, running dot.
    fn paint_tile(&self, cv: &mut Canvas, pal: &Palette, st: &State, s: &crate::apps::Shortcut, cell: Rect, id: u32) {
        let lvl = self.tile_level(id);
        let scale = if self.hits.is_pressed(id) { 0.94 } else { 1.0 + 0.10 * lvl };
        let base = if s.active { with_a(pal.accent, 0.16 + 0.10 * lvl) } else { with_a(pal.fg, 0.05 + 0.10 * lvl) };
        cv.fill_rrect(cell, r_all(8.0), base);
        cv.stroke_rrect(cell, r_all(8.0), 1.0, with_a(if s.active { pal.accent } else { pal.fg }, 0.07 + 0.10 * lvl));
        let edge = icon_for(cell.w) * scale;
        let icon = Rect::around(cell.center(), edge, edge);
        let running = st.apps.running(s);
        cv.with_alpha(if running || s.pin.is_none() { 1.0 } else { 0.62 }, |cv| crate::apps_page::paint_icon(cv, pal, st, s, icon));
        let (cx, bottom) = (cell.center().x, cell.bottom());
        // one dot per open window (up to three) under the icon; a single focused window keeps the accent underline
        if s.active && s.windows <= 1 {
            cv.fill_rrect(rect(cx - 6.0, bottom - 3.5, 12.0, 2.0), r_all(1.0), pal.accent);
        } else if running {
            let n = s.windows.clamp(1, 3);
            let col = if s.active { pal.accent } else { with_a(pal.fg_hi, 0.85) };
            for k in 0..n {
                let x = cx + (k as f32 - (n as f32 - 1.0) / 2.0) * 5.0;
                cv.fill_circle(pt(x, bottom - 2.5), if s.active { 1.7 } else { 1.4 }, col);
            }
        }
        // an unread badge of the configured app takes precedence over the window count
        let badge = s.configured_id.as_deref().and_then(|id| st.apps.button(id)).and_then(|b| st.apps.shown(b).1).map(badge_text);
        if let Some(t) = badge {
            let style = TextStyle::new(8.5, crate::render::rgb(255, 255, 255)).weight(700);
            let w = (cv.measure(&t, &style).0 + 6.0).max(12.0).min(cell.w - 2.0);
            let r = rect(cell.right() - w - 1.0, cell.y + 1.0, w, 12.0);
            cv.fill_rrect(r, r_all(6.0), crate::render::rgb(0xe5, 0x3e, 0x3e));
            cv.text(&t, &style, r.center(), HAlign::Center, VAlign::Middle);
        }
    }

    fn paint_overflow(&self, cv: &mut Canvas, pal: &Palette, cell: Rect, n: usize) {
        let lvl = self.tile_level(ID_OVERFLOW);
        cv.fill_rrect(cell, r_all(8.0), with_a(pal.fg, 0.05 + 0.10 * lvl));
        cv.stroke_rrect(cell, r_all(8.0), 1.0, with_a(pal.fg, 0.07 + 0.10 * lvl));
        cv.text(&format!("+{n}"), &TextStyle::new(pal.fs(11.0), pal.fg_hi).weight(700), cell.center(), HAlign::Center, VAlign::Middle);
    }
}

/// Compact badge text: numbers above 99 read `99+`; anything else is shown as given.
fn badge_text(t: &str) -> String {
    match t.parse::<u32>() { Ok(n) if n > 99 => "99+".to_string(), _ => t.to_string() }
}

/// Equaliser: six rounded bars, accent while playing, dim when paused.
pub fn draw_eq(cv: &mut Canvas, pal: &Palette, r: Rect, bars: &[f32; 6], playing: bool) {
    let n = bars.len() as f32;
    let gap = 3.0 * (r.w / 36.0);
    let bw = (r.w - gap * (n - 1.0)) / n;
    let col = if playing { pal.accent } else { with_a(pal.fg, 0.35) };
    for (i, v) in bars.iter().enumerate() {
        let bh = (v.max(EQ_MIN) * r.h).min(r.h);
        let x = r.x + i as f32 * (bw + gap);
        cv.fill_rrect(rect(x, r.y + (r.h - bh) / 2.0, bw, bh), r_all(bw / 2.0), col);
    }
}

#[allow(dead_code)]
fn _unused(_: &TextSys, _: Point, _: f32) { let _ = window(0.0, 0.0, 1.0); let _ = Cmd::RedrawAll; }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_without_overshoot() {
        let (mut x, mut v) = (0.0f32, 0.0f32);
        let mut peak = 0.0f32;
        for _ in 0..240 {
            let (nx, nv) = spring(x, v, 1.0, 1.0 / 60.0);
            x = nx; v = nv;
            peak = peak.max(x);
        }
        assert!((x - 1.0).abs() < 0.002, "{x}");
        assert!(peak < 1.01, "{peak}");
    }

    #[test]
    fn morph_interpolates_endpoints() {
        let a = pt(10.0, 12.0);
        let b = pt(50.0, 40.0);
        assert_eq!(lerp_pt(a, b, 0.0), a);
        assert_eq!(lerp_pt(a, b, 1.0), b);
        let mid = lerp_pt(a, b, 0.5);
        assert_eq!((mid.x, mid.y), (30.0, 26.0));
        // clock grows 13px -> 36px
        assert_eq!(lerp(13.0, 36.0, 0.5), 24.5);
    }

    fn geom_for(l: crate::config::LayoutSettings, out: (i32, i32)) -> Geom {
        let mut st = State::new();
        st.settings.layout = l;
        st.outputs = vec![crate::wayland::OutputInfo { name: "X".into(), pos: (0, 0), size: out }];
        Geom::compute(&st, Some("X"))
    }

    #[test]
    fn collapsed_body_sits_below_the_top_gap() {
        let g = geom_for(crate::config::LayoutSettings { collapsed_width: 210, collapsed_height: 24, expanded_width: 520, top_margin: 2 }, (1200, 750));
        assert_eq!((g.top_margin, g.coll_h), (2, 24.0));
    }

    #[test]
    fn geometry_never_exceeds_the_output() {
        let l = crate::config::LayoutSettings { collapsed_width: 420, collapsed_height: 48, expanded_width: 960, top_margin: 24 };
        let g = geom_for(l.clone(), (600, 400));
        assert_eq!(g.coll_w, 420.0);
        assert!(g.exp_w * HEADROOM <= 600.0 - 16.0 + 1e-3);
        let (hw, hh) = g.host(10_000.0);
        assert!(hw as f32 <= 584.0 && hh as f32 <= 400.0 - 24.0 - 16.0 + 1.0);
        let tiny = geom_for(l.clone(), (300, 100));
        assert!(tiny.coll_w <= 284.0 && tiny.coll_h <= 60.0);
        let big = geom_for(l, (3000, 2000));
        assert_eq!((big.exp_w, big.coll_w), (960.0, 420.0));
    }

    #[test]
    fn geom_clamps_to_small_outputs() {
        let mut st = State::new();
        st.outputs = vec![crate::wayland::OutputInfo { name: "X".into(), pos: (0, 0), size: (400, 300) }];
        let g = Geom::compute(&st, Some("X"));
        assert!(g.exp_w <= (400.0 - SCREEN_MARGIN) / HEADROOM + 0.01);
        assert!(g.exp_h_max < 300.0);
    }

    // ---- collapsed dock: dwell, tile clicks, animation switch

    fn dock_state(n_apps: usize) -> State {
        let mut st = State::new();
        st.settings.behavior.open_on_hover = true;
        let open = (0..n_apps).map(|i| crate::apps::OpenApp { key: format!("app:zz-none-{i}"), name: format!("Zz{i}"), configured_id: None, desktop_id: None,
            windows: vec![crate::apps::OpenWindow { address: format!("0x{:x}", 0xfff0 + i), class: format!("zz-none-{i}"), initial_class: format!("zz-none-{i}"), title: String::new(), workspace: 1, workspace_name: "1".into(), focus_order: i as i64 }],
            active: i == 0 }).collect();
        st.apps.set_open(open);
        st
    }

    fn step(n: &mut Notch, st: &mut State, secs: f32) -> bool {
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        let mut active = false;
        let mut t = 0.0;
        while t < secs {
            let mut ctx = Ctx { state: st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None };
            active = n.tick(0.016, &mut ctx);
            t += 0.016;
        }
        active
    }

    fn send(n: &mut Notch, st: &mut State, ev: Event) {
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        let mut ctx = Ctx { state: st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None };
        n.event(ev, &mut ctx);
    }

    fn paint(n: &mut Notch, st: &State, scale: f32) -> tiny_skia::Pixmap {
        let mut rig = crate::testkit::Rig::new();
        rig.scale = scale;
        let spec = n.spec(st);
        let (w, h) = ((spec.size.0 as f32 * scale).ceil() as u32, (spec.size.1 as f32 * scale).ceil() as u32);
        let mut pm = tiny_skia::Pixmap::new(w, h).unwrap();
        let mut cmds = Vec::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut rig.text, scale);
            let mut dc = DrawCtx { state: st, cmds: &mut cmds, poster: &rig.poster, id: 1, output: None };
            n.draw(&mut cv, &mut dc);
        }
        pm
    }

    #[test]
    fn hover_over_a_tile_never_expands_and_a_tile_click_does_not_expand_either() {
        let mut st = dock_state(4);
        let mut n = Notch::new(crate::pages::build());
        step(&mut n, &mut st, 0.05);
        let pm = paint(&mut n, &st, 1.6);
        crate::testkit::save_over_backdrop(&pm, 10, "/tmp/notch-dock-4apps.png");
        let tile = n.hits.items_rect(ID_TILE).center();
        send(&mut n, &mut st, Event::PointerEnter(tile));
        assert!(n.dwell_at.is_none() && !n.expanded(), "entering moves nothing");
        std::thread::sleep(Duration::from_millis(500));
        step(&mut n, &mut st, 0.05);
        assert!(!n.expanded(), "no dwell over a tile");
        send(&mut n, &mut st, Event::Press(Button::Left, tile));
        send(&mut n, &mut st, Event::Release(Button::Left, tile));
        step(&mut n, &mut st, 0.05);
        assert!(!n.expanded(), "app click focuses/launches without expanding");
        assert!(st.nav.is_none());
        // right click on a general app opens Applications focused on that group
        send(&mut n, &mut st, Event::Press(Button::Right, tile));
        send(&mut n, &mut st, Event::Release(Button::Right, tile));
        assert_eq!((st.nav.clone(), st.apps.page_focus.as_deref()), (Some(NavTo::Page("apps")), Some("app:zz-none-0")));
    }

    #[test]
    fn dwell_on_the_clock_opens_after_350ms_and_leaving_cancels() {
        let mut st = dock_state(4);
        let mut n = Notch::new(crate::pages::build());
        step(&mut n, &mut st, 0.05);
        paint(&mut n, &st, 1.6);
        let bg = pt(n.geom.coll_w / 2.0 + (n.host.0 as f32 - n.geom.coll_w) / 2.0, 1.0);
        assert!(n.hits.at(bg).is_none());
        send(&mut n, &mut st, Event::PointerEnter(bg));
        assert!(n.dwell_at.is_some() && !n.expanded());
        send(&mut n, &mut st, Event::PointerLeave);
        assert!(n.dwell_at.is_none());
        send(&mut n, &mut st, Event::PointerEnter(bg));
        std::thread::sleep(Duration::from_millis(400));
        step(&mut n, &mut st, 0.05);
        assert!(n.expanded());
        // reverse mid-spring: bounded geometry, velocity kept
        step(&mut n, &mut st, 0.1);
        let mid = n.prog;
        assert!(mid > 0.0 && mid < 1.0);
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        n.set_expanded(false, &mut Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None });
        for _ in 0..40 { step(&mut n, &mut st, 0.016); assert!(n.prog > -0.2 && n.prog < 1.2); }
    }

    #[test]
    fn disabled_animations_snap_and_request_no_frames() {
        let mut st = dock_state(2);
        st.settings.behavior.animations = false;
        st.status.power = Some(crate::power::PowerSnapshot { available: true, ac_online: Some(true), percentage: Some(50.0), batteries: vec![crate::power::BatterySnapshot {
            name: "BAT0".into(), state: crate::power::BatteryState::Charging, percentage: Some(50.0), power_w: None, energy_now_wh: None, energy_full_wh: None, energy_design_wh: None,
            health_pct: None, voltage_v: None, cycles: None, technology: None, manufacturer: None, model: None, time_to_empty_secs: None, time_to_full_secs: None }] });
        let mut n = Notch::new(crate::pages::build());
        step(&mut n, &mut st, 0.05);
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        n.set_expanded(true, &mut Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None });
        assert_eq!(n.prog, 1.0, "snaps to the final state");
        let active = step(&mut n, &mut st, 0.1);
        assert_eq!(n.vel, 0.0);
        assert!(!active || n.page_fade < 1.0, "no continuous frames once settled");
    }

    #[test]
    fn huge_frame_steps_are_clamped() {
        let mut st = dock_state(0);
        let mut n = Notch::new(crate::pages::build());
        step(&mut n, &mut st, 0.05);
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        n.set_expanded(true, &mut Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None });
        let mut ctx = Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None };
        n.tick(30.0, &mut ctx);
        assert!(n.prog < 0.5, "a 30s resume step integrates at most 50ms: {}", n.prog);
    }

    // ---- output selection: the pills follow `State.notch.output`

    fn two_outputs(focused: &str) -> State {
        let mut st = State::new();
        st.outputs = ["X", "Y"].iter().enumerate().map(|(i, n)| crate::wayland::OutputInfo { name: n.to_string(), pos: (1600 * i as i32, 0), size: (1600, 1000) }).collect();
        let mon = |id: i64, name: &str, x: i64| serde_json::json!({"id": id, "name": name, "width": 1600, "height": 1000, "scale": 1.0, "x": x, "y": 0,
            "focused": name == focused, "activeWorkspace": {"id": id + 1}, "specialWorkspace": {"id": 0}});
        st.desk.snap = crate::desktop::parse_snapshot(&serde_json::json!([mon(0, "X", 0), mon(1, "Y", 1600)]), &serde_json::json!([]));
        st
    }

    fn tick_on(n: &mut Notch, st: &mut State, out: &str) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        let mut ctx = Ctx { state: st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: Some(out) };
        n.tick(0.016, &mut ctx);
        cmds
    }

    fn opens(cmds: &[Cmd]) -> Vec<Option<&str>> {
        cmds.iter().filter_map(|c| if let Cmd::Open { output, .. } = c { Some(output.as_deref()) } else { None }).collect()
    }

    #[test]
    fn the_notch_publishes_its_output_and_repaints_everything_when_it_changes() {
        let mut st = two_outputs("X");
        let mut n = Notch::new(Vec::new());
        let cmds = tick_on(&mut n, &mut st, "X");
        assert_eq!(st.notch.output.as_deref(), Some("X"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::RedrawAll)), "pills learn about the first output at once");
        let cmds = tick_on(&mut n, &mut st, "X");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::RedrawAll)), "no repaint storm while nothing moved");
    }

    #[test]
    fn an_idle_notch_hops_to_the_focused_output_and_publishes_it_before_the_swap() {
        let mut st = two_outputs("Y");
        let mut n = Notch::new(Vec::new());
        n.id = 7;
        let cmds = tick_on(&mut n, &mut st, "X");
        assert_eq!(opens(&cmds), [Some("Y")], "exactly one replacement notch");
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Close(7))) && cmds.iter().any(|c| matches!(c, Cmd::RedrawAll)));
        assert_eq!(st.notch.output.as_deref(), Some("Y"), "published before the commands run");
    }

    #[test]
    fn no_hop_while_expanded_hovered_pinned_or_to_a_disconnected_output() {
        let mut st = two_outputs("Y");
        let mut n = Notch::new(Vec::new());
        n.hover = true;
        assert!(opens(&tick_on(&mut n, &mut st, "X")).is_empty(), "hovered");
        let mut n = Notch::new(Vec::new());
        n.target = 1.0;
        assert!(opens(&tick_on(&mut n, &mut st, "X")).is_empty(), "expanded");
        let mut n = Notch::new(Vec::new());
        st.outputs.retain(|o| o.name == "X");
        assert!(opens(&tick_on(&mut n, &mut st, "X")).is_empty(), "Y is not an actual output");
        assert_eq!(st.notch.output.as_deref(), Some("X"));
    }

    #[test]
    fn fixed_mode_keeps_the_notch_on_the_fixed_connector() {
        let mut st = two_outputs("Y");
        st.settings.monitor.mode = crate::config::MonitorMode::Fixed;
        st.settings.monitor.output = Some("X".into());
        let mut n = Notch::new(Vec::new());
        assert!(opens(&tick_on(&mut n, &mut st, "X")).is_empty(), "already on the fixed output despite focus elsewhere");
        let mut n = Notch::new(Vec::new());
        assert_eq!(opens(&tick_on(&mut n, &mut st, "Y")), [Some("X")]);
    }
}


#[cfg(test)]
mod adaptive_tests {
    use super::*;
    use crate::wayland::OutputInfo;

    #[test]
    fn saved_width_is_a_minimum_and_expanded_width_the_cap() {
        let fit = |ow, l, r, pref| fit_collapsed(ow, l, r, 360.0, pref, 560.0);
        // 1200px output, left 150, right 200: four apps keep 360, ten widen to 444, twenty stop at 560
        let pref = |n: usize| crate::dock::preferred_width(40.0, 40.0, false, n);
        assert_eq!(fit(1200.0, 150.0, 200.0, pref(4)), (360.0, 560.0, false));
        assert_eq!(fit(1200.0, 150.0, 200.0, pref(10)), (444.0, 560.0, false));
        assert_eq!(fit(1200.0, 150.0, 200.0, pref(20)), (560.0, 560.0, false));
        // a side pill that leaves less room shrinks the width and the safety cap with it
        let (w, cap, lower) = fit(700.0, 150.0, 200.0, pref(10));
        assert!(!lower && w == cap && cap == 700.0 - 2.0 * 212.0, "{w} {cap}");
    }

    #[test]
    fn too_little_room_between_the_pills_drops_the_notch_below_them() {
        let (w, cap, lower) = fit_collapsed(640.0, 150.0, 250.0, 360.0, 216.0, 560.0);
        assert!(lower && w <= 640.0 - SCREEN_MARGIN && cap <= 640.0 - SCREEN_MARGIN);
        let mut st = State::new();
        st.outputs = vec![OutputInfo { name: "X".into(), pos: (0, 0), size: (640, 500) }];
        let g = Geom::adaptive(&st, Some("X"), Measured { time_w: 40.0, media: false, n_apps: 4, left: 150.0, right: 250.0 });
        assert_eq!(g.top_margin, (st.settings.layout.top_margin as i32).max(30));
        let roomy = Geom::adaptive(&st, Some("X"), Measured { time_w: 40.0, media: false, n_apps: 4, left: 100.0, right: 100.0 });
        assert_eq!(roomy.top_margin, st.settings.layout.top_margin as i32);
        // before the output is known nothing is widened
        assert_eq!(Geom::adaptive(&st, None, Measured { time_w: 40.0, media: false, n_apps: 40, left: 0.0, right: 0.0 }), Geom::compute(&st, None));
    }

    fn dock(n_apps: usize) -> State {
        let mut st = State::new();
        st.outputs = vec![OutputInfo { name: "X".into(), pos: (0, 0), size: (1400, 800) }];
        let open = (0..n_apps).map(|i| crate::apps::OpenApp { key: format!("app:zz-none-{i}"), name: format!("Zz{i}"), configured_id: None, desktop_id: None,
            windows: vec![crate::apps::OpenWindow { address: format!("0x{:x}", 0xfff0 + i), class: format!("zz-none-{i}"), initial_class: format!("zz-none-{i}"), title: String::new(), workspace: 1, workspace_name: "1".into(), focus_order: i as i64 }],
            active: i == 0 }).collect();
        st.apps.set_open(open);
        st
    }

    fn run(n: &mut Notch, st: &mut State, secs: f32) {
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        let mut t = 0.0;
        while t < secs {
            let mut ctx = Ctx { state: st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: Some("X") };
            n.tick(0.016, &mut ctx);
            t += 0.016;
        }
    }

    #[test]
    fn more_apps_widen_the_notch_by_animation_and_snap_without_it() {
        let mut st = dock(4);
        let mut n = Notch::new(crate::pages::build());
        run(&mut n, &mut st, 0.1);
        assert_eq!(n.coll_w, 360.0);
        st = dock(12);
        run(&mut n, &mut st, 0.032);
        assert!(n.geom.coll_w > 360.0 && n.coll_w > 360.0 && n.coll_w < n.geom.coll_w, "growing: {} -> {}", n.coll_w, n.geom.coll_w);
        run(&mut n, &mut st, 2.0);
        assert_eq!(n.coll_w, n.geom.coll_w);
        assert!(n.coll_w >= crate::dock::preferred_width(20.0, 40.0, false, 12), "every app fits: {}", n.coll_w);
        // animations off: snaps immediately and clears the velocity
        st = dock(4);
        st.settings.behavior.animations = false;
        run(&mut n, &mut st, 0.016);
        assert_eq!((n.coll_w, n.coll_v), (n.geom.coll_w, 0.0));
    }

    #[test]
    fn dragging_a_tile_reorders_and_a_plain_click_does_not() {
        let mut st = dock(4);
        st.settings.app_order.clear(); // State::new reads the real config
        st.apps.set_order(&[]);
        let mut n = Notch::new(crate::pages::build());
        run(&mut n, &mut st, 0.1);
        let mut rig = crate::testkit::Rig::new();
        let paint = |n: &mut Notch, st: &State, rig: &mut crate::testkit::Rig| {
            let spec = n.spec(st);
            let mut pm = tiny_skia::Pixmap::new(spec.size.0, spec.size.1).unwrap();
            let mut cmds = Vec::new();
            let mut cv = Canvas::new(&mut pm, &mut rig.text, 1.0);
            let mut dc = DrawCtx { state: st, cmds: &mut cmds, poster: &rig.poster, id: 1, output: Some("X") };
            n.draw(&mut cv, &mut dc);
        };
        let send = |n: &mut Notch, st: &mut State, ev: Event| {
            let mut cmds = Vec::new();
            let poster = crate::wayland::Poster::detached();
            let mut text = TextSys::new();
            n.event(ev, &mut Ctx { state: st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: Some("X") });
        };
        paint(&mut n, &st, &mut rig);
        let first = st.apps.shortcuts[0].key.clone();
        let (a, b, c) = (n.hits.items_rect(ID_TILE).center(), n.hits.items_rect(ID_TILE + 1).center(), n.hits.items_rect(ID_TILE + 2).center());
        send(&mut n, &mut st, Event::PointerEnter(a));
        // a press-release without travel is a click: nothing is reordered
        send(&mut n, &mut st, Event::Press(Button::Left, a));
        send(&mut n, &mut st, Event::PointerMove(pt(a.x + 2.0, a.y)));
        assert!(!n.drag.as_ref().unwrap().active);
        send(&mut n, &mut st, Event::Release(Button::Left, a));
        assert!(st.settings.app_order.is_empty());
        // dragging tile 0 past tile 2 drops it into slot 2
        send(&mut n, &mut st, Event::Press(Button::Left, a));
        send(&mut n, &mut st, Event::PointerMove(pt(c.x + 4.0, a.y)));
        assert!(n.drag.as_ref().unwrap().active);
        paint(&mut n, &st, &mut rig);
        assert_eq!(n.drag.as_ref().unwrap().drop, 2);
        send(&mut n, &mut st, Event::Release(Button::Left, pt(c.x + 4.0, a.y)));
        assert_eq!(st.apps.shortcuts[2].key, first, "dropped into the third slot");
        assert_eq!(st.settings.app_order, st.apps.shortcuts.iter().map(|s| s.key.clone()).collect::<Vec<_>>());
        let _ = b;
    }

    #[test]
    fn a_press_does_not_survive_the_tile_moving_underneath_it() {
        let mut st = dock(4);
        let mut n = Notch::new(crate::pages::build());
        run(&mut n, &mut st, 0.1);
        let mut rig = crate::testkit::Rig::new();
        let paint = |n: &mut Notch, st: &State, rig: &mut crate::testkit::Rig| {
            let spec = n.spec(st);
            let mut pm = tiny_skia::Pixmap::new(spec.size.0, spec.size.1).unwrap();
            let mut cmds = Vec::new();
            let mut cv = Canvas::new(&mut pm, &mut rig.text, 1.0);
            let mut dc = DrawCtx { state: st, cmds: &mut cmds, poster: &rig.poster, id: 1, output: Some("X") };
            n.draw(&mut cv, &mut dc);
        };
        paint(&mut n, &st, &mut rig);
        let p = n.hits.items_rect(ID_TILE).center();
        n.hits.event(&Event::PointerEnter(p));
        n.hits.event(&Event::PointerMove(p));
        let mut cmds = Vec::new();
        let poster = crate::wayland::Poster::detached();
        let mut text = TextSys::new();
        let mut ctx = Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: Some("X") };
        n.event(Event::Press(Button::Left, p), &mut ctx);
        assert!(n.press_key.is_some());
        // the row is reordered/resized: another app now sits where the pressed tile was
        st = dock(12);
        run(&mut n, &mut st, 0.05);
        paint(&mut n, &st, &mut rig);
        assert!(n.press_id.is_none() && n.press_key.is_none(), "stale press cancelled");
    }
}

#[cfg(test)]
mod activation_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct Counting(Rc<Cell<u32>>);
    impl Page for Counting {
        fn natural(&mut self, avail: Size, _: &State, _: &mut TextSys) -> Size { Size { w: avail.w, h: 100.0 } }
        fn draw(&mut self, _: &mut Canvas, _: &mut DrawCtx, _: Rect) {}
        fn event(&mut self, _: &Event, _: Rect, _: &mut Ctx) {}
        fn shown(&mut self, _: &mut Ctx) { self.0.set(self.0.get() + 1); }
    }

    #[test]
    fn navigating_from_the_collapsed_notch_activates_the_page_once() {
        let n_shown = Rc::new(Cell::new(0));
        let mut n = Notch::new(vec![("home", Box::new(Counting(Rc::new(Cell::new(0))))), ("apps", Box::new(Counting(n_shown.clone())))]);
        let mut st = State::new();
        let mut cmds = Vec::new();
        let mut text = TextSys::new();
        let poster = crate::wayland::Poster::detached();
        st.nav = Some(NavTo::Page("apps"));
        let mut ctx = Ctx { state: &mut st, cmds: &mut cmds, text: &mut text, poster: &poster, id: 1, output: None };
        n.tick(0.016, &mut ctx);
        assert!(n.expanded() && n.cur == "apps");
        assert_eq!(n_shown.get(), 1, "page_focus must be consumed once");
        // already expanded: switching away and back activates once per visit
        n.show("home", &mut ctx);
        n.show("apps", &mut ctx);
        assert_eq!(n_shown.get(), 2);
    }
}
