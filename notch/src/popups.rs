//! Control bodies of the notch's `wifi`, `bt`, `audio`, `battery` and `display` pages (hosted by `BodyPage`) and the
//! control fifo. Every control surface lives inside the notch; nothing here opens a surface of its own.

use crate::layout::{Rect, Scroll, Size, pt, rect};
use crate::net;
use crate::page::Page;
use crate::power::{BatteryState, PowerSnapshot};
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{ButtonKind, FieldResult, Hits, TextField, Palette, button, scrollbar, slider, toggle as switch};
use crate::wayland::{Button, Ctx, Cursor, DrawCtx, Event, Poster};
use serde_json::Value;

const MAX_BODY_H: f32 = 420.0;

#[derive(Default)]
pub struct PopupState {
    /// Latest `audio::gather_volume()` document (outputs + streams), refreshed while a volume body is visible.
    pub audio: Option<Value>,
    /// A poll is in flight.
    pub audio_busy: bool,
    /// Mixer commands (stream volume/mute, output switch) still running on workers.
    pub audio_cmds: u32,
    /// Newest refresh already applied; older results arriving later are dropped.
    pub audio_seq: u64,
    pub audio_error: Option<String>,
    /// Power-profile modes offered by power-profiles-daemon; `None` when unavailable or not read yet.
    pub profiles: Option<crate::power::Profiles>,
    pub profile_busy: bool,
    pub profile_error: Option<String>,
    /// `{"mons":[{name,w,h,scale,focused,enabled,brightness}],"text":n}` for the Displays page.
    pub display: Option<Value>,
}

pub fn body_for(kind: &str) -> Option<Box<dyn Body>> {
    Some(match kind {
        "wifi" => Box::new(WifiBody::default()),
        "bluetooth" | "bt" => Box::new(BtBody::default()),
        "volume" | "audio" => Box::new(VolumeBody::default()),
        "battery" => Box::new(BatteryBody::default()),
        "display" => Box::new(DisplayBody::default()),
        _ => return None,
    })
}

// ---------------------------------------------------------------- bodies

pub trait Body {
    fn opened(&mut self, poster: &Poster, st: &State);
    fn natural_h(&mut self, w: f32, st: &State, ts: &mut TextSys) -> f32;
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect);
    fn event(&mut self, ev: &Event, r: Rect, ctx: &mut Ctx);
    fn tick(&mut self, _dt: f32, _ctx: &mut Ctx) -> bool { false }
    fn wants_keyboard(&self) -> bool { false }
    fn cursor(&self) -> Cursor { Cursor::Default }
}

fn header(cv: &mut Canvas, pal: &Palette, r: Rect, title: &str, sub: &str) {
    cv.text(title, &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), pt(r.x, r.y + 2.0), HAlign::Left, VAlign::Top);
    cv.text_in(sub, &pal.text_dim(11.0), rect(r.x, r.y + 22.0, r.w - 120.0, 16.0), HAlign::Left);
}

fn row_bg(cv: &mut Canvas, pal: &Palette, r: Rect, hovered: bool, active: bool) {
    let fill = if active { with_a(pal.accent, 0.18) } else if hovered { with_a(pal.fg, 0.11) } else { with_a(pal.fg, 0.06) };
    cv.fill_rrect(r, r_all(12.0), fill);
}

fn signal_glyph(_pct: u8) -> &'static str { "\u{f1eb}" }

// ---- Wi-Fi
#[derive(Default)]
pub struct WifiBody { hits: Hits, scroll: Scroll, pending: Option<String>, field: TextField }

const ID_TOGGLE: u32 = 1;
const ID_SCAN: u32 = 2;
const ID_CONNECT: u32 = 3;
const ID_CANCEL: u32 = 4;
const ID_ROW0: u32 = 100;

impl Body for WifiBody {
    fn opened(&mut self, poster: &Poster, _: &State) { net::refresh_wifi(poster, true); self.field = TextField::new("Password"); self.field.password = true; }
    fn natural_h(&mut self, _w: f32, st: &State, _: &mut TextSys) -> f32 {
        let n = st.net.wifi.nets.len().max(1) as f32;
        let extra = if self.pending.is_some() { 48.0 } else { 0.0 };
        (46.0 + n * 46.0 + extra).min(MAX_BODY_H)
    }
    fn wants_keyboard(&self) -> bool { self.pending.is_some() }
    fn cursor(&self) -> Cursor { self.hits.cursor() }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        let snap = &st.net.wifi;
        self.hits.clear();
        let (head, sub) = snap.card_text();
        header(cv, pal, r, "Wi-Fi", &st.net.status_line.clone().unwrap_or(sub));
        let _ = head;
        let t = rect(r.right() - 44.0, r.y + 4.0, 44.0, 24.0);
        self.hits.add(t, ID_TOGGLE);
        if snap.available { switch(cv, pal, t, if snap.on { 1.0 } else { 0.0 }); }
        let mut y = r.y + 44.0;
        if let Some(ssid) = &self.pending {
            let f = rect(r.x, y, r.w - 120.0, 32.0);
            self.field.focused = true;
            self.field.draw(cv, pal, f);
            let c = rect(f.right() + 6.0, y + 2.0, 60.0, 28.0);
            let x = rect(c.right() + 4.0, y + 2.0, 46.0, 28.0);
            let hc = self.hits.add(c, ID_CONNECT);
            let hx = self.hits.add(x, ID_CANCEL);
            button(cv, pal, c, "Join", ButtonKind::Accent, hc, false);
            button(cv, pal, x, "×", ButtonKind::Plain, hx, false);
            let _ = ssid;
            y += 44.0;
        }
        let list = rect(r.x, y, r.w, (r.bottom() - y).max(0.0));
        self.scroll.content_h = snap.nets.len() as f32 * 46.0;
        self.scroll.clamp(list.h);
        if snap.nets.is_empty() {
            cv.text_in(if snap.available { "No networks found" } else { "Wi-Fi unavailable" }, &pal.text_dim(12.0), rect(list.x, list.y, list.w, 30.0), HAlign::Center);
        }
        cv.with_clip(list, [0.0; 4], |cv| {
            for (i, n) in snap.nets.iter().enumerate() {
                let rr = rect(list.x, list.y + i as f32 * 46.0 - self.scroll.offset, list.w, 40.0);
                if rr.bottom() < list.y || rr.y > list.bottom() { continue; }
                let hov = self.hits.add_in(rr, list, ID_ROW0 + i as u32);
                row_bg(cv, pal, rr, hov, n.active);
                cv.text(signal_glyph(n.signal), &TextStyle::icon(16.0, if n.active { pal.accent_hi } else { pal.fg }), pt(rr.x + 12.0, rr.y + 20.0), HAlign::Left, VAlign::Middle);
                cv.text_in(&n.ssid, &TextStyle::new(pal.fs(13.0), pal.fg_hi).weight(600), rect(rr.x + 38.0, rr.y, rr.w - 120.0, rr.h), HAlign::Left);
                let badge = if n.active { "Connected" } else if n.saved { "Saved" } else { "" };
                cv.text(badge, &TextStyle::new(pal.fs(10.0), pal.accent_hi).bold(), pt(rr.right() - 12.0 - if n.secure { 18.0 } else { 0.0 }, rr.y + 20.0), HAlign::Right, VAlign::Middle);
                if n.secure { cv.text("\u{f023}", &TextStyle::icon(11.0, pal.dim), pt(rr.right() - 12.0, rr.y + 20.0), HAlign::Right, VAlign::Middle); }
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
    }
    fn event(&mut self, ev: &Event, r: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, r.h - 44.0); return; }
        if let Event::Key(k) = ev {
            if self.pending.is_some() {
                match self.field.key(k) {
                    FieldResult::Submit => self.submit(ctx),
                    FieldResult::Cancel => { self.pending = None; self.field.clear(); }
                    _ => {}
                }
            }
            return;
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_TOGGLE => net::wifi_radio(ctx.poster, !ctx.state.net.wifi.on),
            ID_SCAN => net::refresh_wifi(ctx.poster, true),
            ID_CONNECT => self.submit(ctx),
            ID_CANCEL => { self.pending = None; self.field.clear(); }
            id if id >= ID_ROW0 => {
                let Some(n) = ctx.state.net.wifi.nets.get((id - ID_ROW0) as usize).cloned() else { return };
                if n.active { net::wifi_connect(ctx.poster, &n.ssid, None, false); }
                else if n.secure && !n.saved { self.pending = Some(n.ssid); self.field.clear(); }
                else { net::wifi_connect(ctx.poster, &n.ssid, None, true); }
            }
            _ => {}
        }
    }
}

impl WifiBody {
    fn submit(&mut self, ctx: &mut Ctx) {
        if let Some(ssid) = self.pending.take() {
            let pw = std::mem::take(&mut self.field.text);
            self.field.clear();
            if !pw.is_empty() { net::wifi_connect(ctx.poster, &ssid, Some(pw), true); } else { self.pending = Some(ssid); }
        }
    }
}

// ---- Bluetooth
#[derive(Default)]
pub struct BtBody { hits: Hits, scroll: Scroll }

