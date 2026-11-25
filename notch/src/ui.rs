//! Immediate-mode UI kit: palette, hit regions, text field, small widgets.
//!
//! Pattern for a view:
//! ```ignore
//! // draw():
//! self.hits.clear();
//! let hovered = self.hits.add(rect, ID_PLAY);       // registers + returns hover state
//! draw_button(cv, &pal, rect, hovered, ...);
//! // event():
//! if let Some(click) = self.hits.event(&ev) { match click.id { ID_PLAY => ..., _ => {} } }
//! ```
//! Hit regions from the previous frame serve the next event; later `add`s are on top.

use crate::layout::{Point, Rect, pt, rect};
use crate::render::{Canvas, Color, Fill, HAlign, TextStyle, TextSys, VAlign, mix, r_all, rgb, rgba, with_a};
use crate::theme::{self, Rgb};
use crate::wayland::{Button, Cursor, Event, KeyInput};
use smithay_client_toolkit::seat::keyboard::Keysym;

fn c(rgb_: Rgb) -> Color { rgb(rgb_.0, rgb_.1, rgb_.2) }

/// Colours derived from the Omarchy theme and the notch appearance settings.
#[derive(Clone, Debug)]
pub struct Palette {
    pub bg: Color,
    /// Panel fill: theme bg, fully opaque.
    pub glass: Color,
    pub fg: Color,
    pub fg_hi: Color,
    pub dim: Color,
    pub faint: Color,
    pub edge: Color,
    pub hover: Color,
    pub active: Color,
    pub surface: Color,
    pub accent: Color,
    pub accent_hi: Color,
    pub accent_lo: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub is_light: bool,
    pub radius: f32,
    pub font_scale: f32,
}

impl Palette {
    pub fn current(radius: f32, font_size: f32) -> Palette {
        let t = theme::current();
        let fg = c(t.fg);
        let bg = c(t.bg);
        Palette {
            bg,
            glass: with_a(bg, 1.0),
            fg,
            fg_hi: c(t.fg_hi()),
            dim: with_a(fg, 0.66),
            faint: with_a(fg, 0.40),
            edge: with_a(fg, 0.16),
            hover: with_a(fg, 0.10),
            active: with_a(fg, 0.18),
            surface: with_a(fg, 0.07),
            accent: c(t.accent),
            accent_hi: c(t.accent_hi()),
            accent_lo: c(t.accent_lo()),
            red: c(t.red),
            green: c(t.green),
            yellow: c(t.yellow),
            is_light: t.is_light(),
            radius,
            font_scale: font_size / 12.0,
        }
    }
    /// Scaled font size (settings `font_size` relative to the 12px baseline).
    pub fn fs(&self, px: f32) -> f32 { px * self.font_scale }
    pub fn text(&self, px: f32) -> TextStyle { TextStyle::new(self.fs(px), self.fg) }
    pub fn text_dim(&self, px: f32) -> TextStyle { TextStyle::new(self.fs(px), self.dim) }
    pub fn icon(&self, px: f32) -> TextStyle { TextStyle::icon(px, self.fg) }
}

impl Default for Palette {
    fn default() -> Self { Palette::current(22.0, 12.0) }
}

/// A completed click: press and release on the same region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Click {
    pub id: u32,
    pub button: Button,
    pub pos: Point,
}

#[derive(Default)]
pub struct Hits {
    items: Vec<(Rect, u32, Cursor)>,
    pub pos: Option<Point>,
    pressed: Option<u32>,
}

impl Hits {
    pub fn clear(&mut self) { self.items.clear(); }

    /// Register a region; returns whether the pointer is currently over it (topmost only).
    pub fn add(&mut self, r: Rect, id: u32) -> bool { self.add_cursor(r, id, Cursor::Pointer) }
    pub fn add_cursor(&mut self, r: Rect, id: u32, cursor: Cursor) -> bool {
        self.items.push((r, id, cursor));
        self.pos.is_some_and(|p| r.contains(p)) && self.hit_after_is_none(self.items.len() - 1, self.pos.unwrap())
    }
    /// Register `r` clipped to `viewport` (scroll areas).
    pub fn add_in(&mut self, r: Rect, viewport: Rect, id: u32) -> bool {
        let clipped = r.intersect(viewport);
        if clipped.is_empty() { return false; }
        self.add(clipped, id)
    }
    fn hit_after_is_none(&self, idx: usize, p: Point) -> bool { !self.items[idx + 1..].iter().any(|(r, ..)| r.contains(p)) }

