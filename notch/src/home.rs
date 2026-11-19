//! Home page: greeting + big clock (drawn by the notch as a morphing element),
//! weather, quick-status chips, now-playing card, week strip.

use crate::glue;
use crate::layout::{Rect, Size, pt, rect};
use crate::page::{Morph, Page};
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{Hits, Palette, hover_bg};
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event};
use chrono::{Datelike, Local, NaiveDate, Timelike};

const ID_WEATHER: u32 = 1;
const ID_CHIP_BATT: u32 = 2;
const ID_CHIP_VOL: u32 = 3;
const ID_CHIP_WIFI: u32 = 4;
const ID_CHIP_BT: u32 = 5;
const ID_PLAYER: u32 = 6;
const ID_PLAY: u32 = 7;
const ID_WEEK: u32 = 8;
const ID_DAY0: u32 = 100;

const ROW1_H: f32 = 78.0;
const CHIP_H: f32 = 24.0;
const PLAYER_H: f32 = 64.0;
const WEEK_H: f32 = 92.0;
const GAP: f32 = 12.0;

#[derive(Default)]
pub struct Home {
    hits: Hits,
}

impl Home {
    pub fn new() -> Home { Home::default() }

    fn layout(area: Rect) -> HomeLayout {
        let half = (area.w - GAP) / 2.0;
        let left = rect(area.x, area.y, half, ROW1_H);
        let right = rect(area.x + half + GAP, area.y, half, ROW1_H);
        let chips_y = area.y + ROW1_H + GAP;
        let chip_w = (area.w - 8.0) / 2.0;
        let chips = [
            rect(area.x, chips_y, chip_w, CHIP_H),
            rect(area.x + chip_w + 8.0, chips_y, chip_w, CHIP_H),
            rect(area.x, chips_y + CHIP_H + 8.0, chip_w, CHIP_H),
            rect(area.x + chip_w + 8.0, chips_y + CHIP_H + 8.0, chip_w, CHIP_H),
        ];
        let player = rect(area.x, chips_y + 2.0 * CHIP_H + 8.0 + GAP, area.w, PLAYER_H);
        let week = rect(area.x, player.bottom() + GAP, area.w, WEEK_H);
        HomeLayout { left, right, chips, player, week }
    }
}

struct HomeLayout {
    left: Rect,
    right: Rect,
    chips: [Rect; 4],
    player: Rect,
    week: Rect,
}

fn card(cv: &mut Canvas, pal: &Palette, r: Rect, radius: f32) {
    cv.fill_rrect(r, r_all(radius), pal.glass);
    cv.stroke_rrect(r, r_all(radius), 1.0, with_a(pal.fg, 0.18));
}

fn greeting() -> &'static str {
    match Local::now().hour() { 5..=11 => "Good morning", 12..=17 => "Good afternoon", _ => "Good evening" }
}

pub fn week_days(today: NaiveDate) -> [NaiveDate; 7] {
    let start = today - chrono::Duration::days(today.weekday().num_days_from_sunday() as i64);
    std::array::from_fn(|i| start + chrono::Duration::days(i as i64))
}

impl Page for Home {
    fn natural(&mut self, avail: Size, _st: &State, _ts: &mut TextSys) -> Size {
        let l = Home::layout(rect(0.0, 0.0, avail.w, 0.0));
        Size { w: avail.w, h: l.week.bottom() }
    }