/// One slice of the Bluetooth list: a section header or a device (index into `BtSnap::devs`, which is sorted
/// connected, then paired, then the rest).
#[derive(Clone, Copy, Debug, PartialEq)]
enum BtRow { Header(&'static str, usize), Dev(usize) }

impl BtRow {
    fn h(self) -> f32 { match self { BtRow::Header(..) => 26.0, BtRow::Dev(_) => 52.0 } }
}

const ID_DISC0: u32 = 600;

fn bt_rows(devs: &[net::BtDev]) -> Vec<BtRow> {
    let mut rows = Vec::new();
    for (title, keep) in [("Connected", 0u8), ("Paired", 1), ("Nearby", 2)] {
        let class = |d: &net::BtDev| if d.connected { 0 } else if d.paired { 1 } else { 2 };
        let idx: Vec<usize> = (0..devs.len()).filter(|&i| class(&devs[i]) == keep).collect();
        if idx.is_empty() { continue; }
        rows.push(BtRow::Header(title, idx.len()));
        rows.extend(idx.into_iter().map(BtRow::Dev));
    }
    rows
}

impl Body for BtBody {
    fn opened(&mut self, poster: &Poster, _: &State) { net::refresh_bt(poster); }
    fn natural_h(&mut self, _w: f32, st: &State, _: &mut TextSys) -> f32 {
        let rows: f32 = bt_rows(&st.net.bt.devs).iter().map(|r| r.h()).sum();
        (46.0 + rows.max(52.0)).min(MAX_BODY_H)
    }
    fn cursor(&self) -> Cursor { self.hits.cursor() }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        let snap = &st.net.bt;
        self.hits.clear();
        let (_, sub) = snap.card_text();
        header(cv, pal, r, "Bluetooth", &st.net.status_line.clone().unwrap_or(sub));
        let t = rect(r.right() - 44.0, r.y + 4.0, 44.0, 24.0);
        self.hits.add(t, ID_TOGGLE);
        if snap.available { switch(cv, pal, t, if snap.on { 1.0 } else { 0.0 }); }
        let sc = rect(r.right() - 104.0, r.y + 4.0, 52.0, 24.0);
        let hs = self.hits.add(sc, ID_SCAN);
        if snap.on { button(cv, pal, sc, if st.net.busy { "…" } else { "Scan" }, ButtonKind::Plain, hs, false); }
        let list = rect(r.x, r.y + 44.0, r.w, (r.bottom() - r.y - 44.0).max(0.0));
        let rows = bt_rows(&snap.devs);
        self.scroll.content_h = rows.iter().map(|r| r.h()).sum();
        self.scroll.clamp(list.h);
        if snap.devs.is_empty() {
            cv.text_in(if !snap.available { "Bluetooth unavailable" } else if !snap.on { "Bluetooth is off" } else { "No devices found" }, &pal.text_dim(12.0), rect(list.x, list.y, list.w, 30.0), HAlign::Center);
        }
        cv.with_clip(list, [0.0; 4], |cv| {
            let mut y = list.y - self.scroll.offset;
            for row in &rows {
                let h = row.h();
                let top = y;
                y += h;
                if top + h < list.y || top > list.bottom() { continue; }
                match *row {
                    BtRow::Header(t, n) => {
                        cv.text(&format!("{t} · {n}"), &pal.text_dim(11.0), pt(list.x + 2.0, top + 13.0), HAlign::Left, VAlign::Middle);
                    }
                    BtRow::Dev(i) => {
                        let d = &snap.devs[i];
                        let rr = rect(list.x, top, list.w, 46.0);
                        let hov = self.hits.add_in(rr, list, ID_ROW0 + i as u32);
                        row_bg(cv, pal, rr, hov, d.connected);
                        if d.connected { cv.fill_rrect(rect(rr.x + 5.0, rr.y + 9.0, 3.0, rr.h - 18.0), r_all(1.5), pal.accent_hi); }
                        cv.text(d.glyph(), &TextStyle::icon(18.0, if d.connected { pal.accent_hi } else { pal.fg }), pt(rr.x + 22.0, rr.y + 23.0), HAlign::Left, VAlign::Middle);
                        let right = if d.connected { 96.0 } else { 20.0 };
                        cv.text_in(&d.name, &TextStyle::new(pal.fs(13.0), pal.fg_hi).weight(600), rect(rr.x + 50.0, rr.y + 5.0, rr.w - 50.0 - right - if d.battery.is_some() && d.connected { 40.0 } else { 0.0 }, 18.0), HAlign::Left);
                        let meta = d.audio_status(snap.audio_available).unwrap_or_else(|| if d.connected { "Connected".into() } else if d.paired { "Paired · tap to connect".into() } else { "Tap to pair and connect".into() });
                        cv.text_in(&meta, &pal.text_dim(10.0), rect(rr.x + 50.0, rr.y + 24.0, rr.w - 50.0 - right, 16.0), HAlign::Left);
                        if d.connected {
                            let db = rect(rr.right() - 84.0, rr.y + 11.0, 74.0, 24.0);
                            let hd = self.hits.add_in(db, list, ID_DISC0 + i as u32);
                            button(cv, pal, db, "Disconnect", ButtonKind::Plain, hd, false);
                            if let Some(b) = d.battery {
                                cv.text(&format!("{b}%"), &TextStyle::new(pal.fs(10.0), pal.accent_hi).bold(), pt(db.x - 8.0, rr.y + 23.0), HAlign::Right, VAlign::Middle);
                            }
                        }
                    }
                }
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
    }
    fn event(&mut self, ev: &Event, r: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, r.h - 44.0); return; }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_TOGGLE => net::bt_power(ctx.poster, !ctx.state.net.bt.on),
            ID_SCAN => net::bt_scan(ctx.poster),
            id if (ID_DISC0..ID_DISC0 + 400).contains(&id) => {
                let Some(d) = ctx.state.net.bt.devs.get((id - ID_DISC0) as usize).cloned() else { return };
                if d.connected { net::bt_connect(ctx.poster, &d.mac, &d.name, false); }
            }
            id if (ID_ROW0..ID_DISC0).contains(&id) => {
                let Some(d) = ctx.state.net.bt.devs.get((id - ID_ROW0) as usize).cloned() else { return };
                if d.connected && d.audio_action().is_some() && !d.active { net::bt_use_audio(ctx.poster, &d.mac, &d.name); }
                else if !d.connected { net::bt_connect(ctx.poster, &d.mac, &d.name, true); }
            }
            _ => {}
        }
    }
}

// ---- Volume

/// One horizontal slice of the scrolling list below the master controls.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Row { Header(&'static str), Msg(&'static str), App(usize), Out(usize) }

impl Row {
    fn h(self) -> f32 {
        match self { Row::Header(_) => 24.0, Row::Msg(_) => 30.0, Row::App(_) => APP_ROW_H, Row::Out(_) => 44.0 }
    }
}

const APP_ROW_H: f32 = 64.0;
/// Master controls occupy `r.y + 42 .. r.y + 70`; the list starts below them (and below the error line).
const LIST_TOP: f32 = 84.0;
const ERROR_H: f32 = 20.0;
const ID_SLIDER: u32 = 5;
const ID_MUTE: u32 = 6;
/// Hit IDs of per-application controls carry the sink-input ID in the low 30 bits; outputs use `ID_ROW0 + index`.
const TAG_MASK: u32 = 0xC000_0000;
const TAG_MUTE: u32 = 0x4000_0000;
const TAG_SLIDER: u32 = 0x8000_0000;
const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(800);

fn stream_hit(tag: u32, id: u64) -> u32 { tag | id as u32 }
fn stream_of(hit: u32) -> Option<(u32, u64)> {
    let tag = hit & TAG_MASK;
    (tag == TAG_MUTE || tag == TAG_SLIDER).then_some((tag, (hit & !TAG_MASK) as u64))
}

/// Rows of the list: Applications (or why there are none), then the separate output-device list.
fn volume_rows(audio: Option<&Value>) -> Vec<Row> {
    let Some(a) = audio else { return vec![Row::Msg("Loading…")] };
    if a["available"].as_bool() != Some(true) { return vec![Row::Msg("Audio unavailable")]; }
    let mut rows = vec![Row::Header("Applications")];
    if a["streams_available"].as_bool() != Some(true) {
        rows.push(Row::Msg("Applications unavailable"));
    } else {
        let n = crate::audio::playing_streams(a).count();
        if n == 0 { rows.push(Row::Msg("No applications playing")); } else { rows.extend((0..n).map(Row::App)); }
    }
    let outs = a["outputs"].as_array().map_or(0, Vec::len);
    if outs > 0 {
        rows.push(Row::Header("Output devices"));
        rows.extend((0..outs).map(Row::Out));
    }
    rows
}

fn stream_present(audio: Option<&Value>, id: u64) -> bool {
    audio.is_some_and(|a| crate::audio::playing_streams(a).any(|s| s["id"].as_u64() == Some(id)))
}

fn next_audio_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

/// Store a refresh result unless a newer one is already shown. Always ends the in-flight poll.
fn apply_audio(st: &mut State, seq: u64, v: Value) {
    st.popup.audio_busy = false;
    if seq < st.popup.audio_seq { return; }
    st.popup.audio_seq = seq;
    st.popup.audio = Some(v);
}

/// Read the mixer on a worker. `poll` keeps the visible page refreshing every 0.8 s: the follow-up post redraws it
/// and the next tick polls again (nothing wakes a page that is no longer visible).
fn refresh_audio(poster: &Poster, clear_error: bool, poll: bool) {
    let (p, seq) = (poster.clone(), next_audio_seq());
    std::thread::spawn(move || {
        let v = crate::audio::gather_volume();
        p.post(move |st, _| {
            apply_audio(st, seq, v);
            if clear_error { st.popup.audio_error = None; }
        });
        if poll {
            std::thread::sleep(POLL_EVERY);
            p.post(|_, _| {});
        }
    });
}

/// Error to show for a failed mixer command. A failure for a stream that a successful re-list proves gone is stale
/// UI, not an error; if the re-list failed too, the real error stays.
fn command_error(result: Result<(), String>, stream: Option<u64>, listing: &Value) -> Option<String> {
    let e = result.err()?;
    let relisted = listing["available"].as_bool() == Some(true) && listing["streams_available"].as_bool() == Some(true);
    let gone = stream.is_some_and(|id| relisted && !listing["streams"].as_array().is_some_and(|s| s.iter().any(|x| x["id"].as_u64() == Some(id))));
    if gone { None } else { Some(e) }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MixerCmd { StreamVolume(u64, u8), StreamMute(u64) }

/// Run a mixer command and re-list afterwards, both on a worker.
fn run_mixer(ctx: &mut Ctx, work: impl FnOnce() -> Result<(), String> + Send + 'static, stream: Option<u64>) {
    ctx.state.popup.audio_cmds += 1;
    let p = ctx.poster.clone();
    std::thread::spawn(move || {
        let r = work();
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        let err = command_error(r, stream, &v);
        p.post(move |st, _| {
            st.popup.audio_cmds = st.popup.audio_cmds.saturating_sub(1);
            apply_audio(st, seq, v);
            st.popup.audio_error = err;
        });
    });
}

fn stream_command(ctx: &mut Ctx, cmd: MixerCmd) {
    match cmd {
        MixerCmd::StreamVolume(id, pct) => run_mixer(ctx, move || crate::audio::set_stream_volume(id, pct), Some(id)),
        MixerCmd::StreamMute(id) => run_mixer(ctx, move || crate::audio::toggle_stream_mute(id), Some(id)),
    }
}

/// What the pointer is currently dragging. Stream drags are frozen to the sink-input ID and track rectangle at press.
#[derive(Clone, Copy, Debug)]
enum Drag { Master(f32), Stream { id: u64, track: Rect, pct: f32 } }

#[derive(Default)]
pub struct VolumeBody {
    hits: Hits,
    scroll: Scroll,
    drag: Option<Drag>,
    last_poll: Option<std::time::Instant>,
    /// Master controls were drawn enabled in the last frame.
    master_ok: bool,
    /// Slider track of every stream row drawn last frame.
    tracks: Vec<(u64, Rect)>,
}

impl VolumeBody {
    fn list_top(st: &State) -> f32 { LIST_TOP + if st.popup.audio_error.is_some() { ERROR_H } else { 0.0 } }
}

impl Body for VolumeBody {
    fn opened(&mut self, poster: &Poster, _: &State) {
        self.drag = None;
        self.last_poll = Some(std::time::Instant::now());
        refresh_audio(poster, true, true);
    }
    fn tick(&mut self, _dt: f32, ctx: &mut Ctx) -> bool {
        if let Some(Drag::Stream { id, .. }) = self.drag {
            if !stream_present(ctx.state.popup.audio.as_ref(), id) { self.drag = None; }
        }
        let due = self.last_poll.is_none_or(|t| t.elapsed() >= POLL_EVERY);
        let p = &mut ctx.state.popup;
        if due && self.drag.is_none() && p.audio_cmds == 0 && !p.audio_busy {
            self.last_poll = Some(std::time::Instant::now());
            p.audio_busy = true;
            refresh_audio(ctx.poster, false, true);
        }
        false
    }
    fn natural_h(&mut self, _w: f32, st: &State, _: &mut TextSys) -> f32 {
        let rows: f32 = volume_rows(st.popup.audio.as_ref()).iter().map(|r| r.h()).sum();
        (Self::list_top(st) + rows).min(MAX_BODY_H)
    }
    fn cursor(&self) -> Cursor { self.hits.cursor() }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.hits.clear();
        self.tracks.clear();
        let audio = st.popup.audio.as_ref();
        let vol = &st.status.volume;
        // unknown master values disable the controls instead of reading as 0 %
        let server_ok = audio.is_none_or(|a| a["available"].as_bool() == Some(true) && a["master"]["vol"].is_number());
        self.master_ok = vol.available && server_ok;
        let master_pct = match self.drag { Some(Drag::Master(p)) => Some(p), _ => self.master_ok.then_some(vol.percent as f32) };
        header(cv, pal, r, "Volume", if self.master_ok && vol.muted { "Muted" } else { "" });
        let label = master_pct.map_or("—".to_string(), |p| format!("{p:.0}%"));
        cv.text(&label, &TextStyle::new(pal.fs(14.0), pal.fg_hi).bold(), pt(r.right(), r.y + 2.0), HAlign::Right, VAlign::Top);
        let pct = master_pct.unwrap_or(0.0);
        let m = rect(r.x, r.y + 42.0, 30.0, 28.0);
        let s = rect(r.x + 42.0, r.y + 42.0, r.w - 48.0, 28.0);
        cv.with_alpha(if self.master_ok { 1.0 } else { 0.4 }, |cv| {
            let hm = self.master_ok && self.hits.add(m, ID_MUTE);
            cv.fill_circle(m.center(), 14.0, with_a(pal.fg, if hm { 0.2 } else { 0.1 }));
            cv.text(if vol.muted || pct == 0.0 { "\u{f6a9}" } else { "\u{f028}" }, &TextStyle::icon(13.0, pal.fg_hi), m.center(), HAlign::Center, VAlign::Middle);
            if self.master_ok { self.hits.add_cursor(s, ID_SLIDER, Cursor::Pointer); }
            slider(cv, pal, s, pct / 100.0, if vol.muted { pal.dim } else { pal.accent });
        });
        if let Some(e) = &st.popup.audio_error {
            cv.text_in(e, &TextStyle::new(pal.fs(11.0), pal.red), rect(r.x, r.y + 74.0, r.w, 16.0), HAlign::Left);
        }

        let top = Self::list_top(st);
        let list = rect(r.x, r.y + top, r.w, (r.h - top).max(0.0));
        let rows = volume_rows(audio);
        self.scroll.content_h = rows.iter().map(|r| r.h()).sum();
        self.scroll.clamp(list.h);
        let apps: Vec<&Value> = audio.map(|a| crate::audio::playing_streams(a).collect()).unwrap_or_default();
        let outs: &[Value] = audio.and_then(|a| a["outputs"].as_array()).map_or(&[], Vec::as_slice);
        cv.with_clip(list, [0.0; 4], |cv| {
            let mut y = list.y - self.scroll.offset;
            for row in &rows {
                let rr = rect(list.x, y, list.w, row.h() - 6.0);
                y += row.h();
                if rr.bottom() < list.y || rr.y > list.bottom() { continue; }
                match *row {
                    Row::Header(t) => { cv.text(t, &pal.text_dim(11.0), pt(rr.x + 2.0, rr.y + 12.0), HAlign::Left, VAlign::Middle); }
                    Row::Msg(t) => { cv.text_in(t, &pal.text_dim(12.0), rect(rr.x, rr.y, rr.w, 24.0), HAlign::Center); }
                    Row::App(i) => {
                        let Some(s) = apps.get(i) else { continue };
                        let Some(id) = s["id"].as_u64() else { continue };
                        self.draw_app(cv, pal, s, id, rr, list);
                    }
                    Row::Out(i) => {
                        let Some(o) = outs.get(i) else { continue };
                        let hov = self.hits.add_in(rr, list, ID_ROW0 + i as u32);
                        let def = o["default"].as_bool().unwrap_or(false);
                        row_bg(cv, pal, rr, hov, def);
                        cv.text("\u{f025}", &TextStyle::icon(14.0, if def { pal.accent_hi } else { pal.fg }), pt(rr.x + 12.0, rr.y + 19.0), HAlign::Left, VAlign::Middle);
                        cv.text_in(o["desc"].as_str().unwrap_or(""), &TextStyle::new(pal.fs(12.0), pal.fg_hi).weight(600), rect(rr.x + 36.0, rr.y, rr.w - 48.0, rr.h), HAlign::Left);
                    }
                }
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
    }
    fn event(&mut self, ev: &Event, r: Rect, ctx: &mut Ctx) {
        let top = Self::list_top(ctx.state);
        let slider_rect = rect(r.x + 42.0, r.y + 42.0, r.w - 48.0, 28.0);
        let frac = |track: Rect, x: f32| ((x - track.x) / track.w).clamp(0.0, 1.0);
        match ev {
            Event::Scroll { dy, pos, .. } if self.master_ok && slider_rect.contains(*pos) => { crate::sysinfo::volume_step(ctx.poster, if *dy < 0.0 { 5 } else { -5 }); return; }
            Event::Scroll { dy, .. } => { self.scroll.by(*dy, r.h - top); return; }
            Event::Press(Button::Left, p) if self.master_ok && slider_rect.contains(*p) => { self.drag = Some(Drag::Master(frac(slider_rect, p.x) * 100.0)); }
            Event::Press(Button::Left, p) => {
                if let Some((TAG_SLIDER, id)) = self.hits.at(*p).and_then(stream_of) {
                    if let Some(track) = self.tracks.iter().find(|(i, _)| *i == id).map(|(_, t)| *t) {
                        self.drag = Some(Drag::Stream { id, track, pct: frac(track, p.x) * 100.0 });
                    }
                }
            }
            Event::PointerMove(p) => match &mut self.drag {
                Some(Drag::Master(v)) => *v = frac(slider_rect, p.x) * 100.0,
                Some(Drag::Stream { track, pct, .. }) => *pct = frac(*track, p.x) * 100.0,
                None => {}
            },
            Event::Release(Button::Left, p) if self.drag.is_some() => {
                match self.drag.take() {
                    Some(Drag::Master(_)) => crate::sysinfo::set_volume(ctx.poster, (frac(slider_rect, p.x) * 100.0).round() as u8),
                    Some(Drag::Stream { id, track, .. }) => {
                        // a stream that disappeared mid-drag cancels the drag; nothing is retargeted
                        if stream_present(ctx.state.popup.audio.as_ref(), id) {
                            stream_command(ctx, MixerCmd::StreamVolume(id, (frac(track, p.x) * 100.0).round() as u8));
                        }
                    }
                    None => {}
                }
                self.hits.event(ev);
                return;
            }
            _ => {}
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        if let Some((TAG_MUTE, id)) = stream_of(c.id) {
            if stream_present(ctx.state.popup.audio.as_ref(), id) { stream_command(ctx, MixerCmd::StreamMute(id)); }
            return;
        }
        match c.id {
            ID_MUTE if self.master_ok => crate::sysinfo::toggle_mute(ctx.poster),
            id if (ID_ROW0..TAG_MUTE).contains(&id) => {
                let name = ctx.state.popup.audio.as_ref().and_then(|a| a["outputs"].get((id - ID_ROW0) as usize)).and_then(|o| o["name"].as_str()).map(str::to_string);
                if let Some(name) = name {
                    run_mixer(ctx, move || crate::audio::select_audio_output(&name), None);
                }
            }
            _ => {}
        }
    }
}

impl VolumeBody {
    /// One application row: name, percentage, mute toggle and 0-100 % slider, all addressed by sink-input `id`.
    fn draw_app(&mut self, cv: &mut Canvas, pal: &Palette, s: &Value, id: u64, rr: Rect, list: Rect) {
        row_bg(cv, pal, rr, false, false);
        let vol = s["vol"].as_f64();
        let muted = s["mute"].as_bool();
        let dragged = match self.drag { Some(Drag::Stream { id: d, pct, .. }) if d == id => Some(pct), _ => None };
        let shown = dragged.or(vol.map(|v| v as f32));
        let name = s["name"].as_str().unwrap_or("Unknown");
        cv.text_in(name, &TextStyle::new(pal.fs(12.0), pal.fg_hi).weight(600), rect(rr.x + 12.0, rr.y + 4.0, rr.w - 80.0, 20.0), HAlign::Left);
        // externally set overdrive stays visible in the label
        let label = shown.map_or("—".to_string(), |p| format!("{:.0}%", p));
        cv.text(&label, &TextStyle::new(pal.fs(11.0), if muted == Some(true) { pal.dim } else { pal.accent_hi }).bold(), pt(rr.right() - 12.0, rr.y + 14.0), HAlign::Right, VAlign::Middle);
        let m = rect(rr.x + 10.0, rr.y + 28.0, 26.0, 24.0);
        let track = rect(rr.x + 46.0, rr.y + 28.0, rr.w - 60.0, 24.0);
        let off = muted == Some(true);
        cv.with_alpha(if muted.is_some() { 1.0 } else { 0.4 }, |cv| {
            let hm = muted.is_some() && self.hits.add_in(m, list, stream_hit(TAG_MUTE, id));
            cv.fill_circle(m.center(), 12.0, with_a(pal.fg, if hm { 0.2 } else { 0.1 }));
            cv.text(if off { "\u{f6a9}" } else { "\u{f028}" }, &TextStyle::icon(12.0, pal.fg_hi), m.center(), HAlign::Center, VAlign::Middle);
        });
        cv.with_alpha(if shown.is_some() { 1.0 } else { 0.4 }, |cv| {
            if shown.is_some() {
                self.hits.add_in(track, list, stream_hit(TAG_SLIDER, id));
                self.tracks.push((id, track));
            }
            slider(cv, pal, track, shown.unwrap_or(0.0).min(100.0) / 100.0, if off { pal.dim } else { pal.accent });
        });
    }
}

// ---- Battery
#[derive(Default)]
pub struct BatteryBody {
    hits: Hits,
    /// Highlight position in button-index units; eases toward `target` so a mode switch slides.
    pos: Option<f32>,
    /// Index the highlight is heading for: the clicked mode at once, then the reported active one.
    target: Option<usize>,
}

/// Exponential ease: ~95 % of the way in 170 ms, independent of frame rate.
fn ease(pos: f32, target: f32, dt: f32) -> f32 {
    let d = target - pos;
    if d.abs() < 0.002 { target } else { pos + d * (1.0 - (-dt * 18.0).exp()) }
}

const ID_PROFILE0: u32 = 100;
const PROFILE_H: f32 = 70.0;

/// Read the power profiles on a worker and apply them (optionally switching first).
fn profile_work(poster: &Poster, set: Option<String>) {
    let p = poster.clone();
    std::thread::spawn(move || {
        let err = set.and_then(|n| crate::power::set_profile(&n).err());
        let now = crate::power::profiles();
        p.post(move |st, _| {
            st.popup.profile_busy = false;
            st.popup.profile_error = err;
            st.popup.profiles = now;
        });
    });
}

pub fn fmt_duration(secs: u64) -> String { format!("{} h {} min", secs / 3600, secs % 3600 / 60) }

fn state_text(s: BatteryState) -> &'static str {
    match s { BatteryState::Charging => "Charging", BatteryState::Discharging => "On battery", BatteryState::Full => "Fully charged", BatteryState::NotCharging => "Not charging", BatteryState::Unknown => "Unknown" }
}

pub fn battery_lines(p: &PowerSnapshot) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let Some(b) = p.batteries.first() else { return v };
    v.push(("Status".into(), state_text(b.state).into()));
    if let Some(a) = p.ac_online { v.push(("Power source".into(), if a { "AC adapter".into() } else { "Battery".into() })); }
    if let Some(w) = b.power_w { v.push(("Power".into(), format!("{w:.1} W"))); }
    if let Some(s) = b.time_to_empty_secs.filter(|_| b.state == BatteryState::Discharging) { v.push(("Time left".into(), fmt_duration(s))); }
    if let Some(s) = b.time_to_full_secs.filter(|_| b.state == BatteryState::Charging) { v.push(("Until full".into(), fmt_duration(s))); }
    if let (Some(n), Some(f)) = (b.energy_now_wh, b.energy_full_wh) { v.push(("Energy".into(), format!("{n:.1} / {f:.1} Wh"))); }
    if let Some(h) = b.health_pct { v.push(("Health".into(), format!("{h:.0}%"))); }
    if let Some(c) = b.cycles { v.push(("Cycles".into(), c.to_string())); }
    if let Some(t) = &b.technology { v.push(("Technology".into(), t.clone())); }
    v
}

impl BatteryBody {
    fn lines_h(st: &State) -> f32 { st.status.power.as_ref().map_or(0, |p| battery_lines(p).len()) as f32 * 24.0 }
    fn has_battery(st: &State) -> bool { st.status.power.as_ref().is_some_and(|p| p.available && !p.batteries.is_empty()) }
    fn profile_top(st: &State) -> f32 { if Self::has_battery(st) { 70.0 + Self::lines_h(st) + 8.0 } else { 34.0 } }
}

impl Body for BatteryBody {
    fn opened(&mut self, poster: &Poster, _: &State) { profile_work(poster, None); }
    fn natural_h(&mut self, _w: f32, st: &State, _: &mut TextSys) -> f32 {
        Self::profile_top(st) + if st.popup.profiles.is_some() { PROFILE_H } else { 0.0 }
    }
    fn cursor(&self) -> Cursor { self.hits.cursor() }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect) {
        let pal = &dc.state.pal;
        self.hits.clear();
        let top = Self::profile_top(dc.state);
        if let Some(p) = dc.state.status.power.as_ref().filter(|_| Self::has_battery(dc.state)) {
            let pct = p.percentage.unwrap_or(0.0);
            let charging = crate::power::actively_charging(p);
            let col = if charging { pal.accent_hi } else if pct <= 15.0 { pal.red } else if pct <= 25.0 { pal.yellow } else { pal.fg_hi };
            cv.text(&format!("{pct:.0}%"), &TextStyle::new(pal.fs(30.0), col).bold(), pt(r.x, r.y), HAlign::Left, VAlign::Top);
            cv.text(if charging { "\u{f0e7}" } else { "\u{f240}" }, &TextStyle::icon(24.0, col), pt(r.right(), r.y + 18.0), HAlign::Right, VAlign::Middle);
            let bar = rect(r.x, r.y + 52.0, r.w, 6.0);
            cv.fill_rrect(bar, r_all(3.0), pal.active);
            cv.fill_rrect(rect(bar.x, bar.y, bar.w * (pct as f32 / 100.0).clamp(0.0, 1.0), bar.h), r_all(3.0), col);
            for (i, (k, v)) in battery_lines(p).iter().enumerate() {
                let y = r.y + 70.0 + i as f32 * 24.0;
                cv.text(k, &pal.text_dim(12.0), pt(r.x, y), HAlign::Left, VAlign::Top);
                cv.text(v, &pal.text(12.0), pt(r.right(), y), HAlign::Right, VAlign::Top);
            }
        } else {
            header(cv, pal, r, "Battery", "No battery detected");
        }
        let Some(pr) = dc.state.popup.profiles.as_ref() else { return };
        let y = r.y + top;
        cv.text("Power mode", &pal.text_dim(11.0), pt(r.x + 2.0, y + 6.0), HAlign::Left, VAlign::Middle);
        if let Some(e) = &dc.state.popup.profile_error {
            cv.text_in(e, &TextStyle::new(pal.fs(11.0), pal.red), rect(r.x + 90.0, y, r.w - 90.0, 14.0), HAlign::Right);
        }
        let n = pr.available.len().max(1) as f32;
        let gap = 8.0;
        let w = (r.w - gap * (n - 1.0)) / n;
        let busy = dc.state.popup.profile_busy;
        let active = pr.available.iter().position(|p| *p == pr.active);
        let pos = self.pos.or(active.map(|i| i as f32)).unwrap_or(0.0);
        let btn = |i: f32| rect(r.x + i * (w + gap), y + 18.0, w, 32.0);
        for (i, _) in pr.available.iter().enumerate() {
            let b = btn(i as f32);
            let hov = !busy && self.hits.add(b, ID_PROFILE0 + i as u32);
            cv.fill_rrect(b, r_all(16.0), if hov { pal.hover } else { pal.surface });
        }
        if active.is_some() || self.target.is_some() {
            let h = btn(pos);
            cv.fill_rrect(h, r_all(16.0), pal.accent);
        }
        let on_text = if pal.is_light { crate::render::rgb(255, 255, 255) } else { pal.bg };
        for (i, name) in pr.available.iter().enumerate() {
            // text turns to the on-accent colour while the highlight is under it
            let under = (1.0 - (pos - i as f32).abs()).clamp(0.0, 1.0);
            let col = crate::render::mix(pal.fg, on_text, under);
            cv.text_in(&crate::power::profile_label(name), &TextStyle::new(pal.fs(12.0), col).weight(600), btn(i as f32), HAlign::Center);
        }
    }
    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        let pr = ctx.state.popup.profiles.as_ref();
        let reported = pr.and_then(|p| p.available.iter().position(|a| *a == p.active));
        // an error or a settled switch drops the optimistic target back to what the system reports
        if !ctx.state.popup.profile_busy { self.target = reported; }
        let Some(t) = self.target.map(|t| t as f32) else { return false };
        let cur = self.pos.unwrap_or(t);
        let next = ease(cur, t, dt);
        self.pos = Some(next);
        next != t
    }
    fn event(&mut self, ev: &Event, _: Rect, ctx: &mut Ctx) {
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left || ctx.state.popup.profile_busy { return; }
        let Some(i) = c.id.checked_sub(ID_PROFILE0) else { return };
        let Some(name) = ctx.state.popup.profiles.as_ref().and_then(|p| p.available.get(i as usize)).cloned() else { return };
        ctx.state.popup.profile_busy = true;
        self.target = Some(i as usize);
        profile_work(ctx.poster, Some(name));
    }
}

// ---------------------------------------------------------------- notch sub-pages

/// Hosts a popup body as a notch page (`wifi`, `bt`, `audio`, `battery`) with a header back button.
pub struct BodyPage { body: Box<dyn Body>, hits: Hits }

impl BodyPage {
    pub fn new(kind: &str) -> Option<BodyPage> { body_for(kind).map(|body| BodyPage { body, hits: Hits::default() }) }
}

impl Page for BodyPage {
    fn natural(&mut self, avail: Size, st: &State, ts: &mut TextSys) -> Size {
        Size { w: avail.w, h: self.body.natural_h(avail.w, st, ts) + 40.0 }
    }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let pal = &dc.state.pal;
        self.hits.clear();
        let back = rect(area.x, area.y, 60.0, 26.0);
        let hb = self.hits.add(back, 1);
        button(cv, pal, back, "\u{f053} Back", ButtonKind::Plain, hb, false);
        self.body.draw(cv, dc, rect(area.x, area.y + 40.0, area.w, area.h - 40.0));
    }
    fn event(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx) {
        if let Some(c) = self.hits.event(ev) { if c.id == 1 { ctx.state.nav = Some(NavTo::Page("controls")); return; } }
        self.body.event(ev, rect(area.x, area.y + 40.0, area.w, area.h - 40.0), ctx);
    }
    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool { self.body.tick(dt, ctx) }
    fn shown(&mut self, ctx: &mut Ctx) { self.body.opened(ctx.poster, ctx.state); }
    fn wants_keyboard(&self) -> bool { self.body.wants_keyboard() }
    fn cursor(&self) -> Cursor { let c = self.hits.cursor(); if c != Cursor::Default { c } else { self.body.cursor() } }
}

