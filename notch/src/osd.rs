//! Volume / brightness key overlay: a pill that grows out of the collapsed-notch size and shows icon, bar and
//! percentage, then folds back. Also used for the on/off, media and Wi-Fi/Bluetooth connect toasts.
//!
//! Trigger contract (unchanged from the GTK version): the key scripts change the level and then run
//! `waybar-popupctl osd-volume|osd-brightness`, which writes a line to `$XDG_RUNTIME_DIR/waybar-popupd.fifo`;
//! `popups::start_fifo` forwards it to [`request`], which reads the level on a worker thread and calls [`show`].
use crate::layout::{Rect, rect};
use crate::net::{NetEvent, NetKind, NetState};
use crate::render::{Canvas, Fill, HAlign, TextStyle, VAlign, mix, r_all, with_a};
use crate::state::State;
use crate::ui::{Palette, glass};
use crate::wayland::{Cmd, Ctx, DrawCtx, Event, KbMode, PanelSpec, Poster, View};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_SMALL: (f32, f32) = (190.0, 26.0); // matches the default collapsed notch
const BIG: (f32, f32) = (320.0, 58.0);
const WIN: (u32, u32) = (360, 80);
const HIDE_MS: u64 = 1500;
/// Critically damped spring shared with the notch (stiffness, damping).
const SPRING_K: f32 = 250.0;
const SPRING_C: f32 = 31.6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconFx { Plain, On, Pulse, Fail }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarFx { Normal, Connecting, Failed }

/// What the overlay currently shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub glyph: String,
    /// Bar fill 0..=1.
    pub frac: f32,
    pub muted: bool,
    pub text: String,
    pub icon_fx: IconFx,
    pub bar_fx: BarFx,
}

#[derive(Default)]
pub struct OsdState {
    pub toast: Option<Toast>,
    hold_until: Option<Instant>,
    /// Size of the collapsed notch (the overlay's resting size). Set by the notch whenever it changes.
    pub collapsed: Option<(f32, f32)>,
    /// Output the overlay appears on (the notch's destination).
    pub output: Option<String>,
    /// Fullscreen: nothing is shown.
    pub suppressed: bool,
    /// Last `state.net.event.seq` already turned into a toast.
    net_seen: u64,
    toggle_seen: u64,
    /// The overlay surface exists (the shell clears it when the surface closed together with its output).
    pub open: bool,
}

fn glyph_for_level(brightness: bool, level: f64, muted: bool) -> &'static str {
    if brightness { "\u{f00df}" }
    else if muted || level <= 0.0 { "\u{f075f}" }
    else if level < 34.0 { "\u{f057f}" }
    else if level < 67.0 { "\u{f0580}" }
    else { "\u{f057e}" }
}

fn short(name: &str) -> String {
    if name.chars().count() > 14 { name.chars().take(14).collect::<String>() + "…" } else { name.to_string() }
}

/// Volume (0..=150) or brightness (0..=100) toast.
pub fn level_toast(kind: &str, level: f64, muted: bool) -> Toast {
    Toast {
        glyph: glyph_for_level(kind == "brightness", level, muted).into(),
        frac: (level / 100.0).clamp(0.0, 1.0) as f32,
        muted,
        text: if muted { "mute".into() } else { format!("{}%", level.round() as i64) },
        icon_fx: IconFx::Plain,
        bar_fx: BarFx::Normal,
    }
}

pub fn toggle_toast(glyph: &str, on: bool) -> Toast {
    Toast { glyph: glyph.into(), frac: if on { 1.0 } else { 0.0 }, muted: !on, text: if on { "On" } else { "Off" }.into(), icon_fx: IconFx::Plain, bar_fx: BarFx::Normal }
}

pub fn media_toast(playing: bool, title: &str) -> Toast {
    Toast {
        glyph: if playing { "\u{f04b}" } else { "\u{f04c}" }.into(),
        frac: if playing { 1.0 } else { 0.0 },
        muted: !playing,
        text: short(title),
        icon_fx: IconFx::Plain,
        bar_fx: BarFx::Normal,
    }
}

