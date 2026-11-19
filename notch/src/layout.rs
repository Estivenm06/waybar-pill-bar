//! Geometry primitives and scroll state.
//!
//! The UI is immediate-mode: a view computes rectangles every frame with
//! these helpers and draws/registers hit regions as it goes. There is no
//! retained widget tree. All units are logical pixels (f32); the canvas applies
//! the output scale.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub const fn pt(x: f32, y: f32) -> Point { Point { x, y } }
pub const fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect { Rect { x, y, w, h } }

pub fn lerp(a: f32, b: f32, t: f32) -> f32 { a + (b - a) * t }
pub fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect { x: lerp(a.x, b.x, t), y: lerp(a.y, b.y, t), w: lerp(a.w, b.w, t), h: lerp(a.h, b.h, t) }
}
pub fn lerp_pt(a: Point, b: Point, t: f32) -> Point { Point { x: lerp(a.x, b.x, t), y: lerp(a.y, b.y, t) } }
/// Map `t` from the window `[lo, hi]` to 0..=1 (clamped). Used to stage fades.
pub fn window(t: f32, lo: f32, hi: f32) -> f32 { ((t - lo) / (hi - lo)).clamp(0.0, 1.0) }
/// Smooth ease for staged fades.
pub fn smooth(t: f32) -> f32 { let t = t.clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) }

impl Rect {
    pub fn right(&self) -> f32 { self.x + self.w }
    pub fn bottom(&self) -> f32 { self.y + self.h }
    pub fn center(&self) -> Point { pt(self.x + self.w / 2.0, self.y + self.h / 2.0) }
    pub fn size(&self) -> Size { Size { w: self.w, h: self.h } }
    pub fn contains(&self, p: Point) -> bool { p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom() }
    pub fn inset(&self, l: f32, t: f32, r: f32, b: f32) -> Rect {
        rect(self.x + l, self.y + t, (self.w - l - r).max(0.0), (self.h - t - b).max(0.0))
    }
    pub fn pad(&self, p: f32) -> Rect { self.inset(p, p, p, p) }
    pub fn pad_xy(&self, x: f32, y: f32) -> Rect { self.inset(x, y, x, y) }
    pub fn translate(&self, dx: f32, dy: f32) -> Rect { rect(self.x + dx, self.y + dy, self.w, self.h) }
    /// Same centre, new size.
    pub fn centered(&self, w: f32, h: f32) -> Rect {
        let c = self.center();
        rect(c.x - w / 2.0, c.y - h / 2.0, w, h)
    }
    /// A rect of size `w`x`h` centred on `c`.
    pub fn around(c: Point, w: f32, h: f32) -> Rect { rect(c.x - w / 2.0, c.y - h / 2.0, w, h) }
    pub fn scale_about_center(&self, k: f32) -> Rect { self.centered(self.w * k, self.h * k) }
    pub fn intersect(&self, o: Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        rect(x, y, (self.right().min(o.right()) - x).max(0.0), (self.bottom().min(o.bottom()) - y).max(0.0))
    }
    pub fn union(&self, o: Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        rect(x, y, self.right().max(o.right()) - x, self.bottom().max(o.bottom()) - y)
    }
    pub fn is_empty(&self) -> bool { self.w <= 0.0 || self.h <= 0.0 }
}

/// Vertical scroll state for a clipped viewport. `content_h` is set by the
/// view each frame; `offset` is clamped to the scrollable range.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scroll {
    pub offset: f32,
    pub content_h: f32,
}

impl Scroll {
    pub fn max(&self, viewport_h: f32) -> f32 { (self.content_h - viewport_h).max(0.0) }
    pub fn by(&mut self, delta: f32, viewport_h: f32) { self.offset = (self.offset + delta).clamp(0.0, self.max(viewport_h)); }
    pub fn clamp(&mut self, viewport_h: f32) { self.offset = self.offset.clamp(0.0, self.max(viewport_h)); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_helpers() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        assert!(a.contains(pt(9.9, 0.0)) && !a.contains(pt(10.0, 5.0)));
        assert_eq!(a.intersect(rect(5.0, 5.0, 10.0, 10.0)), rect(5.0, 5.0, 5.0, 5.0));
        assert_eq!(window(0.75, 0.5, 1.0), 0.5);
    }
}