    fn morph(&self, area: Rect, st: &State) -> Morph {
        let l = Home::layout(area);
        let np = glue::now_playing(st);
        let w2 = l.right;
        Morph {
            clock: Some((pt(l.left.x + 50.0, l.left.y + 14.0 + 24.0), 36.0)),
            weather_icon: Some((pt(w2.x + 10.0, w2.y + 12.0), 20.0)),
            art: np.present.then(|| rect(l.player.x + 8.0, l.player.y + 8.0, 48.0, 48.0)),
            eq: None,
        }
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        let l = Home::layout(area);
        self.hits.clear();
        let now = Local::now();

        // greeting + date (the clock itself is the notch's morphing element)
        cv.text(greeting(), &TextStyle::new(pal.fs(11.0), with_a(pal.fg, 0.65)).bold(), pt(l.left.x, l.left.y), HAlign::Left, VAlign::Top);
        let date = now.format("%A, %-d %B").to_string();
        cv.text_in(&date, &pal.text_dim(12.0), rect(l.left.x, l.left.y + 14.0 + 48.0, l.left.w, 16.0), HAlign::Left);

        // weather
        {
            let hovered = self.hits.add(l.right, ID_WEATHER);
            hover_bg(cv, pal, l.right.pad(-4.0), 10.0, hovered, self.hits.is_pressed(ID_WEATHER));
            let (temp, loc, wind) = match &st.weather.report {
                Some(r) => (
                    format!("{:.0}°C", r.temperature_c),
                    r.location.clone(),
                    if st.weather.error.is_some() || st.weather.fetching { weather_status(st) } else {
                        r.wind_kmh.map(|s| format!("Wind {:.0} km/h{}", s, r.wind_direction.as_deref().map(|d| format!(" {d}")).unwrap_or_default())).unwrap_or_else(|| "Wind unavailable".into())
                    },
                ),
                None => ("—".to_string(), "Weather unavailable".to_string(), weather_status(st)),
            };
            let x = l.right.x + 26.0;
            cv.text_in(&temp, &TextStyle::new(pal.fs(20.0), pal.fg_hi).bold(), rect(x, l.right.y, l.right.w - 26.0, 24.0), HAlign::Left);
            cv.text_in(&loc, &pal.text(11.0), rect(l.right.x, l.right.y + 30.0, l.right.w, 15.0), HAlign::Left);
            cv.text_in(&wind, &pal.text_dim(11.0), rect(l.right.x, l.right.y + 46.0, l.right.w, 15.0), HAlign::Left);
        }

        // status chips
        let s = &st.status;
        let charging = s.power.as_ref().is_some_and(glue::actively_charging);
        let batt = s.power.as_ref().map_or_else(|| "\u{f240} …".to_string(), crate::sysinfo::battery_chip);
        let vol = crate::sysinfo::volume_chip(&s.volume);
        let wifi = crate::sysinfo::wifi_chip(&s.wifi);
        let bt = crate::sysinfo::bt_chip(&s.bt);
        for (i, (id, text)) in [(ID_CHIP_BATT, batt), (ID_CHIP_VOL, vol), (ID_CHIP_WIFI, wifi), (ID_CHIP_BT, bt)].into_iter().enumerate() {
            let r = l.chips[i];
            let hovered = self.hits.add(r, id);
            let hot = id == ID_CHIP_BATT && charging;
            cv.fill_rrect(r, r_all(8.0), if hot { with_a(pal.accent, 0.22) } else { pal.glass });
            cv.stroke_rrect(r, r_all(8.0), 1.0, with_a(pal.fg, 0.18));
            if hovered { cv.fill_rrect(r, r_all(8.0), with_a(pal.fg, 0.10)); }
            let col = if hot { pal.accent_hi } else if hovered { pal.fg_hi } else { pal.fg };
            // glyph in the symbols font, remainder in the UI font
            let first = text.chars().next().unwrap_or(' ');
            if (first as u32) >= 0xE000 {
                let rest: String = text.chars().skip(1).collect::<String>().trim_start().to_string();
                cv.text(&first.to_string(), &TextStyle::icon(pal.fs(11.0), col), pt(r.x + 8.0, r.y + r.h / 2.0), HAlign::Left, VAlign::Middle);
                cv.text_in(&rest, &TextStyle::new(pal.fs(11.0), col), rect(r.x + 26.0, r.y, r.w - 32.0, r.h), HAlign::Left);
            } else {
                cv.text_in(&text, &TextStyle::new(pal.fs(11.0), col), r.pad_xy(8.0, 0.0), HAlign::Left);
            }
        }

        // now-playing card
        let np = glue::now_playing(st);
        {
            let r = l.player;
            let hovered = self.hits.add(r, ID_PLAYER);
            cv.fill_rrect(r, r_all(14.0), with_a(pal.bg, 0.30));
            cv.with_clip(r, r_all(14.0), |cv| {
                if let Some(img) = &np.art {
                    cv.with_alpha(0.65, |cv| cv.image_cover(img, r, [0.0; 4]));
                    cv.fill_rect(r, crate::render::Fill::Linear { a: pt(r.x, r.y), b: pt(r.right(), r.y), stops: vec![(0.0, with_a(pal.bg, 0.40)), (1.0, with_a(pal.bg, 0.88))] });
                }
                if hovered { cv.fill_rect(r, with_a(pal.fg, 0.04)); }
            });
            cv.stroke_rrect(r, r_all(14.0), 1.0, pal.edge);
            let art = rect(r.x + 8.0, r.y + 8.0, 48.0, 48.0);
            // album art itself is the notch's morphing element while a player exists
            if !np.present {
                cv.fill_rrect(art, r_all(10.0), pal.hover);
                cv.text("\u{f001}", &TextStyle::icon(22.0, pal.dim), art.center(), HAlign::Center, VAlign::Middle);
            }
            let play = rect(r.right() - 8.0 - 32.0, r.y + 16.0, 32.0, 32.0);
            let title = if np.present && !np.title.is_empty() { np.title.clone() } else { "Nothing playing".to_string() };
            let tx = art.right() + 10.0;
            let tw = play.x - 10.0 - tx;
            cv.text_in(&title, &TextStyle::new(pal.fs(13.0), pal.fg_hi).bold(), rect(tx, r.y + 12.0, tw, 20.0), HAlign::Left);
            if !np.artist.is_empty() { cv.text_in(&np.artist, &TextStyle::new(pal.fs(11.0), with_a(pal.fg, 0.72)), rect(tx, r.y + 32.0, tw, 18.0), HAlign::Left); }
            let ph = self.hits.add(play, ID_PLAY);
            cv.fill_circle(play.center(), 16.0, with_a(pal.fg, if ph { 0.24 } else { 0.14 }));
            cv.text(if np.playing { "\u{f04c}" } else { "\u{f04b}" }, &TextStyle::icon(14.0, pal.fg_hi), play.center(), HAlign::Center, VAlign::Middle);
        }

        // week strip
        {
            let r = l.week;
            let hovered_card = self.hits.add(r, ID_WEEK);
            let _ = hovered_card;
            card(cv, pal, r, 14.0);
            let today = now.date_naive();
            cv.text_in(&today.format("%B %Y").to_string(), &TextStyle::new(pal.fs(12.0), pal.fg_hi).bold(), rect(r.x + 12.0, r.y + 10.0, r.w - 24.0, 16.0), HAlign::Left);
            let cols = 7.0;
            let cw = (r.w - 24.0) / cols;
            for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
                cv.text(d, &TextStyle::new(pal.fs(10.0), with_a(pal.fg, 0.45)), pt(r.x + 12.0 + cw * (i as f32 + 0.5), r.y + 38.0), HAlign::Center, VAlign::Middle);
            }
            for (i, d) in week_days(today).into_iter().enumerate() {
                let c = rect(r.x + 12.0 + cw * i as f32 + (cw - 28.0) / 2.0, r.y + 50.0, 28.0, 28.0);
                let hov = self.hits.add(c, ID_DAY0 + i as u32);
                if hov { cv.fill_rrect(c, r_all(12.0), with_a(pal.fg, 0.12)); }
                let other = d.month() != today.month() || d.year() != today.year();
                let is_today = d == today;
                if is_today { cv.stroke_rrect(c, r_all(12.0), 1.0, pal.accent_hi); }
                let col = if is_today { pal.fg_hi } else if other { with_a(pal.fg, 0.25) } else { pal.fg };
                let mut sty = TextStyle::new(pal.fs(12.0), col);
                if is_today { sty = sty.bold(); }
                cv.text(&d.day().to_string(), &sty, c.center(), HAlign::Center, VAlign::Middle);
            }
        }
    }

    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn event(&mut self, ev: &Event, _area: Rect, ctx: &mut Ctx) {
        let Some(click) = self.hits.event(ev) else { return };
        if click.button != Button::Left { return; }
        match click.id {
            ID_WEATHER => ctx.state.nav = Some(NavTo::Page("weather")),
            ID_CHIP_BATT => ctx.state.nav = Some(NavTo::Page("battery")),
            ID_CHIP_VOL => ctx.state.nav = Some(NavTo::Page("audio")),
            ID_CHIP_WIFI => ctx.state.nav = Some(NavTo::Page("wifi")),
            ID_CHIP_BT => ctx.state.nav = Some(NavTo::Page("bt")),
            ID_PLAY => glue::play_pause(),
            ID_PLAYER => ctx.state.nav = Some(NavTo::Page("media")),
            ID_WEEK => { ctx.state.cal_goto = None; ctx.state.nav = Some(NavTo::Page("calendar")) }
            id if (ID_DAY0..ID_DAY0 + 7).contains(&id) => {
                let d = week_days(Local::now().date_naive())[(id - ID_DAY0) as usize];
                ctx.state.cal_goto = Some(d);
                ctx.state.nav = Some(NavTo::Page("calendar"));
            }
            _ => {}
        }
    }
}

