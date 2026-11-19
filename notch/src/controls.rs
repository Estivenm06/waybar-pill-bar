//! Controls tab (utility tiles) and Clipboard tab.

use crate::layout::{Rect, Scroll, Size, pt, rect};
use crate::page::Page;
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::sysinfo::{self, CaptureAction};
use crate::ui::{ButtonKind, Hits, button, scrollbar};
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event, Poster};
use std::time::Duration;

const COLS: usize = 3;
const TILE_H: f32 = 68.0;
const GAP: f32 = 8.0;
const ID_TILE0: u32 = 100;
const ID_SETTINGS: u32 = 99;
const ID_CONFIRM_YES: u32 = 1;
const ID_CONFIRM_NO: u32 = 2;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tile { Screenshot, Record, Nightlight, Dnd, Wifi, Bluetooth, Lock, ClearClipboard, Audio, Display, Settings }

impl Tile {
    fn from_id(id: &str) -> Option<Tile> {
        Some(match id {
            "screenshot" => Tile::Screenshot, "record" => Tile::Record, "nightlight" => Tile::Nightlight, "dnd" => Tile::Dnd,
            "wifi" => Tile::Wifi, "bluetooth" => Tile::Bluetooth, "lock" => Tile::Lock, "clear-clipboard" => Tile::ClearClipboard,
            "audio" => Tile::Audio, "display" => Tile::Display, _ => return None,
        })
    }
}

#[derive(Default)]
pub struct Controls {
    hits: Hits,
    confirm_clear: bool,
    tiles: Vec<Tile>,
}

fn tile_visual(t: Tile, st: &State) -> (&'static str, &'static str, bool) {
    let s = &st.sys;
    match t {
        Tile::Screenshot => ("\u{f030}", "Screenshot", false),
        Tile::Record => (if s.recording { "\u{f04d}" } else { "\u{f03d}" }, if s.recording { "Stop recording" } else { "Record" }, s.recording),
        Tile::Nightlight => (if s.nightlight { "\u{f06e}" } else { "\u{f070}" }, "Reading mode", s.nightlight),
        Tile::Dnd => (if s.dnd { "\u{f1f6}" } else { "\u{f0f3}" }, "Do not disturb", s.dnd),
        Tile::Wifi => (if st.status.wifi.on { "\u{f1eb}" } else { "\u{f092e}" }, "Wi-Fi", st.status.wifi.on),
        Tile::Bluetooth => (if st.status.bt.on { "\u{f293}" } else { "\u{f294}" }, "Bluetooth", st.status.bt.on),
        Tile::Lock => ("\u{f023}", "Lock", false),
        Tile::ClearClipboard => ("\u{f1f8}", "Clear clipboard", false),
        Tile::Audio => ("\u{f028}", "Audio", false),
        Tile::Display => ("\u{f108}", "Displays", false),
        Tile::Settings => ("\u{f013}", "Settings", false),
    }
}

/// Collapse the notch, wait for the collapse frames, then run `f` (screenshots/locks must not capture the notch).
fn after_collapse(ctx: &mut Ctx, f: impl FnOnce(&Poster) + Send + 'static) {
    ctx.state.nav = Some(NavTo::Collapse);
    let p = ctx.poster.clone();
    std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(600)); f(&p); });
}

impl Controls {
    pub fn new() -> Controls { Controls::default() }

    fn refresh_tiles(&mut self, st: &State) {
        self.tiles = st.settings.utilities.iter().filter_map(|u| Tile::from_id(u)).collect();
        self.tiles.push(Tile::Settings);
    }

    fn rows(&self) -> usize { self.tiles.len().div_ceil(COLS) }
}

