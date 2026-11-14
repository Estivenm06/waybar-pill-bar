//! Volume / brightness key overlay: the collapsed notch springs wider and shows icon, bar and percentage.
use crate::{SPRING_C, SPRING_K};
use gtk::{cairo, gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

const DEFAULT_SMALL: (f64, f64) = (190.0, 26.0);   // matches the default collapsed notch
const BIG: (f64, f64) = (320.0, 58.0);
const WIN: (i32, i32) = (360, 80);
const HIDE_MS: u64 = 1500;
pub const NET_DISCONNECTED: u8 = 0;
pub const NET_CONNECTING: u8 = 1;
pub const NET_CONNECTED: u8 = 2;
pub const NET_FAILED: u8 = 3;

pub struct Osd {
    win: gtk::ApplicationWindow,
    card: gtk::Box,
    content: gtk::Box,
    icon: gtk::Label,
    bar: gtk::ProgressBar,
    pct: gtk::Label,
    x: Cell<f64>,
    v: Cell<f64>,
    target: Cell<f64>,
    animating: Cell<bool>,
    hide_id: RefCell<Option<glib::SourceId>>,
    suppressed: Cell<bool>,
    mon: RefCell<String>,
    animations: Cell<bool>,
    small: Cell<(f64, f64)>,
}

impl Osd {
    pub fn new(app: &gtk::Application) -> Rc<Self> {
        let win = gtk::ApplicationWindow::new(app);
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some("waybar-osd"));
        win.set_anchor(Edge::Bottom, true);
        win.set_margin(Edge::Bottom, 36);
        win.set_exclusive_zone(-1);
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_default_size(WIN.0, WIN.1);

        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.set_widget_name("osd-card");
        card.set_halign(gtk::Align::Center);
        card.set_valign(gtk::Align::End);
        card.set_overflow(gtk::Overflow::Hidden);
        card.set_size_request(DEFAULT_SMALL.0 as i32, DEFAULT_SMALL.1 as i32);

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_halign(gtk::Align::Center);
        content.set_valign(gtk::Align::Center);
        content.set_size_request(BIG.0 as i32 - 36, -1);
        content.set_opacity(0.0);
        let icon = gtk::Label::new(None);
        icon.add_css_class("osd-icon");
        let bar = gtk::ProgressBar::new();
        bar.add_css_class("osd-bar");
        bar.set_hexpand(true);
        bar.set_valign(gtk::Align::Center);
        let pct = gtk::Label::new(None);
        pct.add_css_class("osd-pct");
        pct.set_width_chars(4);
        pct.set_xalign(1.0);
        content.append(&icon);
        content.append(&bar);
        content.append(&pct);

        let overlay = gtk::Overlay::new();
        overlay.set_vexpand(true);
        overlay.set_hexpand(true);
        overlay.set_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
        overlay.add_overlay(&content);
        overlay.set_measure_overlay(&content, false);
        card.append(&overlay);
        win.set_child(Some(&card));

        Rc::new(Osd {
            win, card, content, icon, bar, pct, x: Cell::new(0.0), v: Cell::new(0.0), target: Cell::new(0.0),
            animating: Cell::new(false), hide_id: RefCell::new(None), suppressed: Cell::new(false),
            mon: RefCell::new(String::new()), animations: Cell::new(true), small: Cell::new(DEFAULT_SMALL),
        })
    }

    fn apply(&self) {
        let x = self.x.get();
        let small = self.small.get();
        let w = (small.0 + (BIG.0 - small.0) * x).max(small.0);
        let h = (small.1 + (BIG.1 - small.1) * x).max(small.1);
        self.card.set_size_request(w as i32, h as i32);
        self.content.set_opacity(((x - 0.45) / 0.5).clamp(0.0, 1.0));
    }

    /// Output the overlay appears on (the notch's destination).
    pub fn set_monitor(&self, gm: &gdk::Monitor) {
        let name = gm.connector().map(|c| c.to_string()).unwrap_or_default();
        if *self.mon.borrow() == name { return; }
        *self.mon.borrow_mut() = name;
        let shown = self.win.is_visible();
        if shown { self.win.set_visible(false); }
        self.win.set_monitor(Some(gm));
        if shown && !self.suppressed.get() {
            self.win.present();
            if let Some(surf) = self.win.surface() { surf.set_input_region(Some(&cairo::Region::create())); }
        }
    }

    pub fn set_animations(&self, on: bool) {
        self.animations.set(on);
        if on { self.win.remove_css_class("no-anim") } else { self.win.add_css_class("no-anim") }
    }

    /// Resting size of the card, equal to the collapsed notch.
    pub fn set_collapsed(&self, w: f64, h: f64) {
        self.small.set((w, h));
        self.apply();
    }

    /// Show the overlay with a level (0..=150) and an optional "muted" state; repeated calls just refresh.
    pub fn show(self: &Rc<Self>, kind: &str, level: f64, muted: bool) {
        let glyph = if kind == "brightness" {
            "\u{f00df}"
        } else if muted || level <= 0.0 {
            "\u{f075f}"
        } else if level < 34.0 {
            "\u{f057f}"
        } else if level < 67.0 {
            "\u{f0580}"
        } else {
            "\u{f057e}"
        };
        let text = if muted { "mute".to_string() } else { format!("{}%", level.round() as i64) };
        self.flash(glyph, (level / 100.0).clamp(0.0, 1.0), muted, &text);
    }

    /// On/off toast (e.g. Do Not Disturb): icon, a full/empty bar and "On"/"Off".
    pub fn show_toggle(self: &Rc<Self>, glyph: &str, on: bool) {
        self.flash(glyph, if on { 1.0 } else { 0.0 }, !on, if on { "On" } else { "Off" });
    }

    pub fn set_suppressed(&self, s: bool) {
        self.suppressed.set(s);
        if s { self.win.set_visible(false); }
    }

    pub fn show_media(self: &Rc<Self>, playing: bool, title: &str) {
        let t: String = if title.chars().count() > 14 {
            title.chars().take(14).collect::<String>() + "…"
        } else { title.to_string() };
        self.flash(if playing { "\u{f04b}" } else { "\u{f04c}" }, if playing { 1.0 } else { 0.0 }, !playing, &t);
    }

    fn flash(self: &Rc<Self>, glyph: &str, frac: f64, muted: bool, text: &str) {
        self.flash_ex(glyph, frac, muted, text, HIDE_MS, "", "");
    }

    /// Wi-Fi / Bluetooth connect state toast: blue glowing icon when on, pulsing bar while connecting, dim when off.
    pub fn show_net(self: &Rc<Self>, kind: &str, state: u8, name: &str) {
        let glyph = if kind == "wifi" { "\u{f1eb}" } else { "\u{f293}" };
        let t: String = if name.chars().count() > 14 { name.chars().take(14).collect::<String>() + "…" } else { name.to_string() };
        match state {
            NET_CONNECTING => self.flash_ex(glyph, 0.5, false, &t, 20_000, "osd-pulse", "connecting"),
            NET_CONNECTED => self.flash_ex(glyph, 1.0, false, &t, 2200, "osd-on", ""),
            NET_FAILED => self.flash_ex(glyph, 1.0, false, "failed", 2600, "osd-fail", "failed"),
            _ => self.flash_ex(glyph, 0.0, true, &t, HIDE_MS, "", ""),
        }
    }

    fn flash_ex(self: &Rc<Self>, glyph: &str, frac: f64, muted: bool, text: &str, hold_ms: u64, icon_cls: &str, bar_cls: &str) {
        if self.suppressed.get() { return; }
        self.icon.set_text(glyph);
        for c in ["osd-on", "osd-pulse", "osd-fail"] { self.icon.remove_css_class(c); }
        if !icon_cls.is_empty() { self.icon.add_css_class(icon_cls); }
        self.bar.remove_css_class("connecting");
        self.bar.remove_css_class("failed");
        if !bar_cls.is_empty() { self.bar.add_css_class(bar_cls); }
        self.bar.set_fraction(frac);
        if muted { self.bar.add_css_class("muted") } else { self.bar.remove_css_class("muted") }
        self.pct.set_text(text);

        if let Some(id) = self.hide_id.borrow_mut().take() { id.remove(); }
        let o = self.clone();
        *self.hide_id.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(hold_ms), move || {
            o.hide_id.borrow_mut().take();
            o.target.set(0.0);
            o.run();
        }));
        self.target.set(1.0);
        self.win.present();
        if let Some(surf) = self.win.surface() {
            surf.set_input_region(Some(&cairo::Region::create())); // pure overlay: clicks pass straight through
        }
        self.run();
    }

    fn run(self: &Rc<Self>) {
        if !self.animations.get() {
            // reduced motion: jump to the target instead of springing
            self.x.set(self.target.get());
            self.v.set(0.0);
            self.apply();
            if self.target.get() == 0.0 { self.win.set_visible(false); }
            return;
        }
        if self.animating.replace(true) { return; }
        let o = self.clone();
        let last = Cell::new(0i64);
        self.card.add_tick_callback(move |_, clock| {
            let now = clock.frame_time();
            let dt = if last.get() == 0 { 16_000 } else { now - last.get() };
            last.set(now);
            let dt = (dt as f64 / 1e6).min(0.033);
            let (x, v, tg) = (o.x.get(), o.v.get(), o.target.get());
            let v2 = v + (SPRING_K * (tg - x) - SPRING_C * v) * dt;
            let mut nx = x + v2 * dt;
            let mut nv = v2;
            let settled = (nx - tg).abs() < 0.0015 && nv.abs() < 0.02;
            if settled { nx = tg; nv = 0.0; }
            o.x.set(nx);
            o.v.set(nv);
            o.apply();
            if settled {
                o.animating.set(false);
                if tg == 0.0 { o.win.set_visible(false); }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}
