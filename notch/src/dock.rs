//! Collapsed-notch layout: a compact clock block followed by app tiles. Pure geometry (no drawing) so every
//! size/count combination is testable; `notch::Notch` paints the result and registers the hit rectangles.
use crate::layout::{Point, Rect, pt, rect};

/// Padding between the notch edge and its content.
pub const PAD: f32 = 10.0;
/// Gap between tiles.
pub const GAP: f32 = 6.0;
/// Gap between the clock, the media slot and the app row.
pub const SECTION_GAP: f32 = 10.0;
/// Equaliser box width (matches `notch::draw_eq`'s design size).
pub const EQ_W: f32 = 36.0;
/// Tile hit cell at the reference 40px height; the icon is 26px inside it.
pub const CELL: f32 = 32.0;
pub const ICON: f32 = 26.0;

/// Text extents the layout needs.
#[derive(Clone, Copy, Debug)]
pub struct Dims {
    pub day_w: f32,
    pub time_w: f32,
    pub temp_w: f32,
    pub icon_w: f32,
    /// Width reserved for the charging bolt before the clock (0 when not charging).
    pub bolt_w: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Collapsed {
    /// Left edge of the weekday text, when there is room for it.
    pub day_x: Option<f32>,
    pub clock_c: Point,
    /// Charging bolt, left of the clock.
    pub bolt: Option<Point>,
    /// False when the notch is narrower than clock + `+N`: only the overflow tile remains.
    pub clock: bool,
    /// Media status slot (a tile-sized square), when media is present.
    pub art: Option<Rect>,
    /// App tiles (hit cells) with the index into the shortcut list.
    pub tiles: Vec<(Rect, usize)>,
    /// `+N` tile and how many apps it stands for.
    pub overflow: Option<(Rect, usize)>,
    pub eq: Option<Rect>,
    /// Weather glyph centre and temperature text left edge, when shown.
    pub weather: Option<(Point, f32)>,
    /// Where the weather glyph morphs from when it is not shown collapsed.
    pub weather_anchor: Point,
    /// Cell edge actually used (scaled down for short notches).
    pub cell: f32,
}

/// Cell edge for a collapsed height: 32 at 40px, never taller than the notch minus 8px of air.
pub fn cell_for(coll_h: f32) -> f32 { CELL.min((coll_h - 8.0).max(12.0)) }

/// Icon edge inside a cell (26/32 of it).
pub fn icon_for(cell: f32) -> f32 { (cell * ICON / CELL).round().max(10.0) }

/// Narrowest notch that shows the clock, the media tile (when playing) and every app tile.
pub fn preferred_width(time_w: f32, coll_h: f32, media: bool, n_apps: usize) -> f32 {
    let cell = cell_for(coll_h);
    let mut w = 2.0 * PAD + time_w;
    if media { w += GAP + cell + SECTION_GAP + EQ_W; }
    if n_apps > 0 { w += SECTION_GAP + n_apps as f32 * cell + (n_apps as f32 - 1.0) * GAP; }
    w
}

pub fn collapsed_layout(coll: Rect, d: Dims, media: bool, n_apps: usize) -> Collapsed {
    let cy = coll.y + coll.h / 2.0;
    let cell = cell_for(coll.h);
    let avail = (coll.w - 2.0 * PAD).max(0.0);

    // priority: clock + `+N` first, then media, then the apps; narrower than clock + `+N` drops the clock
    let clock = !(n_apps > 0 && avail < d.time_w + SECTION_GAP + cell);
    let media = media && clock && (n_apps == 0 || avail >= d.time_w + GAP + cell + SECTION_GAP + cell);
    let time_w = if clock { d.time_w + d.bolt_w } else { 0.0 };

    let mut used = time_w;
    if media { used += GAP + cell; }

    // app row: all of it when it fits, else as many as fit plus a `+N` tile (at least the `+N` tile)
    let lead = if used > 0.0 { SECTION_GAP } else { 0.0 };
    let room = avail - used - lead;
    let (shown, over) = if n_apps == 0 { (0, false) }
        else if n_apps as f32 * cell + (n_apps as f32 - 1.0) * GAP <= room { (n_apps, false) }
        else { ((((room + GAP) / (cell + GAP)).floor() as i64 - 1).max(0) as usize, true) };
    let tiles = shown + over as usize;
    // an overflow-only tile on a notch narrower than one cell is clipped to the notch
    let last_w = if shown == 0 && over { cell.min(avail) } else { cell };
    if tiles > 0 { used += lead + (tiles as f32 - 1.0) * (cell + GAP) + last_w; }

    // optional extras by priority, only with spare width
    let mut spare = avail - used;
    let eq = media && spare >= SECTION_GAP + EQ_W;
    if eq { spare -= SECTION_GAP + EQ_W; used += SECTION_GAP + EQ_W; }
    let weather_w = d.icon_w + 3.0 + d.temp_w;
    let weather = spare >= SECTION_GAP + weather_w;
    if weather { spare -= SECTION_GAP + weather_w; used += SECTION_GAP + weather_w; }
    let day = !media && spare >= d.day_w + 8.0;
    let total = used + if day { d.day_w + 8.0 } else { 0.0 };

    let mut x = coll.x + (coll.w - total) / 2.0;
    let mut out = Collapsed { cell, clock, ..Default::default() };
    if day { out.day_x = Some(x); x += d.day_w + 8.0; }
    if clock && d.bolt_w > 0.0 { out.bolt = Some(pt(x + d.bolt_w / 2.0, cy)); }
    out.clock_c = pt(x + (time_w - d.time_w) + d.time_w / 2.0, cy);
    x += time_w;
    if media { x += GAP; out.art = Some(rect(x, cy - cell / 2.0, cell, cell)); x += cell; }
    if tiles > 0 {
        x += lead;
        for i in 0..shown {
            out.tiles.push((rect(x, cy - cell / 2.0, cell, cell), i));
            x += cell + GAP;
        }
        if over { out.overflow = Some((rect(x, cy - cell / 2.0, last_w, cell), n_apps - shown)); x += last_w; } else { x -= GAP; }
    }
    if eq { x += SECTION_GAP; out.eq = Some(rect(x, cy - 8.0, EQ_W, 16.0)); x += EQ_W; }
    if weather {
        x += SECTION_GAP;
        out.weather = Some((pt(x + d.icon_w / 2.0, cy), x + d.icon_w + 3.0));
    }
    out.weather_anchor = out.weather.map_or(pt(out.clock_c.x + d.time_w / 2.0 + 8.0 + d.icon_w / 2.0, cy), |(c, _)| c);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Dims = Dims { day_w: 24.0, time_w: 40.0, temp_w: 30.0, icon_w: 12.0, bolt_w: 0.0 };

    fn all_inside(l: &Collapsed, coll: Rect) {
        let inner = coll.pad_xy(0.0, 0.0);
        for (r, _) in l.tiles.iter().chain(l.overflow.iter()) {
            assert!(r.x >= inner.x + PAD - 0.01 && r.right() <= inner.right() - PAD + 0.01, "{r:?} outside {coll:?}");
            assert!(r.y >= coll.y && r.bottom() <= coll.bottom());
        }
    }

    fn overlaps(a: &Rect, b: &Rect) -> bool { a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom() }

    #[test]
    fn every_size_and_count_keeps_tiles_inside_without_overlap_and_exposes_all_apps() {
        for (w, h) in [(180.0, 24.0), (360.0, 40.0), (420.0, 48.0)] {
            for media in [false, true] {
                for n in [0usize, 1, 6, 20] {
                    let coll = rect(100.0, 0.0, w, h);
                    let l = collapsed_layout(coll, D, media, n);
                    all_inside(&l, coll);
                    let mut rects: Vec<Rect> = l.tiles.iter().map(|(r, _)| *r).chain(l.overflow.map(|(r, _)| r)).chain(l.art).collect();
                    rects.push(rect(l.clock_c.x - D.time_w / 2.0, 0.0, D.time_w, h));
                    for i in 0..rects.len() { for j in i + 1..rects.len() { assert!(!overlaps(&rects[i], &rects[j]), "{w}x{h} media={media} n={n}: {:?} vs {:?}", rects[i], rects[j]); } }
                    let hidden = l.overflow.map_or(0, |(_, k)| k);
                    assert_eq!(l.tiles.len() + hidden, n, "{w}x{h} n={n}: every app is a tile or counted in +N");
                    assert_eq!(l.overflow.is_some(), l.tiles.len() < n);
                    if n > 0 { assert!(!l.tiles.is_empty() || l.overflow.is_some(), "minimum width keeps at least the overflow tile"); }
                    if n == 0 { assert!(l.tiles.is_empty() && l.overflow.is_none(), "no blank placeholder tiles"); }
                    assert_eq!(l.art.is_some(), media);
                    let c = l.cell;
                    assert!(c <= CELL && c <= h - 8.0 + 0.01 || h < 20.0);
                }
            }
        }
    }

    #[test]
    fn optional_text_appears_only_with_spare_width_and_media_never_hides_apps() {
        let coll = rect(0.0, 0.0, 360.0, 40.0);
        let few = collapsed_layout(coll, D, false, 2);
        assert!(few.weather.is_some() && few.day_x.is_some());
        let many = collapsed_layout(coll, D, false, 6);
        assert_eq!(many.tiles.len(), 6, "six tiles fit in 360px");
        let playing = collapsed_layout(coll, D, true, 6);
        assert_eq!(playing.tiles.len(), 6, "media takes one slot but app tiles stay");
        let crowded = collapsed_layout(rect(0.0, 0.0, 360.0, 40.0), D, false, 7);
        assert!(crowded.weather.is_none() && crowded.day_x.is_none(), "apps first, extras only from spare room");
        let tight = collapsed_layout(rect(0.0, 0.0, 180.0, 24.0), D, false, 8);
        assert!(tight.weather.is_none() && tight.overflow.is_some());
    }

    #[test]
    fn short_notches_scale_tiles_down() {
        assert_eq!(cell_for(40.0), 32.0);
        assert_eq!(cell_for(24.0), 16.0);
        assert_eq!(icon_for(32.0), 26.0);
        assert!(icon_for(16.0) < 16.0);
    }

    #[test]
    fn preferred_width_covers_clock_media_and_every_app() {
        assert_eq!(preferred_width(40.0, 40.0, false, 0), 60.0);
        assert_eq!(preferred_width(40.0, 40.0, false, 4), 216.0);
        assert_eq!(preferred_width(40.0, 40.0, false, 10), 444.0);
        assert_eq!(preferred_width(40.0, 40.0, true, 10), 444.0 + GAP + 32.0 + SECTION_GAP + EQ_W);
        // the width it asks for really shows every app without overflow
        for n in [1usize, 4, 10, 17] {
            let w = preferred_width(D.time_w, 40.0, true, n);
            let l = collapsed_layout(rect(0.0, 0.0, w, 40.0), D, true, n);
            assert_eq!((l.tiles.len(), l.overflow.is_none(), l.art.is_some()), (n, true, true), "n={n}");
        }
    }

    #[test]
    fn tight_rows_drop_media_then_the_clock_but_never_an_app() {
        // 20 apps at 560: exact +N remainder
        let l = collapsed_layout(rect(0.0, 0.0, 560.0, 40.0), D, false, 20);
        assert_eq!(l.tiles.len() + l.overflow.unwrap().1, 20);
        // clock + overflow fit but clock + media + overflow do not: media goes first
        let w = 2.0 * PAD + D.time_w + SECTION_GAP + 32.0;
        let l = collapsed_layout(rect(0.0, 0.0, w, 40.0), D, true, 5);
        assert!(l.clock && l.art.is_none() && l.overflow.map(|o| o.1) == Some(5));
        // narrower than clock + overflow: overflow-only tile, clipped inside the notch
        let coll = rect(0.0, 0.0, 2.0 * PAD + 20.0, 40.0);
        let l = collapsed_layout(coll, D, true, 5);
        let (r, n) = l.overflow.unwrap();
        assert!(!l.clock && l.tiles.is_empty() && n == 5 && l.art.is_none());
        assert!(r.x >= coll.x + PAD - 0.01 && r.right() <= coll.right() - PAD + 0.01 && r.w > 0.0);
    }
}
