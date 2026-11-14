//! Notification cards (notch-style black pills) for notifications forwarded by the Omarchy shell's relay.
//! Cards hang below the right-hand Waybar pill. Do-Not-Disturb is shared with the Omarchy state file so the Controls
//! tile and Waybar keep working.
use gtk::{gdk, glib, prelude::*};
use crate::notification_relay::Command;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

const MAX_CARDS: usize = 5;
const DEFAULT_MS: u64 = 6000;
const SLIDE_MS: u32 = 240;

pub const REASON_EXPIRED: u32 = 1;
pub const REASON_DISMISSED: u32 = 2;
pub const REASON_CLOSED: u32 = 3;

#[derive(Clone, Debug)]
pub struct Note {
    pub id: u32,
    pub app: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>, // (key, label)
    pub urgency: u8,                    // 0 low, 1 normal, 2 critical
    pub timeout_ms: i32,                // -1 default, 0 persistent
}

pub fn dnd_path() -> String {
    format!("{}/.local/state/omarchy/notifications.json", std::env::var("HOME").unwrap_or_default())
}

pub fn dnd_on() -> bool {
    std::fs::read_to_string(dnd_path()).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["dnd"].as_bool()).unwrap_or(false)
}

pub fn set_dnd(on: bool) {
    let p = dnd_path();
    if let Some(dir) = std::path::Path::new(&p).parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(&p, format!("{{\n  \"version\": 3,\n  \"dnd\": {on}\n}}\n"));
}

// ------------------------------------------------------------------ UI
struct Card {
    id: u32,
    rev: gtk::Revealer,
    timer: RefCell<Option<glib::SourceId>>,
}

pub struct NotifyUi {
    win: gtk::ApplicationWindow,
    col: gtk::Box,
    cards: RefCell<Vec<Rc<Card>>>,
    hidden: Cell<bool>,
    animations: Cell<bool>,
    mon: RefCell<String>,
    relay: std::sync::mpsc::Sender<Command>,
}

fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

impl NotifyUi {
    pub fn new(app: &gtk::Application, relay: std::sync::mpsc::Sender<Command>) -> Rc<Self> {
        let win = gtk::ApplicationWindow::new(app);
        win.init_layer_shell();
        win.set_layer(Layer::Overlay);
        win.set_namespace(Some("waybar-notifications"));
        win.set_anchor(Edge::Top, true);
        win.set_anchor(Edge::Right, true);
        win.set_margin(Edge::Top, 34); // Waybar bar height 26 + 8 gap
        win.set_margin(Edge::Right, 6); // matches the Waybar right margin
        win.set_exclusive_zone(-1);
        win.set_keyboard_mode(KeyboardMode::None);
        win.set_default_size(1, 1);
        let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
        col.set_halign(gtk::Align::End);
        col.set_valign(gtk::Align::Start);
        win.set_child(Some(&col));
        Rc::new(NotifyUi { win, col, cards: RefCell::new(Vec::new()), hidden: Cell::new(false), animations: Cell::new(true), mon: RefCell::new(String::new()), relay })
    }

    fn emit_closed(&self, id: u32, reason: u32) {
        // closure reason 3 means the sender already removed it (replacement/external close): nothing to echo
        if reason != REASON_CLOSED { let _ = self.relay.send(Command::Close { id, reason }); }
    }

    fn emit_action(&self, id: u32, key: &str) {
        let _ = self.relay.send(Command::Action { id, key: key.to_owned() });
    }

    fn sync_visibility(&self) {
        let show = !self.cards.borrow().is_empty() && !self.hidden.get();
        self.win.set_visible(show);
    }

    /// Fullscreen: hide the stack (timers keep running); restore after.
    pub fn set_hidden(&self, h: bool) {
        self.hidden.set(h);
        self.sync_visibility();
    }

    /// Output the cards appear on (the notch's destination).
    pub fn set_monitor(&self, gm: &gdk::Monitor) {
        let name = gm.connector().map(|c| c.to_string()).unwrap_or_default();
        if *self.mon.borrow() == name { return; }
        *self.mon.borrow_mut() = name;
        self.win.set_visible(false);
        self.win.set_monitor(Some(gm));
        self.sync_visibility();
    }

    pub fn set_animations(&self, on: bool) {
        self.animations.set(on);
        if on { self.win.remove_css_class("no-anim") } else { self.win.add_css_class("no-anim") }
    }

    fn slide_ms(&self) -> u32 {
        if self.animations.get() { SLIDE_MS } else { 0 }
    }

    fn make_card(self: &Rc<Self>, n: &Note) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        card.add_css_class("ncard");
        card.set_overflow(gtk::Overflow::Hidden);
        card.set_size_request(380, -1);