    #[cfg(test)]
    pub fn items_rect(&self, id: u32) -> Rect { self.items.iter().find(|(_, i, _)| *i == id).map(|(r, ..)| *r).expect("registered hit") }
    pub fn at(&self, p: Point) -> Option<u32> { self.items.iter().rev().find(|(r, ..)| r.contains(p)).map(|(_, id, _)| *id) }
    pub fn hovered(&self) -> Option<u32> { self.pos.and_then(|p| self.at(p)) }
    pub fn is_hover(&self, id: u32) -> bool { self.hovered() == Some(id) }
    pub fn is_pressed(&self, id: u32) -> bool { self.pressed == Some(id) && self.is_hover(id) }
    pub fn cursor(&self) -> Cursor {
        self.pos.and_then(|p| self.items.iter().rev().find(|(r, ..)| r.contains(p))).map_or(Cursor::Default, |(_, _, c)| *c)
    }

    /// Feed a pointer event; returns a click when a press/release pair lands on one region.
    pub fn event(&mut self, ev: &Event) -> Option<Click> {
        match ev {
            Event::PointerEnter(p) | Event::PointerMove(p) => { self.pos = Some(*p); None }
            Event::PointerLeave => { self.pos = None; self.pressed = None; None }
            Event::Press(_, p) => { self.pos = Some(*p); self.pressed = self.at(*p); None }
            Event::Release(b, p) => {
                self.pos = Some(*p);
                let down = self.pressed.take();
                let up = self.at(*p);
                match (down, up) { (Some(a), Some(b2)) if a == b2 => Some(Click { id: a, button: *b, pos: *p }), _ => None }
            }
            _ => None,
        }
    }
}

// ---- text field ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldResult { None, Changed, Submit, Cancel }

#[derive(Clone, Debug, Default)]
pub struct TextField {
    pub text: String,
    /// Caret position in chars.
    pub cursor: usize,
    /// Selection anchor in chars (selection is anchor..cursor).
    pub anchor: Option<usize>,
    pub focused: bool,
    pub placeholder: String,
    pub password: bool,
    scroll_x: f32,
}