/// Wi-Fi / Bluetooth connect state toast with its hold time: glowing icon when on, pulsing bar while connecting.
pub fn net_toast(kind: NetKind, state: NetState, name: &str) -> (Toast, u64) {
    let glyph = if kind == NetKind::Wifi { "\u{f1eb}" } else { "\u{f293}" }.to_string();
    let t = |frac: f32, muted: bool, text: String, icon_fx, bar_fx| Toast { glyph: glyph.clone(), frac, muted, text, icon_fx, bar_fx };
    match state {
        NetState::Connecting => (t(0.5, false, short(name), IconFx::Pulse, BarFx::Connecting), 20_000),
        NetState::Connected => (t(1.0, false, short(name), IconFx::On, BarFx::Normal), 2200),
        NetState::Failed => (t(1.0, false, "failed".into(), IconFx::Fail, BarFx::Failed), 2600),
        NetState::Disconnected => (t(0.0, true, short(name), IconFx::Plain, BarFx::Normal), HIDE_MS),
    }
}

/// Show `toast` for `hold_ms` (UI thread). Opens the overlay surface when it is not on screen yet.
pub fn push(st: &mut State, cmds: &mut Vec<Cmd>, toast: Toast, hold_ms: u64) {
    let o = &mut st.osd;
    if o.suppressed { return; }
    o.toast = Some(toast);
    o.hold_until = Some(Instant::now() + Duration::from_millis(hold_ms));
    if !o.open {
        o.open = true;
        cmds.push(Cmd::Open { view: Box::new(OsdView::default()), output: o.output.clone() });
    }
}

/// Show the overlay with a level (volume 0..=150 / brightness 0..=100) and an optional muted state.
pub fn show(poster: &Poster, kind: &str, level: f64, muted: bool) {
    let toast = level_toast(kind, level, muted);
    poster.post(move |st, cmds| push(st, cmds, toast, HIDE_MS));
}

pub fn show_toggle(poster: &Poster, glyph: &str, on: bool) {
    let toast = toggle_toast(glyph, on);
    poster.post(move |st, cmds| push(st, cmds, toast, HIDE_MS));
}

pub fn show_media(poster: &Poster, playing: bool, title: &str) {
    let toast = media_toast(playing, title);
    poster.post(move |st, cmds| push(st, cmds, toast, HIDE_MS));
}

fn out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).stderr(Stdio::null()).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// `osd-volume` / `osd-brightness` request from the key scripts: read the level off the UI thread, then show.
pub fn request(poster: &Poster, kind: &str) {
    let (poster, kind) = (poster.clone(), kind.to_string());
    std::thread::spawn(move || {
        let (level, muted) = if kind == "brightness" {
            (out("omarchy-brightness-display", &["--no-osd"]).trim_end_matches('%').parse::<f64>().unwrap_or(0.0), false)
        } else {
            (out("pamixer", &["--get-volume"]).parse::<f64>().unwrap_or(0.0), out("pamixer", &["--get-mute"]) == "true")
        };
        show(&poster, &kind, level, muted);
    });
}

/// Turn a new Wi-Fi/Bluetooth action state (`state.net.event`) into a toast. Idempotent per event, so any view
/// (the pills) may call it every tick.
pub fn sync_net(st: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(NetEvent { kind, state, name, seq }) = st.net.event.clone() else { return };
    if seq == st.osd.net_seen { return; }
    st.osd.net_seen = seq;
    let (toast, hold) = net_toast(kind, state, &name);
    push(st, cmds, toast, hold);
}

/// Turn every pending event (Wi-Fi/Bluetooth action, Reading Mode/DND flip, play/pause) into a toast. Idempotent, so
/// any view may call it every tick.
pub fn sync(st: &mut State, cmds: &mut Vec<Cmd>) {
    sync_net(st, cmds);
    if let Some(t) = st.sys.toggle.clone() {
        if t.seq != st.osd.toggle_seen {
            st.osd.toggle_seen = t.seq;
            push(st, cmds, toggle_toast(t.glyph, t.on), HIDE_MS);
        }
    }
    if let Some((playing, title)) = st.media.playback_event.take() {
        push(st, cmds, media_toast(playing, &title), HIDE_MS);
    }
}

