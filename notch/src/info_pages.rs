//! Weather detail page and the AI-usage tab.

use crate::home::weather_status;
use crate::layout::{Rect, Size, pt, rect};
use crate::page::Page;
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, mix, r_all, rgb, with_a};
use crate::state::{NavTo, State};
use crate::ui::{ButtonKind, Hits, Palette, button};
use crate::usage;
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event};

const ID_BACK: u32 = 1;
const ID_REFRESH: u32 = 2;

// ---------------------------------------------------------------- weather

#[derive(Default)]
pub struct WeatherPage { hits: Hits }

impl WeatherPage { pub fn new() -> Self { Self::default() } }

impl Page for WeatherPage {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        let days = st.weather.report.as_ref().map_or(0, |r| r.forecast.len().min(5)) as f32;
        Size { w: avail.w, h: 40.0 + 150.0 + days * 24.0 + 30.0 }
    }
    fn shown(&mut self, ctx: &mut Ctx) { crate::weather::request(&mut ctx.state.weather); }
    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.hits.clear();
        let w = &st.weather;
        let back = rect(area.x, area.y, 60.0, 26.0);
        let hb = self.hits.add(back, ID_BACK);
        button(cv, pal, back, "\u{f053} Back", ButtonKind::Plain, hb, false);
        cv.text("Weather", &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), pt(area.x + 72.0, area.y + 13.0), HAlign::Left, VAlign::Middle);
        let rf = rect(area.right() - 74.0, area.y, 74.0, 26.0);
        let label = if w.report.is_none() && w.error.is_some() { "Retry" } else { "Refresh" };
        let hr = self.hits.add(rf, ID_REFRESH);
        button(cv, pal, rf, label, ButtonKind::Plain, hr && !w.fetching, false);

        let card = rect(area.x, area.y + 38.0, area.w, 140.0);
        cv.fill_rrect(card, r_all(14.0), with_a(pal.fg, 0.06));
        cv.stroke_rrect(card, r_all(14.0), 1.0, with_a(pal.fg, 0.08));
        let c = card.pad(14.0);
        match &w.report {
            Some(r) => {
                cv.text_in(&r.location, &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), rect(c.x, c.y, c.w, 20.0), HAlign::Left);
                cv.text(&r.icon, &TextStyle::icon(pal.fs(30.0), pal.fg_hi), pt(c.x + 16.0, c.y + 52.0), HAlign::Center, VAlign::Middle);
                cv.text(&format!("{:.0}°C", r.temperature_c), &TextStyle::new(pal.fs(30.0), pal.fg_hi).bold(), pt(c.x + 44.0, c.y + 52.0), HAlign::Left, VAlign::Middle);
                cv.text_in(&r.description, &pal.text_dim(12.0), rect(c.x, c.y + 78.0, c.w, 18.0), HAlign::Left);
                let wind = r.wind_kmh.map(|s| format!("Wind {:.0} km/h{}", s, r.wind_direction.as_deref().map(|d| format!(" {d}")).unwrap_or_default())).unwrap_or_else(|| "Wind unavailable".into());
                let feels = r.feels_like_c.map(|v| format!("Feels like {v:.0}°C")).unwrap_or_else(|| "Feels like unavailable".into());
                let hum = r.humidity_pct.map(|v| format!("Humidity {v}%")).unwrap_or_else(|| "Humidity unavailable".into());
                let col = c.w / 3.0;
                for (i, t) in [feels, hum, wind].iter().enumerate() {
                    cv.text_in(t, &pal.text(11.0), rect(c.x + col * i as f32, c.y + 100.0, col - 4.0, 16.0), HAlign::Left);
                }
                for (i, d) in r.forecast.iter().take(5).enumerate() {
                    let y = card.bottom() + 10.0 + i as f32 * 24.0;
                    let low = d.low_c.map(|v| format!("{v:.0}°C")).unwrap_or_else(|| "—".into());
                    let high = d.high_c.map(|v| format!("{v:.0}°C")).unwrap_or_else(|| "—".into());
                    cv.text(&d.icon, &TextStyle::icon(pal.fs(13.0), pal.fg), pt(area.x + 8.0, y + 10.0), HAlign::Left, VAlign::Middle);
                    cv.text_in(&d.date.format("%a %-d %b").to_string(), &pal.text(12.0), rect(area.x + 34.0, y, 110.0, 20.0), HAlign::Left);
                    cv.text_in(&format!("Low {low}  ·  High {high}"), &pal.text_dim(12.0), rect(area.x + 150.0, y, area.w - 150.0, 20.0), HAlign::Left);
                }
            }
            None => {
                cv.text_in(w.query.display_name(), &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), rect(c.x, c.y, c.w, 20.0), HAlign::Left);
                cv.text_in("Weather unavailable", &TextStyle::new(pal.fs(20.0), pal.fg_hi).bold(), rect(c.x, c.y + 34.0, c.w, 30.0), HAlign::Left);
                cv.text_in(if w.fetching { "Loading current conditions…" } else { "Use Retry to fetch current conditions" }, &pal.text_dim(12.0), rect(c.x, c.y + 72.0, c.w, 18.0), HAlign::Left);
            }
        }
        cv.text_in(&weather_status(st), &pal.text_dim(11.0), rect(area.x, area.bottom() - 18.0, area.w, 18.0), HAlign::Left);
    }

    fn event(&mut self, ev: &Event, _: Rect, ctx: &mut Ctx) {
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_BACK => ctx.state.nav = Some(NavTo::Page("home")),
            ID_REFRESH if !ctx.state.weather.fetching => crate::weather::request(&mut ctx.state.weather),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------- AI usage

#[derive(Default)]
pub struct AiPage;

impl AiPage { pub fn new() -> Self { AiPage } }

const AI_ROW_H: f32 = 84.0;

fn spark(cv: &mut Canvas, r: Rect, vals: &[f32; 7], col: crate::render::Color) {
    let n = vals.len() as f32;
    let bw = (r.w - 3.0 * (n - 1.0)) / n;
    for (i, v) in vals.iter().enumerate() {
        let h = (v.max(0.06) * r.h).min(r.h);
        cv.fill_rrect(rect(r.x + i as f32 * (bw + 3.0), r.bottom() - h, bw, h), r_all(bw / 2.0), with_a(col, if i == 6 { 1.0 } else { 0.55 }));
    }
}

fn row_height(_r: &usage::Row) -> f32 { AI_ROW_H }

impl Page for AiPage {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        let rows = st.sys.usage.rows();
        let h = if rows.is_empty() { 70.0 } else { rows.iter().map(|r| row_height(r) + 10.0).sum::<f32>() - 10.0 };
        Size { w: avail.w, h }
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let pal: &Palette = &dc.state.pal;
        let rows = dc.state.sys.usage.rows();
        if rows.is_empty() {
            cv.text_wrap(usage::EMPTY_TEXT, &pal.text_dim(12.0), area.pad_xy(4.0, 8.0), HAlign::Left, 4);
            return;
        }
        let mut y = area.y;
        for r in &rows {
            let rr = rect(area.x, y, area.w, row_height(r));
            cv.fill_rrect(rr, r_all(14.0), with_a(pal.fg, 0.06));
            cv.stroke_rrect(rr, r_all(14.0), 1.0, with_a(pal.fg, 0.08));
            let col = rgb((r.rgb.0 * 255.0) as u8, (r.rgb.1 * 255.0) as u8, (r.rgb.2 * 255.0) as u8);
            let dot = if r.live { col } else { with_a(col, 0.45) };
            cv.fill_circle(pt(rr.x + 16.0, rr.y + 18.0), 4.0, dot);
            cv.text(r.title, &TextStyle::new(pal.fs(12.0), pal.fg_hi).bold(), pt(rr.x + 28.0, rr.y + 18.0), HAlign::Left, VAlign::Middle);
            cv.text(&r.big, &TextStyle::new(pal.fs(22.0), pal.fg_hi).bold(), pt(rr.x + 14.0, rr.y + 42.0), HAlign::Left, VAlign::Middle);
            let bw = cv.measure(&r.big, &TextStyle::new(pal.fs(22.0), pal.fg_hi).bold()).0;
            cv.text(r.sub, &pal.text_dim(11.0), pt(rr.x + 20.0 + bw, rr.y + 46.0), HAlign::Left, VAlign::Middle);
            cv.text_in(&r.info, &pal.text_dim(10.5), rect(rr.x + 14.0, rr.y + 58.0, rr.w - 28.0, 14.0), HAlign::Left);
            if let Some(d) = &r.detail { cv.text_in(d, &pal.text_dim(10.0), rect(rr.x + 14.0, rr.y + 70.0, rr.w - 28.0, 12.0), HAlign::Left); }
            spark(cv, rect(rr.right() - 14.0 - 84.0, rr.y + 12.0, 84.0, 30.0), &r.spark, mix(col, pal.fg, 0.1));
            y += rr.h + 10.0;
        }
    }

    fn event(&mut self, _: &Event, _: Rect, _: &mut Ctx) {}
}
