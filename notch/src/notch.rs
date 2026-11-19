//! The notch: one layer surface whose content *morphs* between the collapsed
//! pill and the expanded card instead of cross-fading.
//!
//! `prog` (0 collapsed .. 1 expanded) is a critically damped spring. The
//! clock, weather glyph and album art are drawn by the notch itself at
//! positions/sizes interpolated between their collapsed spot and the target
//! the current page reports through `Page::morph`. Everything else (tab bar,
//! page content) fades in once `prog` passes 0.5.

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
    pub coll_w: f32,
    pub coll_h: f32,
    pub exp_w: f32,
    pub exp_h_max: f32,
    pub top_margin: i32,
}

impl Geom {
    pub fn compute(st: &State, output: Option<&str>) -> Geom {
        let l = &st.settings.layout;
        let logical = output.and_then(|n| st.outputs.iter().find(|o| o.name == n)).map(|o| (o.size.0 as f32, o.size.1 as f32));
        let (lw, lh) = logical.filter(|(w, h)| *w > 0.0 && *h > 0.0).unwrap_or((1.0e6, 1.0e6));
        let top = l.top_margin as f32;
        let avail_w = (lw - SCREEN_MARGIN).max(1.0);
        let avail_h = (lh - top - SCREEN_MARGIN).max(1.0);
        Geom {
            coll_w: (l.collapsed_width as f32).min(avail_w),
            coll_h: (l.collapsed_height as f32).min(avail_h),
            exp_w: (l.expanded_width as f32).min(avail_w / HEADROOM),
            exp_h_max: avail_h / HEADROOM,
            top_margin: l.top_margin as i32,
        }
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
    charge_phase: f32,
    kb: bool,
    last_rect: Rect,
    area: Rect,
    tabs_rect: Rect,
    morph: Morph,
    output_seen: Option<String>,
    viz_on: bool,
    /// Fullscreen window on this output (or a capture freeze): draw nothing.
    hidden: bool,
}

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
            geom: Geom { coll_w: 210.0, coll_h: 24.0, exp_w: 520.0, exp_h_max: 800.0, top_margin: 2 },
            host: (582, 627),
            hover: false,
            collapse_at: None,
            reset_at: None,
            charge_phase: 0.0,
            kb: false,
            last_rect: Rect::default(),
            area: Rect::default(),
            tabs_rect: Rect::default(),
            morph: Morph::default(),
            output_seen: None,
            viz_on: false,
            hidden: false,
        }
    }

    fn page_idx(&self, id: &str) -> Option<usize> { self.pages.iter().position(|(n, _)| *n == id) }

    fn expanded(&self) -> bool { self.target > 0.5 }

    fn tab_for(&self, page: &'static str) -> &'static str {
        match page {
            "weather" => "home",
            "settings" | "wifi" | "bt" | "audio" | "battery" | "display" => "controls",
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
            "settings" | "wifi" | "bt" | "audio" | "battery" | "display" => "controls",
            _ => self.tab_for(id),
        };
        self.page_fade = 0.0;
        if let Some(i) = self.page_idx(id) { self.pages[i].1.shown(ctx); }
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

    fn tick_inner(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        let new_geom = Geom::compute(ctx.state, ctx.output);
        if new_geom != self.geom { self.geom = new_geom; }
        if self.size_t == (0.0, 0.0) { self.size = (self.geom.coll_w, self.geom.coll_h); }
        {
            let out = ctx.output.and_then(|n| ctx.state.outputs.iter().find(|o| o.name == n));
            let origin = out.map_or((0.0, 0.0), |o| (((o.size.0 as f32 - self.host.0 as f32) / 2.0).floor(), self.geom.top_margin as f32));
            let info = crate::state::NotchInfo { output: ctx.output.map(str::to_string), origin, expanded: self.target > 0.5, hover: self.hover };
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
                    if let Some(id) = TABS.iter().map(|t| t.0).chain(["weather", "settings", "wifi", "bt", "audio", "battery", "display", "whatsapp"]).find(|p| *p == id) {
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
        let (tw, th) = if self.target > 0.5 { self.size_t } else { (self.geom.coll_w, self.geom.coll_h) };
        let (w, vw) = spring(self.size.0, self.size_v.0, tw, dt);
        let (h, vh) = spring(self.size.1, self.size_v.1, th, dt);
        let sdone = (w - tw).abs() < 0.5 && vw.abs() < 1.0 && (h - th).abs() < 0.5 && vh.abs() < 1.0;
        if anim && !sdone { self.size = (w, h); self.size_v = (vw, vh); active = true; } else { self.size = (tw, th); self.size_v = (0.0, 0.0); }
        if self.page_fade < 1.0 { self.page_fade = (self.page_fade + dt / 0.16).min(1.0); active = true; }
        let charging = ctx.state.status.power.as_ref().is_some_and(crate::glue::actively_charging);
        if charging { self.charge_phase = (self.charge_phase + dt / 3.0) % 1.0; active = true; }
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
        let g = Geom::compute(st, self.output_seen.as_deref());
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
        ctx.state.osd.collapsed = Some((self.geom.coll_w, self.geom.coll_h));
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
            Event::PointerEnter(_) => {
                self.hover = true;
                self.collapse_at = None;
                if open_on_hover { self.set_expanded(true, ctx); }
            }
            Event::PointerLeave => {
                self.hover = false;
                if self.expanded() { self.schedule_collapse(ctx); }
            }
            _ => {}
        }
        if !self.expanded() {
            if let Event::Release(Button::Left, _) = ev { self.set_expanded(true, ctx); }
            self.hits.event(&ev);
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

        let w = lerp(g.coll_w, self.size.0, tc).max(g.coll_w);
        let h = lerp(g.coll_h, self.size.1, tc).max(g.coll_h);
        let w = if self.target > 0.5 || self.prog > 0.01 { self.size.0.max(g.coll_w).min(w.max(self.size.0 * tc)) } else { w };
        let nrect = rect((host_w - w) / 2.0, 0.0, w, h.max(self.size.1 * tc).max(g.coll_h));
        self.last_rect = nrect;
        let radius = pal.radius.min(nrect.h / 2.0);
        let radii = r_all(radius);

        // surface + charging edge
        let charging = st.status.power.as_ref().is_some_and(crate::glue::actively_charging);
        glass(cv, &pal, nrect, radii);
        if charging {
            let pulse = 0.5 + 0.5 * (self.charge_phase * std::f32::consts::TAU).sin();
            cv.stroke_rrect(nrect, radii, 1.0, with_a(mix(pal.accent, pal.accent_hi, pulse), 0.65 + 0.35 * pulse));
        }

        // layout reference: the notch at its target size, centred on the surface
        let (tw, th) = if self.expanded() || self.prog > 0.01 { self.size_t } else { (g.coll_w, g.coll_h) };
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
    }
}

impl Notch {
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
        let coll = rect((host_w - g.coll_w) / 2.0, 0.0, g.coll_w, g.coll_h);
        let cy = coll.y + coll.h / 2.0;
        let media = Self::collapsed_media(st);
        let (time, period) = Self::clock_strings(st);
        let (w_icon, w_temp) = Self::weather_strings(st);
        let p = self.prog; // may overshoot a hair; positions follow it, alphas use the clamped value
        let m = self.morph;

        let day_st = TextStyle::new(pal.fs(10.0), with_a(pal.fg, 0.65)).weight(600);
        let time_st = TextStyle::new(pal.fs(13.0), pal.fg_hi).bold();
        let temp_st = TextStyle::new(pal.fs(11.0), pal.fg_hi).weight(600);
        let icon_sz = pal.fs(12.0);

        // --- collapsed layout positions
        let (clock_c0, icon_c0, temp_x0, day_x0, art0, eq0);
        let day = chrono::Local::now().format("%a").to_string();
        let day_w = cv.measure(&day, &day_st).0;
        let time_w = cv.measure(&time, &time_st).0;
        let temp_w = cv.measure(&w_temp, &temp_st).0;
        let icon_w = icon_sz;
        if media.is_none() {
            let total = day_w + 8.0 + time_w + 8.0 + icon_w + 3.0 + temp_w;
            let x0 = coll.x + (coll.w - total) / 2.0;
            day_x0 = x0;
            clock_c0 = pt(x0 + day_w + 8.0 + time_w / 2.0, cy);
            icon_c0 = pt(x0 + day_w + 8.0 + time_w + 8.0 + icon_w / 2.0, cy);
            temp_x0 = x0 + day_w + 8.0 + time_w + 8.0 + icon_w + 3.0;
            art0 = Rect::default();
            eq0 = Rect::default();
        } else {
            day_x0 = 0.0;
            clock_c0 = pt(coll.center().x, cy);
            let total = icon_w + 3.0 + temp_w;
            let x0 = coll.x + (coll.w - total) / 2.0;
            icon_c0 = pt(x0 + icon_w / 2.0, cy);
            temp_x0 = x0 + icon_w + 3.0;
            let side = (g.coll_h - 4.0).clamp(1.0, 20.0);
            art0 = rect(coll.x + 12.0, cy - side / 2.0, side, side);
            eq0 = rect(coll.right() - 12.0 - 36.0, cy - 8.0, 36.0, 16.0);
        }

        // --- plain collapsed bits (fade out first)
        if co > 0.0 {
            cv.with_alpha(co, |cv| {
                if media.is_none() { cv.text(&day, &day_st, pt(day_x0, cy), HAlign::Left, VAlign::Middle); }
                cv.text(&w_temp, &temp_st, pt(temp_x0, cy), HAlign::Left, VAlign::Middle);
                if let Some(mm) = &media {
                    draw_eq(cv, pal, eq0, &mm.bars, mm.playing);
                }
                if st.status.power.as_ref().is_some_and(crate::glue::actively_charging) {
                    cv.text("\u{f0e7}", &TextStyle::icon(10.0, pal.accent_hi), pt(coll.right() - 4.0, coll.y + 6.0), HAlign::Right, VAlign::Middle);
                }
            });
        }

        // --- morphing elements
        // clock
        {
            let (c, size, a) = match (m.clock, media.is_none()) {
                (Some((tp, ts)), true) => (lerp_pt(clock_c0, tp, p), lerp(pal.fs(13.0), ts, p), 1.0),
                (Some((tp, ts)), false) => (tp, ts, eo),
                (None, true) => (clock_c0, pal.fs(13.0), co),
                (None, false) => (clock_c0, pal.fs(13.0), 0.0),
            };
            if a > 0.0 {
                let big = ((size - pal.fs(13.0)) / (36.0 - pal.fs(13.0))).clamp(0.0, 1.0);
                let colr = mix(pal.fg_hi, pal.fg_hi, big);
                let sty = TextStyle::new(size, colr).bold();
                cv.with_alpha(a, |cv| {
                    let w = cv.text(&time, &sty, c, HAlign::Center, VAlign::Middle);
                    if !period.is_empty() && eo > 0.0 {
                        let ps = TextStyle::new(pal.fs(11.0), with_a(pal.fg, 0.72));
                        cv.with_alpha(eo, |cv| cv.text(&period, &ps, pt(c.x + w / 2.0 + 6.0, c.y + size * 0.22), HAlign::Left, VAlign::Middle));
                    }
                });
            }
        }
        // weather glyph
        {
            let (c, size, a) = match m.weather_icon {
                Some((tp, ts)) => (lerp_pt(icon_c0, tp, p), lerp(icon_sz, ts, p), 1.0),
                None => (icon_c0, icon_sz, co),
            };
            if a > 0.0 { cv.with_alpha(a, |cv| { cv.text(&w_icon, &TextStyle::icon(size, pal.fg_hi), c, HAlign::Center, VAlign::Middle); }); }
        }
        // album art
        if let Some(mm) = &media {
            let (r, a) = match m.art {
                Some(tr) => (lerp_rect(art0, tr, p), 1.0),
                None => (art0, co),
            };
            if a > 0.0 {
                let rad = lerp(5.0, 10.0, p.clamp(0.0, 1.0));
                cv.with_alpha(a, |cv| match &mm.art {
                    Some(img) => cv.image_cover(img, r, r_all(rad)),
                    None => {
                        cv.fill_rrect(r, r_all(rad), pal.hover);
                        cv.text("\u{f001}", &TextStyle::icon(r.h * 0.55, pal.dim), r.center(), HAlign::Center, VAlign::Middle);
                    }
                });
            }
        } else if let Some(tr) = m.art {
            let _ = tr; // no player: Home draws its own placeholder
        }
        let _ = (nrect, tc);
    }
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