impl Page for Controls {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        self.refresh_tiles(st);
        let extra = if self.confirm_clear { 40.0 } else { 0.0 } + if status_text(st).is_some() { 22.0 } else { 0.0 };
        Size { w: avail.w, h: self.rows() as f32 * (TILE_H + GAP) - GAP + extra }
    }

    fn shown(&mut self, ctx: &mut Ctx) { self.confirm_clear = false; ctx.state.sys.message = None; }
    fn hidden(&mut self, _: &mut Ctx) { self.confirm_clear = false; }
    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.refresh_tiles(st);
        self.hits.clear();
        let tw = (area.w - GAP * (COLS as f32 - 1.0)) / COLS as f32;
        for (i, t) in self.tiles.iter().enumerate() {
            let (c, r) = (i % COLS, i / COLS);
            let rr = rect(area.x + c as f32 * (tw + GAP), area.y + r as f32 * (TILE_H + GAP), tw, TILE_H);
            let id = if *t == Tile::Settings { ID_SETTINGS } else { ID_TILE0 + i as u32 };
            let hov = self.hits.add(rr, id);
            let (glyph, label, active) = tile_visual(*t, st);
            let rec = *t == Tile::Record && active;
            let fill = if rec { with_a(pal.red, if hov { 0.34 } else { 0.22 }) }
                else if active { with_a(pal.accent, if hov { 0.34 } else { 0.22 }) }
                else { with_a(pal.fg, if hov { 0.14 } else { 0.06 }) };
            cv.fill_rrect(rr, r_all(12.0), fill);
            cv.stroke_rrect(rr, r_all(12.0), 1.0, if hov { with_a(pal.accent, 0.5) } else { with_a(pal.fg, 0.08) });
            let col = if rec { pal.red } else if active { pal.accent } else if hov { pal.fg_hi } else { with_a(pal.fg, 0.75) };
            let busy = st.sys.capture_pending && matches!(t, Tile::Screenshot | Tile::Record);
            cv.with_alpha(if busy { 0.5 } else { 1.0 }, |cv| {
                cv.text(glyph, &TextStyle::icon(pal.fs(20.0), col), pt(rr.center().x, rr.y + 24.0), HAlign::Center, VAlign::Middle);
                cv.text_in(label, &TextStyle::new(pal.fs(11.0), pal.fg_hi), rect(rr.x + 4.0, rr.y + 40.0, rr.w - 8.0, 20.0), HAlign::Center);
            });
        }
        let mut y = area.y + self.rows() as f32 * (TILE_H + GAP) - GAP + 8.0;
        if self.confirm_clear {
            let row = rect(area.x, y, area.w, 30.0);
            cv.text_in("Clear clipboard history?", &pal.text(12.0), rect(row.x, row.y, row.w - 150.0, row.h), HAlign::Left);
            let yes = rect(row.right() - 140.0, row.y, 66.0, 28.0);
            let no = rect(row.right() - 68.0, row.y, 66.0, 28.0);
            let hy = self.hits.add(yes, ID_CONFIRM_YES);
            let hn = self.hits.add(no, ID_CONFIRM_NO);
            button(cv, pal, yes, "Clear", ButtonKind::Danger, hy, false);
            button(cv, pal, no, "Cancel", ButtonKind::Plain, hn, false);
            y += 40.0;
        }
        if let Some(t) = status_text(st) {
            cv.text_in(&t, &pal.text_dim(11.0), rect(area.x, y - 4.0, area.w, 18.0), HAlign::Left);
        }
    }

    fn event(&mut self, ev: &Event, _: Rect, ctx: &mut Ctx) {
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_CONFIRM_YES => { self.confirm_clear = false; sysinfo::clip_clear(ctx.poster); }
            ID_CONFIRM_NO => self.confirm_clear = false,
            ID_SETTINGS => ctx.state.nav = Some(NavTo::Page("settings")),
            id if id >= ID_TILE0 => {
                let Some(t) = self.tiles.get((id - ID_TILE0) as usize).copied() else { return };
                match t {
                    Tile::Screenshot if !ctx.state.sys.capture_pending => after_collapse(ctx, |p| sysinfo::capture(p, CaptureAction::Screenshot)),
                    Tile::Record if !ctx.state.sys.capture_pending => {
                        let a = if ctx.state.sys.recording { CaptureAction::RecordStop } else { CaptureAction::RecordStart };
                        after_collapse(ctx, move |p| sysinfo::capture(p, a));
                    }
                    Tile::Nightlight => sysinfo::toggle_nightlight(ctx.poster),
                    Tile::Dnd => sysinfo::toggle_dnd(ctx.poster),
                    Tile::Wifi => ctx.state.nav = Some(NavTo::Page("wifi")),
                    Tile::Bluetooth => ctx.state.nav = Some(NavTo::Page("bt")),
                    Tile::Lock if sysinfo::on_path("omarchy") => after_collapse(ctx, |p| sysinfo::lock_screen(p)),
                    Tile::ClearClipboard => self.confirm_clear = true,
                    Tile::Audio => ctx.state.nav = Some(NavTo::Page("audio")),
                    Tile::Display => ctx.state.nav = Some(NavTo::Page("display")),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn status_text(st: &State) -> Option<String> {
    st.sys.capture_error.clone().or_else(|| st.sys.message.clone())
}

// ---------------------------------------------------------------- clipboard

const ROW_H: f32 = 34.0;

#[derive(Default)]
pub struct Clipboard {
    hits: Hits,
    scroll: Scroll,
}

impl Clipboard {
    pub fn new() -> Clipboard { Clipboard::default() }
}

impl Page for Clipboard {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        let n = st.sys.clip.len().clamp(1, 8) as f32;
        Size { w: avail.w, h: 30.0 + n * (ROW_H + 6.0) }
    }
    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.hits.clear();
        cv.text("Clipboard", &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), pt(area.x, area.y + 8.0), HAlign::Left, VAlign::Middle);
        let hist = &st.sys.clip;
        if let Some(cs) = &st.sys.clip_status {
            cv.text_in(&format!("{cs:?}"), &pal.text_dim(11.0), rect(area.x + 100.0, area.y, area.w - 100.0, 18.0), HAlign::Right);
        }
        let list = rect(area.x, area.y + 28.0, area.w, (area.h - 28.0).max(0.0));
        self.scroll.content_h = hist.len() as f32 * (ROW_H + 6.0);
        self.scroll.clamp(list.h);
        if hist.is_empty() {
            cv.text_in("Nothing copied yet", &pal.text_dim(12.0), rect(list.x, list.y + 8.0, list.w, 24.0), HAlign::Center);
        }
        cv.with_clip(list, [0.0; 4], |cv| {
            for (i, e) in hist.iter().enumerate() {
                let rr = rect(list.x, list.y + i as f32 * (ROW_H + 6.0) - self.scroll.offset, list.w - 6.0, ROW_H);
                if rr.bottom() < list.y || rr.y > list.bottom() { continue; }
                let hov = self.hits.add_in(rr, list, 100 + i as u32);
                cv.fill_rrect(rr, r_all(10.0), if hov { with_a(pal.accent, 0.25) } else { with_a(pal.fg, 0.07) });
                cv.text_in(&sysinfo::clip_preview(e, 80), &TextStyle::new(pal.fs(12.0), if hov { pal.fg_hi } else { pal.fg }), rr.pad_xy(10.0, 0.0), HAlign::Left);
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
    }

    fn event(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, area.h - 28.0); return; }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left || c.id < 100 { return; }
        if let Some(e) = ctx.state.sys.clip.get((c.id - 100) as usize).cloned() {
            sysinfo::clip_copy(&e);
            ctx.state.nav = Some(NavTo::Collapse);
        }
    }
}
