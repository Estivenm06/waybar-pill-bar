//! Media tab: cover, title, seek bar, transport controls, lyrics.

use crate::layout::{Point, Rect, Size, pt, rect};
use crate::media::{self, LyricsState, MediaPresence, fmt_time};
use crate::page::Page;
use crate::render::{Canvas, Fill, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::State;
use crate::ui::{Hits, Palette};
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event};

const MAX_H: f32 = 440.0;
const ID_PLAY: u32 = 1;
const ID_PREV: u32 = 2;
const ID_NEXT: u32 = 3;
const ID_BACK30: u32 = 4;
const ID_FWD30: u32 = 5;
const ID_SEEK: u32 = 6;
const ID_OPEN: u32 = 7;
const ID_SOURCE: u32 = 8;

/// Square cover side: at most 128 and the page width minus margins, shrunk to what the fixed controls leave
/// of the budget, never below 64.
pub fn cover_side(page_width: f32, budget: f32, fixed: f32) -> f32 {
    (page_width.max(1.0) - 16.0).min(128.0).min((budget - fixed).max(64.0)).max(1.0)
}

const FIXED_H: f32 = 22.0 /*title*/ + 18.0 /*artist*/ + 50.0 /*lyrics*/ + 20.0 /*seek*/ + 16.0 /*times*/ + 48.0 /*controls*/ + 6.0 * 6.0;

#[derive(Default)]
pub struct MediaPage {
    hits: Hits,
    seeking: Option<f32>,
    seek_rect: Rect,
}

impl MediaPage {
    pub fn new() -> Self { Self::default() }

    fn side(&self, avail: Size) -> f32 { cover_side(avail.w, avail.h.min(MAX_H - 80.0), FIXED_H) }
}

fn seek_fraction(x: f32, r: Rect) -> f32 { ((x - r.x) / r.w).clamp(0.0, 1.0) }