// ------------------------------------------------------------------ view

/// The overlay surface: bottom-centre, click-through, closes itself once folded away.
#[derive(Default)]
pub struct OsdView {
    /// Spring position (0 = collapsed notch size, 1 = full), velocity, clock for the glow/pulse effects.
    x: f32,
    v: f32,
    t: f32,
}

impl OsdView {
    fn small(st: &State) -> (f32, f32) { st.osd.collapsed.unwrap_or(DEFAULT_SMALL) }
}

impl View for OsdView {
    fn spec(&self, _st: &State) -> PanelSpec {
        PanelSpec {
            namespace: "waybar-osd",
            layer: Layer::Overlay,
            anchor: Anchor::BOTTOM,
            size: WIN,
            margin: (0, 0, 36, 0),
            exclusive: -1,
            kb: KbMode::None,
        }
    }

    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        let animate = ctx.state.settings.behavior.animations;
        let o = &mut ctx.state.osd;
        // follow the notch to another output
        if o.output.is_some() && ctx.output.is_some() && o.output.as_deref() != ctx.output {
            ctx.cmds.push(Cmd::Close(ctx.id));
            ctx.cmds.push(Cmd::Open { view: Box::new(OsdView::default()), output: o.output.clone() });
            return false;
        }
        let holding = o.hold_until.is_some_and(|h| Instant::now() < h) && !o.suppressed;
        let target = if holding { 1.0 } else { 0.0 };
        self.t += dt;
        if !animate {
            self.x = target;
            self.v = 0.0;
        } else {
            // <=4 ms substeps keep the spring stable and frame-rate independent
            let n = ((dt / 0.004).ceil() as u32).max(1);
            let h = dt / n as f32;
            for _ in 0..n {
                self.v += (SPRING_K * (target - self.x) - SPRING_C * self.v) * h;
                self.x += self.v * h;
            }
            if (self.x - target).abs() < 0.0015 && self.v.abs() < 0.02 { self.x = target; self.v = 0.0; }
        }
        if target == 0.0 && self.x <= 0.0 {
            o.open = false;
            o.toast = None;
            ctx.cmds.push(Cmd::Close(ctx.id));
            return false;
        }
        true
    }

    fn input_region(&self) -> Option<Vec<Rect>> { Some(Vec::new()) } // pure overlay: clicks pass straight through

    fn event(&mut self, _ev: Event, _ctx: &mut Ctx) {}

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx) {
        let Some(toast) = dc.state.osd.toast.as_ref() else { return };
        let pal = &dc.state.pal;
        let (sw, sh) = Self::small(dc.state);
        let x = self.x.clamp(0.0, 1.2);
        let w = (sw + (BIG.0 - sw) * x).max(sw);
        let h = (sh + (BIG.1 - sh) * x).max(sh);
        let card = rect((WIN.0 as f32 - w) / 2.0, WIN.1 as f32 - h, w, h);
        glass(cv, pal, card, r_all(22.0));
        let alpha = ((x - 0.45) / 0.5).clamp(0.0, 1.0);
        if alpha > 0.0 {
            cv.with_alpha(alpha, |cv| draw_content(cv, pal, toast, card, self.t));
        }
    }
}