// ---------------------------------------------------------------- control fifo

/// Notch pages a fifo request may open (`page <id>`).
const FIFO_PAGES: [&str; 13] = ["home", "media", "calendar", "clipboard", "ai", "controls", "weather", "settings", "wifi", "bt", "audio", "battery", "display"];

/// Page a fifo line asks for: `volume|audio`, `bluetooth|bt`, `wifi`, `battery`, `calendar`, `settings`, `display`
/// or `page <id>`. Anything else is ignored.
fn fifo_page(kind: &str, arg: &str) -> Option<&'static str> {
    let id = match kind {
        "volume" | "audio" => "audio",
        "bluetooth" | "bt" => "bt",
        "wifi" | "battery" | "calendar" | "settings" | "display" => kind,
        "page" => arg,
        _ => return None,
    };
    FIFO_PAGES.into_iter().find(|p| *p == id)
}

/// `waybar-popupctl <kind>` (volume|wifi|bluetooth|battery|calendar|settings|display open the matching notch page;
/// osd-volume|osd-brightness; `page <id>` for any notch page; reload-theme) writes `"<kind> <nanos>"` lines to
/// `$XDG_RUNTIME_DIR/waybar-popupd.fifo`.
pub fn start_fifo(poster: &Poster) {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") else { return };
    let path = std::path::Path::new(&rt).join("waybar-popupd.fifo");
    if let Ok(m) = std::fs::symlink_metadata(&path) { if m.file_type().is_fifo() { let _ = std::fs::remove_file(&path); } else { return; } }
    if nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600)).is_err() { return; }
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let poster = poster.clone();
    std::thread::spawn(move || loop {
        // opening read+write keeps the fifo alive between writers
        let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { return };
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let mut words = line.split_whitespace();
            let kind = words.next().unwrap_or("").to_string();
            let arg = words.next().unwrap_or("").to_string();
            let p = poster.clone();
            match kind.as_str() {
                "osd-volume" => crate::osd::request(&p, "volume"),
                "osd-brightness" => crate::osd::request(&p, "brightness"),
                "reload-theme" => poster.post(|st, _| {
                    crate::theme::reload();
                    let (s, _) = crate::config::load_settings();
                    st.settings = s;
                    st.refresh_palette();
                }),
                k => {
                    if let Some(id) = fifo_page(k, &arg) { poster.post(move |st, _| { st.nav = Some(NavTo::Page(id)); }); }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Point;
    use crate::audio::stub::Pactl;
    use crate::net::{WifiNet, WifiSnap};
    use crate::state::Volume;
    use crate::testkit::{Pump, Rig};
    use serde_json::json;
    use tiny_skia::Pixmap;

    #[test]
    fn profile_highlight_slides_monotonically_and_settles() {
        let (mut p, mut last) = (0.0f32, 0.0f32);
        let mut frames = 0;
        while p != 2.0 && frames < 120 { p = ease(p, 2.0, 0.016); assert!(p >= last && p <= 2.0); last = p; frames += 1; }
        assert_eq!(p, 2.0);
        assert!(frames > 4 && frames < 60, "slide takes a visible but short time: {frames} frames");
    }

    #[test]
    fn bluetooth_rows_group_connected_paired_and_nearby() {
        let d = |connected, paired| net::BtDev { connected, paired, ..Default::default() };
        let devs = [d(true, true), d(true, false), d(false, true), d(false, false), d(false, false)];
        assert_eq!(bt_rows(&devs), [BtRow::Header("Connected", 2), BtRow::Dev(0), BtRow::Dev(1), BtRow::Header("Paired", 1), BtRow::Dev(2), BtRow::Header("Nearby", 2), BtRow::Dev(3), BtRow::Dev(4)]);
        assert!(bt_rows(&[]).is_empty());
    }

    #[test]
    fn duration_format() { assert_eq!(fmt_duration(3 * 3600 + 25 * 60 + 9), "3 h 25 min"); }

    #[test]
    fn fifo_aliases_select_the_same_notch_pages_as_the_pill() {
        assert_eq!(fifo_page("volume", ""), Some("audio"));
        assert_eq!(fifo_page("audio", ""), Some("audio"));
        assert_eq!(fifo_page("bluetooth", ""), Some("bt"));
        assert_eq!(fifo_page("bt", ""), Some("bt"));
        for k in ["wifi", "battery", "calendar", "settings", "display"] { assert_eq!(fifo_page(k, ""), Some(k)); }
        assert_eq!(fifo_page("page", "weather"), Some("weather"));
        assert_eq!(fifo_page("page", "audio"), Some("audio"));
        assert_eq!(fifo_page("page", "bogus"), None);
        assert_eq!(fifo_page("bogus", "audio"), None);
        assert_eq!(fifo_page("", ""), None);
        // every page the fifo can request exists in the notch
        let built: Vec<&str> = crate::pages::build().iter().map(|(id, _)| *id).collect();
        for id in ["wifi", "bt", "audio", "battery", "display", "calendar", "settings"] { assert!(built.contains(&id), "{id}"); }
    }

    #[test]
    fn body_kinds_cover_every_control_page() {
        for k in ["wifi", "bluetooth", "bt", "volume", "audio", "battery", "display"] { assert!(body_for(k).is_some(), "{k}"); }
        assert!(body_for("bogus").is_none());
    }

    #[test]
    fn wifi_body_renders_networks_in_a_notch_page() {
        let mut rig = Rig::new();
        rig.st.net.wifi = WifiSnap { on: true, available: true, nets: vec![
            WifiNet { ssid: "Home".into(), signal: 80, secure: true, active: true, saved: true },
            WifiNet { ssid: "Cafe".into(), signal: 40, secure: true, active: false, saved: false },
        ] };
        let mut page = BodyPage::new("wifi").unwrap();
        let mut pm = Pixmap::new(480, 240).unwrap();
        let mut cmds = Vec::new();
        {
            let mut cv = Canvas::new(&mut pm, &mut rig.text, 1.0);
            let mut dc = DrawCtx { state: &rig.st, cmds: &mut cmds, poster: &rig.poster, id: 1, output: None };
            page.draw(&mut cv, &mut dc, rect(0.0, 0.0, 480.0, 240.0));
        }
        crate::testkit::save_over_backdrop(&pm, 10, "/tmp/popup_wifi.png");
        assert!(pm.pixels().iter().any(|p| p.alpha() > 0));
    }

    // ---- volume body: per-application controls

    const AREA_W: f32 = 320.0;

    struct Scene { rig: Rig, pump: Pump, body: VolumeBody, area: Rect, _pactl: Pactl }

    impl Scene {
        fn new(inputs: &[(u64, &str, u32, bool, bool)]) -> Scene {
            let pactl = Pactl::install();
            pactl.set_inputs(inputs);
            let (mut rig, pump) = Rig::pumped();
            rig.st.status.volume = Volume { available: true, percent: 50, muted: false };
            Scene { rig, pump, body: VolumeBody::default(), area: rect(0.0, 0.0, AREA_W, MAX_BODY_H), _pactl: pactl }
        }
        fn pactl(&self) -> &Pactl { &self._pactl }
        fn with_ctx<R>(&mut self, f: impl FnOnce(&mut VolumeBody, &mut Ctx) -> R) -> R {
            let mut cmds = Vec::new();
            let Scene { rig, body, .. } = self;
            let mut ctx = Ctx { state: &mut rig.st, cmds: &mut cmds, text: &mut rig.text, poster: &rig.poster, id: 1, output: None };
            f(body, &mut ctx)
        }
        fn pump_until(&mut self, what: &str, cond: impl Fn(&State) -> bool) { self.pump.until(&mut self.rig.st, what, cond); }
        /// Open the page and wait for its first listing.
        fn show(&mut self) {
            self.with_ctx(|b, c| b.opened(c.poster, c.state));
            self.pump_until("the first listing", |st| st.popup.audio.is_some());
            self.draw();
        }
        fn draw(&mut self) -> Pixmap {
            let mut pm = Pixmap::new(AREA_W as u32, MAX_BODY_H as u32).unwrap();
            let mut cmds = Vec::new();
            let Scene { rig, body, area, .. } = self;
            let mut cv = Canvas::new(&mut pm, &mut rig.text, 1.0);
            let mut dc = DrawCtx { state: &rig.st, cmds: &mut cmds, poster: &rig.poster, id: 1, output: None };
            body.draw(&mut cv, &mut dc, *area);
            pm
        }
        fn event(&mut self, ev: Event) {
            let area = self.area;
            self.with_ctx(|b, c| b.event(&ev, area, c));
        }
        fn click(&mut self, p: Point) {
            for ev in [Event::PointerMove(p), Event::Press(Button::Left, p), Event::Release(Button::Left, p)] { self.event(ev); }
        }
        /// A point whose topmost hit region is `id`.
        fn find(&self, id: u32) -> Option<Point> {
            let mut y = 0.0;
            while y < MAX_BODY_H {
                let mut x = 0.0;
                while x < AREA_W {
                    if self.body.hits.at(pt(x, y)) == Some(id) { return Some(pt(x, y)); }
                    x += 3.0;
                }
                y += 3.0;
            }
            None
        }
        fn mute_btn(&self, id: u64) -> Option<Point> { self.find(stream_hit(TAG_MUTE, id)) }
        fn track(&self, id: u64) -> Rect { self.body.tracks.iter().find(|(i, _)| *i == id).unwrap().1 }
        fn at_frac(&self, id: u64, f: f32) -> Point { let t = self.track(id); pt(t.x + t.w * f, t.center().y) }
        /// Wait for every mixer command to finish and its re-listing to be applied.
        fn settle(&mut self) {
            self.pump_until("mixer commands", |st| st.popup.audio_cmds == 0);
            self.draw();
        }
        fn ids(&self) -> Vec<u64> { self.body.tracks.iter().map(|(i, _)| *i).collect() }
    }

    #[test]
    fn uncorked_streams_are_listed_muted_ones_stay_and_corked_ones_do_not() {
        let mut s = Scene::new(&[(7, "Player", 40, true, false), (8, "Paused", 40, false, true), (9, "Browser", 70, false, false)]);
        s.show();
        assert_eq!(s.ids(), [7, 9]);
        assert!(s.mute_btn(7).is_some() && s.mute_btn(9).is_some(), "muted streams keep a mute button to unmute");
        assert!(s.mute_btn(8).is_none(), "corked streams are not offered");
        assert_eq!(volume_rows(s.rig.st.popup.audio.as_ref()),
            [Row::Header("Applications"), Row::App(0), Row::App(1), Row::Header("Output devices"), Row::Out(0)]);
    }

    #[test]
    fn changing_one_stream_leaves_the_master_and_other_streams_alone() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false), (8, "Browser", 60, false, false)]);
        s.show();
        let p = s.at_frac(7, 0.25);
        s.click(p);
        s.settle();
        let m = s.mute_btn(8).unwrap();
        s.click(m);
        s.settle();
        assert_eq!(s.pactl().mutations(), ["set-sink-input-volume 7 25%", "set-sink-input-mute 8 toggle"]);
        assert!(s.rig.st.popup.audio_error.is_none());
    }

    #[test]
    fn stream_volume_is_capped_at_100_percent_but_overdrive_is_still_shown() {
        let mut s = Scene::new(&[(7, "Loud", 120, false, false)]);
        s.show();
        let t = s.track(7);
        s.event(Event::PointerMove(pt(t.x + 3.0, t.center().y)));
        s.event(Event::Press(Button::Left, pt(t.x + 3.0, t.center().y)));
        s.event(Event::PointerMove(pt(t.right() + 80.0, t.center().y)));
        s.event(Event::Release(Button::Left, pt(t.right() + 80.0, t.center().y)));
        s.settle();
        assert_eq!(s.pactl().mutations(), ["set-sink-input-volume 7 100%"]);
        // the mixer's own report of an overdriven stream is shown as is, never clamped
        let doc = s.rig.st.popup.audio.clone().unwrap();
        assert_eq!(crate::audio::playing_streams(&doc).next().unwrap()["vol"], 120.0);
    }

    #[test]
    fn a_drag_stays_on_the_stream_and_track_it_started_on() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false), (9, "Browser", 70, false, false)]);
        s.show();
        let start = s.at_frac(7, 0.2);
        let frozen = s.track(7);
        s.event(Event::PointerMove(start));
        s.event(Event::Press(Button::Left, start));
        // the rows reorder under the pointer: 9 now sits first
        s.pactl().set_inputs(&[(9, "Browser", 70, false, false), (7, "Player", 40, false, false)]);
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        s.draw();
        assert_ne!(s.track(7), frozen, "the row really moved");
        let release = pt(frozen.x + frozen.w * 0.8, frozen.center().y);
        s.event(Event::PointerMove(release));
        s.event(Event::Release(Button::Left, release));
        s.settle();
        assert_eq!(s.pactl().mutations(), ["set-sink-input-volume 7 80%"], "ID 7 at the frozen rectangle, never ID 9");
    }

    #[test]
    fn a_stream_that_disappears_mid_drag_cancels_the_drag() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false), (9, "Browser", 70, false, false)]);
        s.show();
        let start = s.at_frac(7, 0.2);
        s.event(Event::PointerMove(start));
        s.event(Event::Press(Button::Left, start));
        assert!(matches!(s.body.drag, Some(Drag::Stream { id: 7, .. })));
        s.pactl().set_inputs(&[(9, "Browser", 70, false, false)]);
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        s.with_ctx(|b, c| b.tick(0.016, c));
        assert!(s.body.drag.is_none());
        s.draw();
        let p = s.at_frac(9, 0.5);
        s.event(Event::PointerMove(p));
        s.event(Event::Release(Button::Left, p));
        assert_eq!(s.rig.st.popup.audio_cmds, 0);
        assert!(s.pactl().mutations().is_empty(), "nothing is retargeted to another stream");
    }

    #[test]
    fn a_failing_command_shows_its_error_unless_the_stream_just_vanished() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        s.pactl().set("fail-set", "Failure: No such entity\n");
        let m = s.mute_btn(7).unwrap();
        s.click(m);
        s.settle();
        let e = s.rig.st.popup.audio_error.clone().expect("the real error is shown");
        assert!(e.contains("No such entity"), "{e}");
        assert!(VolumeBody::list_top(&s.rig.st) > LIST_TOP, "the error line takes room in the page");
        drop(s);

        // the stream ends before the click lands: the stale row goes away silently
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        s.pactl().set_inputs(&[]);
        s.pactl().set("fail-set", "Failure: No such entity\n");
        let m = s.mute_btn(7).unwrap();
        s.click(m);
        s.settle();
        assert!(s.rig.st.popup.audio_error.is_none());
        assert!(s.ids().is_empty());

        // if the re-listing fails too, the real error stays
        drop(s);
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        s.pactl().set("no-inputs", "");
        s.pactl().set("fail-set", "Failure: No such entity\n");
        let m = s.mute_btn(7).unwrap();
        s.click(m);
        s.settle();
        assert!(s.rig.st.popup.audio_error.as_deref().is_some_and(|e| e.contains("No such entity")));
        assert!(s.ids().is_empty(), "the stale list is cleared, not kept");
    }

    #[test]
    fn command_error_logic() {
        let listed = json!({"available": true, "streams_available": true, "streams": [{"id": 7}]});
        assert_eq!(command_error(Ok(()), Some(7), &listed), None);
        assert_eq!(command_error(Err("x".into()), Some(7), &listed).as_deref(), Some("x"));
        assert_eq!(command_error(Err("x".into()), Some(8), &listed), None, "stream 8 is gone");
        assert_eq!(command_error(Err("x".into()), None, &listed).as_deref(), Some("x"), "output switches always report");
        let broken = json!({"available": true, "streams_available": false, "streams": []});
        assert_eq!(command_error(Err("x".into()), Some(8), &broken).as_deref(), Some("x"));
    }

    #[test]
    fn unavailable_states_are_distinct_and_disable_the_controls() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        assert!(s.body.master_ok && !s.body.tracks.is_empty());
        // sink-input enumeration fails: master stays usable, the applications section says so
        s.pactl().set("no-inputs", "");
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        s.draw();
        assert_eq!(volume_rows(s.rig.st.popup.audio.as_ref())[..2], [Row::Header("Applications"), Row::Msg("Applications unavailable")]);
        assert!(s.body.tracks.is_empty() && s.body.master_ok);
        // the audio server itself is gone: everything is cleared and disabled
        s.pactl().set("no-server", "");
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        s.draw();
        assert_eq!(volume_rows(s.rig.st.popup.audio.as_ref()), [Row::Msg("Audio unavailable")]);
        assert!(!s.body.master_ok && s.body.tracks.is_empty());
        assert!(s.find(ID_SLIDER).is_none() && s.find(ID_MUTE).is_none(), "no master controls without a server");
        // empty list
        s.pactl().clear("no-server");
        s.pactl().clear("no-inputs");
        s.pactl().set_inputs(&[]);
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        assert_eq!(volume_rows(s.rig.st.popup.audio.as_ref())[..2], [Row::Header("Applications"), Row::Msg("No applications playing")]);
        assert_eq!(volume_rows(None), [Row::Msg("Loading…")]);
    }

    #[test]
    fn missing_master_or_stream_values_disable_controls_instead_of_reading_as_zero() {
        let mut s = Scene::new(&[]);
        s.pactl().set("sink-inputs.json", r#"[{"index": 4, "properties": {"application.name": "Odd"}, "volume": {}}]"#);
        s.show();
        assert!(s.body.tracks.is_empty(), "unknown volume: no slider to drag");
        assert!(s.mute_btn(4).is_none(), "unknown mute state: no toggle");
        s.pactl().set("sinks.json", r#"[{"index":1,"name":"sink0","description":"Speakers","ports":[],"volume":{}}]"#);
        let (seq, v) = (next_audio_seq(), crate::audio::gather_volume());
        apply_audio(&mut s.rig.st, seq, v);
        s.draw();
        assert!(s.find(ID_SLIDER).is_none() && s.find(ID_MUTE).is_none(), "unknown master volume: nothing to drag");
    }

    #[test]
    fn switching_the_output_device_does_not_touch_any_volume() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        let row = s.find(ID_ROW0).expect("the output row exists");
        s.click(row);
        s.settle();
        assert_eq!(s.pactl().mutations(), ["set-default-sink sink0"]);
    }

    #[test]
    fn polling_pauses_while_a_drag_or_command_is_active_and_never_overlaps() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false)]);
        s.show();
        let lists = |s: &Scene| s.pactl().log().iter().filter(|l| l.ends_with("list sinks")).count();
        let base = lists(&s);
        let start = s.at_frac(7, 0.3);
        s.event(Event::PointerMove(start));
        s.event(Event::Press(Button::Left, start));
        s.with_ctx(|b, c| { b.last_poll = None; b.tick(0.016, c); });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(lists(&s), base, "no poll while dragging");
        s.body.drag = None;
        s.rig.st.popup.audio_cmds = 1;
        s.with_ctx(|b, c| { b.last_poll = None; b.tick(0.016, c); });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(lists(&s), base, "no poll while a command runs");
        s.rig.st.popup.audio_cmds = 0;
        s.rig.st.popup.audio_busy = true;
        s.with_ctx(|b, c| { b.last_poll = None; b.tick(0.016, c); });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(lists(&s), base, "no second poll while one is in flight");
        s.rig.st.popup.audio_busy = false;
        s.with_ctx(|b, c| { b.last_poll = None; b.tick(0.016, c); });
        s.pump_until("the poll result", |st| !st.popup.audio_busy);
        assert_eq!(lists(&s), base + 1);
    }

    #[test]
    fn many_streams_scroll_inside_one_clipped_list() {
        let inputs: Vec<(u64, String)> = (100..140).map(|i| (i, format!("App {i}"))).collect();
        let refs: Vec<(u64, &str, u32, bool, bool)> = inputs.iter().map(|(i, n)| (*i, n.as_str(), 30, false, false)).collect();
        let mut s = Scene::new(&refs);
        s.show();
        let nat = s.with_ctx(|b, c| b.natural_h(AREA_W, c.state, c.text));
        assert_eq!(nat, MAX_BODY_H, "the body is bounded");
        assert!(s.body.scroll.content_h > MAX_BODY_H * 3.0);
        assert!(s.mute_btn(139).is_none(), "rows below the viewport are not hittable");
        let first = s.ids().first().copied().unwrap();
        for _ in 0..80 {
            let p = pt(AREA_W / 2.0, 200.0);
            s.event(Event::Scroll { dx: 0.0, dy: 60.0, steps: 1, pos: p });
        }
        s.draw();
        assert!(s.body.scroll.offset > 0.0);
        assert!(!s.ids().contains(&first) && s.ids().contains(&139), "{:?}", s.ids());
        let m = s.mute_btn(139).expect("the last stream is reachable after scrolling");
        s.click(m);
        s.settle();
        assert_eq!(s.pactl().mutations(), ["set-sink-input-mute 139 toggle"]);
    }

    #[test]
    fn rendering_draws_every_state_without_overflowing_the_page() {
        let mut s = Scene::new(&[(7, "Player", 40, false, false), (9, "Browser", 120, true, false)]);
        s.show();
        s.rig.st.popup.audio_error = Some("Application mute failed: boom".into());
        let pm = s.draw();
        crate::testkit::save_over_backdrop(&pm, 10, "/tmp/popup_volume_apps.png");
        assert!(pm.pixels().iter().any(|p| p.alpha() > 0));
        let nat = s.with_ctx(|b, c| b.natural_h(AREA_W, c.state, c.text));
        assert!(nat < MAX_BODY_H);
    }
}