impl Page for MediaPage {
    fn natural(&mut self, avail: Size, _: &State, _: &mut TextSys) -> Size {
        let side = self.side(avail);
        Size { w: avail.w, h: (side + FIXED_H + 8.0).min(MAX_H - 66.0) }
    }

    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw_backdrop(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, card: Rect) {
        let pal = &dc.state.pal;
        let m = &dc.state.media;
        if let Some(img) = &m.art {
            cv.with_alpha(0.65 * 0.25, |cv| cv.image_cover(img, card, [0.0; 4]));
        }
        cv.fill_rect(card, Fill::Linear {
            a: pt(card.x, card.y), b: pt(card.x, card.bottom()),
            stops: vec![(0.0, with_a(pal.bg, 0.28 * 0.25)), (1.0, with_a(pal.bg, 0.92 * 0.25))],
        });
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal: &Palette = &st.pal;
        let m = &st.media;
        self.hits.clear();
        let side = self.side(area.size());
        let cover = rect(area.x + (area.w - side) / 2.0, area.y, side, side);
        let present = m.presence == MediaPresence::Present;
        let skeleton = m.presence == MediaPresence::Skeleton;

        // cover with soft shadow + gloss edge
        cv.fill_rrect(cover.translate(0.0, 4.0).pad(-2.0), r_all(14.0), with_a(pal.bg, 0.35));
        match (&m.art, present) {
            (Some(img), true) => cv.image_cover(img, cover, r_all(12.0)),
            _ => {
                cv.fill_rrect(cover, r_all(12.0), with_a(pal.fg, if skeleton { 0.06 } else { 0.10 }));
                cv.text("\u{f001}", &TextStyle::icon(side * 0.4, pal.dim), cover.center(), HAlign::Center, VAlign::Middle);
            }
        }
        cv.stroke_rrect(cover, r_all(12.0), 1.0, pal.edge);

        // source chip: which player is shown; a click cycles through the running players
        if present && !m.player.is_empty() {
            let label = media::player_label(&m.player);
            let many = m.players.len() > 1;
            let text = if many { format!("{label} · {}/{}", m.players.iter().position(|p| p.instance == m.player).map_or(1, |i| i + 1), m.players.len()) } else { label };
            let w = (text.chars().count() as f32 * 6.6 + 34.0).min(area.w * 0.45);
            let chip = rect(area.right() - w, area.y, w, 22.0);
            let hov = many && self.hits.add(chip, ID_SOURCE);
            cv.fill_rrect(chip, r_all(11.0), with_a(pal.fg, if hov { 0.20 } else { 0.10 }));
            cv.fill_circle(pt(chip.x + 12.0, chip.y + 11.0), 3.5, if m.playing { pal.accent_hi } else { pal.dim });
            cv.text_in(&text, &TextStyle::new(pal.fs(10.5), pal.fg_hi).weight(600), rect(chip.x + 22.0, chip.y, chip.w - 28.0, chip.h), HAlign::Left);
        }

        let mut y = cover.bottom() + 8.0;
        let (title, artist) = if present {
            (if m.title.is_empty() { "Unknown title".to_string() } else { m.title.clone() }, m.artist.clone())
        } else if skeleton { (String::new(), String::new()) } else { ("Nothing playing".to_string(), String::new()) };
        if skeleton {
            cv.fill_rrect(rect(area.x + area.w * 0.25, y + 4.0, area.w * 0.5, 14.0), r_all(7.0), pal.hover);
            cv.fill_rrect(rect(area.x + area.w * 0.35, y + 26.0, area.w * 0.3, 10.0), r_all(5.0), pal.surface);
        } else {
            cv.text_in(&title, &TextStyle::new(pal.fs(18.0), pal.fg_hi).bold(), rect(area.x, y, area.w, 24.0), HAlign::Center);
            cv.text_in(&artist, &TextStyle::new(pal.fs(11.0), with_a(pal.fg, 0.72)), rect(area.x, y + 24.0, area.w, 16.0), HAlign::Center);
        }
        y += 42.0;

        // lyrics
        let (cur, next) = match m.lyrics_state {
            LyricsState::Loading => ("Loading lyrics…".to_string(), String::new()),
            LyricsState::NotFound if present => ("No lyrics found".to_string(), String::new()),
            _ => (m.lyric_current.clone(), m.lyric_next.clone()),
        };
        cv.text_in(&cur, &TextStyle::new(pal.fs(13.0), pal.fg_hi).bold(), rect(area.x, y, area.w, 20.0), HAlign::Center);
        cv.text_in(&next, &TextStyle::new(pal.fs(12.0), with_a(pal.fg, 0.60)), rect(area.x, y + 20.0, area.w, 18.0), HAlign::Center);
        y += 46.0;

        // seek
        let seek = rect(area.x, y, area.w, 14.0);
        self.seek_rect = seek;
        self.hits.add(seek.pad_xy(0.0, -6.0), ID_SEEK);
        let frac = self.seeking.unwrap_or(m.fraction() as f32).clamp(0.0, 1.0);
        let track = rect(seek.x, seek.y + 5.0, seek.w, 4.0);
        cv.fill_rrect(track, r_all(2.0), with_a(pal.fg, 0.24));
        cv.fill_rrect(rect(track.x, track.y, track.w * frac, 4.0), r_all(2.0), pal.fg_hi);
        if self.hits.is_hover(ID_SEEK) || self.seeking.is_some() { cv.fill_circle(pt(track.x + track.w * frac, track.y + 2.0), 5.0, pal.fg_hi); }
        let shown_pos = if let Some(f) = self.seeking { f as f64 * m.length } else { m.position };
        cv.text(&fmt_time(shown_pos), &pal.text_dim(10.0), pt(area.x, y + 22.0), HAlign::Left, VAlign::Middle);
        cv.text(&if m.length > 0.0 { fmt_time(m.length) } else { "Live".to_string() }, &pal.text_dim(10.0), pt(area.right(), y + 22.0), HAlign::Right, VAlign::Middle);
        y += 34.0;

        // controls
        let ctl_y = y;
        let buttons: [(u32, &str, f32); 5] = [(ID_BACK30, "\u{f04a}", 40.0), (ID_PREV, "\u{f048}", 40.0), (ID_PLAY, if m.playing { "\u{f04c}" } else { "\u{f04b}" }, 48.0), (ID_NEXT, "\u{f051}", 40.0), (ID_FWD30, "\u{f04e}", 40.0)];
        let total: f32 = buttons.iter().map(|b| b.2).sum::<f32>() + 8.0 * 4.0;
        let mut x = area.x + (area.w - total) / 2.0;
        for (id, glyph, d) in buttons {
            let r = rect(x, ctl_y + (48.0 - d) / 2.0, d, d);
            let hov = self.hits.add(r, id);
            let play = id == ID_PLAY;
            let fill = if play { if hov { pal.accent_hi } else { pal.fg_hi } } else { with_a(pal.fg, if hov { 0.24 } else { 0.14 }) };
            cv.fill_circle(r.center(), d / 2.0, fill);
            cv.text(glyph, &TextStyle::icon(16.0, if play { pal.bg } else { pal.fg_hi }), r.center(), HAlign::Center, VAlign::Middle);
            x += d + 8.0;
        }
        if m.can_raise() {
            let r = rect(area.right() - 28.0, ctl_y + 10.0, 28.0, 28.0);
            let hov = self.hits.add(r, ID_OPEN);
            cv.fill_circle(r.center(), 14.0, with_a(pal.fg, if hov { 0.24 } else { 0.1 }));
            cv.text(if m.raise_busy { "\u{f110}" } else { "\u{f08e}" }, &TextStyle::icon(12.0, pal.fg_hi), r.center(), HAlign::Center, VAlign::Middle);
        }
        if let Some(e) = &m.raise_error { cv.text_in(e, &pal.text(10.0), rect(area.x, ctl_y + 50.0, area.w, 14.0), HAlign::Center); }
    }

