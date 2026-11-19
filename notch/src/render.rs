//! CPU renderer: tiny-skia for shapes/images, cosmic-text for text.
//!
//! `Canvas` draws in logical pixels onto a premultiplied RGBA `Pixmap`; the
//! output scale is applied internally. Group opacity is a multiplier
//! (`with_alpha`) applied to every colour/image drawn inside the closure, and
//! `with_clip` intersects a rounded-rect mask.

use crate::layout::{Point, Rect, pt, rect};
use cosmic_text::{Attrs, Buffer, Family as CFamily, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight, Wrap};
use std::collections::HashMap;
use std::rc::Rc;
use tiny_skia::{FillRule, FilterQuality, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pattern, Pixmap, SpreadMode, Stroke, Transform};

pub type Color = tiny_skia::Color;

pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color::from_rgba(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a.clamp(0.0, 1.0)).unwrap()
}
pub fn rgb(r: u8, g: u8, b: u8) -> Color { rgba(r, g, b, 1.0) }
pub fn with_a(c: Color, a: f32) -> Color { Color::from_rgba(c.red(), c.green(), c.blue(), (c.alpha() * a).clamp(0.0, 1.0)).unwrap() }
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Color::from_rgba(l(a.red(), b.red()), l(a.green(), b.green()), l(a.blue(), b.blue()), l(a.alpha(), b.alpha()).clamp(0.0, 1.0)).unwrap()
}