// ---- Displays
const SCALES: [&str; 4] = ["1", "1.25", "1.6", "2"];
const TEXT_STOPS: [i64; 7] = [9, 10, 11, 12, 14, 16, 20];

fn gather_display() -> Value {
    let mons: Vec<Value> = crate::hypr::req("j/monitors all").and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_array().cloned()).unwrap_or_default().iter().map(|m| {
        let enabled = !m["disabled"].as_bool().unwrap_or(false);
        let name = m["name"].as_str().unwrap_or("");
        let brightness = if enabled { net::sh_out("omarchy-brightness-display", &["--no-osd", "--monitor", name]).parse::<i64>().ok() } else { None };
        serde_json::json!({"name": name, "w": m["width"], "h": m["height"], "scale": m["scale"], "focused": m["focused"].as_bool().unwrap_or(false), "enabled": enabled, "brightness": brightness})
    }).collect();
    let home = std::env::var("HOME").unwrap_or_default();
    let text = std::fs::read_to_string(format!("{home}/.config/omarchy/shell.toml")).ok()
        .and_then(|t| t.lines().find(|l| l.contains("base-size")).map(String::from))
        .and_then(|l| l.split('=').nth(1).map(|v| v.trim().to_string()))
        .and_then(|v| v.parse::<i64>().ok()).unwrap_or(12);
    serde_json::json!({"mons": mons, "text": text})
}