fn draw_content(cv: &mut Canvas, pal: &Palette, t: &Toast, card: Rect, clock: f32) {
    // content row of the full-size card: icon, bar, percentage (284px wide, centred)
    let row = rect(card.x + (card.w - (BIG.0 - 36.0)) / 2.0, card.y + (card.h - 30.0) / 2.0, BIG.0 - 36.0, 30.0);
    let icon_box = rect(row.x, row.y, 24.0, row.h);
    let pct_style = TextStyle::new(pal.fs(13.0), pal.fg_hi).bold();
    let pct_w = cv.measure(&t.text, &pct_style).0.max(44.0); // the label grows with long names; the bar yields
    let pct_box = rect(row.right() - pct_w, row.y, pct_w, row.h);
    let bar = rect(icon_box.right() + 12.0, row.y + row.h / 2.0 - 4.0, pct_box.x - 12.0 - (icon_box.right() + 12.0), 8.0);

    let pulse = (clock * std::f32::consts::TAU / 0.9).sin() * 0.5 + 0.5;
    let glow = |cycle: f32, repeats: f32| -> f32 {
        let ph = clock / cycle;
        if ph >= repeats { 0.15 } else { 0.15 + 0.85 * ((ph * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2).sin() * 0.5 + 0.5) }
    };
    let (icon_color, glow_color, glow_k) = match t.icon_fx {
        IconFx::Plain => (pal.fg_hi, pal.accent, 0.0),
        IconFx::On => (pal.accent_hi, pal.accent, glow(0.8, 2.0)),
        IconFx::Pulse => (pal.accent_hi, pal.accent, 0.15 + 0.85 * pulse),
        IconFx::Fail => (crate::render::rgb(0xff, 0x6b, 0x6b), pal.red, glow(0.5, 3.0)),
    };
    let c = icon_box.center();
    if glow_k > 0.0 {
        cv.fill_circle(c, 9.0 + 9.0 * glow_k, with_a(glow_color, 0.10 * glow_k));
        cv.fill_circle(c, 6.0 + 4.0 * glow_k, with_a(glow_color, 0.22 * glow_k));
    }
    cv.text(&t.glyph, &TextStyle::icon(20.0, icon_color), c, HAlign::Center, VAlign::Middle);

    // track + fill
    cv.fill_rrect(bar, r_all(5.0), with_a(pal.fg, 0.16));
    let fill = rect(bar.x, bar.y, bar.w * t.frac.clamp(0.0, 1.0), bar.h);
    if fill.w > 0.5 {
        let a = if t.bar_fx == BarFx::Connecting { 0.35 + 0.65 * pulse } else { 1.0 };
        let paint: Fill = if t.muted { with_a(pal.fg, 0.3).into() } else if t.bar_fx == BarFx::Failed { pal.red.into() }
            else { crate::ui::gradient_h(pal.accent_lo, mix(pal.accent, pal.fg_hi, 0.65), fill) };
        cv.with_alpha(a, |cv| cv.fill_rrect(fill, r_all(5.0), paint));
    }
    cv.text(&t.text, &pct_style, pct_box.center_right(), HAlign::Right, VAlign::Middle);
}