impl TextField {
    pub fn new(placeholder: &str) -> Self { TextField { placeholder: placeholder.into(), ..Default::default() } }
    pub fn set_text(&mut self, s: &str) { self.text = s.to_string(); self.cursor = s.chars().count(); self.anchor = None; }
    pub fn clear(&mut self) { self.set_text(""); }
    fn len(&self) -> usize { self.text.chars().count() }
    fn byte(&self, ci: usize) -> usize { self.text.char_indices().nth(ci).map_or(self.text.len(), |(b, _)| b) }
    fn selection(&self) -> Option<(usize, usize)> {
        self.anchor.filter(|a| *a != self.cursor).map(|a| (a.min(self.cursor), a.max(self.cursor)))
    }
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(a, b)| self.text[self.byte(a)..self.byte(b)].to_string())
    }
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else { self.anchor = None; return false };
        let (ba, bb) = (self.byte(a), self.byte(b));
        self.text.replace_range(ba..bb, "");
        self.cursor = a;
        self.anchor = None;
        true
    }
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        let b = self.byte(self.cursor);
        self.text.insert_str(b, &clean);
        self.cursor += clean.chars().count();
    }
    fn word_left(&self) -> usize {
        let cs: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i > 0 && cs[i - 1].is_whitespace() { i -= 1; }
        while i > 0 && !cs[i - 1].is_whitespace() { i -= 1; }
        i
    }
    fn word_right(&self) -> usize {
        let cs: Vec<char> = self.text.chars().collect();
        let mut i = self.cursor;
        while i < cs.len() && !cs[i].is_whitespace() { i += 1; }
        while i < cs.len() && cs[i].is_whitespace() { i += 1; }
        i
    }
    fn move_to(&mut self, to: usize, extend: bool) {
        if extend { self.anchor.get_or_insert(self.cursor); } else { self.anchor = None; }
        self.cursor = to.min(self.len());
    }

    pub fn key(&mut self, k: &KeyInput) -> FieldResult {
        let before = self.text.clone();
        match k.sym {
            Keysym::Return | Keysym::KP_Enter => return FieldResult::Submit,
            Keysym::Escape => return FieldResult::Cancel,
            Keysym::BackSpace => {
                if !self.delete_selection() && self.cursor > 0 {
                    let from = if k.ctrl { self.word_left() } else { self.cursor - 1 };
                    let (bf, bt) = (self.byte(from), self.byte(self.cursor));
                    self.text.replace_range(bf..bt, "");
                    self.cursor = from;
                }
            }
            Keysym::Delete => {
                if !self.delete_selection() && self.cursor < self.len() {
                    let to = if k.ctrl { self.word_right() } else { self.cursor + 1 };
                    let (bf, bt) = (self.byte(self.cursor), self.byte(to));
                    self.text.replace_range(bf..bt, "");
                }
            }
            Keysym::Left => { let to = if k.ctrl { self.word_left() } else { self.cursor.saturating_sub(1) }; self.move_to(to, k.shift) }
            Keysym::Right => { let to = if k.ctrl { self.word_right() } else { self.cursor + 1 }; self.move_to(to, k.shift) }
            Keysym::Home => self.move_to(0, k.shift),
            Keysym::End => self.move_to(self.len(), k.shift),
            _ if k.ctrl => match k.sym {
                Keysym::a => { self.anchor = Some(0); self.cursor = self.len(); }
                _ => {}
            },
            _ => {
                if k.alt { return FieldResult::None; }
                if let Some(t) = &k.text { self.insert(t); }
            }
        }
        if self.text != before { FieldResult::Changed } else { FieldResult::None }
    }

    fn shown(&self) -> String { if self.password { "•".repeat(self.len()) } else { self.text.clone() } }

    /// Place the caret at logical x (relative to the field's content origin).
    pub fn click_at(&mut self, ts: &mut TextSys, st: &TextStyle, x: f32, extend: bool) {
        let shown = self.shown();
        let target = x + self.scroll_x;
        let mut best = 0usize;
        let mut best_d = f32::MAX;
        let chars: Vec<char> = shown.chars().collect();
        for i in 0..=chars.len() {
            let w = ts.measure(&chars[..i].iter().collect::<String>(), st, None).0;
            let d = (w - target).abs();
            if d < best_d { best_d = d; best = i; }
        }
        self.move_to(best, extend);
    }

    /// Draw the field inside `r` (rounded surface with accent ring when focused).
    pub fn draw(&mut self, cv: &mut Canvas, pal: &Palette, r: Rect) {
        let radius = (r.h / 2.0).min(10.0);
        cv.fill_rrect(r, r_all(radius), pal.surface);
        cv.stroke_rrect(r, r_all(radius), 1.0, if self.focused { pal.accent } else { pal.edge });
        let st = pal.text(12.0);
        let inner = r.pad_xy(10.0, 2.0);
        let shown = self.shown();
        if shown.is_empty() {
            cv.text_in(&self.placeholder, &pal.text(12.0).color(pal.faint), inner, HAlign::Left);
            if self.focused { cv.fill_rect(rect(inner.x, r.y + r.h / 2.0 - 7.0, 1.2, 14.0), pal.fg); }
            return;
        }
        let caret_x = cv.measure(&shown.chars().take(self.cursor).collect::<String>(), &st).0;
        if caret_x - self.scroll_x > inner.w - 2.0 { self.scroll_x = caret_x - inner.w + 2.0; }
        if caret_x - self.scroll_x < 0.0 { self.scroll_x = caret_x; }
        cv.with_clip(inner, [0.0; 4], |cv| {
            if let Some((a, b)) = self.selection() {
                let xa = cv.measure(&shown.chars().take(a).collect::<String>(), &st).0;
                let xb = cv.measure(&shown.chars().take(b).collect::<String>(), &st).0;
                cv.fill_rect(rect(inner.x + xa - self.scroll_x, r.y + 5.0, xb - xa, r.h - 10.0), with_a(pal.accent, 0.35));
            }
            cv.text(&shown, &st, pt(inner.x - self.scroll_x, r.y + r.h / 2.0), HAlign::Left, VAlign::Middle);
            if self.focused { cv.fill_rect(rect(inner.x + caret_x - self.scroll_x, r.y + r.h / 2.0 - 7.0, 1.2, 14.0), pal.fg); }
        });
    }
}

