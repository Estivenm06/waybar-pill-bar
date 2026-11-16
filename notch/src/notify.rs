//! Notification cards (notch-style black pills) for notifications forwarded by the Omarchy shell's relay.
//! Cards hang below the right-hand Waybar pill. Do-Not-Disturb is shared with the Omarchy state file so the Controls
//! tile and Waybar keep working.
//!
//! Everything a notification sender controls is bounded here: at most five active cards, at most five retiring
//! widgets/timers, a bounded GTK-side event consumer, and file icons decoded by one worker thread (never on the GTK
//! thread) through capacity-five channels.
use gtk::{gdk, glib, prelude::*};
use crate::image_loader::{self, DecodedImage};
use crate::notification_relay::{Command, Event};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

const MAX_CARDS: usize = 5;
const MAX_RETIRING: usize = 5;
const DEFAULT_MS: u64 = 6000;
const SLIDE_MS: u32 = 240;
/// File icons are shown at 36 logical pixels; 72 covers a 2x scale.
const ICON_EDGE: i32 = 72;
const ICON_QUEUE: usize = 5;
/// Events (or icon completions) the GTK thread handles before yielding to its main loop.
pub const EVENT_BATCH: usize = 16;

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

// ------------------------------------------------------------------ bounded consumption

/// Handles items from `rx` on the GLib main loop: at most [`EVENT_BATCH`] per turn, then a one-millisecond timeout
/// future so timers, drawing and input run between batches of a flood. Below the batch size it sleeps on the channel.
/// `handle` returns `false` to stop consuming.
pub async fn consume<T>(rx: async_channel::Receiver<T>, mut handle: impl FnMut(T) -> bool) {
    while let Ok(first) = rx.recv().await {
        if !handle(first) { return; }
        let mut taken = 1;
        while taken < EVENT_BATCH {
            let Ok(item) = rx.try_recv() else { break };
            if !handle(item) { return; }
            taken += 1;
        }
        if taken == EVENT_BATCH { glib::timeout_future(Duration::from_millis(1)).await; }
    }
}

/// The one ordered handling path of a relay event: Do-Not-Disturb rejection, card creation or closure, then exactly
/// one acknowledgement so the shell may send another event.
pub fn dispatch(ev: Event, dnd: impl FnOnce() -> bool, relay: &std::sync::mpsc::Sender<Command>, show: impl FnOnce(Note), close: impl FnOnce(u32)) {
    let seq = match ev {
        Event::Notify { seq, note } => {
            if note.urgency < 2 && dnd() {
                // swallowed: the sender sees a normal expiry
                let _ = relay.send(Command::Close { id: note.id, reason: REASON_EXPIRED });
            } else {
                show(note);
            }
            seq
        }
        Event::Closed { seq, id } => {
            close(id);
            seq
        }
    };
    let _ = relay.send(Command::Ack { seq });
}

// ------------------------------------------------------------------ file icons

/// Which card generation may still receive an icon for a notification id. Every new card (including a replacement
/// of the same id) starts a new generation; closing a card ends it.
#[derive(Default)]
struct IconGenerations {
    last: u64,
    current: HashMap<u32, u64>,
}

impl IconGenerations {
    fn begin(&mut self, id: u32) -> u64 {
        self.last += 1;
        self.current.insert(id, self.last);
        self.last
    }

    fn end(&mut self, id: u32) {
        self.current.remove(&id);
    }

    fn accepts(&self, id: u32, generation: u64) -> bool {
        self.current.get(&id) == Some(&generation)
    }
}

pub struct IconJob {
    pub id: u32,
    pub generation: u64,
    pub path: PathBuf,
}

pub struct IconDone {
    pub id: u32,
    pub generation: u64,
    pub image: Result<DecodedImage, String>,
}

/// One decoder thread for every notification icon. Requests wait in a queue of five; when it is full the oldest
/// not-yet-started job is discarded for the newest. Completions are returned through a second queue of five.
pub struct IconWorker {
    jobs: async_channel::Sender<IconJob>,
    oldest: async_channel::Receiver<IconJob>,
}