    fn event(&mut self, ev: &Event, _: Rect, ctx: &mut Ctx) {
        let len = ctx.state.media.length;
        match ev {
            Event::Press(Button::Left, p) if self.hits.at(*p) == Some(ID_SEEK) && len > 0.0 => self.seeking = Some(seek_fraction(p.x, self.seek_rect)),
            Event::PointerMove(p) if self.seeking.is_some() => self.seeking = Some(seek_fraction(p.x, self.seek_rect)),
            Event::Release(Button::Left, p) if self.seeking.is_some() => {
                let f = seek_fraction(p.x, self.seek_rect);
                self.seeking = None;
                media::seek_to(ctx.poster, f as f64 * len);
                self.hits.event(ev);
                return;
            }
            _ => {}
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_PLAY => media::play_pause(),
            ID_PREV => media::prev(),
            ID_NEXT => media::next(),
            ID_BACK30 => media::skip(-30),
            ID_FWD30 => media::skip(30),
            ID_OPEN => media::raise_player(ctx.poster),
            ID_SOURCE => ctx.state.media.cycle_player(),
            _ => {}
        }
    }
}

#[allow(dead_code)]
fn _p(_: Point) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_side_rules() {
        assert_eq!(cover_side(488.0, 400.0, 100.0), 128.0);
        assert_eq!(cover_side(100.0, 400.0, 100.0), 84.0);
        assert_eq!(cover_side(488.0, 200.0, 190.0), 64.0);
    }

    #[test]
    fn seek_fraction_clamps() {
        let r = rect(10.0, 0.0, 100.0, 10.0);
        assert_eq!(seek_fraction(0.0, r), 0.0);
        assert_eq!(seek_fraction(60.0, r), 0.5);
        assert_eq!(seek_fraction(500.0, r), 1.0);
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::page::testutil::Harness;
    use crate::render::Image;

    #[test]
    fn render_playing_png() {
        let mut st = State::new();
        let mut px = vec![0u8; 64 * 64 * 4];
        for (i, p) in px.chunks_mut(4).enumerate() { p.copy_from_slice(&[(i % 64 * 4) as u8, (i / 64 * 4) as u8, 160, 255]); }
        st.media.presence = MediaPresence::Present;
        st.media.playing = true;
        st.media.title = "Song Title".into();
        st.media.artist = "Some Artist".into();
        st.media.length = 200.0;
        st.media.position = 70.0;
        st.media.art = Image::from_rgba8(64, 64, &px);
        st.media.lyric_current = "Current lyric line".into();
        st.media.lyric_next = "Next lyric line".into();
        let mut h = Harness::new();
        let mut p = MediaPage::new();
        h.render(&mut p, &mut st, "/tmp/media_page.png");
    }
}