// ---- small widgets -------------------------------------------------------

/// Glass panel: opaque theme fill with a hairline edge.
pub fn glass(cv: &mut Canvas, pal: &Palette, r: Rect, radii: [f32; 4]) {
    cv.fill_rrect(r, radii, pal.glass);
    cv.stroke_rrect(r, radii, 1.0, pal.edge);
}

/// Round/rounded hover highlight behind icon buttons.
pub fn hover_bg(cv: &mut Canvas, pal: &Palette, r: Rect, radius: f32, hovered: bool, pressed: bool) {
    if pressed { cv.fill_rrect(r, r_all(radius), pal.active); } else if hovered { cv.fill_rrect(r, r_all(radius), pal.hover); }
}

#[derive(Clone, Copy, PartialEq)]
pub enum ButtonKind { Plain, Filled, Accent, Danger }

/// Text button; returns nothing, register hits separately (so the caller picks the id).
pub fn button(cv: &mut Canvas, pal: &Palette, r: Rect, label: &str, kind: ButtonKind, hovered: bool, pressed: bool) {
    let radius = (r.h / 2.0).min(r.h);
    let (fill, text): (Color, Color) = match kind {
        ButtonKind::Plain => (if pressed { pal.active } else if hovered { pal.hover } else { pal.surface }, pal.fg),
        ButtonKind::Filled => (if hovered { pal.active } else { pal.hover }, pal.fg_hi),
        ButtonKind::Accent => (if hovered { pal.accent_hi } else { pal.accent }, if pal.is_light { rgb(255, 255, 255) } else { pal.bg }),
        ButtonKind::Danger => (if hovered { mix(pal.red, pal.fg, 0.2) } else { pal.red }, rgb(255, 255, 255)),
    };
    cv.fill_rrect(r, r_all(radius), fill);
    let st = TextStyle::new(pal.fs(11.5), text).weight(600);
    cv.text_in(label, &st, r.pad_xy(8.0, 0.0), HAlign::Center);
}

/// iOS-style switch; `t` is the animated 0..1 position.
pub fn toggle(cv: &mut Canvas, pal: &Palette, r: Rect, t: f32) {
    let h = r.h;
    cv.fill_rrect(r, r_all(h / 2.0), mix(pal.active, pal.accent, t));
    let knob = h - 4.0;
    let x = r.x + 2.0 + (r.w - knob - 4.0) * t;
    cv.fill_circle(pt(x + knob / 2.0, r.y + h / 2.0), knob / 2.0, if t > 0.5 { rgb(255, 255, 255) } else { pal.fg_hi });
}

/// Horizontal slider track + knob. `frac` 0..1.
pub fn slider(cv: &mut Canvas, pal: &Palette, r: Rect, frac: f32, fill: Color) {
    let track = rect(r.x, r.y + r.h / 2.0 - 2.0, r.w, 4.0);
    cv.fill_rrect(track, r_all(2.0), pal.active);
    let f = frac.clamp(0.0, 1.0);
    cv.fill_rrect(rect(track.x, track.y, track.w * f, track.h), r_all(2.0), fill);
    cv.fill_circle(pt(r.x + r.w * f, r.y + r.h / 2.0), 6.0, pal.fg_hi);
}

/// Thin overlay scrollbar for a scrollable column.
pub fn scrollbar(cv: &mut Canvas, pal: &Palette, view: Rect, offset: f32, content_h: f32) {
    if content_h <= view.h + 0.5 { return; }
    let track_h = view.h - 8.0;
    let thumb_h = (track_h * view.h / content_h).max(24.0);
    let y = view.y + 4.0 + (track_h - thumb_h) * (offset / (content_h - view.h));
    cv.fill_rrect(rect(view.right() - 4.0, y, 3.0, thumb_h), r_all(1.5), pal.faint);
}