impl IconWorker {
    pub fn start(load: impl Fn(&Path) -> Result<DecodedImage, String> + Send + 'static) -> (IconWorker, async_channel::Receiver<IconDone>) {
        let (jobs, rx) = async_channel::bounded::<IconJob>(ICON_QUEUE);
        let (done_tx, done_rx) = async_channel::bounded::<IconDone>(ICON_QUEUE);
        let oldest = rx.clone();
        let _ = std::thread::Builder::new().name("notify-icons".into()).spawn(move || {
            while let Ok(job) = rx.recv_blocking() {
                let image = load(&job.path);
                if done_tx.send_blocking(IconDone { id: job.id, generation: job.generation, image }).is_err() { return; }
            }
        });
        (IconWorker { jobs, oldest }, done_rx)
    }

    pub fn submit(&self, mut job: IconJob) {
        loop {
            match self.jobs.try_send(job) {
                Ok(()) | Err(async_channel::TrySendError::Closed(_)) => return,
                Err(async_channel::TrySendError::Full(j)) => {
                    job = j;
                    let _ = self.oldest.try_recv(); // drop the oldest job that has not started
                }
            }
        }
    }
}

// ------------------------------------------------------------------ UI
struct Card {
    id: u32,
    rev: gtk::Revealer,
    icon: gtk::Image,
    timer: RefCell<Option<glib::SourceId>>,
}

/// A widget sliding out; its removal timer is cancelled if the queue overflows.
struct Retiring {
    token: u64,
    rev: gtk::Revealer,
    timer: glib::SourceId,
}

