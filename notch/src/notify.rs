//! Notification cards (notch-style glass pills) for notifications forwarded by the Omarchy shell's relay.
//! Cards hang below the right-hand pill. Do-Not-Disturb is shared with the Omarchy state file so the Controls
//! tile keeps working.
//!
//! Everything a notification sender controls is bounded here: at most five active cards, at most five retiring
//! cards, relay events handled one at a time (the shell never has more than 32 unacknowledged), and file icons
//! decoded by one worker thread (never on the UI thread) through capacity-five queues.
//!
//! Data flow: relay reader thread -> `start()` pump -> `Poster` -> `handle_event` on the UI thread -> `Notes`
//! (the `State` slice) -> `CardsView` (the layer surface that draws the slice).
use crate::image_loader::{self, DecodedImage};
use crate::layout::{Rect, rect, smooth};
use crate::notification_relay::{self, Command, Event};
use crate::render::{Canvas, HAlign, Image, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::State;
use crate::ui::{Hits, Palette, glass};
use crate::wayland::{Button, Cmd, Ctx, Cursor, DrawCtx, Event as UiEvent, KbMode, PanelSpec, Poster, View};
use smithay_client_toolkit::shell::wlr_layer::{Anchor, Layer};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_CARDS: usize = 5;
const MAX_RETIRING: usize = 5;
const DEFAULT_MS: u64 = 6000;
/// Slide-in / slide-out duration of a card.
const SLIDE_S: f32 = 0.24;
/// File icons are shown at 36 logical pixels; 72 covers a 2x scale.
const ICON_EDGE: i32 = 72;
const ICON_QUEUE: usize = 5;

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
    pub suppress_sound: bool,           // the sender's `suppress-sound` hint
}

// ------------------------------------------------------------------ notification sound

const SOUND_FILE: &str = "/usr/share/sounds/freedesktop/stereo/message.oga";
const SOUND_LIMIT: Duration = Duration::from_secs(3);

/// Whether a card that passed the Do-Not-Disturb gate should beep: the switch is on, the urgency is normal or
/// critical, the sender did not ask for silence and no live card with that id exists (updates stay silent).
pub fn should_play(enabled: bool, n: &Note, has_live_card: bool) -> bool {
    enabled && n.urgency >= 1 && !n.suppress_sound && !has_live_card
}

/// One bounded request slot to a single playback worker: while a sound plays, further requests are dropped
/// rather than queued.
pub struct SoundQueue(mpsc::SyncSender<()>);

impl SoundQueue {
    /// `play` runs on the worker thread; its error is logged once, never repeated, and never reaches the UI.
    pub fn start(play: impl Fn() -> io::Result<()> + Send + 'static) -> SoundQueue {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        let _ = std::thread::Builder::new().name("notify-sound".into()).spawn(move || {
            let mut logged = false;
            while rx.recv().is_ok() {
                if let Err(e) = play() {
                    if !logged { logged = true; eprintln!("waybar-notch: notification sound unavailable: {e}"); }
                }
                // requests that arrived during playback are dropped, never replayed
                while rx.try_recv().is_ok() {}
            }
        });
        SoundQueue(tx)
    }

    /// Non-blocking; a full slot or a dead worker silently drops the request.
    pub fn request(&self) { let _ = self.0.try_send(()); }
}