/// Parse `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(r,g,b)` or `rgba(r,g,b,a)`.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        let v = |i: usize, n: usize| u8::from_str_radix(h.get(i..i + n)?, 16).ok();
        return match h.len() {
            3 => Some(rgb(v(0, 1)? * 17, v(1, 1)? * 17, v(2, 1)? * 17)),
            6 => Some(rgb(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
            8 => Some(rgba(v(0, 2)?, v(2, 2)?, v(4, 2)?, v(6, 2)? as f32 / 255.0)),
            _ => None,
        };
    }
    let inner = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb("))?.strip_suffix(')')?;
    let p: Vec<f32> = inner.split(',').map(|x| x.trim().parse().ok()).collect::<Option<_>>()?;
    match p.len() {
        3 => Some(rgb(p[0] as u8, p[1] as u8, p[2] as u8)),
        4 => Some(rgba(p[0] as u8, p[1] as u8, p[2] as u8, p[3])),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub enum Fill {
    Solid(Color),
    /// Linear gradient between two logical points.
    Linear { a: Point, b: Point, stops: Vec<(f32, Color)> },
}

impl From<Color> for Fill { fn from(c: Color) -> Self { Fill::Solid(c) } }

/// Vertical gradient helper (top to bottom over `r`).
pub fn vgradient(r: Rect, top: Color, bottom: Color) -> Fill {
    Fill::Linear { a: pt(r.x, r.y), b: pt(r.x, r.bottom()), stops: vec![(0.0, top), (1.0, bottom)] }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Family { Sans, Mono, /// Nerd-font symbol glyphs.
    Icon, Named(String) }

#[derive(Clone, Debug)]
pub struct TextStyle {
    pub size: f32,
    pub color: Color,
    pub weight: u16,
    pub family: Family,
    pub italic: bool,
}

impl TextStyle {
    pub fn new(size: f32, color: Color) -> Self { TextStyle { size, color, weight: 400, family: Family::Sans, italic: false } }
    pub fn bold(mut self) -> Self { self.weight = 700; self }
    pub fn weight(mut self, w: u16) -> Self { self.weight = w; self }
    pub fn mono(mut self) -> Self { self.family = Family::Mono; self }
    pub fn icon(size: f32, color: Color) -> Self { TextStyle { family: Family::Icon, ..TextStyle::new(size, color) } }
    pub fn italic(mut self) -> Self { self.italic = true; self }
    pub fn color(mut self, c: Color) -> Self { self.color = c; self }
    pub fn size(mut self, s: f32) -> Self { self.size = s; self }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HAlign { Left, Center, Right }
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VAlign { Top, Middle }

#[derive(Hash, PartialEq, Eq)]
struct TextKey {
    text: String,
    size_q: u32,
    weight: u16,
    family: String,
    italic: bool,
    width_q: u32,
}

/// Shared font machinery: one per process.
pub struct TextSys {
    pub fonts: FontSystem,
    swash: SwashCache,
    cache: HashMap<TextKey, Buffer>,
    ui_family: Option<String>,
    mono_family: String,
    weights: HashMap<(String, u16), u16>,
}

const ICON_FONT: &str = "Symbols Nerd Font";

impl TextSys {
    pub fn new() -> Self {
        TextSys { fonts: FontSystem::new(), swash: SwashCache::new(), cache: HashMap::new(), ui_family: None, mono_family: "monospace".into(), weights: HashMap::new() }
    }
    /// Override the sans family (appearance.font_family). `None` = system default.
    pub fn set_ui_family(&mut self, f: Option<String>) {
        if self.ui_family != f { self.ui_family = f; self.cache.clear(); }
    }

    fn family_name(&self, f: &Family) -> String {
        match f {
            Family::Sans => self.ui_family.clone().unwrap_or_else(|| "sans-serif".into()),
            Family::Mono => self.mono_family.clone(),
            Family::Icon => ICON_FONT.into(),
            Family::Named(n) => n.clone(),
        }
    }

    /// Nearest weight the family really ships. cosmic-text otherwise swaps in a
    /// different family that has the exact weight, so a regular-only font
    /// would turn bold text into another typeface.
    fn resolve_weight(&mut self, family: &str, want: u16) -> u16 {
        if let Some(w) = self.weights.get(&(family.to_string(), want)) { return *w; }
        let have: Vec<u16> = self.fonts.db().faces().filter(|f| f.families.iter().any(|(n, _)| n == family)).map(|f| f.weight.0).collect();
        let w = have.iter().copied().min_by_key(|w| w.abs_diff(want)).unwrap_or(want);
        self.weights.insert((family.to_string(), want), w);
        w
    }

    fn make_key(&self, text: &str, st: &TextStyle, width: Option<f32>) -> TextKey {
        TextKey {
            text: text.to_string(),
            size_q: (st.size * 8.0).round() as u32,
            weight: st.weight,
            family: self.family_name(&st.family),
            italic: st.italic,
            width_q: width.map_or(u32::MAX, |w| (w * 4.0).round() as u32),
        }
    }

    /// Shape (or fetch from cache) the buffer for `text`, returning its key.
    fn shape(&mut self, text: &str, st: &TextStyle, width: Option<f32>) -> TextKey {
        let fam = self.family_name(&st.family);
        let mut st = st.clone();
        if !matches!(fam.as_str(), "sans-serif" | "monospace") { st.weight = self.resolve_weight(&fam, st.weight); }
        let st = &st;
        let key = self.make_key(text, st, width);
        if self.cache.len() > 600 { self.cache.clear(); }
        if !self.cache.contains_key(&key) {
            let mut b = Buffer::new(&mut self.fonts, Metrics::new(st.size, (st.size * 1.3).ceil()));
            b.set_wrap(if width.is_some() { Wrap::WordOrGlyph } else { Wrap::None });
            b.set_size(width, None);
            let cf = match key.family.as_str() {
                "sans-serif" => CFamily::SansSerif,
                "monospace" => CFamily::Monospace,
                n => CFamily::Name(n),
            };
            let attrs = Attrs::new().family(cf).weight(Weight(st.weight)).style(if st.italic { Style::Italic } else { Style::Normal });
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(&mut self.fonts, false);
            self.cache.insert(self.make_key(text, st, width), b);
        }
        key
    }

    fn buffer(&mut self, text: &str, st: &TextStyle, width: Option<f32>) -> &Buffer {
        let key = self.shape(text, st, width);
        &self.cache[&key]
    }

    /// Logical size of `text` laid out on one line (or wrapped at `width`).
    pub fn measure(&mut self, text: &str, st: &TextStyle, width: Option<f32>) -> (f32, f32) {
        let b = self.buffer(text, st, width);
        let mut w = 0.0f32;
        let mut h = 0.0f32;
        for run in b.layout_runs() {
            w = w.max(run.line_w);
            h = h.max(run.line_top + run.line_height);
        }
        if h == 0.0 { h = (st.size * 1.3).ceil(); }
        (w, h)
    }

    /// `text` clipped with an ellipsis to fit `max_w`.
    pub fn ellipsize(&mut self, text: &str, st: &TextStyle, max_w: f32) -> String {
        if self.measure(text, st, None).0 <= max_w { return text.to_string(); }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let s: String = chars[..mid].iter().collect::<String>() + "…";
            if self.measure(&s, st, None).0 <= max_w { lo = mid } else { hi = mid - 1 }
        }
        chars[..lo].iter().collect::<String>().trim_end().to_string() + "…"
    }
}

/// Decoded, premultiplied RGBA image.
#[derive(Clone)]
pub struct Image(pub Rc<Pixmap>);

impl Image {
    pub fn from_rgba8(w: u32, h: u32, straight: &[u8]) -> Option<Image> {
        let mut pm = Pixmap::new(w, h)?;
        if straight.len() < (w * h * 4) as usize { return None; }
        for (dst, src) in pm.data_mut().chunks_exact_mut(4).zip(straight.chunks_exact(4)) {
            let a = src[3] as u32;
            dst[0] = ((src[0] as u32 * a + 127) / 255) as u8;
            dst[1] = ((src[1] as u32 * a + 127) / 255) as u8;
            dst[2] = ((src[2] as u32 * a + 127) / 255) as u8;
            dst[3] = src[3];
        }
        Some(Image(Rc::new(pm)))
    }
    pub fn width(&self) -> u32 { self.0.width() }
    pub fn height(&self) -> u32 { self.0.height() }
}

pub struct Canvas<'a> {
    pm: &'a mut Pixmap,
    pub text: &'a mut TextSys,
    scale: f32,
    origin: Point,
    origins: Vec<Point>,
    alpha: f32,
    masks: Vec<Mask>,
}

pub const R0: [f32; 4] = [0.0; 4];
pub fn r_all(r: f32) -> [f32; 4] { [r; 4] }
/// Radii for a shape rounded only at the bottom (tl, tr, br, bl order).
pub fn r_bottom(r: f32) -> [f32; 4] { [0.0, 0.0, r, r] }

pub fn rrect_path(r: Rect, radii: [f32; 4]) -> Option<Path> {
    let max = (r.w.min(r.h) / 2.0).max(0.0);
    let [tl, tr, br, bl] = radii.map(|v| v.clamp(0.0, max));
    const K: f32 = 0.552_284_75;
    let mut pb = PathBuilder::new();
    pb.move_to(r.x + tl, r.y);
    pb.line_to(r.right() - tr, r.y);
    if tr > 0.0 { pb.cubic_to(r.right() - tr + tr * K, r.y, r.right(), r.y + tr - tr * K, r.right(), r.y + tr); }
    pb.line_to(r.right(), r.bottom() - br);
    if br > 0.0 { pb.cubic_to(r.right(), r.bottom() - br + br * K, r.right() - br + br * K, r.bottom(), r.right() - br, r.bottom()); }
    pb.line_to(r.x + bl, r.bottom());
    if bl > 0.0 { pb.cubic_to(r.x + bl - bl * K, r.bottom(), r.x, r.bottom() - bl + bl * K, r.x, r.bottom() - bl); }
    pb.line_to(r.x, r.y + tl);
    if tl > 0.0 { pb.cubic_to(r.x, r.y + tl - tl * K, r.x + tl - tl * K, r.y, r.x + tl, r.y); }
    pb.close();
    pb.finish()
}

impl<'a> Canvas<'a> {
    pub fn new(pm: &'a mut Pixmap, text: &'a mut TextSys, scale: f32) -> Self {
        Canvas { pm, text, scale, origin: pt(0.0, 0.0), origins: Vec::new(), alpha: 1.0, masks: Vec::new() }
    }

    pub fn clear(&mut self) { self.pm.fill(tiny_skia::Color::TRANSPARENT); }
    pub fn scale(&self) -> f32 { self.scale }
    /// Logical size of the target.
    pub fn size(&self) -> (f32, f32) { (self.pm.width() as f32 / self.scale, self.pm.height() as f32 / self.scale) }

    fn tf(&self) -> Transform { Transform::from_scale(self.scale, self.scale).pre_translate(self.origin.x, self.origin.y) }
    fn mask(&self) -> Option<&Mask> { self.masks.last() }

    fn paint_for(&self, fill: &Fill) -> Paint<'static> {
        let mut p = Paint::default();
        p.anti_alias = true;
        match fill {
            Fill::Solid(c) => p.set_color(with_a(*c, self.alpha)),
            Fill::Linear { a, b, stops } => {
                let st: Vec<GradientStop> = stops.iter().map(|(o, c)| GradientStop::new(*o, with_a(*c, self.alpha))).collect();
                match LinearGradient::new(tiny_skia::Point::from_xy(a.x, a.y), tiny_skia::Point::from_xy(b.x, b.y), st, SpreadMode::Pad, Transform::identity()) {
                    Some(sh) => p.shader = sh,
                    None => p.set_color(with_a(stops.last().map(|s| s.1).unwrap_or(Color::TRANSPARENT), self.alpha)),
                }
            }
        }
        p
    }

    /// Run `f` with translated origin.
    pub fn with_translate<R>(&mut self, dx: f32, dy: f32, f: impl FnOnce(&mut Self) -> R) -> R {
        self.origins.push(self.origin);
        self.origin = pt(self.origin.x + dx, self.origin.y + dy);
        let r = f(self);
        self.origin = self.origins.pop().unwrap();
        r
    }

    /// Multiply opacity of everything drawn inside `f`.
    pub fn with_alpha<R>(&mut self, a: f32, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.alpha;
        self.alpha = prev * a.clamp(0.0, 1.0);
        let r = f(self);
        self.alpha = prev;
        r
    }
    pub fn alpha(&self) -> f32 { self.alpha }

    /// Intersect the clip with a rounded rect while running `f`.
    pub fn with_clip<R>(&mut self, r: Rect, radii: [f32; 4], f: impl FnOnce(&mut Self) -> R) -> R {
        let mut m = match self.masks.last() {
            Some(prev) => prev.clone(),
            None => {
                let mut m = Mask::new(self.pm.width(), self.pm.height()).unwrap();
                m.fill_path(&PathBuilder::from_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, self.pm.width() as f32, self.pm.height() as f32).unwrap()), FillRule::Winding, false, Transform::identity());
                m
            }
        };
        match rrect_path(r, radii) {
            Some(path) => m.intersect_path(&path, FillRule::Winding, true, self.tf()),
            None => m.clear(),
        }
        self.masks.push(m);
        let out = f(self);
        self.masks.pop();
        out
    }

    pub fn fill_rrect(&mut self, r: Rect, radii: [f32; 4], fill: impl Into<Fill>) {
        if r.is_empty() { return; }
        let Some(path) = rrect_path(r, radii) else { return };
        let paint = self.paint_for(&fill.into());
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.fill_path(&path, &paint, FillRule::Winding, tf, mask);
    }

    pub fn fill_rect(&mut self, r: Rect, fill: impl Into<Fill>) { self.fill_rrect(r, R0, fill); }

    pub fn stroke_rrect(&mut self, r: Rect, radii: [f32; 4], width: f32, color: Color) {
        if r.is_empty() { return; }
        // Stroke centred on the path: inset by half the width so the stroke stays inside `r`.
        let h = width / 2.0;
        let inner = rect(r.x + h, r.y + h, r.w - width, r.h - width);
        let radii = radii.map(|v| (v - h).max(0.0));
        let Some(path) = rrect_path(inner, radii) else { return };
        let paint = self.paint_for(&Fill::Solid(color));
        let stroke = Stroke { width, ..Stroke::default() };
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.stroke_path(&path, &paint, &stroke, tf, mask);
    }

    pub fn fill_circle(&mut self, c: Point, radius: f32, fill: impl Into<Fill>) {
        self.fill_rrect(Rect::around(c, radius * 2.0, radius * 2.0), r_all(radius), fill);
    }

    pub fn line(&mut self, a: Point, b: Point, width: f32, color: Color) {
        let mut pb = PathBuilder::new();
        pb.move_to(a.x, a.y);
        pb.line_to(b.x, b.y);
        let Some(path) = pb.finish() else { return };
        let paint = self.paint_for(&Fill::Solid(color));
        let stroke = Stroke { width, line_cap: tiny_skia::LineCap::Round, ..Stroke::default() };
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.stroke_path(&path, &paint, &stroke, tf, mask);
    }

    /// Open polyline (sparklines/graphs).
    pub fn polyline(&mut self, pts: &[Point], width: f32, color: Color) {
        let mut pb = PathBuilder::new();
        for (i, p) in pts.iter().enumerate() {
            if i == 0 { pb.move_to(p.x, p.y) } else { pb.line_to(p.x, p.y) }
        }
        let Some(path) = pb.finish() else { return };
        let paint = self.paint_for(&Fill::Solid(color));
        let stroke = Stroke { width, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Stroke::default() };
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.stroke_path(&path, &paint, &stroke, tf, mask);
    }

    pub fn fill_polygon(&mut self, pts: &[Point], fill: impl Into<Fill>) {
        let mut pb = PathBuilder::new();
        for (i, p) in pts.iter().enumerate() {
            if i == 0 { pb.move_to(p.x, p.y) } else { pb.line_to(p.x, p.y) }
        }
        pb.close();
        let Some(path) = pb.finish() else { return };
        let paint = self.paint_for(&fill.into());
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.fill_path(&path, &paint, FillRule::Winding, tf, mask);
    }

    /// Soft glow: stacked translucent strokes fading outward from `r`.
    pub fn glow_rrect(&mut self, r: Rect, radii: [f32; 4], spread: f32, color: Color) {
        let steps = 6;
        for i in 0..steps {
            let k = i as f32 / steps as f32;
            let grow = spread * (1.0 - k);
            let a = color.alpha() * (k * k) * 0.35;
            let rr = rect(r.x - grow, r.y - grow, r.w + 2.0 * grow, r.h + 2.0 * grow);
            self.stroke_rrect(rr, radii.map(|v| v + grow), spread / steps as f32 + 0.5, with_a(color, a / color.alpha().max(0.001)));
        }
    }

    /// Draw `img` covering `dst` (cropped, centred) with rounded corners.
    pub fn image_cover(&mut self, img: &Image, dst: Rect, radii: [f32; 4]) {
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw == 0.0 || ih == 0.0 || dst.is_empty() { return; }
        let k = (dst.w / iw).max(dst.h / ih);
        let (w, h) = (iw * k, ih * k);
        self.image_at(img, rect(dst.x + (dst.w - w) / 2.0, dst.y + (dst.h - h) / 2.0, w, h), dst, radii);
    }

    /// Draw `img` stretched to `dst`.
    pub fn image(&mut self, img: &Image, dst: Rect, radii: [f32; 4]) { self.image_at(img, dst, dst, radii) }

    fn image_at(&mut self, img: &Image, placed: Rect, clip: Rect, radii: [f32; 4]) {
        let Some(path) = rrect_path(clip, radii) else { return };
        let sx = placed.w / img.width() as f32;
        let sy = placed.h / img.height() as f32;
        let pat = Pattern::new(img.0.as_ref().as_ref(), SpreadMode::Pad, FilterQuality::Bilinear, self.alpha, Transform::from_row(sx, 0.0, 0.0, sy, placed.x, placed.y));
        let mut paint = Paint::default();
        paint.shader = pat;
        paint.anti_alias = true;
        let (tf, mask) = (self.tf(), self.masks.last());
        self.pm.fill_path(&path, &paint, FillRule::Winding, tf, mask);
    }

    pub fn measure(&mut self, s: &str, st: &TextStyle) -> (f32, f32) { self.text.measure(s, st, None) }

    /// Draw one line of text. `pos` is the anchor: x per `h`, y is the top (`Top`) or the vertical middle.
    /// Returns the logical width.
    pub fn text(&mut self, s: &str, st: &TextStyle, pos: Point, h: HAlign, v: VAlign) -> f32 {
        if s.is_empty() || st.size <= 0.0 { return 0.0; }
        let (w, th) = self.text.measure(s, st, None);
        let x = match h { HAlign::Left => pos.x, HAlign::Center => pos.x - w / 2.0, HAlign::Right => pos.x - w };
        let y = match v { VAlign::Top => pos.y, VAlign::Middle => pos.y - th / 2.0 };
        self.draw_buffer(s, st, None, x, y);
        w
    }

    /// Single line, ellipsised to the rectangle's width, vertically centred in it.
    pub fn text_in(&mut self, s: &str, st: &TextStyle, r: Rect, h: HAlign) -> f32 {
        let fitted = self.text.ellipsize(s, st, r.w);
        let x = match h { HAlign::Left => r.x, HAlign::Center => r.x + r.w / 2.0, HAlign::Right => r.right() };
        self.text(&fitted, st, pt(x, r.y + r.h / 2.0), h, VAlign::Middle)
    }

    /// Word-wrapped text in `r.w`; draws at most `max_lines` lines. Returns drawn height.
    pub fn text_wrap(&mut self, s: &str, st: &TextStyle, r: Rect, h: HAlign, max_lines: usize) -> f32 {
        if s.is_empty() { return 0.0; }
        let line_h = (st.size * 1.3).ceil();
        let lines: Vec<(String, f32)> = {
            let b = self.text.buffer(s, st, Some(r.w));
            b.layout_runs().take(max_lines).map(|run| {
                let end = run.glyphs.iter().map(|g| g.end).max().unwrap_or(0);
                let start = run.glyphs.iter().map(|g| g.start).min().unwrap_or(0);
                (run.text.get(start..end).unwrap_or("").trim_end().to_string(), run.line_w)
            }).collect()
        };
        let n = lines.len();
        for (i, (l, _)) in lines.iter().enumerate() {
            let y = r.y + i as f32 * line_h;
            let shown = if i + 1 == max_lines && i + 1 == n && self.text.measure(s, st, Some(r.w)).1 > line_h * n as f32 + 0.5 {
                self.text.ellipsize(&format!("{l}…"), st, r.w)
            } else { l.clone() };
            let x = match h { HAlign::Left => r.x, HAlign::Center => r.x + r.w / 2.0, HAlign::Right => r.right() };
            let (w, _) = self.text.measure(&shown, st, None);
            let x = match h { HAlign::Left => x, HAlign::Center => x - w / 2.0, HAlign::Right => x - w };
            self.draw_buffer(&shown, st, None, x, y);
        }
        n as f32 * line_h
    }

    fn draw_buffer(&mut self, s: &str, st: &TextStyle, width: Option<f32>, x: f32, y: f32) {
        let scale = self.scale;
        let ox = (x + self.origin.x) * scale;
        let oy = (y + self.origin.y) * scale;
        let color = with_a(st.color, self.alpha);
        let key = self.text.shape(s, st, width);
        let TextSys { fonts, swash, cache, .. } = &mut *self.text;
        let Some(buf) = cache.get(&key) else { return };
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let mask = self.masks.last();
        let data = self.pm.data_mut();
        let (cr, cg, cb, ca) = (color.red(), color.green(), color.blue(), color.alpha());
        // The family has no bold face: embolden by overdrawing the glyph slightly offset.
        let fake_bold = st.weight >= 600 && key.weight < 600;
        let offs: &[f32] = if fake_bold { &[0.0, (st.size * scale * 0.04).max(0.6)] } else { &[0.0] };
        for run in buf.layout_runs() {
            for g in run.glyphs {
              for &dx in offs {
                let pg = g.physical((ox + dx, oy + run.line_y * scale), scale);
                let Some(img) = swash.get_image(fonts, pg.cache_key).as_ref() else { continue };
                let (gx, gy) = (pg.x + img.placement.left, pg.y - img.placement.top);
                let (iw, ih) = (img.placement.width as i32, img.placement.height as i32);
                for row in 0..ih {
                    let py = gy + row;
                    if py < 0 || py >= ph { continue; }
                    for col in 0..iw {
                        let px = gx + col;
                        if px < 0 || px >= pw { continue; }
                        let i = (row * iw + col) as usize;
                        // Source premultiplied RGBA (0..1).
                        let (sr, sg, sb, sa) = match img.content {
                            SwashContent::Mask => {
                                let cov = img.data[i] as f32 / 255.0 * ca;
                                (cr * cov, cg * cov, cb * cov, cov)
                            }
                            SwashContent::SubpixelMask => {
                                let cov = (img.data[i * 3] as f32 + img.data[i * 3 + 1] as f32 + img.data[i * 3 + 2] as f32) / (3.0 * 255.0) * ca;
                                (cr * cov, cg * cov, cb * cov, cov)
                            }
                            SwashContent::Color => {
                                let a = img.data[i * 4 + 3] as f32 / 255.0 * self.alpha;
                                (img.data[i * 4] as f32 / 255.0 * a, img.data[i * 4 + 1] as f32 / 255.0 * a, img.data[i * 4 + 2] as f32 / 255.0 * a, a)
                            }
                        };
                        if sa <= 0.0 { continue; }
                        let cov_clip = match mask { Some(m) => m.data()[(py * pw + px) as usize] as f32 / 255.0, None => 1.0 };
                        if cov_clip <= 0.0 { continue; }
                        let d = ((py * pw + px) * 4) as usize;
                        let inv = 1.0 - sa * cov_clip;
                        for (k, s) in [sr, sg, sb, sa].into_iter().enumerate() {
                            let dv = data[d + k] as f32 / 255.0;
                            data[d + k] = ((s * cov_clip + dv * inv) * 255.0 + 0.5).min(255.0) as u8;
                        }
                    }
                }
              }
            }
        }
    }
}