pub struct NotifyUi {
    win: gtk::ApplicationWindow,
    col: gtk::Box,
    cards: RefCell<Vec<Rc<Card>>>,
    retiring: RefCell<VecDeque<Retiring>>,
    retire_token: Cell<u64>,
    gens: RefCell<IconGenerations>,
    icons: IconWorker,
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

/// Cards show at most a few lines; laying out thousands of characters (one unbreakable word is enough) makes
/// GTK ask the GPU for a surface far beyond its limits and crashes the whole process. Clip before layout.
const CARD_APP_CHARS: usize = 64;
const CARD_SUMMARY_CHARS: usize = 160;
const CARD_BODY_CHARS: usize = 400;

fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
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
        let (icons, done) = IconWorker::start(|p| image_loader::load_file(p, ICON_EDGE));
        let ui = Rc::new(NotifyUi {
            win, col, cards: RefCell::new(Vec::new()), retiring: RefCell::new(VecDeque::new()), retire_token: Cell::new(0),
            gens: RefCell::new(IconGenerations::default()), icons, hidden: Cell::new(false), animations: Cell::new(true),
            mon: RefCell::new(String::new()), relay,
        });
        let weak = Rc::downgrade(&ui);
        glib::MainContext::default().spawn_local(consume(done, move |d| match weak.upgrade() {
            Some(u) => { u.apply_icon(d); true }
            None => false,
        }));
        ui
    }

    /// Consume relay events on the GTK main loop (bounded batches); every event is acknowledged once handled.
    pub fn run_events(self: &Rc<Self>, events: async_channel::Receiver<Event>) {
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(consume(events, move |ev| {
            let Some(u) = weak.upgrade() else { return false };
            let (shower, closer) = (u.clone(), u.clone());
            dispatch(ev, dnd_on, &u.relay, move |n| shower.show(n), move |id| closer.close(id, REASON_CLOSED, true));
            true
        }));
    }

    /// A finished icon decode: applied only while the card that asked for it is still the active one for that id.
    fn apply_icon(&self, done: IconDone) {
        if !self.gens.borrow().accepts(done.id, done.generation) { return; }
        let Ok(image) = done.image else { return }; // rejected or undecodable: the fallback icon stays
        if let Some(card) = self.cards.borrow().iter().find(|c| c.id == done.id) {
            card.icon.set_paintable(Some(&image_loader::texture(image)));
        }
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

    /// The card widget, its icon (a fallback until a file icon is decoded) and the local file to decode, if any.
    fn make_card(self: &Rc<Self>, n: &Note) -> (gtk::Box, gtk::Image, Option<PathBuf>) {
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

        // A sender-supplied file is never opened here: the fallback shows at once and a worker decodes the file.
        let local = n.icon.starts_with('/') || n.icon.starts_with("file://");
        let img = if !local && !n.icon.is_empty() {
            gtk::Image::from_icon_name(&n.icon)
        } else {
            gtk::Image::from_icon_name("dialog-information-symbolic")
        };
        let icon_path = if local { image_loader::local_path(&n.icon) } else { None };
        img.set_pixel_size(36);
        img.set_valign(gtk::Align::Start);
        img.add_css_class("nicon");
        inner.append(&img);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        if !n.app.is_empty() {
            let app = gtk::Label::new(Some(&clip_chars(&n.app, CARD_APP_CHARS)));
            app.add_css_class("napp");
            app.set_xalign(0.0);
            text.append(&app);
        }
        let sum = gtk::Label::new(Some(&clip_chars(&strip_markup(&n.summary), CARD_SUMMARY_CHARS)));
        sum.add_css_class("nsum");
        sum.set_xalign(0.0);
        sum.set_wrap(true);
        sum.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        sum.set_lines(2);
        sum.set_ellipsize(gtk::pango::EllipsizeMode::End);
        sum.set_max_width_chars(34);
        text.append(&sum);
        if !n.body.is_empty() {
            let body = gtk::Label::new(Some(&clip_chars(&strip_markup(&n.body), CARD_BODY_CHARS)));
            body.add_css_class("nbody");
            body.set_xalign(0.0);
            body.set_wrap(true);
            body.set_wrap_mode(gtk::pango::WrapMode::WordChar);
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
        (card, img, icon_path)
    }

    pub fn show(self: &Rc<Self>, n: Note) {
        // a replaces_id update swaps the card in place of the old one; the old widget goes away synchronously
        self.close_with(n.id, REASON_CLOSED, false, false);

        let (widget, icon, icon_path) = self.make_card(&n);
        let rev = gtk::Revealer::new();
        rev.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        rev.set_transition_duration(self.slide_ms());
        rev.set_child(Some(&widget));
        self.col.prepend(&rev);

        let card = Rc::new(Card { id: n.id, rev: rev.clone(), icon, timer: RefCell::new(None) });
        self.cards.borrow_mut().insert(0, card.clone());
        let generation = self.gens.borrow_mut().begin(n.id);
        if let Some(path) = icon_path { self.icons.submit(IconJob { id: n.id, generation, path }); }

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

        // cap the stack: expire the oldest without animating it
        let overflow = { let c = self.cards.borrow(); if c.len() > MAX_CARDS { Some(c[c.len() - 1].id) } else { None } };
        if let Some(id) = overflow { self.close_with(id, REASON_EXPIRED, true, false); }
    }

    /// Close a card. Dismissals and expiries slide out; a sender-side closure (replacement or external close) does not.
    pub fn close(self: &Rc<Self>, id: u32, reason: u32, notify: bool) {
        self.close_with(id, reason, notify, reason != REASON_CLOSED);
    }

    fn close_with(self: &Rc<Self>, id: u32, reason: u32, notify: bool, animate: bool) {
        let card = {
            let mut cards = self.cards.borrow_mut();
            match cards.iter().position(|c| c.id == id) {
                Some(i) => cards.remove(i),
                None => return,
            }
        };
        self.gens.borrow_mut().end(id);
        if let Some(t) = card.timer.borrow_mut().take() { t.remove(); }
        if notify { self.emit_closed(id, reason); }
        self.retire(card.rev.clone(), animate);
    }

    /// Remove a closed card's widget, sliding it out when asked. At most [`MAX_RETIRING`] widgets (each with one
    /// timer) are ever in flight: the oldest is removed immediately when a sixth arrives.
    fn retire(self: &Rc<Self>, rev: gtk::Revealer, animate: bool) {
        if !animate || self.hidden.get() || !self.win.is_visible() {
            self.col.remove(&rev);
            self.sync_visibility();
            return;
        }
        loop {
            let oldest = {
                let mut q = self.retiring.borrow_mut();
                if q.len() < MAX_RETIRING { break; }
                q.pop_front()
            };
            if let Some(old) = oldest {
                old.timer.remove();
                self.col.remove(&old.rev);
            }
        }
        let token = self.retire_token.get() + 1;
        self.retire_token.set(token);
        rev.set_reveal_child(false);
        let u = self.clone();
        let timer = glib::timeout_add_local_once(Duration::from_millis(self.slide_ms() as u64 + 30), move || u.finish_retire(token));
        self.retiring.borrow_mut().push_back(Retiring { token, rev, timer });
    }

    fn finish_retire(&self, token: u64) {
        let done = {
            let mut q = self.retiring.borrow_mut();
            q.iter().position(|r| r.token == token).and_then(|i| q.remove(i))
        };
        // the timer that called us has fired and is gone; only the widget is left to remove
        if let Some(r) = done {
            self.col.remove(&r.rev);
            self.sync_visibility();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    #[test]
    fn card_text_is_clipped_on_char_boundaries() {
        assert_eq!(clip_chars("short", 10), "short");
        assert_eq!(clip_chars(&"x".repeat(20000), CARD_BODY_CHARS).chars().count(), CARD_BODY_CHARS);
        assert_eq!(clip_chars("éééé", 2), "éé");
        assert_eq!(clip_chars("", 3), "");
    }

    fn note(id: u32, urgency: u8) -> Note {
        Note { id, app: "A".into(), icon: String::new(), summary: "S".into(), body: "B".into(), actions: Vec::new(), urgency, timeout_ms: -1 }
    }

    fn tiny() -> DecodedImage {
        DecodedImage { width: 1, height: 1, rowstride: 4, alpha: true, bytes: glib::Bytes::from_owned(vec![0u8; 4]) }
    }

    /// Runs `dispatch` and reports (shown ids, closed ids, relay commands as strings).
    fn run(ev: Event, dnd: bool) -> (Vec<u32>, Vec<u32>, Vec<String>) {
        let (rtx, rrx) = mpsc::channel();
        let (shown, closed) = (std::cell::RefCell::new(Vec::new()), std::cell::RefCell::new(Vec::new()));
        dispatch(ev, || dnd, &rtx, |n| shown.borrow_mut().push(n.id), |id| closed.borrow_mut().push(id));
        drop(rtx);
        let cmds = rrx.try_iter().map(|c| match c {
            Command::Ack { seq } => format!("ack {seq}"),
            Command::Close { id, reason } => format!("close {id} {reason}"),
            Command::Action { id, key } => format!("action {id} {key}"),
        }).collect();
        (shown.into_inner(), closed.into_inner(), cmds)
    }

    #[test]
    fn every_event_is_acknowledged_exactly_once() {
        assert_eq!(run(Event::Notify { seq: 4, note: note(7, 1) }, false), (vec![7], vec![], vec!["ack 4".to_string()]));
        assert_eq!(run(Event::Notify { seq: 5, note: note(7, 2) }, true), (vec![7], vec![], vec!["ack 5".to_string()]), "critical ignores Do-Not-Disturb");
        assert_eq!(run(Event::Closed { seq: 6, id: 7 }, true), (vec![], vec![7], vec!["ack 6".to_string()]));
    }

    #[test]
    fn do_not_disturb_expires_the_notification_and_still_acknowledges() {
        for urgency in [0, 1] {
            let (shown, closed, cmds) = run(Event::Notify { seq: 9, note: note(3, urgency) }, true);
            assert!(shown.is_empty() && closed.is_empty());
            assert_eq!(cmds, vec![format!("close 3 {REASON_EXPIRED}"), "ack 9".to_string()], "the expiry reaches the sender before the next event is allowed");
        }
    }

    #[test]
    fn do_not_disturb_is_only_read_for_notify_events() {
        let (rtx, _rrx) = mpsc::channel();
        dispatch(Event::Closed { seq: 1, id: 1 }, || panic!("a closed event must not consult Do-Not-Disturb"), &rtx, |_| {}, |_| {});
    }

    #[test]
    fn stale_icon_completions_cannot_update_a_newer_card() {
        let mut g = IconGenerations::default();
        let first = g.begin(7);
        assert!(g.accepts(7, first));
        let replacement = g.begin(7); // a replaces_id update builds a new card for the same id
        assert!(!g.accepts(7, first), "the old card's decode must be ignored");
        assert!(g.accepts(7, replacement));
        assert!(!g.accepts(8, replacement), "another id never matches");
        g.end(7); // the card was closed before its icon arrived
        assert!(!g.accepts(7, replacement));
        let reborn = g.begin(7);
        assert!(reborn > replacement && g.accepts(7, reborn) && !g.accepts(7, replacement));
    }

    #[test]
    fn a_full_icon_queue_discards_the_oldest_unstarted_job() {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (started_tx, started_rx) = mpsc::channel::<PathBuf>();
        let (worker, done) = IconWorker::start(move |p| {
            started_tx.send(p.to_path_buf()).unwrap();
            gate_rx.recv().unwrap(); // deterministic gate: the worker finishes only when the test says so
            Ok(tiny())
        });
        let job = |id: u32| IconJob { id, generation: id as u64 * 10, path: PathBuf::from(format!("/icon/{id}")) };
        worker.submit(job(1));
        assert_eq!(started_rx.recv().unwrap(), PathBuf::from("/icon/1")); // job 1 is running and blocked
        for id in 2..=7 { worker.submit(job(id)); } // five fit; the sixth evicts the oldest waiting job (2)
        for _ in 0..6 { gate_tx.send(()).unwrap(); }
        let got: Vec<(u32, u64)> = (0..6).map(|_| { let d = done.recv_blocking().unwrap(); assert!(d.image.is_ok()); (d.id, d.generation) }).collect();
        assert_eq!(got, vec![(1, 10), (3, 30), (4, 40), (5, 50), (6, 60), (7, 70)]);
    }

    #[test]
    fn decoder_errors_come_back_as_errors_not_panics() {
        let (worker, done) = IconWorker::start(|_| Err("rejected".to_string()));
        worker.submit(IconJob { id: 1, generation: 1, path: PathBuf::from("/nope") });
        let d = done.recv_blocking().unwrap();
        assert_eq!((d.id, d.generation, d.image.err().as_deref()), (1, 1, Some("rejected")));
    }

    #[test]
    fn the_consumer_yields_to_the_main_loop_between_batches() {
        let ctx = glib::MainContext::new();
        let total = EVENT_BATCH * 3;
        let (tx, rx) = async_channel::unbounded::<usize>();
        for i in 0..total { tx.try_send(i).unwrap(); }
        drop(tx);
        let consumed = Arc::new(AtomicUsize::new(0));
        let at_heartbeat = Arc::new(AtomicUsize::new(usize::MAX));
        ctx.block_on(async {
            let (c, beat, attach_ctx) = (consumed.clone(), at_heartbeat.clone(), ctx.clone());
            consume(rx, move |_| {
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    // a default-priority idle callback: it can only run once the consumer gives the loop a turn
                    let (c, beat) = (c.clone(), beat.clone());
                    glib::idle_source_new(None, glib::Priority::DEFAULT, move || {
                        beat.store(c.load(Ordering::SeqCst), Ordering::SeqCst);
                        glib::ControlFlow::Break
                    }).attach(Some(&attach_ctx));
                }
                true
            }).await;
        });
        assert_eq!(consumed.load(Ordering::SeqCst), total, "every queued event is eventually handled");
        assert_eq!(at_heartbeat.load(Ordering::SeqCst), EVENT_BATCH, "the main loop ran after one batch, long before the flood was consumed");
    }

    #[test]
    fn the_consumer_sleeps_below_the_batch_size_and_can_stop() {
        let ctx = glib::MainContext::new();
        let (tx, rx) = async_channel::unbounded::<u32>();
        for i in 0..5 { tx.try_send(i).unwrap(); }
        let seen = std::cell::RefCell::new(Vec::new());
        ctx.block_on(consume(rx, |i| { seen.borrow_mut().push(i); i < 3 }));
        assert_eq!(*seen.borrow(), vec![0, 1, 2, 3], "handle returning false stops consumption");
        drop(tx);
    }
}