        let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        bar.add_css_class("nbar");
        bar.add_css_class(match n.urgency { 0 => "nbar-low", 2 => "nbar-crit", _ => "nbar-norm" });
        card.append(&bar);

        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        inner.set_margin_start(12);
        inner.set_margin_end(14);
        inner.set_margin_top(10);
        inner.set_margin_bottom(10);
        inner.set_hexpand(true);

        let img = if n.icon.starts_with('/') || n.icon.starts_with("file://") {
            gtk::Image::from_file(n.icon.trim_start_matches("file://"))
        } else if !n.icon.is_empty() {
            gtk::Image::from_icon_name(&n.icon)
        } else {
            gtk::Image::from_icon_name("dialog-information-symbolic")
        };
        img.set_pixel_size(36);
        img.set_valign(gtk::Align::Start);
        img.add_css_class("nicon");
        inner.append(&img);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        if !n.app.is_empty() {
            let app = gtk::Label::new(Some(&n.app));
            app.add_css_class("napp");
            app.set_xalign(0.0);
            text.append(&app);
        }
        let sum = gtk::Label::new(Some(&strip_markup(&n.summary)));
        sum.add_css_class("nsum");
        sum.set_xalign(0.0);
        sum.set_wrap(true);
        sum.set_lines(2);
        sum.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sum.set_max_width_chars(34);
        text.append(&sum);
        if !n.body.is_empty() {
            let body = gtk::Label::new(Some(&strip_markup(&n.body)));
            body.add_css_class("nbody");
            body.set_xalign(0.0);
            body.set_wrap(true);
            body.set_lines(3);
            body.set_ellipsize(gtk::pango::EllipsizeMode::End);
            body.set_max_width_chars(38);
            text.append(&body);
        }
        let btns: Vec<&(String, String)> = n.actions.iter().filter(|(k, _)| k != "default").collect();
        if !btns.is_empty() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            row.set_margin_top(6);
            for (key, label) in btns {
                let b = gtk::Button::with_label(label);
                b.add_css_class("nbtn");
                let (u, id, key) = (self.clone(), n.id, key.clone());
                b.connect_clicked(move |_| {
                    u.emit_action(id, &key);
                    u.close(id, REASON_DISMISSED, true);
                });
                row.append(&b);
            }
            text.append(&row);
        }
        inner.append(&text);
        card.append(&inner);

        // left click: default action + dismiss; right click: dismiss
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let (u, id) = (self.clone(), n.id);
        let has_default = n.actions.iter().any(|(k, _)| k == "default");
        click.connect_released(move |g, _, _, _| {
            if g.current_button() == 1 && has_default { u.emit_action(id, "default"); }
            u.close(id, REASON_DISMISSED, true);
        });
        card.add_controller(click);
        card
    }

    pub fn show(self: &Rc<Self>, n: Note) {
        // a replaces_id update swaps the card in place of the old one
        self.close(n.id, REASON_CLOSED, false);

        let rev = gtk::Revealer::new();
        rev.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        rev.set_transition_duration(self.slide_ms());
        rev.set_child(Some(&self.make_card(&n)));
        self.col.prepend(&rev);

        let card = Rc::new(Card { id: n.id, rev: rev.clone(), timer: RefCell::new(None) });
        self.cards.borrow_mut().insert(0, card.clone());

        let ms = match (n.timeout_ms, n.urgency) {
            (_, 2) | (0, _) => 0,
            (t, _) if t < 0 => DEFAULT_MS,
            (t, _) => t as u64,
        };
        if ms > 0 {
            let (u, id) = (self.clone(), n.id);
            *card.timer.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(ms), move || {
                // the timer fired: drop the stored id so close() does not try to remove it again
                if let Some(c) = u.cards.borrow().iter().find(|c| c.id == id) { c.timer.borrow_mut().take(); }
                u.close(id, REASON_EXPIRED, true);
            }));
        }
        self.sync_visibility();
        rev.set_reveal_child(true);

        // cap the stack: dismiss the oldest
        let overflow = { let c = self.cards.borrow(); if c.len() > MAX_CARDS { Some(c[c.len() - 1].id) } else { None } };
        if let Some(id) = overflow { self.close(id, REASON_EXPIRED, true); }
    }

    pub fn close(self: &Rc<Self>, id: u32, reason: u32, notify: bool) {
        let card = {
            let mut cards = self.cards.borrow_mut();
            match cards.iter().position(|c| c.id == id) {
                Some(i) => cards.remove(i),
                None => return,
            }
        };
        if let Some(t) = card.timer.borrow_mut().take() { t.remove(); }
        if notify { self.emit_closed(id, reason); }
        let (u, rev) = (self.clone(), card.rev.clone());
        if self.hidden.get() || !self.win.is_visible() {
            self.col.remove(&rev);
            self.sync_visibility();
            return;
        }
        rev.set_reveal_child(false);
        glib::timeout_add_local_once(Duration::from_millis(self.slide_ms() as u64 + 30), move || {
            u.col.remove(&rev);
            u.sync_visibility();
        });
    }
}
