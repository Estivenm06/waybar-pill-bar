//! Notch pages. The notch (notch.rs) owns the tab bar, the morphing header and
//! the open/close springs; each tab is a `Page` drawing into the content area.
//!
//! Coordinates: pages draw and receive pointer events in *notch-surface*
//! coordinates (the same space as `area`), no translation needed.

use crate::layout::{Point, Rect, Size};
use crate::render::{Canvas, TextSys};
use crate::state::State;
use crate::wayland::{Ctx, DrawCtx, Event};

/// Elements shared between the collapsed notch and the Home page. The notch
/// moves/scales them from their collapsed spot to the target given here. A page
/// that returns `None` for an element makes it fade out while expanding.
#[derive(Clone, Copy, Debug, Default)]
pub struct Morph {
    /// Centre and font size (px) of the clock.
    pub clock: Option<(Point, f32)>,
    /// Centre and font size of the weather glyph.
    pub weather_icon: Option<(Point, f32)>,
    /// Album art square.
    pub art: Option<Rect>,
    /// Equaliser bars box.
    pub eq: Option<Rect>,
}

pub trait Page {
    /// Content size (logical px) wanted at the current state, within `avail`
    /// (the card minus tab bar and margins). Height may exceed `avail.h`; the
    /// notch clamps and the page then scrolls itself.
    fn natural(&mut self, avail: Size, st: &State, ts: &mut TextSys) -> Size;
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect);
    /// Optional full-card decoration drawn *behind* the tab bar and page (e.g. album tint). `card` is the whole expanded notch rect.
    fn draw_backdrop(&mut self, _cv: &mut Canvas, _dc: &mut DrawCtx, _card: Rect) {}
    fn event(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx);
    /// Advance page-local animation; true while more frames are needed.
    fn tick(&mut self, _dt: f32, _ctx: &mut Ctx) -> bool { false }
    /// Page needs keyboard focus (text fields).
    fn wants_keyboard(&self) -> bool { false }
    /// Page is mid-edit: hover-away must not collapse the notch.
    fn keeps_open(&self) -> bool { false }
    /// The page became the visible one (refresh data here).
    fn shown(&mut self, _ctx: &mut Ctx) {}
    fn hidden(&mut self, _ctx: &mut Ctx) {}
    fn morph(&self, _area: Rect, _st: &State) -> Morph { Morph::default() }
    /// Pointer cursor for the current hover position.
    fn cursor(&self) -> crate::wayland::Cursor { crate::wayland::Cursor::Default }
}

/// Headless harness for page tests: render a page to a PNG and feed it events.
#[cfg(test)]
pub mod testutil {
    use super::*;
    use crate::layout::rect;
    use crate::render::{Canvas, TextSys};
    use crate::ui::glass;
    use crate::wayland::{Cmd, Ctx, DrawCtx, Event, Poster};
    use tiny_skia::Pixmap;

    pub struct Harness {
        pub ts: TextSys,
        pub poster: Poster,
        pub cmds: Vec<Cmd>,
        pub scale: f32,
    }

    impl Harness {
        pub fn new() -> Harness {
            let mut ts = TextSys::new();
            ts.set_ui_family(Some(crate::theme::current().font));
            Harness { ts, poster: Poster::detached(), cmds: Vec::new(), scale: 1.6 }
        }

        /// Natural size at the notch's default card width (520 - 2*16).
        pub fn natural(&mut self, page: &mut dyn Page, st: &State) -> Size {
            page.natural(Size { w: 488.0, h: 700.0 }, st, &mut self.ts)
        }

        /// Draw into the content area of a glass card and save `path`. Returns the area used.
        pub fn render(&mut self, page: &mut dyn Page, st: &mut State, path: &str) -> Rect {
            let nat = self.natural(page, st);
            let area = rect(16.0, 12.0, 488.0, nat.h.min(700.0));
            let (w, h) = (520.0, area.h + 24.0);
            let mut pm = Pixmap::new((w * self.scale) as u32, (h * self.scale) as u32).unwrap();
            pm.fill(tiny_skia::Color::from_rgba8(60, 70, 90, 255));
            let poster = self.poster.clone();
            {
                let mut cv = Canvas::new(&mut pm, &mut self.ts, self.scale);
                glass(&mut cv, &st.pal, rect(0.0, 0.0, w, h), [22.0; 4]);
                let mut dc = DrawCtx { state: st, cmds: &mut self.cmds, poster: &poster, id: 1, output: None };
                page.draw(&mut cv, &mut dc, area);
            }
            pm.save_png(path).unwrap();
            area
        }

        pub fn send(&mut self, page: &mut dyn Page, st: &mut State, area: Rect, ev: Event) {
            let poster = self.poster.clone();
            let mut ctx = Ctx { state: st, cmds: &mut self.cmds, text: &mut self.ts, poster: &poster, id: 1, output: None };
            page.event(&ev, area, &mut ctx);
        }

        pub fn tick(&mut self, page: &mut dyn Page, st: &mut State, dt: f32) -> bool {
            let poster = self.poster.clone();
            let mut ctx = Ctx { state: st, cmds: &mut self.cmds, text: &mut self.ts, poster: &poster, id: 1, output: None };
            page.tick(dt, &mut ctx)
        }

        pub fn show(&mut self, page: &mut dyn Page, st: &mut State) {
            let poster = self.poster.clone();
            let mut ctx = Ctx { state: st, cmds: &mut self.cmds, text: &mut self.ts, poster: &poster, id: 1, output: None };
            page.shown(&mut ctx);
        }
    }
}