/// Copy a premultiplied RGBA pixmap into a Wayland ARGB8888 (little-endian BGRA) buffer.
pub fn blit_argb8888(src: &Pixmap, dst: &mut [u8]) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.data().chunks_exact(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#fff"), Some(rgb(255, 255, 255)));
        assert_eq!(parse_color("#102030"), Some(rgb(16, 32, 48)));
        assert!(parse_color("#12").is_none());
        let c = parse_color("rgba(10, 20, 30, 0.5)").unwrap();
        assert!((c.alpha() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rrect_fills_inside_and_not_corner() {
        let mut pm = Pixmap::new(40, 40).unwrap();
        let mut ts = TextSys::new();
        let mut cv = Canvas::new(&mut pm, &mut ts, 1.0);
        cv.fill_rrect(rect(0.0, 0.0, 40.0, 40.0), r_all(16.0), rgb(255, 0, 0));
        let px = |x: u32, y: u32| pm.pixel(x, y).unwrap().alpha();
        assert_eq!(px(20, 20), 255);
        assert_eq!(px(0, 0), 0);
    }

    #[test]
    fn clip_and_alpha_apply() {
        let mut pm = Pixmap::new(20, 20).unwrap();
        let mut ts = TextSys::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut ts, 1.0);
            cv.with_clip(rect(0.0, 0.0, 10.0, 20.0), R0, |cv| cv.with_alpha(0.5, |cv| cv.fill_rect(rect(0.0, 0.0, 20.0, 20.0), rgb(0, 0, 255))));
        }
        assert_eq!(pm.pixel(15, 5).unwrap().alpha(), 0);
        let a = pm.pixel(5, 5).unwrap().alpha();
        assert!((126..=129).contains(&a), "{a}");
    }
}