/// Last-updated / loading line shown under the weather when the report is stale or failing.
pub fn weather_status(st: &State) -> String {
    let w = &st.weather;
    if let Some(at) = w.last_success {
        let time = at.format(st.settings.behavior.clock_format.chrono_pattern()).to_string();
        let day = if at.date_naive() == Local::now().date_naive() { String::new() } else { format!("{} ", at.format("%b %-d")) };
        format!("Last updated {day}{time}{}", if w.error.is_some() { " · Refresh failed" } else if w.fetching { " · Refreshing…" } else { "" })
    } else if w.fetching {
        "Loading weather…".into()
    } else {
        "Weather unavailable".into()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn week_starts_on_sunday() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 18).unwrap(); // Wednesday
        let w = week_days(d);
        assert_eq!(w[0], NaiveDate::from_ymd_opt(2026, 11, 15).unwrap());
        assert_eq!(w[3], d);
        assert_eq!(w[6], NaiveDate::from_ymd_opt(2026, 11, 21).unwrap());
    }

    #[test]
    fn layout_stacks_without_overlap() {
        let l = Home::layout(rect(0.0, 0.0, 488.0, 0.0));
        assert!(l.chips[0].y >= l.left.bottom());
        assert!(l.player.y >= l.chips[3].bottom());
        assert!(l.week.y >= l.player.bottom());
    }
}