fn refresh_display(poster: &Poster) {
    let p = poster.clone();
    std::thread::spawn(move || { let v = gather_display(); p.post(move |st, _| st.popup.display = Some(v)); });
}

fn run_then_refresh(poster: &Poster, prog: String, args: Vec<String>) {
    let p = poster.clone();
    std::thread::spawn(move || {
        let _ = std::process::Command::new(&prog).args(&args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        std::thread::sleep(std::time::Duration::from_millis(400));
        let v = gather_display();
        p.post(move |st, _| st.popup.display = Some(v));
    });
}

#[derive(Default)]
pub struct DisplayBody { hits: Hits, scroll: Scroll, drag: Option<(usize, f32)>, sliders: Vec<(Rect, String)> }

#[derive(Clone)]
enum DAct { Toggle(String, bool), Scale(&'static str), Text(i64) }

impl Body for DisplayBody {
    fn opened(&mut self, poster: &Poster, _: &State) { refresh_display(poster); }
    fn natural_h(&mut self, _w: f32, st: &State, _: &mut TextSys) -> f32 {
        let n = st.popup.display.as_ref().and_then(|d| d["mons"].as_array()).map_or(0, Vec::len) as f32;
        (60.0 + n * 34.0 + n * 34.0 + 110.0).min(MAX_BODY_H)
    }
    fn cursor(&self) -> Cursor { self.hits.cursor() }
    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, r: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.hits.clear();
        self.sliders.clear();
        let Some(d) = st.popup.display.as_ref() else { header(cv, pal, r, "Displays", "Loading…"); return };
        let mons = d["mons"].as_array().cloned().unwrap_or_default();
        header(cv, pal, r, "Displays", if mons.is_empty() { "No displays available" } else { "" });
        let enabled = mons.iter().filter(|m| m["enabled"].as_bool().unwrap_or(false)).count();
        let mut y = r.y + 40.0 - self.scroll.offset;
        let top = r.y + 36.0;
        let view = rect(r.x, top, r.w, r.h - 36.0);
        let mut total = 0.0;
        cv.with_clip(view, [0.0; 4], |cv| {
            for (i, m) in mons.iter().enumerate() {
                let on = m["enabled"].as_bool().unwrap_or(false);
                let name = m["name"].as_str().unwrap_or("").to_string();
                cv.text_in(&format!("{name}  {}x{}{}", m["w"], m["h"], if m["focused"].as_bool().unwrap_or(false) { "  · focused" } else { "" }), &pal.text(12.0), rect(r.x, y, r.w - 60.0, 28.0), HAlign::Left);
                let b = rect(r.right() - 48.0, y + 2.0, 48.0, 24.0);
                let id = 300 + i as u32;
                let hov = self.hits.add_in(b, view, id);
                let can = !(on && enabled <= 1);
                cv.with_alpha(if can { 1.0 } else { 0.4 }, |cv| button(cv, pal, b, if on { "On" } else { "Off" }, if on { ButtonKind::Accent } else { ButtonKind::Plain }, hov && can, false));
                y += 34.0; total += 34.0;
            }
            for (i, m) in mons.iter().filter(|m| m["enabled"].as_bool().unwrap_or(false) && m["brightness"].is_number()).enumerate() {
                if i == 0 { cv.text("Brightness", &pal.text_dim(11.0), pt(r.x, y + 8.0), HAlign::Left, VAlign::Middle); y += 22.0; total += 22.0; }
                let name = m["name"].as_str().unwrap_or("");
                let pct = self.drag.filter(|(k, _)| *k == i).map(|(_, v)| v).unwrap_or(m["brightness"].as_f64().unwrap_or(0.0) as f32);
                cv.text_in(name, &pal.text(11.0), rect(r.x, y, 90.0, 28.0), HAlign::Left);
                let s = rect(r.x + 96.0, y, r.w - 100.0, 28.0);
                self.hits.add_in(s, view, 400 + i as u32);
                self.sliders.push((s, name.to_string()));
                slider(cv, pal, s, pct / 100.0, pal.accent);
                y += 32.0; total += 32.0;
            }
            cv.text("Scale (focused)", &pal.text_dim(11.0), pt(r.x, y + 8.0), HAlign::Left, VAlign::Middle);
            y += 22.0; total += 22.0;
            let foc_scale = mons.iter().find(|m| m["focused"].as_bool().unwrap_or(false)).and_then(|m| m["scale"].as_f64()).unwrap_or(0.0);
            let bw = (r.w - 3.0 * 6.0) / 4.0;
            for (i, sc) in SCALES.iter().enumerate() {
                let b = rect(r.x + i as f32 * (bw + 6.0), y, bw, 26.0);
                let hov = self.hits.add_in(b, view, 500 + i as u32);
                let on = (foc_scale - sc.parse::<f64>().unwrap_or(0.0)).abs() < 0.01;
                button(cv, pal, b, &format!("{sc}x"), if on { ButtonKind::Accent } else { ButtonKind::Plain }, hov, false);
            }
            y += 34.0; total += 34.0;
            cv.text("Text size", &pal.text_dim(11.0), pt(r.x, y + 8.0), HAlign::Left, VAlign::Middle);
            y += 22.0; total += 22.0;
            let cur = d["text"].as_i64().unwrap_or(12);
            let near = *TEXT_STOPS.iter().min_by_key(|s| (**s - cur).abs()).unwrap();
            let bw = (r.w - 6.0 * 6.0) / 7.0;
            for (i, t) in TEXT_STOPS.iter().enumerate() {
                let b = rect(r.x + i as f32 * (bw + 6.0), y, bw, 26.0);
                let hov = self.hits.add_in(b, view, 600 + i as u32);
                button(cv, pal, b, &t.to_string(), if *t == near { ButtonKind::Accent } else { ButtonKind::Plain }, hov, false);
            }
            total += 34.0;
        });
        self.scroll.content_h = total + 40.0;
        self.scroll.clamp(view.h);
    }
    fn event(&mut self, ev: &Event, r: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, r.h - 36.0); return; }
        let frac = |s: &Rect, x: f32| ((x - s.x) / s.w).clamp(0.0, 1.0) * 100.0;
        match ev {
            Event::Press(Button::Left, p) => {
                if let Some(i) = self.sliders.iter().position(|(s, _)| s.contains(*p)) { self.drag = Some((i, frac(&self.sliders[i].0, p.x))); }
            }
            Event::PointerMove(p) => { if let Some((i, _)) = self.drag { self.drag = Some((i, frac(&self.sliders[i].0, p.x))); } }
            Event::Release(Button::Left, p) => {
                if let Some((i, _)) = self.drag.take() {
                    let v = frac(&self.sliders[i].0, p.x).round() as i64;
                    let name = self.sliders[i].1.clone();
                    run_then_refresh(ctx.poster, "omarchy-brightness-display".into(), vec!["--no-osd".into(), "--monitor".into(), name, format!("{v}%")]);
                    self.hits.event(ev);
                    return;
                }
            }
            _ => {}
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        let Some(d) = ctx.state.popup.display.clone() else { return };
        let mons = d["mons"].as_array().cloned().unwrap_or_default();
        let act = match c.id {
            300..=399 => mons.get((c.id - 300) as usize).map(|m| DAct::Toggle(m["name"].as_str().unwrap_or("").to_string(), m["enabled"].as_bool().unwrap_or(false))),
            500..=599 => SCALES.get((c.id - 500) as usize).map(|s| DAct::Scale(s)),
            600..=699 => TEXT_STOPS.get((c.id - 600) as usize).map(|t| DAct::Text(*t)),
            _ => None,
        };
        match act {
            Some(DAct::Toggle(name, on)) => {
                let expr = if on { format!("hl.monitor({{ output = \"{name}\", disabled = true }})") } else {
                    format!("hl.monitor({{ output = \"{name}\", disabled = false, mode = \"preferred\", position = \"auto\", scale = \"auto\" }})") };
                run_then_refresh(ctx.poster, "hyprctl".into(), vec!["eval".into(), expr]);
            }
            Some(DAct::Scale(s)) => run_then_refresh(ctx.poster, "omarchy-hyprland-monitor-scaling".into(), vec![s.to_string()]),
            Some(DAct::Text(t)) => run_then_refresh(ctx.poster, "omarchy-display-text-size".into(), vec![t.to_string()]),
            None => {}
        }
    }
}