/// Play the freedesktop "message" sound with PipeWire, killing the player after 3 seconds.
pub fn play_message_sound() -> io::Result<()> {
    if !Path::new(SOUND_FILE).is_file() { return Err(io::Error::new(io::ErrorKind::NotFound, format!("{SOUND_FILE} is missing"))); }
    let mut child = Proc::new("pw-play").arg(SOUND_FILE).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    let end = Instant::now() + SOUND_LIMIT;
    loop {
        match child.try_wait()? {
            Some(s) if s.success() => return Ok(()),
            Some(s) => return Err(io::Error::other(format!("pw-play exited with {s}"))),
            None if Instant::now() >= end => { let _ = child.kill(); let _ = child.wait(); return Ok(()); }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
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

/// The one ordered handling path of a relay event: Do-Not-Disturb rejection, card creation or closure, then exactly
/// one acknowledgement so the shell may send another event.
pub fn dispatch(ev: Event, dnd: impl FnOnce() -> bool, relay: &mpsc::Sender<Command>, show: impl FnOnce(Note), close: impl FnOnce(u32)) {
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

struct JobQueue {
    jobs: VecDeque<IconJob>,
    closed: bool,
}

/// One decoder thread for every notification icon. Requests wait in a queue of five; when it is full the oldest
/// not-yet-started job is discarded for the newest. Completions are returned through a second queue of five.
pub struct IconWorker {
    queue: Arc<(Mutex<JobQueue>, Condvar)>,
}

impl IconWorker {
    pub fn start(load: impl Fn(&Path) -> Result<DecodedImage, String> + Send + 'static) -> (IconWorker, mpsc::Receiver<IconDone>) {
        let queue = Arc::new((Mutex::new(JobQueue { jobs: VecDeque::new(), closed: false }), Condvar::new()));
        let (done_tx, done_rx) = mpsc::sync_channel::<IconDone>(ICON_QUEUE);
        let q = queue.clone();
        let _ = std::thread::Builder::new().name("notify-icons".into()).spawn(move || {
            loop {
                let job = {
                    let (lock, cv) = &*q;
                    let mut g = lock.lock().unwrap();
                    loop {
                        if let Some(j) = g.jobs.pop_front() { break j; }
                        if g.closed { return; }
                        g = cv.wait(g).unwrap();
                    }
                };
                let image = load(&job.path);
                if done_tx.send(IconDone { id: job.id, generation: job.generation, image }).is_err() { return; }
            }
        });
        (IconWorker { queue }, done_rx)
    }

    pub fn submit(&self, job: IconJob) {
        let (lock, cv) = &*self.queue;
        let mut g = lock.lock().unwrap();
        while g.jobs.len() >= ICON_QUEUE { g.jobs.pop_front(); } // drop the oldest job that has not started
        g.jobs.push_back(job);
        cv.notify_one();
    }
}

impl Drop for IconWorker {
    fn drop(&mut self) {
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().closed = true;
        cv.notify_all();
    }
}

/// Worker-side icon decode: absolute paths are sender-supplied files (hardened loader); anything else is a themed
/// icon name resolved through the icon theme.
fn load_icon(p: &Path) -> Result<DecodedImage, String> {
    if p.is_absolute() { return image_loader::load_file(p, ICON_EDGE); }
    let name = p.to_str().ok_or("invalid icon name")?;
    let found = crate::icon_theme::find(name, 36).ok_or("icon not found")?;
    image_loader::load_file(&found, ICON_EDGE)
}

// ------------------------------------------------------------------ text hygiene

pub fn strip_markup(s: &str) -> String {
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

/// Cards show at most a few lines; laying out thousands of characters (one unbreakable word is enough) is wasted
/// work and can exhaust memory in the shaper. Clip before layout.
const CARD_APP_CHARS: usize = 64;
const CARD_SUMMARY_CHARS: usize = 160;
const CARD_BODY_CHARS: usize = 400;

pub fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

/// How long a card stays: `None` = until dismissed (critical or persistent).
fn lifetime(n: &Note) -> Option<Duration> {
    match (n.timeout_ms, n.urgency) {
        (_, 2) | (0, _) => None,
        (t, _) if t < 0 => Some(Duration::from_millis(DEFAULT_MS)),
        (t, _) => Some(Duration::from_millis(t as u64)),
    }
}

// ------------------------------------------------------------------ state slice

pub struct Card {
    pub note: Note,
    /// Cleaned, clipped display strings.
    pub app: String,
    pub summary: String,
    pub body: String,
    generation: u64,
    deadline: Option<Instant>,
    /// Slide progress 0..=1 (grows while shown, shrinks while closing).
    progress: f32,
    closing: bool,
    icon: Option<Image>,
    lay: Option<CardLayout>,
}

impl Card {
    fn new(n: Note, generation: u64, now: Instant) -> Card {
        Card {
            app: clip_chars(&n.app, CARD_APP_CHARS),
            summary: clip_chars(&strip_markup(&n.summary), CARD_SUMMARY_CHARS),
            body: clip_chars(&strip_markup(&n.body), CARD_BODY_CHARS),
            generation,
            deadline: lifetime(&n).map(|d| now + d),
            progress: 0.0,
            closing: false,
            icon: None,
            lay: None,
            note: n,
        }
    }

    pub fn closing(&self) -> bool { self.closing }
}

#[derive(Default)]
pub struct Notes {
    /// Newest first; closing cards stay until their slide-out finishes.
    pub cards: Vec<Card>,
    gens: IconGenerations,
    icons: Option<IconWorker>,
    relay: Option<mpsc::Sender<Command>>,
    sound: Option<SoundQueue>,
    /// Fullscreen: hide the stack (timers keep running).
    pub hidden: bool,
    /// Output the stack appears on (the notch's destination); `None` = compositor's choice.
    pub output: Option<String>,
    open: bool,
}

impl Notes {
    fn emit_closed(&self, id: u32, reason: u32) {
        // closure reason 3 means the sender already removed it (replacement/external close): nothing to echo
        if reason != REASON_CLOSED { if let Some(r) = &self.relay { let _ = r.send(Command::Close { id, reason }); } }
    }

    pub fn action(&self, id: u32, key: &str) {
        if let Some(r) = &self.relay { let _ = r.send(Command::Action { id, key: key.to_owned() }); }
    }

    /// Number of cards that are not sliding out.
    fn active(&self) -> usize { self.cards.iter().filter(|c| !c.closing).count() }

    /// A live (not sliding out) card with this id exists.
    fn has_live_card(&self, id: u32) -> bool { self.cards.iter().any(|c| c.note.id == id && !c.closing) }

    pub fn show(&mut self, n: Note, cmds: &mut Vec<Cmd>) {
        // a replaces_id update swaps the card in place of the old one; the old card goes away synchronously
        self.close_with(n.id, REASON_CLOSED, false, false);

        let generation = self.gens.begin(n.id);
        let local = image_loader::local_path(&n.icon);
        let source = local.or_else(|| (!n.icon.is_empty() && !n.icon.starts_with('/') && !n.icon.contains(':')).then(|| PathBuf::from(&n.icon)));
        let id = n.id;
        self.cards.insert(0, Card::new(n, generation, Instant::now()));
        if let (Some(path), Some(w)) = (source, &self.icons) { w.submit(IconJob { id, generation, path }); }

        // cap the stack: expire the oldest without animating it
        if self.active() > MAX_CARDS {
            if let Some(old) = self.cards.iter().rev().find(|c| !c.closing).map(|c| c.note.id) {
                self.close_with(old, REASON_EXPIRED, true, false);
            }
        }
        if !self.open {
            self.open = true;
            cmds.push(Cmd::Open { view: Box::new(CardsView::default()), output: self.output.clone() });
        }
    }

    /// Close a card. Dismissals and expiries slide out; a sender-side closure (replacement or external close) does not.
    pub fn close(&mut self, id: u32, reason: u32, notify: bool) {
        self.close_with(id, reason, notify, reason != REASON_CLOSED);
    }

    fn close_with(&mut self, id: u32, reason: u32, notify: bool, animate: bool) {
        let Some(i) = self.cards.iter().position(|c| c.note.id == id && !c.closing) else { return };
        self.gens.end(id);
        if notify { self.emit_closed(id, reason); }
        if !animate || self.hidden {
            self.cards.remove(i);
            return;
        }
        self.cards[i].closing = true;
        self.cards[i].deadline = None;
        // at most MAX_RETIRING sliding-out cards: the oldest is removed immediately when another arrives
        while self.cards.iter().filter(|c| c.closing).count() > MAX_RETIRING {
            if let Some(j) = self.cards.iter().rposition(|c| c.closing) { self.cards.remove(j); }
        }
    }

    /// A finished icon decode: applied only while the card that asked for it is still the active one for that id.
    fn apply_icon(&mut self, done: IconDone) {
        if !self.gens.accepts(done.id, done.generation) { return; }
        let Ok(image) = done.image else { return }; // rejected or undecodable: the fallback icon stays
        if let Some(card) = self.cards.iter_mut().find(|c| c.note.id == done.id && !c.closing) {
            card.icon = image.into_image();
        }
    }

    /// Advance slide animations and expire due cards. Returns true while another frame is needed soon.
    fn step(&mut self, dt: f32, animate: bool, now: Instant) -> bool {
        let due: Vec<u32> = self.cards.iter().filter(|c| !c.closing && c.deadline.is_some_and(|d| now >= d)).map(|c| c.note.id).collect();
        for id in due { self.close(id, REASON_EXPIRED, true); }
        let mut busy = false;
        for c in &mut self.cards {
            let (target, k) = if c.closing { (0.0, -1.0) } else { (1.0, 1.0) };
            if !animate { c.progress = target; continue; }
            if c.progress != target {
                c.progress = (c.progress + k * dt / SLIDE_S).clamp(0.0, 1.0);
                busy = true;
            }
        }
        self.cards.retain(|c| !(c.closing && c.progress <= 0.0));
        // wake precisely for the next expiry instead of polling at the idle second tick
        busy || self.cards.iter().any(|c| !c.closing && c.deadline.is_some_and(|d| d.saturating_duration_since(now) < Duration::from_millis(1100)))
    }
}

/// Handle one relay event on the UI thread (see `dispatch`).
fn handle_event(st: &mut State, cmds: &mut Vec<Cmd>, ev: Event) {
    let Some(relay) = st.notes.relay.clone() else { return };
    let sounds = st.settings.behavior.notification_sounds;
    let cell = std::cell::RefCell::new((&mut st.notes, cmds));
    dispatch(ev, dnd_on, &relay,
        |n| {
            let mut g = cell.borrow_mut();
            let (notes, cmds) = &mut *g;
            // decided before the card replaces an older one with the same id
            let play = should_play(sounds, &n, notes.has_live_card(n.id));
            notes.show(n, cmds);
            if play { if let Some(q) = &notes.sound { q.request(); } }
        },
        |id| cell.borrow_mut().0.close(id, REASON_CLOSED, false));
}

/// Bind the relay socket and start the worker threads. Quickshell owns `org.freedesktop.Notifications` and relays
/// every notification here; a transport failure exits the app (there would be no notification UI).
pub fn start(shell_pid: i32, poster: &Poster) -> std::io::Result<()> {
    let (etx, erx) = mpsc::sync_channel::<Event>(notification_relay::INGRESS);
    let (ftx, frx) = mpsc::channel::<String>();
    let cmds = notification_relay::start(shell_pid, ftx, etx)?;
    let (worker, done) = IconWorker::start(load_icon);
    let sound = SoundQueue::start(play_message_sound);
    poster.post(move |st, _| {
        st.notes.relay = Some(cmds);
        st.notes.icons = Some(worker);
        st.notes.sound = Some(sound);
    });
    let p = poster.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = erx.recv() { p.post(move |st, cmds| handle_event(st, cmds, ev)); }
    });
    let p = poster.clone();
    std::thread::spawn(move || {
        while let Ok(d) = done.recv() { p.post(move |st, _| st.notes.apply_icon(d)); }
    });
    let p = poster.clone();
    std::thread::spawn(move || {
        if let Ok(why) = frx.recv() {
            eprintln!("notification relay failed: {why}");
            p.post(|_, cmds| cmds.push(Cmd::Exit));
        }
    });
    Ok(())
}

// ------------------------------------------------------------------ cards view

const CARD_W: f32 = 380.0;
const GAP: f32 = 8.0;
const RADIUS: f32 = 20.0;
const ICON: f32 = 36.0;
/// Text column width: card minus accent bar, margins and icon.
const TEXT_W: f32 = CARD_W - 4.0 - 12.0 - ICON - 12.0 - 14.0;
const BTN_H: f32 = 20.0;

#[derive(Clone, Copy)]
struct CardLayout {
    fs: f32,
    h: f32,
    app_h: f32,
    sum_h: f32,
    body_h: f32,
    btn_row: bool,
}

fn app_style(p: &Palette) -> TextStyle { TextStyle::new(p.fs(10.0), p.dim).bold() }
fn sum_style(p: &Palette) -> TextStyle { TextStyle::new(p.fs(13.0), p.fg_hi).bold() }
fn body_style(p: &Palette) -> TextStyle { TextStyle::new(p.fs(12.0), with_a(p.fg, 0.85)) }
fn btn_style(p: &Palette) -> TextStyle { TextStyle::new(p.fs(11.0), p.fg_hi) }
fn line_h(st: &TextStyle) -> f32 { (st.size * 1.3).ceil() }

fn action_buttons(c: &Card) -> Vec<&(String, String)> { c.note.actions.iter().filter(|(k, _)| k != "default").collect() }

fn layout_card(ts: &mut TextSys, p: &Palette, c: &Card) -> CardLayout {
    let wrapped = |ts: &mut TextSys, s: &str, st: &TextStyle, cap: f32| {
        if s.is_empty() { return 0.0; }
        let lh = line_h(st);
        let lines = (ts.measure(s, st, Some(TEXT_W)).1 / lh).round().clamp(1.0, cap);
        lines * lh
    };
    let app_h = if c.app.is_empty() { 0.0 } else { line_h(&app_style(p)) };
    let sum_h = wrapped(ts, &c.summary, &sum_style(p), 2.0);
    let body_h = wrapped(ts, &c.body, &body_style(p), 3.0);
    let btn_row = !action_buttons(c).is_empty();
    let parts: Vec<f32> = [app_h, sum_h, body_h].into_iter().filter(|h| *h > 0.0).collect();
    let mut text_h = parts.iter().sum::<f32>() + 2.0 * parts.len().saturating_sub(1) as f32;
    if btn_row { text_h += 6.0 + BTN_H + if parts.is_empty() { 0.0 } else { 2.0 }; }
    CardLayout { fs: p.font_scale, h: 10.0 + text_h.max(ICON) + 10.0, app_h, sum_h, body_h, btn_row }
}

/// The notification stack surface: one layer surface, top-right under the pill, sized to its cards.
#[derive(Default)]
pub struct CardsView {
    hits: Hits,
    /// Note id per card slot registered in the last frame (hit ids are `slot * 16 + part`).
    slots: Vec<u32>,
    height: f32,
    hidden: bool,
}

impl CardsView {
    fn visible_height(cards: &[Card]) -> f32 {
        let mut h = 0.0;
        for c in cards {
            if let Some(l) = &c.lay { h += (l.h + GAP) * smooth(c.progress); }
        }
        (h - GAP).max(0.0)
    }
}

impl View for CardsView {
    fn spec(&self, _st: &State) -> PanelSpec {
        PanelSpec {
            namespace: "waybar-notifications",
            layer: Layer::Overlay,
            anchor: Anchor::TOP | Anchor::RIGHT,
            size: (CARD_W as u32, self.height.ceil().max(1.0) as u32),
            margin: (34, 6, 0, 0),
            exclusive: -1,
            kb: KbMode::None,
        }
    }

    fn tick(&mut self, dt: f32, ctx: &mut Ctx) -> bool {
        let animate = ctx.state.settings.behavior.animations;
        let State { notes, pal, .. } = &mut *ctx.state;
        // the stack follows the notch to another output: re-create it there
        if notes.output.is_some() && ctx.output.is_some() && notes.output.as_deref() != ctx.output {
            ctx.cmds.push(Cmd::Close(ctx.id));
            ctx.cmds.push(Cmd::Open { view: Box::new(CardsView::default()), output: notes.output.clone() });
            return false;
        }
        let busy = notes.step(dt, animate, Instant::now());
        for c in notes.cards.iter_mut() {
            if c.lay.is_none_or(|l| l.fs != pal.font_scale) { c.lay = Some(layout_card(ctx.text, pal, c)); }
        }
        self.hidden = notes.hidden;
        self.height = if self.hidden { 0.0 } else { Self::visible_height(&notes.cards) };
        if notes.cards.is_empty() {
            notes.open = false;
            ctx.cmds.push(Cmd::Close(ctx.id));
        }
        busy
    }

    fn input_region(&self) -> Option<Vec<Rect>> {
        if self.hidden { Some(Vec::new()) } else { None }
    }

    fn ticks_seconds(&self) -> bool { true }

    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx) {
        self.hits.clear();
        self.slots.clear();
        if self.hidden { return; }
        let pal = &dc.state.pal;
        let mut y = 0.0;
        for c in &dc.state.notes.cards {
            let Some(l) = c.lay else { continue };
            let e = smooth(c.progress);
            let vis = rect(0.0, y, CARD_W, l.h * e);
            if vis.h >= 0.5 {
                let slot = self.slots.len() as u32;
                if !c.closing { self.slots.push(c.note.id); } else { self.slots.push(u32::MAX); }
                cv.with_alpha(e.sqrt(), |cv| cv.with_clip(vis, r_all(0.0), |cv| {
                    draw_card(cv, pal, c, &l, y, if c.closing { None } else { Some((&mut self.hits, slot, vis)) });
                }));
            }
            y += (l.h + GAP) * e;
        }
    }

    fn event(&mut self, ev: UiEvent, ctx: &mut Ctx) {
        let Some(click) = self.hits.event(&ev) else { return };
        let Some(&id) = self.slots.get((click.id / 16) as usize) else { return };
        if id == u32::MAX { return; }
        let part = click.id % 16;
        let notes = &mut ctx.state.notes;
        let Some(card) = notes.cards.iter().find(|c| c.note.id == id && !c.closing) else { return };
        if part == 0 {
            // left click: default action + dismiss; any other button: dismiss
            if click.button == Button::Left && card.note.actions.iter().any(|(k, _)| k == "default") { notes.action(id, "default"); }
        } else if let Some((key, _)) = action_buttons(card).get(part as usize - 1) {
            let key = key.clone();
            notes.action(id, &key);
        }
        notes.close(id, REASON_DISMISSED, true);
    }
}

/// Draw one card with its top at `y` (full height; the caller clips to the revealed part).
fn draw_card(cv: &mut Canvas, p: &Palette, c: &Card, l: &CardLayout, y: f32, hits: Option<(&mut Hits, u32, Rect)>) {
    let r = rect(0.0, y, CARD_W, l.h);
    glass(cv, p, r, r_all(RADIUS));
    let bar = match c.note.urgency { 0 => with_a(p.fg, 0.3), 2 => p.red, _ => p.accent };
    cv.with_clip(r.pad(1.0), r_all(RADIUS - 1.0), |cv| cv.fill_rect(rect(r.x, r.y, 5.0, r.h), bar));

    // icon: decoded image or a generic glyph
    let ir = rect(4.0 + 12.0, y + 10.0, ICON, ICON);
    match &c.icon {
        Some(img) => cv.image_cover(img, ir, r_all(8.0)),
        None => {
            cv.text("\u{f05a}", &TextStyle::icon(26.0, p.accent), ir.center(), HAlign::Center, VAlign::Middle);
        }
    }

    let tx = ir.right() + 12.0;
    let mut ty = y + 10.0;
    // each text block sits 2px below the previous one
    fn place(ty: &mut f32, first: &mut bool, h: f32) -> f32 {
        if !*first { *ty += 2.0; }
        *first = false;
        let t = *ty;
        *ty += h;
        t
    }
    let mut first = true;
    if l.app_h > 0.0 {
        let t = place(&mut ty, &mut first, l.app_h);
        cv.text_in(&c.app, &app_style(p), rect(tx, t, TEXT_W, l.app_h), HAlign::Left);
    }
    if l.sum_h > 0.0 {
        let t = place(&mut ty, &mut first, l.sum_h);
        cv.text_wrap(&c.summary, &sum_style(p), rect(tx, t, TEXT_W, l.sum_h), HAlign::Left, 2);
    }
    if l.body_h > 0.0 {
        let t = place(&mut ty, &mut first, l.body_h);
        cv.text_wrap(&c.body, &body_style(p), rect(tx, t, TEXT_W, l.body_h), HAlign::Left, 3);
    }
    let mut hits = hits;
    if let Some((h, slot, vis)) = hits.as_mut() { h.add_in(r, *vis, *slot * 16); }
    if l.btn_row {
        let by = ty + 6.0 + if first { 0.0 } else { 2.0 };
        let st = btn_style(p);
        let mut x = tx;
        for (i, (_, label)) in action_buttons(c).into_iter().enumerate() {
            let label = cv.text.ellipsize(label, &st, TEXT_W - 20.0);
            let w = (cv.measure(&label, &st).0 + 20.0).min(TEXT_W);
            if x + w > tx + TEXT_W + 0.5 { break; }
            let br = rect(x, by, w, BTN_H);
            let hovered = match hits.as_mut() { Some((h, slot, vis)) => h.add_in(br, *vis, *slot * 16 + 1 + i as u32), None => false };
            cv.fill_rrect(br, r_all(9.0), with_a(p.fg, if hovered { 0.20 } else { 0.10 }));
            cv.text(&label, &st, br.center(), HAlign::Center, VAlign::Middle);
            x += w + 6.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn card_text_is_clipped_on_char_boundaries() {
        assert_eq!(clip_chars("short", 10), "short");
        assert_eq!(clip_chars(&"x".repeat(20000), CARD_BODY_CHARS).chars().count(), CARD_BODY_CHARS);
        assert_eq!(clip_chars("éééé", 2), "éé");
        assert_eq!(clip_chars("", 3), "");
    }

    #[test]
    fn markup_is_stripped_and_entities_decoded() {
        assert_eq!(strip_markup("<b>Hi</b> &amp; <i>bye</i> &lt;3 &quot;q&quot; it&#39;s"), "Hi & bye <3 \"q\" it's");
        assert_eq!(strip_markup("a < b"), "a ");
    }

    fn note(id: u32, urgency: u8) -> Note {
        Note { id, app: "A".into(), icon: String::new(), summary: "S".into(), body: "B".into(), actions: Vec::new(), urgency, timeout_ms: -1, suppress_sound: false }
    }

    fn tiny() -> DecodedImage { DecodedImage { width: 1, height: 1, rgba: vec![0u8; 4] } }

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

    fn sounds(enabled: bool, urgency: u8, suppress: bool, live: bool) -> bool {
        should_play(enabled, &Note { suppress_sound: suppress, ..note(1, urgency) }, live)
    }

    #[test]
    fn sound_policy_is_switch_urgency_hint_and_freshness() {
        assert!(sounds(true, 1, false, false) && sounds(true, 2, false, false));
        assert!(!sounds(false, 1, false, false), "switch off");
        assert!(!sounds(true, 0, false, false), "low urgency");
        assert!(!sounds(true, 1, true, false), "suppress-sound");
        assert!(!sounds(true, 1, false, true), "update of a live card");
    }

    #[test]
    fn sound_only_follows_the_dnd_gate_and_updates_never_repeat() {
        // dispatch already drops DND-rejected notes before `show`, so the sound decision in `show` never sees them
        let (rtx, _rrx) = mpsc::channel();
        let shown = std::cell::Cell::new(0);
        dispatch(Event::Notify { seq: 1, note: note(3, 1) }, || true, &rtx, |_| shown.set(shown.get() + 1), |_| {});
        assert_eq!(shown.get(), 0);
        let mut notes = Notes::default();
        let mut cmds = Vec::new();
        notes.show(note(5, 1), &mut cmds);
        assert!(notes.has_live_card(5) && !notes.has_live_card(6));
    }

    #[test]
    fn bursts_never_build_a_backlog_and_failures_do_not_stop_the_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let q = SoundQueue::start(move || {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Err(io::Error::other("player failed"))
        });
        for _ in 0..200 { q.request(); }
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "requests during playback are dropped");
        q.request();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the worker survives a failed player");
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
    fn card_interior_is_opaque_over_any_backdrop() {
        let mut rig = crate::testkit::Rig::new();
        let mut cmds = Vec::new();
        rig.st.notes.show(note(1, 1), &mut cmds);
        let mut v = CardsView::default();
        rig.run(&mut v, 1.0);
        let pm = rig.draw(&mut v);
        crate::ui::assert_opaque_interior(&pm, "notification card");
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
        let got: Vec<(u32, u64)> = (0..6).map(|_| { let d = done.recv().unwrap(); assert!(d.image.is_ok()); (d.id, d.generation) }).collect();
        assert_eq!(got, vec![(1, 10), (3, 30), (4, 40), (5, 50), (6, 60), (7, 70)]);
    }

    #[test]
    fn decoder_errors_come_back_as_errors_not_panics() {
        let (worker, done) = IconWorker::start(|_| Err("rejected".to_string()));
        worker.submit(IconJob { id: 1, generation: 1, path: PathBuf::from("/nope") });
        let d = done.recv().unwrap();
        assert_eq!((d.id, d.generation, d.image.err().as_deref()), (1, 1, Some("rejected")));
    }

    #[test]
    fn lifetimes_follow_urgency_and_timeout() {
        let n = |t: i32, u: u8| Note { timeout_ms: t, ..note(1, u) };
        assert_eq!(lifetime(&n(-1, 1)), Some(Duration::from_millis(DEFAULT_MS)));
        assert_eq!(lifetime(&n(2500, 0)), Some(Duration::from_millis(2500)));
        assert_eq!(lifetime(&n(0, 1)), None, "0 means persistent");
        assert_eq!(lifetime(&n(1000, 2)), None, "critical never expires on its own");
    }

    #[test]
    fn replacement_swaps_in_place_and_the_stack_is_capped() {
        let mut notes = Notes::default();
        let mut cmds = Vec::new();
        notes.show(note(1, 1), &mut cmds);
        assert!(matches!(cmds.as_slice(), [Cmd::Open { .. }]), "the first card opens the surface");
        notes.show(note(1, 1), &mut cmds);
        assert_eq!(notes.cards.len(), 1, "same id replaces the old card");
        assert_eq!(cmds.len(), 1, "no second surface");
        for id in 2..=7 { notes.show(note(id, 1), &mut cmds); }
        assert_eq!(notes.active(), MAX_CARDS);
        assert_eq!(notes.cards[0].note.id, 7, "newest first");
        assert!(notes.cards.iter().all(|c| c.note.id != 1), "the oldest was expired");
    }

    #[test]
    fn dismissal_slides_out_then_removes_and_expiry_fires_once_due() {
        let mut notes = Notes::default();
        let mut cmds = Vec::new();
        let mut n = note(5, 1);
        n.timeout_ms = 500;
        notes.show(n, &mut cmds);
        let t0 = Instant::now();
        assert!(notes.step(0.1, true, t0), "sliding in");
        assert!(notes.cards[0].progress > 0.0 && notes.cards[0].progress < 1.0);
        notes.step(1.0, true, t0);
        assert_eq!(notes.cards[0].progress, 1.0);
        assert!(!notes.cards[0].closing);
        notes.step(0.016, true, t0 + Duration::from_millis(600));
        assert!(notes.cards[0].closing, "expired by deadline");
        assert!(notes.cards[0].deadline.is_none());
        notes.step(1.0, true, t0 + Duration::from_secs(2));
        assert!(notes.cards.is_empty(), "removed once slid out");
        // no animations: immediate
        notes.show(note(6, 1), &mut cmds);
        notes.step(0.0, false, t0);
        assert_eq!(notes.cards[0].progress, 1.0);
        notes.close(6, REASON_DISMISSED, true);
        notes.step(0.0, false, t0);
        assert!(notes.cards.is_empty());
    }

    #[test]
    fn icon_arrival_only_applies_to_the_current_card() {
        let mut notes = Notes::default();
        let mut cmds = Vec::new();
        notes.show(note(1, 1), &mut cmds);
        let old_gen = notes.cards[0].generation;
        notes.show(note(1, 1), &mut cmds);
        notes.apply_icon(IconDone { id: 1, generation: old_gen, image: Ok(DecodedImage { width: 1, height: 1, rgba: vec![255; 4] }) });
        assert!(notes.cards[0].icon.is_none());
        let g = notes.cards[0].generation;
        notes.apply_icon(IconDone { id: 1, generation: g, image: Ok(DecodedImage { width: 1, height: 1, rgba: vec![255; 4] }) });
        assert!(notes.cards[0].icon.is_some());
    }

    #[test]
    fn preview_card_stack() {
        let mut rig = crate::testkit::Rig::new();
        let mut cmds = Vec::new();
        let mk = |id: u32, app: &str, sum: &str, body: &str, urg: u8, actions: &[(&str, &str)]| Note {
            id, app: app.into(), icon: String::new(), summary: sum.into(), body: body.into(),
            actions: actions.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), urgency: urg, timeout_ms: 0, suppress_sound: false,
        };
        rig.st.notes.show(mk(1, "Calendar", "Standup in 5 minutes", "", 0, &[]), &mut cmds);
        rig.st.notes.show(mk(2, "Discord", "<b>Ana</b> sent a message", "hey, are you joining the call? we are waiting for you in the usual channel and everyone is already here", 1, &[("default", "Open"), ("reply", "Reply"), ("mute", "Mute")]), &mut cmds);
        rig.st.notes.show(mk(3, "Battery", "Battery critically low", "Plug in the charger now", 2, &[("ok", "Dismiss")]), &mut cmds);
        let mut v = CardsView::default();
        rig.snap(&mut v, "rp_notify");
        assert_eq!(rig.st.notes.cards.len(), 3);
    }
}