trait CenterRight { fn center_right(&self) -> crate::layout::Point; }
impl CenterRight for Rect {
    fn center_right(&self) -> crate::layout::Point { crate::layout::pt(self.right(), self.y + self.h / 2.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_glyph_and_text() {
        let g = |k, l, m| level_toast(k, l, m).glyph;
        assert_eq!(g("volume", 0.0, false), "\u{f075f}");
        assert_eq!(g("volume", 20.0, false), "\u{f057f}");
        assert_eq!(g("volume", 50.0, false), "\u{f0580}");
        assert_eq!(g("volume", 80.0, false), "\u{f057e}");
        assert_eq!(g("volume", 80.0, true), "\u{f075f}");
        assert_eq!(g("brightness", 10.0, false), "\u{f00df}");
        assert_eq!(level_toast("volume", 62.4, false).text, "62%");
        assert_eq!(level_toast("volume", 62.4, true).text, "mute");
        let boosted = level_toast("volume", 130.0, false);
        assert_eq!((boosted.frac, boosted.text.as_str()), (1.0, "130%"), "bar saturates, text keeps the boost");
    }

    #[test]
    fn net_toasts_hold_and_style_by_state() {
        let (t, hold) = net_toast(NetKind::Wifi, NetState::Connecting, "A very long network name here");
        assert_eq!((hold, t.icon_fx, t.bar_fx, t.glyph.as_str()), (20_000, IconFx::Pulse, BarFx::Connecting, "\u{f1eb}"));
        assert_eq!(t.text, "A very long ne…");
        let (t, hold) = net_toast(NetKind::Bt, NetState::Failed, "x");
        assert_eq!((hold, t.text.as_str(), t.glyph.as_str()), (2600, "failed", "\u{f293}"));
        let (t, _) = net_toast(NetKind::Bt, NetState::Disconnected, "Buds");
        assert!(t.muted && t.frac == 0.0);
    }

    #[test]
    fn push_opens_once_and_respects_suppression() {
        let mut st = State::new();
        let mut cmds = Vec::new();
        st.osd.suppressed = true;
        push(&mut st, &mut cmds, level_toast("volume", 10.0, false), 1000);
        assert!(cmds.is_empty() && st.osd.toast.is_none());
        st.osd.suppressed = false;
        push(&mut st, &mut cmds, level_toast("volume", 10.0, false), 1000);
        push(&mut st, &mut cmds, level_toast("volume", 20.0, false), 1000);
        assert_eq!(cmds.len(), 1, "one surface for repeated key presses");
        assert_eq!(st.osd.toast.as_ref().unwrap().text, "20%");
    }

    #[test]
    fn net_events_toast_once_per_sequence_number() {
        let mut st = State::new();
        let mut cmds = Vec::new();
        st.net.event = Some(NetEvent { kind: NetKind::Wifi, state: NetState::Connected, name: "Home".into(), seq: 3 });
        sync_net(&mut st, &mut cmds);
        sync_net(&mut st, &mut cmds);
        assert_eq!(st.osd.toast.as_ref().unwrap().text, "Home");
        st.osd.toast = None;
        sync_net(&mut st, &mut cmds);
        assert!(st.osd.toast.is_none(), "an already-shown event is not replayed");
    }

    #[test]
    fn overlay_springs_out_then_closes_itself() {
        let mut rig = crate::testkit::Rig::new();
        let mut cmds = Vec::new();
        push(&mut rig.st, &mut cmds, level_toast("volume", 40.0, false), 60_000);
        let mut v = OsdView::default();
        rig.run(&mut v, 0.1);
        assert!(v.x > 0.0, "growing while held");
        rig.st.osd.hold_until = Some(Instant::now()); // the hold time has run out
        let cmds = rig.run(&mut v, 2.5);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Close(1))), "closes after folding back");
        assert!(!rig.st.osd.open);
    }

    #[test]
    fn overlay_interior_is_opaque_over_any_backdrop() {
        let mut rig = crate::testkit::Rig::new();
        let mut cmds = Vec::new();
        push(&mut rig.st, &mut cmds, level_toast("volume", 40.0, false), 60_000);
        let mut v = OsdView::default();
        rig.run(&mut v, 0.8);
        let pm = rig.draw(&mut v);
        crate::ui::assert_opaque_interior(&pm, "osd");
    }

    #[test]
    fn preview_overlays() {
        let mut rig = crate::testkit::Rig::new();
        let mut cmds = Vec::new();
        for (name, toast) in [
            ("rp_osd_volume", level_toast("volume", 62.0, false)),
            ("rp_osd_muted", level_toast("volume", 40.0, true)),
            ("rp_osd_bright", level_toast("brightness", 85.0, false)),
            ("rp_osd_wifi", net_toast(NetKind::Wifi, NetState::Connected, "Example Network").0),
            ("rp_osd_failed", net_toast(NetKind::Bt, NetState::Failed, "Buds").0),
        ] {
            push(&mut rig.st, &mut cmds, toast, 60_000);
            let mut v = OsdView::default();
            rig.run(&mut v, 0.6);
            let pm = rig.draw(&mut v);
            crate::testkit::save_over_backdrop(&pm, 10, &format!("/tmp/{name}.png"));
            rig.st.osd.open = false;
        }
    }
}