pub fn gradient_h(a: Color, b: Color, r: Rect) -> Fill {
    Fill::Linear { a: pt(r.x, r.y), b: pt(r.right(), r.y), stops: vec![(0.0, a), (1.0, b)] }
}

/// Convenience for translucent colours from the 0-255 domain.
pub fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Color { rgba(r, g, b, a as f32 / 255.0) }

#[cfg(test)]
mod tests {
    use super::*;

    fn key(sym: Keysym, text: Option<&str>) -> KeyInput { KeyInput { sym, text: text.map(String::from), ctrl: false, alt: false, shift: false } }

    #[test]
    fn field_edits() {
        let mut f = TextField::new("x");
        for ch in ["h", "i", "!"] { f.key(&key(Keysym::a, Some(ch))); }
        assert_eq!(f.text, "hi!");
        assert_eq!(f.key(&key(Keysym::BackSpace, None)), FieldResult::Changed);
        assert_eq!(f.text, "hi");
        f.key(&key(Keysym::Left, None));
        f.key(&key(Keysym::a, Some("é")));
        assert_eq!(f.text, "héi");
        let mut sel = key(Keysym::a, None);
        sel.ctrl = true;
        f.key(&sel);
        f.key(&key(Keysym::a, Some("z")));
        assert_eq!(f.text, "z");
        assert_eq!(f.key(&key(Keysym::Return, None)), FieldResult::Submit);
    }

    #[test]
    fn hits_click_needs_same_region() {
        let mut h = Hits::default();
        h.add(rect(0.0, 0.0, 10.0, 10.0), 1);
        h.add(rect(20.0, 0.0, 10.0, 10.0), 2);
        h.event(&Event::Press(Button::Left, pt(5.0, 5.0)));
        assert_eq!(h.event(&Event::Release(Button::Left, pt(25.0, 5.0))), None);
        h.event(&Event::Press(Button::Left, pt(25.0, 5.0)));
        assert_eq!(h.event(&Event::Release(Button::Left, pt(26.0, 5.0))).map(|c| c.id), Some(2));
    }

    #[test]
    fn glass_interior_is_opaque_over_any_backdrop() {
        let pal = Palette::default();
        assert_eq!(pal.glass.alpha(), 1.0);
        let mut pm = tiny_skia::Pixmap::new(120, 60).unwrap();
        let mut ts = crate::render::TextSys::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut ts, 1.0);
            glass(&mut cv, &pal, rect(0.0, 0.0, 120.0, 60.0), r_all(22.0));
        }
        assert_opaque_interior(&pm, "glass");
    }
}

/// Test helper: the settled interior of a drawn surface is fully opaque, so it is the same colour over any backdrop.
/// Probes just inside the left edge of the painted bounds at mid height (clear of rounded corners).
#[cfg(test)]
pub fn assert_opaque_interior(pm: &tiny_skia::Pixmap, what: &str) {
    use tiny_skia::{Color, Paint, PixmapPaint, Transform};
    let (w, h) = (pm.width(), pm.height());
    let x_min = (0..w).find(|&x| (0..h).any(|y| pm.pixel(x, y).unwrap().alpha() > 0)).unwrap_or_else(|| panic!("{what}: nothing drawn"));
    let (px, py) = (x_min + 10, h / 2);
    let over = |bg: Color| {
        let mut b = tiny_skia::Pixmap::new(w, h).unwrap();
        let mut paint = Paint::default();
        paint.set_color(bg);
        b.fill_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32).unwrap(), &paint, Transform::identity(), None);
        b.draw_pixmap(0, 0, pm.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        let p = b.pixel(px, py).unwrap();
        (p.red(), p.green(), p.blue(), p.alpha())
    };
    assert_eq!(pm.pixel(px, py).unwrap().alpha(), 255, "{what}: interior not opaque");
    let (a, b) = (over(Color::from_rgba8(255, 255, 255, 255)), over(Color::from_rgba8(0, 0, 0, 255)));
    assert_eq!(a, b, "{what}: interior depends on backdrop");
    assert_eq!(a.3, 255);
}
