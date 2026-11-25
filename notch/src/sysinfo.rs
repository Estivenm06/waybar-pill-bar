//! System data layer: quick stats, clipboard history, AI usage, Controls-tile state and the pollers that keep
//! `state.status` (volume, Wi-Fi, Bluetooth, battery) current. Everything blocking runs on worker threads and
//! reaches the UI through `Poster::post`.
//!
//! `start` spawns the pollers once; `set_active(true)` (notch expanded) makes the heavier ones run and the status
//! pollers tick faster, `set_active(false)` parks them again.

use crate::power;
use crate::private_files;
use crate::state::{BtStatus, State, Volume, WifiStatus};
use crate::usage::{self, Usage};
use crate::wayland::Poster;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MAX_HIST: usize = 50;

pub const ICON_VOL: &str = "\u{f057e}";
pub const ICON_MUTE: &str = "\u{f075f}";

// ── state slice ─────────────────────────────────────────────────────────────

/// Outcome line of the last "clear clipboard" (shown on the Controls page).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipStatus { pub message: String, pub success: bool }

/// What the Controls tiles hand to Omarchy; each runs only after the notch is collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureAction { Screenshot, RecordStart, RecordStop }

impl CaptureAction {
    pub fn args(self) -> &'static [&'static str] {
        match self {
            CaptureAction::Screenshot => &["screenshot"],
            CaptureAction::RecordStart => &["screenrecord"],
            CaptureAction::RecordStop => &["screenrecord", "--stop-recording"],
        }
    }
    pub fn failure(self) -> &'static str {
        match self { CaptureAction::Screenshot => "Screenshot command failed", _ => "Recording command failed" }
    }
}

/// A Reading Mode / Do Not Disturb change worth a toast (never the very first read).
#[derive(Clone, Debug, PartialEq)]
pub struct ToggleEvent { pub glyph: &'static str, pub on: bool, pub seq: u64 }

#[derive(Default)]
pub struct Sys {
    /// CPU load 0..=100 (needs two samples; 0 until the first interval has passed).
    pub cpu: f32,
    /// RAM in use, 0..=100.
    pub ram: f32,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    /// Package/CPU temperature in °C.
    pub temp_c: Option<f32>,
    pub usage: Usage,
    /// Clipboard history, newest first, at most `MAX_HIST`.
    pub clip: Vec<String>,
    /// A clear is in progress: copies queued before it must not refill the history.
    clip_clearing: bool,
    pub clip_status: Option<ClipStatus>,
    pub recording: bool,
    pub nightlight: bool,
    pub dnd: bool,
    /// A screenshot/recording helper is running (or about to).
    pub capture_pending: bool,
    /// Failure text of the last capture helper, cleared on the next attempt.
    pub capture_error: Option<String>,
    /// One-off message for the Controls page (e.g. "Lock failed: …").
    pub message: Option<String>,
    pub toggle: Option<ToggleEvent>,
    toggle_seq: u64,
    nl_known: bool,
    dnd_known: bool,
    /// A sound-preference save is running.
    pub notification_sounds_pending: bool,
}

impl Sys {
    /// Add a copied text to the visible history (unless a clear is running). Memory only: the clipboard service owns
    /// the history file. True when the list changed.
    pub fn clip_push(&mut self, s: String) -> bool {
        !self.clip_clearing && push_history(&mut self.clip, s)
    }

    /// Home chip text for the newest copy.
    pub fn clip_chip(&self) -> String {
        match self.clip.first() {
            Some(e) => format!("\u{f0ea}  {}", clip_preview(e, 50)),
            None => "\u{f0ea}  Nothing copied yet".into(),
        }
    }

    fn set_nightlight(&mut self, on: bool) {
        if self.nl_known && self.nightlight != on { self.toast(if on { "\u{f06e}" } else { "\u{f070}" }, on); }
        self.nightlight = on;
        self.nl_known = true;
    }

    fn set_dnd(&mut self, on: bool) {
        if self.dnd_known && self.dnd != on { self.toast(if on { "\u{f0f4}" } else { "\u{f0f3}" }, on); }
        self.dnd = on;
        self.dnd_known = true;
    }

    fn toast(&mut self, glyph: &'static str, on: bool) {
        self.toggle_seq += 1;
        self.toggle = Some(ToggleEvent { glyph, on, seq: self.toggle_seq });
    }
}

// ── activity gate ───────────────────────────────────────────────────────────

struct Gate { active: AtomicBool, epoch: Mutex<u64>, cv: Condvar }

static GATE: Gate = Gate { active: AtomicBool::new(false), epoch: Mutex::new(0), cv: Condvar::new() };

impl Gate {
    fn set(&self, on: bool) {
        if self.active.swap(on, Ordering::SeqCst) != on {
            *self.epoch.lock().unwrap_or_else(|e| e.into_inner()) += 1;
            self.cv.notify_all();
        }
    }

    fn active(&self) -> bool { self.active.load(Ordering::SeqCst) }

    /// Sleep up to `d`; returns early when the active flag flips.
    fn sleep(&self, d: Duration) {
        let g = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        let start = *g;
        let _ = self.cv.wait_timeout_while(g, d, |e| *e == start);
    }

    fn wait_active(&self) {
        while !self.active() { self.sleep(Duration::from_secs(3600)); }
    }
}

/// Expanded notch: run the stats/usage/controls pollers and tick Wi-Fi/Bluetooth faster. Collapsed: dormant.
pub fn set_active(on: bool) { GATE.set(on); }

// ── start ───────────────────────────────────────────────────────────────────

/// Spawn every poller (once). Safe to call again; later calls do nothing.
pub fn start(poster: &Poster) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) { return; }
    let _ = CLIP.set(clip_service(poster, ClipConfig::system()));
    power_worker(poster.clone());
    volume_worker(poster.clone());
    net_worker(poster.clone());
    stats_worker(poster.clone());
    usage_worker(poster.clone());
    controls_worker(poster.clone());
}

// ── battery ─────────────────────────────────────────────────────────────────

fn power_worker(poster: Poster) {
    std::thread::spawn(move || {
        let mut previous = None;
        loop {
            let snapshot = power::snapshot();
            if previous.as_ref() != Some(&snapshot) {
                previous = Some(snapshot.clone());
                poster.post(move |st: &mut State, _| st.status.power = Some(snapshot));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Home chip text for the battery.
pub fn battery_chip(p: &power::PowerSnapshot) -> String {
    if !p.available { return "Power \u{2014}".into(); }
    if p.batteries.is_empty() {
        return if p.ac_online == Some(true) { "AC".into() } else { "No battery".into() };
    }
    let glyph = if power::actively_charging(p) { "\u{f0e7}" } else { "\u{f240}" };
    match p.percentage { Some(v) => format!("{glyph} {}%", v.round() as u32), None => format!("{glyph} \u{2014}") }
}

// ── volume ──────────────────────────────────────────────────────────────────

fn read_volume() -> Volume {
    match crate::audio::master() {
        Some((v, muted)) => Volume { available: true, percent: v.round().clamp(0.0, 255.0) as u8, muted },
        None => Volume::default(),
    }
}

fn post_volume(poster: &Poster, v: Volume) {
    poster.post(move |st: &mut State, _| st.status.volume = v);
}

/// Event-driven: `pactl subscribe` wakes the reader on sink/server changes; a slow timer covers a dead subscriber.
fn volume_worker(poster: Poster) {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || loop {
        let child = Command::new("pactl").arg("subscribe").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if (line.contains(" on sink #") || line.contains(" on server")) && tx.send(()).is_err() { return; }
                }
            }
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_secs(3));
    });
    std::thread::spawn(move || {
        let mut last: Option<Volume> = None;
        loop {
            let v = read_volume();
            if last.as_ref() != Some(&v) {
                last = Some(v.clone());
                post_volume(&poster, v);
            }
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(60)); // coalesce a burst (slider drags, scrolling)
                    while rx.try_recv().is_ok() {}
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Target of one scroll/key step: never pushes past 100 % (software boost is only reached with the slider) and
/// never pulls an already boosted level down on "up".
fn step_target(cur: f64, delta: i8) -> u8 {
    let t = cur + delta as f64;
    let t = if delta > 0 { t.min(cur.max(100.0)) } else { t };
    t.round().clamp(0.0, 150.0) as u8
}

fn run_pactl(args: &[&str]) {
    if let Err(e) = crate::audio::run_pactl(args) { eprintln!("pactl {}: {e}", args.join(" ")); }
}

/// Set the default sink volume (0..=150 %) and unmute it.
pub fn set_volume(poster: &Poster, percent: u8) {
    let percent = percent.min(150);
    poster.post(move |st: &mut State, _| {
        st.status.volume = Volume { available: true, percent, muted: false };
    });
    let poster = poster.clone();
    std::thread::spawn(move || {
        run_pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{percent}%")]);
        run_pactl(&["set-sink-mute", "@DEFAULT_SINK@", "0"]);
        post_volume(&poster, read_volume());
    });
}

/// Relative change in percentage points (e.g. ±5 for the pill's scroll wheel); raising also unmutes.
pub fn volume_step(poster: &Poster, delta: i8) {
    let poster = poster.clone();
    std::thread::spawn(move || {
        let Some((cur, _)) = crate::audio::master() else { return };
        run_pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{}%", step_target(cur, delta))]);
        if delta > 0 { run_pactl(&["set-sink-mute", "@DEFAULT_SINK@", "0"]); }
        post_volume(&poster, read_volume());
    });
}

pub fn toggle_mute(poster: &Poster) {
    let poster = poster.clone();
    std::thread::spawn(move || {
        run_pactl(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]);
        post_volume(&poster, read_volume());
    });
}

/// Current level for the OSD: `kind` is "volume" or "brightness". Returns (percent, muted). Blocking.
pub fn read_osd(kind: &str) -> (f64, bool) {
    if kind == "brightness" {
        let out = crate::net::sh_out("omarchy-brightness-display", &["--no-osd"]);
        (out.trim_end_matches('%').parse::<f64>().unwrap_or(0.0), false)
    } else {
        crate::audio::master().unwrap_or((0.0, false))
    }
}

/// Home chip text for the volume.
pub fn volume_chip(v: &Volume) -> String {
    if !v.available { "Audio unavailable".into() }
    else if v.muted { format!("{ICON_MUTE} Muted") }
    else { format!("{ICON_VOL} {}%", v.percent) }
}

/// Home chip text for Wi-Fi.
pub fn wifi_chip(w: &WifiStatus) -> String {
    if !w.on { return "\u{f092e} off".into(); }
    match &w.ssid { Some(n) => format!("\u{f1eb} {n}"), None => "\u{f1eb} offline".into() }
}

/// Home chip text for Bluetooth.
pub fn bt_chip(b: &BtStatus) -> String {
    format!("\u{f293} {}", if b.on { "on" } else { "off" })
}

// ── Wi-Fi / Bluetooth status ────────────────────────────────────────────────

/// Keeps `status.wifi` / `status.bt` fresh: every 12 s collapsed, every 5 s while the notch is open.
fn net_worker(poster: Poster) {
    std::thread::spawn(move || {
        let (mut wifi, mut bt): (Option<WifiStatus>, Option<BtStatus>) = (None, None);
        loop {
            let w = crate::net::wifi_status();
            if wifi.as_ref() != Some(&w) {
                wifi = Some(w.clone());
                poster.post(move |st: &mut State, _| st.status.wifi = w);
            }
            let b = crate::net::bt_status();
            if bt.as_ref() != Some(&b) {
                bt = Some(b.clone());
                poster.post(move |st: &mut State, _| crate::net::apply_bt_status(st, b));
            }
            GATE.sleep(Duration::from_secs(if GATE.active() { 5 } else { 12 }));
        }
    });
}

// ── clipboard ───────────────────────────────────────────────────────────────
//
// One coordinator thread owns the history mirror, every history-file read/write, and the `wl-paste --watch` child
// (its own process group) with the reader thread that frames its output. The UI thread only mirrors what the
// coordinator posts, tagged with a generation that Clear bumps, so nothing read before a Clear can reappear after it.

/// Longest selection kept, in bytes. The watcher's shell applies it before NULs are removed.
const CLIP_MAX_BYTES: usize = 20000;
/// Run by `wl-paste --watch` for every selection: cap, drain what `head` left (wl-paste must never see a closed pipe),
/// strip NULs so the payload cannot forge a frame delimiter, then emit the single NUL that ends the frame.
const CLIP_FILTER: &str = "{ head -c 20000; cat >/dev/null; } | tr -d '\\000'; printf '\\0'";

struct ClipConfig {
    history: PathBuf,
    /// Program and arguments of the watcher; each selection arrives as one NUL-terminated frame on its stdout.
    watch: Vec<String>,
    /// Program and arguments that clear the system selection.
    clear: Vec<String>,
    restart_delay: Duration,
    /// Test hook: the coordinator waits here after capturing its startup generation and before reading the file.
    #[cfg(test)]
    startup_gate: Option<Arc<std::sync::Barrier>>,
}

impl ClipConfig {
    fn system() -> ClipConfig {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
        ClipConfig {
            history: hist_path(),
            watch: v(&["wl-paste", "--type", "text", "--watch", "sh", "-c", CLIP_FILTER]),
            clear: v(&["wl-copy", "--clear"]),
            restart_delay: Duration::from_secs(2),
            #[cfg(test)]
            startup_gate: None,
        }
    }
}

enum ClipCommand {
    Frame { generation: u64, text: String },
    Applied { generation: u64 },
    Clear { generation: u64 },
    Resume { generation: u64 },
    Ended { generation: u64 },
    #[cfg(test)]
    Shutdown,
}

enum ReaderRelease { Next, Stop }

#[derive(Clone)]
struct ClipService {
    generation: Arc<AtomicU64>,
    tx: mpsc::Sender<ClipCommand>,
}

static CLIP: OnceLock<ClipService> = OnceLock::new();

fn clip_service(poster: &Poster, cfg: ClipConfig) -> ClipService {
    let (tx, rx) = mpsc::channel();
    let svc = ClipService { generation: Arc::new(AtomicU64::new(0)), tx };
    let startup = svc.generation.load(Ordering::SeqCst);
    let (poster, inner) = (poster.clone(), svc.clone());
    std::thread::spawn(move || clip_coordinator(cfg, poster, inner, startup, rx));
    svc
}

struct Watcher {
    generation: u64,
    child: std::process::Child,
    release: mpsc::Sender<ReaderRelease>,
    reader: std::thread::JoinHandle<()>,
}

impl Watcher {
    fn spawn(cfg: &ClipConfig, generation: u64, tx: &mpsc::Sender<ClipCommand>) -> Option<Watcher> {
        use std::os::unix::process::CommandExt;
        let (prog, args) = cfg.watch.split_first()?;
        let mut child = Command::new(prog).args(args)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0).spawn().ok()?;
        let Some(out) = child.stdout.take() else { kill_group(&mut child); return None };
        let (release, rx) = mpsc::channel();
        let tx = tx.clone();
        let reader = std::thread::spawn(move || clip_reader(out, generation, tx, rx));
        Some(Watcher { generation, child, release, reader })
    }

    /// Stop the reader, kill the whole watcher process group (so no descendant keeps stdout open), reap, join.
    fn stop(mut self) {
        let _ = self.release.send(ReaderRelease::Stop);
        kill_group(&mut self.child);
        let _ = self.reader.join();
    }
}

/// SIGKILL the child's process group, then reap the child. Called before the reap, so the group id cannot be reused.
fn kill_group(child: &mut std::process::Child) {
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
    let _ = child.wait();
}

/// Owns the watcher's stdout. Sends one frame, then waits to be released, so at most one selection is outstanding.
/// Bytes read past a frame's terminator are kept for the next frame; an oversized frame is dropped through its end.
fn clip_reader(mut out: std::process::ChildStdout, generation: u64, tx: mpsc::Sender<ClipCommand>, release: mpsc::Receiver<ReaderRelease>) {
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut frame: Vec<u8> = Vec::new();
        let mut oversized = false;
        loop {
            let end = carry.iter().position(|&b| b == 0);
            let piece_len = end.unwrap_or(carry.len());
            if frame.len() + piece_len > CLIP_MAX_BYTES { oversized = true; } else { frame.extend_from_slice(&carry[..piece_len]); }
            if let Some(i) = end { carry.drain(..=i); break; }
            carry.clear();
            match out.read(&mut buf) {
                Ok(n) if n > 0 => carry.extend_from_slice(&buf[..n]),
                _ => { let _ = tx.send(ClipCommand::Ended { generation }); return; }
            }
        }
        if oversized { continue; }
        let text = String::from_utf8_lossy(&frame).into_owned();
        if tx.send(ClipCommand::Frame { generation, text }).is_err() { return; }
        match release.recv() { Ok(ReaderRelease::Next) => {} _ => return }
    }
}

fn clip_coordinator(cfg: ClipConfig, poster: Poster, svc: ClipService, startup: u64, rx: mpsc::Receiver<ClipCommand>) {
    let ClipService { generation: shared, tx } = svc;
    #[cfg(test)]
    if let Some(gate) = &cfg.startup_gate { gate.wait(); }
    let mut mirror = load_history_at(&cfg.history).unwrap_or_else(|e| { eprintln!("clipboard history: {e}"); Vec::new() });
    mirror.truncate(MAX_HIST);
    {
        let (snapshot, shared, ack) = (mirror.clone(), shared.clone(), tx.clone());
        poster.post(move |st: &mut State, _| {
            if shared.load(Ordering::SeqCst) == startup && !st.sys.clip_clearing { st.sys.clip = snapshot; }
            let _ = ack.send(ClipCommand::Applied { generation: startup });
        });
    }
    let mut watcher: Option<Watcher> = None;
    let mut restart_at: Option<Instant> = None;
    let mut starting = true;
    // (Re)start capture for `generation`, or schedule a retry.
    let begin = |watcher: &mut Option<Watcher>, restart_at: &mut Option<Instant>, generation: u64| {
        *watcher = Watcher::spawn(&cfg, generation, &tx);
        *restart_at = if watcher.is_none() { Some(Instant::now() + cfg.restart_delay) } else { None };
    };
    loop {
        let cmd = match restart_at {
            Some(t) => match rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() { Ok(c) => Some(c), Err(_) => break },
        };
        let current = shared.load(Ordering::SeqCst);
        let Some(cmd) = cmd else { begin(&mut watcher, &mut restart_at, current); continue };
        match cmd {
            ClipCommand::Applied { generation } if generation == current => match &watcher {
                Some(w) if w.generation == generation => { let _ = w.release.send(ReaderRelease::Next); }
                None if starting && restart_at.is_none() => { starting = false; begin(&mut watcher, &mut restart_at, current); }
                _ => {}
            },
            ClipCommand::Resume { generation } if generation == current && watcher.is_none() => {
                starting = false;
                begin(&mut watcher, &mut restart_at, current);
            }
            ClipCommand::Frame { generation, text } if generation == current => {
                let Some(w) = watcher.as_ref().filter(|w| w.generation == generation) else { continue };
                if !push_history(&mut mirror, text.clone()) { let _ = w.release.send(ReaderRelease::Next); continue; }
                if let Err(e) = save_history_at(&cfg.history, &mirror) { eprintln!("clipboard history: {e}"); }
                let (shared, ack) = (shared.clone(), tx.clone());
                poster.post(move |st: &mut State, _| {
                    if shared.load(Ordering::SeqCst) == generation && !st.sys.clip_clearing { st.sys.clip_push(text); }
                    let _ = ack.send(ClipCommand::Applied { generation });
                });
            }
            ClipCommand::Ended { generation } if generation == current && watcher.as_ref().is_some_and(|w| w.generation == generation) => {
                if let Some(w) = watcher.take() { w.stop(); }
                restart_at = Some(Instant::now() + cfg.restart_delay);
            }
            ClipCommand::Clear { generation } => {
                if let Some(w) = watcher.take() { w.stop(); }
                restart_at = None;
                starting = false;
                mirror.clear();
                let disk_cleared = match save_history_at(&cfg.history, &[]) {
                    Ok(()) => true,
                    Err(e) => { eprintln!("clipboard history: {e}"); false }
                };
                let (message, success) = system_clear(&cfg.clear);
                let (mut message, success) = clear_result(disk_cleared, message, success);
                if !success { message.push_str("; clipboard capture paused until Clear is retried"); }
                let (shared, ack) = (shared.clone(), tx.clone());
                poster.post(move |st: &mut State, _| {
                    if shared.load(Ordering::SeqCst) != generation { return; }
                    st.sys.clip_clearing = false;
                    st.sys.clip_status = Some(ClipStatus { message, success });
                    if success { let _ = ack.send(ClipCommand::Resume { generation }); }
                });
            }
            #[cfg(test)]
            ClipCommand::Shutdown => break,
            _ => {} // a stale generation: Frame/Applied/Ended from before the latest Clear
        }
    }
    if let Some(w) = watcher.take() { w.stop(); }
}

/// Clear the Wayland selection. A missing program is a reported unavailable system clipboard, not a success.
fn system_clear(cmd: &[String]) -> (String, bool) {
    let Some((prog, args)) = cmd.split_first() else { return ("History cleared; system clipboard unavailable".to_string(), false) };
    if !on_path(prog) { return ("History cleared; system clipboard unavailable".to_string(), false); }
    match Command::new(prog).args(args).status() {
        Ok(s) if s.success() => ("Clipboard cleared".to_string(), true),
        Ok(s) => (format!("History cleared; {prog} --clear failed ({s})"), false),
        Err(e) => (format!("History cleared; {prog} failed: {e}"), false),
    }
}

/// Insert `s` at the front: trimmed of trailing newlines, blank entries ignored, duplicates moved up, capped.
fn push_history(hist: &mut Vec<String>, s: String) -> bool {
    let s = s.trim_end_matches('\n').to_string();
    if s.trim().is_empty() { return false; }
    hist.retain(|x| *x != s);
    hist.insert(0, s);
    hist.truncate(MAX_HIST);
    true
}

/// First non-blank line, ellipsised to `max` characters (as the Home chip and the history rows show it).
pub fn clip_preview(entry: &str, max: usize) -> String {
    let one = entry.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if one.chars().count() > max { one.chars().take(max - 1).collect::<String>() + "…" } else { one.to_string() }
}

/// Put `text` on the Wayland clipboard.
pub fn clip_copy(text: &str) {
    if let Ok(mut c) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
        if let Some(mut si) = c.stdin.take() { let _ = si.write_all(text.as_bytes()); }
        std::thread::spawn(move || { let _ = c.wait(); });
    }
}

/// Clear the history (memory, file) and the Wayland selection. The UI half runs at once: it bumps the shared
/// generation, empties the visible list and stops accepting captures; the clipboard service then stops the watcher,
/// persists `[]` and clears the selection, and its tagged result lands in `sys.clip_status`. Capture resumes only
/// after a fully successful clear. A clear already in progress absorbs further requests.
pub fn clip_clear(poster: &Poster) {
    if let Some(svc) = CLIP.get() { clip_clear_with(svc, poster); }
}

fn clip_clear_with(svc: &ClipService, poster: &Poster) {
    let svc = svc.clone();
    poster.post(move |st: &mut State, _| {
        if st.sys.clip_clearing { return; }
        let generation = svc.generation.fetch_add(1, Ordering::SeqCst) + 1;
        st.sys.clip_clearing = true;
        st.sys.clip_status = None;
        st.sys.clip.clear();
        let _ = svc.tx.send(ClipCommand::Clear { generation });
    });
}

fn hist_path() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(base).join("waybar-notch").join("clipboard.json")
}

/// Largest clipboard history file we are willing to read back.
const MAX_HISTORY_BYTES: u64 = 8 * 1024 * 1024;

/// Reads the clipboard history. A missing file is an empty history; the content is never echoed in errors.
fn load_history_at(path: &Path) -> Result<Vec<String>, String> {
    match private_files::read_private_bytes(path, MAX_HISTORY_BYTES)? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| format!("{} is not valid clipboard history", path.display())),
    }
}

static HIST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically replaces the history file; the content only ever exists in 0600 files.
fn save_history_at(path: &Path, history: &[String]) -> Result<(), String> {
    let text = serde_json::to_string(history).map_err(|e| e.to_string())?;
    private_files::write_private_file(path, text.as_bytes(), ".clipboard.json.tmp", &HIST_SEQ)
}

/// Outcome shown after a clear: it is only a full success when the history file was cleared as well.
fn clear_result(disk_cleared: bool, message: String, success: bool) -> (String, bool) {
    if disk_cleared { (message, success) } else { ("Clipboard history file could not be cleared".to_string(), false) }
}

pub fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

// ── quick stats (CPU / RAM / temperature) ───────────────────────────────────

/// (busy jiffies, total jiffies) from the aggregate `cpu` line of /proc/stat.
fn parse_cpu(stat: &str) -> Option<(u64, u64)> {
    let line = stat.lines().next()?.strip_prefix("cpu")?;
    let v: Vec<u64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 { return None; }
    let idle = v[3] + v[4];
    let total: u64 = v.iter().take(8).sum();
    Some((total.saturating_sub(idle), total))
}

fn cpu_percent(prev: (u64, u64), cur: (u64, u64)) -> f32 {
    let (db, dt) = (cur.0.saturating_sub(prev.0), cur.1.saturating_sub(prev.1));
    if dt == 0 { 0.0 } else { (db as f32 / dt as f32 * 100.0).clamp(0.0, 100.0) }
}

/// (used GB, total GB) from /proc/meminfo, "used" being total minus MemAvailable.
fn parse_meminfo(text: &str) -> Option<(f32, f32)> {
    let kb = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)?.trim().strip_suffix("kB")?.trim().parse::<f64>().ok());
    let (total, avail) = (kb("MemTotal:")?, kb("MemAvailable:")?);
    if total <= 0.0 { return None; }
    let gb = 1024.0 * 1024.0;
    Some((((total - avail).max(0.0) / gb) as f32, (total / gb) as f32))
}

/// CPU temperature: a labelled package sensor of a known CPU hwmon driver, else the first thermal zone.
fn read_temp() -> Option<f32> {
    let milli = |p: &Path| std::fs::read_to_string(p).ok()?.trim().parse::<f32>().ok().map(|m| m / 1000.0);
    for dir in std::fs::read_dir("/sys/class/hwmon").ok()?.flatten().map(|e| e.path()) {
        let name = std::fs::read_to_string(dir.join("name")).unwrap_or_default();
        if !matches!(name.trim(), "coretemp" | "k10temp" | "zenpower" | "cpu_thermal") { continue; }
        let mut first = None;
        for i in 1..=16 {
            let Some(t) = milli(&dir.join(format!("temp{i}_input"))) else { continue };
            let label = std::fs::read_to_string(dir.join(format!("temp{i}_label"))).unwrap_or_default();
            if matches!(label.trim(), "Package id 0" | "Tctl" | "Tdie") { return Some(t); }
            first.get_or_insert(t);
        }
        if first.is_some() { return first; }
    }
    let mut zones: Vec<PathBuf> = std::fs::read_dir("/sys/class/thermal").ok()?.flatten().map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("thermal_zone"))).collect();
    zones.sort();
    let pkg = zones.iter().find(|z| std::fs::read_to_string(z.join("type")).is_ok_and(|t| t.trim() == "x86_pkg_temp"));
    pkg.or(zones.first()).and_then(|z| milli(&z.join("temp")))
}

fn read_cpu_sample() -> Option<(u64, u64)> { parse_cpu(&std::fs::read_to_string("/proc/stat").ok()?) }

/// While the notch is expanded: CPU/RAM/temperature every 2 s.
fn stats_worker(poster: Poster) {
    std::thread::spawn(move || {
        let mut prev: Option<((u64, u64), Instant)> = None;
        loop {
            GATE.wait_active();
            let Some(mut sample) = read_cpu_sample() else { GATE.sleep(Duration::from_secs(2)); continue };
            let base = match prev {
                Some((p, at)) if at.elapsed() < Duration::from_secs(6) => p,
                _ => { // first sample after a dormant spell: measure a short interval instead
                    let first = sample;
                    std::thread::sleep(Duration::from_millis(300));
                    sample = read_cpu_sample().unwrap_or(first);
                    first
                }
            };
            prev = Some((sample, Instant::now()));
            let cpu = cpu_percent(base, sample);
            let mem = std::fs::read_to_string("/proc/meminfo").ok().and_then(|t| parse_meminfo(&t));
            let temp = read_temp();
            poster.post(move |st: &mut State, _| {
                let s = &mut st.sys;
                s.cpu = cpu;
                if let Some((used, total)) = mem {
                    s.ram_used_gb = used;
                    s.ram_total_gb = total;
                    s.ram = if total > 0.0 { used / total * 100.0 } else { 0.0 };
                }
                s.temp_c = temp;
            });
            GATE.sleep(Duration::from_secs(2));
        }
    });
}

// ── AI usage ────────────────────────────────────────────────────────────────

/// Polls AI usage every 2 s while the notch is expanded; the tracker only reads new log bytes.
fn usage_worker(poster: Poster) {
    std::thread::spawn(move || {
        let mut tracker = usage::Tracker::default();
        let mut last: Option<Usage> = None;
        loop {
            GATE.wait_active();
            let u = tracker.poll(&usage::Sources::detect(), chrono::Local::now());
            if last.as_ref() != Some(&u) {
                last = Some(u.clone());
                poster.post(move |st: &mut State, _| st.sys.usage = u);
            }
            GATE.sleep(Duration::from_secs(2));
        }
    });
}

// ── Controls tiles ──────────────────────────────────────────────────────────

/// Same match Omarchy uses to find its recorder; `-x` would fail on the truncated comm name.
pub fn recording_active() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| {
        e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.starts_with(b"gpu-screen-recorder"))
    })
}

/// Hyprsunset's colour temperature below the 6000K identity value means Reading Mode is on.
fn parse_nightlight(text: &str) -> bool {
    let digits: String = text.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().is_ok_and(|t| t < 6000)
}

/// Same predicate as `omarchy toggle nightlight --status`. Unavailable or malformed output is "off".
pub fn nightlight_enabled() -> bool {
    let Ok(out) = Command::new("hyprctl").args(["hyprsunset", "temperature"]).stderr(Stdio::null()).output() else { return false };
    out.status.success() && parse_nightlight(&String::from_utf8_lossy(&out.stdout))
}

fn dnd_path() -> String {
    format!("{}/.local/state/omarchy/notifications.json", std::env::var("HOME").unwrap_or_default())
}

fn parse_dnd(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v["dnd"].as_bool()).unwrap_or(false)
}

/// Do Not Disturb lives in Omarchy's notification state file.
pub fn dnd_enabled() -> bool {
    std::fs::read_to_string(dnd_path()).map(|s| parse_dnd(&s)).unwrap_or(false)
}

fn write_dnd(on: bool) {
    let p = dnd_path();
    if let Some(dir) = Path::new(&p).parent() { let _ = std::fs::create_dir_all(dir); }
    let _ = std::fs::write(&p, format!("{{\n  \"version\": 3,\n  \"dnd\": {on}\n}}\n"));
}

/// Reading Mode writes in flight (`omarchy toggle nightlight` can take ~2 s) and a counter bumped by each toggle.
/// A poll that started before a toggle, or ran while one was pending, must not publish its stale reading over it.
static NL_PENDING: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static NL_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn post_controls(poster: &Poster, recording: bool, nightlight: Option<bool>, dnd: bool) {
    poster.post(move |st: &mut State, _| {
        st.sys.recording = recording;
        if let Some(n) = nightlight { st.sys.set_nightlight(n); }
        st.sys.set_dnd(dnd);
    });
}

fn refresh_controls(poster: &Poster) {
    use std::sync::atomic::Ordering::SeqCst;
    let epoch = NL_EPOCH.load(SeqCst);
    let nl = nightlight_enabled();
    let fresh = NL_PENDING.load(SeqCst) == 0 && NL_EPOCH.load(SeqCst) == epoch;
    post_controls(poster, recording_active(), fresh.then_some(nl), dnd_enabled());
}

/// Recording / Reading Mode / DND state every 3 s while the notch is expanded.
fn controls_worker(poster: Poster) {
    std::thread::spawn(move || loop {
        GATE.wait_active();
        refresh_controls(&poster);
        GATE.sleep(Duration::from_secs(3));
    });
}

/// Flip Reading Mode through Omarchy: the tile flips at once, a second click while the first is still being applied
/// is ignored (it would flip it straight back), and the real state is published when the command finishes.
pub fn toggle_nightlight(poster: &Poster) {
    use std::sync::atomic::Ordering::SeqCst;
    if NL_PENDING.fetch_add(1, SeqCst) > 0 { NL_PENDING.fetch_sub(1, SeqCst); return; }
    NL_EPOCH.fetch_add(1, SeqCst);
    poster.post(|st: &mut State, _| { let on = !st.sys.nightlight; st.sys.set_nightlight(on); });
    let poster = poster.clone();
    std::thread::spawn(move || {
        let _ = Command::new("sh").arg("-c").arg("omarchy toggle nightlight")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        let on = nightlight_enabled();
        NL_EPOCH.fetch_add(1, SeqCst);
        NL_PENDING.fetch_sub(1, SeqCst);
        poster.post(move |st: &mut State, _| st.sys.set_nightlight(on));
    });
}

/// Flip Do Not Disturb (notifications are silenced while on).
pub fn toggle_dnd(poster: &Poster) {
    write_dnd(!dnd_enabled());
    let on = dnd_enabled();
    poster.post(move |st: &mut State, _| st.sys.set_dnd(on));
}

/// Flip notification sounds (`notch.behavior.notification_sounds`, not DND). The save runs off the UI thread; the
/// tile changes only once it succeeded, and clicks while a save is pending are ignored.
pub fn toggle_notification_sounds(st: &mut State, poster: &Poster) {
    if st.sys.notification_sounds_pending { return; }
    st.sys.notification_sounds_pending = true;
    st.sys.message = None;
    let poster = poster.clone();
    std::thread::spawn(move || {
        let r = crate::config::toggle_notification_sounds();
        poster.post(move |st: &mut State, _| apply_notification_sounds_result(st, r));
    });
}

fn apply_notification_sounds_result(st: &mut State, r: Result<bool, String>) {
    st.sys.notification_sounds_pending = false;
    match r {
        Ok(on) => st.settings.behavior.notification_sounds = on,
        Err(e) => st.sys.message = Some(format!("Notification sounds not saved: {e}")),
    }
}

/// Run one capture helper. The caller collapses the notch and waits for the collapsed frames first.
/// `capture_pending` is set until the helper exits; a failure ends up in `capture_error`.
pub fn capture(poster: &Poster, action: CaptureAction) {
    poster.post(|st: &mut State, _| { st.sys.capture_pending = true; st.sys.capture_error = None; });
    let poster = poster.clone();
    std::thread::spawn(move || {
        // stdout must not be piped: a background recorder inherits it and would hold the pipe open
        let r = match Command::new("omarchy").args(action.args()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit()).status() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(s.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let recording = recording_active();
        poster.post(move |st: &mut State, _| {
            st.sys.capture_pending = false;
            st.sys.recording = recording;
            st.sys.capture_error = r.err().map(|e| format!("{}: {e}", action.failure()));
        });
    });
}

/// `omarchy system lock`; a failure shows up in `sys.message`.
pub fn lock_screen(poster: &Poster) {
    let poster = poster.clone();
    std::thread::spawn(move || {
        let msg = match Command::new("omarchy").args(["system", "lock"]).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
            Ok(o) if o.status.success() => return,
            Ok(o) => format!("Lock failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
            Err(e) => format!("Lock failed: {e}"),
        };
        poster.post(move |st: &mut State, _| st.sys.message = Some(msg));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_toggle_result_changes_only_the_sound_setting() {
        let mut st = State::new();
        st.sys.notification_sounds_pending = true;
        st.sys.dnd = true;
        st.sys.message = Some("old".into());
        apply_notification_sounds_result(&mut st, Ok(true));
        assert!(st.settings.behavior.notification_sounds && !st.sys.notification_sounds_pending && st.sys.dnd);
        st.sys.notification_sounds_pending = true;
        apply_notification_sounds_result(&mut st, Err("disk full".into()));
        assert!(st.settings.behavior.notification_sounds, "failure leaves the saved value");
        assert!(!st.sys.notification_sounds_pending);
        assert_eq!(st.sys.message.as_deref(), Some("Notification sounds not saved: disk full"));
    }

    #[test]
    fn history_push_dedups_trims_and_caps() {
        let mut h = vec!["a".to_string(), "b".to_string()];
        assert!(push_history(&mut h, "b\n\n".into()));
        assert_eq!(h, ["b", "a"]);
        assert!(!push_history(&mut h, " \n\t".into()));
        assert_eq!(h, ["b", "a"]);
        for i in 0..80 { push_history(&mut h, format!("e{i}")); }
        assert_eq!((h.len(), h[0].as_str()), (MAX_HIST, "e79"));
    }

    #[test]
    fn clearing_blocks_queued_copies() {
        let mut s = Sys { clip_clearing: true, ..Sys::default() };
        assert!(!s.clip_push("queued before the clear".into()));
        assert!(s.clip.is_empty());
    }

    #[test]
    fn previews_use_the_first_non_blank_line() {
        assert_eq!(clip_preview("\n  hello \nworld", 50), "hello");
        assert_eq!(clip_preview(&"x".repeat(60), 50), format!("{}…", "x".repeat(49)));
        assert_eq!(clip_preview("", 50), "");
        let s = Sys { clip: vec!["copied".into()], ..Sys::default() };
        assert_eq!(s.clip_chip(), "\u{f0ea}  copied");
        assert_eq!(Sys::default().clip_chip(), "\u{f0ea}  Nothing copied yet");
    }

    #[test]
    fn cpu_and_memory_parsing() {
        let a = parse_cpu("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 1 1 1 1\n").unwrap();
        assert_eq!(a, (200, 1000));
        let b = parse_cpu("cpu  200 0 200 750 150 0 0 0 0 0\n").unwrap();
        assert!((cpu_percent(a, b) - 66.667).abs() < 0.01);
        assert_eq!(cpu_percent(a, a), 0.0);
        assert_eq!(parse_cpu("intr 1 2 3"), None);
        let (used, total) = parse_meminfo("MemTotal:       16777216 kB\nMemFree: 1 kB\nMemAvailable:    8388608 kB\n").unwrap();
        assert!((used - 8.0).abs() < 0.001 && (total - 16.0).abs() < 0.001);
        assert_eq!(parse_meminfo("MemTotal: 5 kB\n"), None);
    }

    #[test]
    fn volume_steps_stay_within_bounds() {
        assert_eq!(step_target(50.0, 5), 55);
        assert_eq!(step_target(98.0, 5), 100);
        assert_eq!(step_target(120.0, 5), 120); // boosted: "up" never lowers it, never raises it
        assert_eq!(step_target(120.0, -5), 115);
        assert_eq!(step_target(3.0, -5), 0);
        assert_eq!(step_target(100.0, 5), 100);
    }

    #[test]
    fn chips_follow_the_legacy_texts() {
        assert_eq!(volume_chip(&Volume { available: true, percent: 42, muted: false }), format!("{ICON_VOL} 42%"));
        assert_eq!(volume_chip(&Volume { available: true, percent: 42, muted: true }), format!("{ICON_MUTE} Muted"));
        assert_eq!(volume_chip(&Volume::default()), "Audio unavailable");
        let w = |on, ssid: Option<&str>| WifiStatus { available: true, on, ssid: ssid.map(str::to_string), signal: 50 };
        assert_eq!(wifi_chip(&w(false, None)), "\u{f092e} off");
        assert_eq!(wifi_chip(&w(true, None)), "\u{f1eb} offline");
        assert_eq!(wifi_chip(&w(true, Some("Home"))), "\u{f1eb} Home");
        assert_eq!(bt_chip(&BtStatus { available: true, on: true, connected: vec![] }), "\u{f293} on");
        let p = |bat: bool, ac, pct, charging| power::PowerSnapshot {
            available: true, ac_online: ac, percentage: pct,
            batteries: if bat { vec![power::BatterySnapshot {
                name: "BAT".into(), state: if charging { power::BatteryState::Charging } else { power::BatteryState::Discharging },
                percentage: pct, power_w: None, energy_now_wh: None, energy_full_wh: None, energy_design_wh: None, health_pct: None,
                voltage_v: None, cycles: None, technology: None, manufacturer: None, model: None, time_to_empty_secs: None, time_to_full_secs: None,
            }] } else { vec![] },
        };
        assert_eq!(battery_chip(&p(true, Some(false), Some(71.6), false)), "\u{f240} 72%");
        assert_eq!(battery_chip(&p(true, Some(true), Some(40.0), true)), "\u{f0e7} 40%");
        assert_eq!(battery_chip(&p(true, None, None, false)), "\u{f240} \u{2014}");
        assert_eq!(battery_chip(&p(false, Some(true), None, false)), "AC");
        assert_eq!(battery_chip(&p(false, Some(false), None, false)), "No battery");
        assert_eq!(battery_chip(&power::PowerSnapshot { available: false, ac_online: None, batteries: vec![], percentage: None }), "Power \u{2014}");
    }

    #[test]
    fn nightlight_and_dnd_parsing() {
        assert!(parse_nightlight("4000"));
        assert!(parse_nightlight("Current temperature: 3500K\n"));
        assert!(!parse_nightlight("6000"));
        assert!(!parse_nightlight("unavailable"));
        assert!(parse_dnd(r#"{"version": 3, "dnd": true}"#));
        assert!(!parse_dnd(r#"{"dnd": "yes"}"#));
        assert!(!parse_dnd("not json"));
    }

    #[test]
    fn toggles_toast_only_on_change_after_the_first_read() {
        let mut s = Sys::default();
        s.set_nightlight(true);
        assert!(s.toggle.is_none());
        s.set_nightlight(true);
        assert!(s.toggle.is_none());
        s.set_nightlight(false);
        assert_eq!(s.toggle, Some(ToggleEvent { glyph: "\u{f070}", on: false, seq: 1 }));
        s.set_dnd(false);
        s.set_dnd(true);
        assert_eq!(s.toggle, Some(ToggleEvent { glyph: "\u{f0f4}", on: true, seq: 2 }));
    }

    #[test]
    fn capture_actions_map_to_omarchy_args() {
        assert_eq!(CaptureAction::RecordStop.args(), ["screenrecord", "--stop-recording"]);
        assert_eq!(CaptureAction::Screenshot.failure(), "Screenshot command failed");
        assert_eq!(CaptureAction::RecordStart.failure(), "Recording command failed");
    }

    #[test]
    fn gate_wakes_sleepers_when_the_flag_flips() {
        let g = std::sync::Arc::new(Gate { active: AtomicBool::new(false), epoch: Mutex::new(0), cv: Condvar::new() });
        let sleeper = g.clone();
        let t = std::thread::spawn(move || { let s = Instant::now(); sleeper.sleep(Duration::from_secs(30)); s.elapsed() });
        std::thread::sleep(Duration::from_millis(100));
        g.set(true);
        assert!(t.join().unwrap() < Duration::from_secs(5));
        assert!(g.active());
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hist-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(p: &Path) -> u32 { std::fs::metadata(p).unwrap().mode() & 0o777 }

    #[test]
    fn create_update_clear_reload_roundtrip() {
        let d = tmpdir("roundtrip");
        let p = d.join("waybar-notch").join("clipboard.json");
        assert_eq!(load_history_at(&p).unwrap(), Vec::<String>::new());
        let h = vec!["synthetic-secret".to_string(), "second".to_string()];
        save_history_at(&p, &h).unwrap();
        assert_eq!(load_history_at(&p).unwrap(), h);
        save_history_at(&p, &["third".to_string()]).unwrap();
        assert_eq!(load_history_at(&p).unwrap(), vec!["third".to_string()]);
        save_history_at(&p, &[]).unwrap();
        assert_eq!(load_history_at(&p).unwrap(), Vec::<String>::new());
        let names: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Child half of `files_are_private_under_umask_022`; a subprocess keeps the umask change local.
    #[test]
    fn history_umask_child() {
        let Ok(d) = std::env::var("HIST_CHILD_DIR") else { return };
        let p = Path::new(&d).join("waybar-notch").join("clipboard.json");
        save_history_at(&p, &["synthetic".to_string()]).unwrap();
        let before = (std::fs::read(&p).unwrap(), std::fs::metadata(&p).unwrap().mode() & 0o777);
        let start = HIST_SEQ.load(Ordering::Relaxed);
        let dir = p.parent().unwrap();
        let collisions: Vec<_> = (0..16).map(|i| dir.join(format!(".clipboard.json.tmp.{}.{}", std::process::id(), start + i))).collect();
        for c in &collisions { std::fs::write(c, b"sentinel").unwrap(); }
        let failed = save_history_at(&p, &[]).is_err();
        assert!(failed);
        assert_eq!((std::fs::read(&p).unwrap(), std::fs::metadata(&p).unwrap().mode() & 0o777), before);
        assert_eq!(load_history_at(&p).unwrap(), vec!["synthetic".to_string()]);
        for c in &collisions { assert_eq!(std::fs::read(c).unwrap(), b"sentinel"); }
        assert_eq!(clear_result(!failed, "Clipboard cleared".into(), true), ("Clipboard history file could not be cleared".to_string(), false));
    }

    #[test]
    fn files_are_private_under_umask_022() {
        use std::os::unix::process::CommandExt;
        let d = tmpdir("umask");
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        let child = format!("{}::history_umask_child", module_path!().split_once("::").unwrap().1);
        cmd.args(["--exact", &child, "--nocapture"]).env("HIST_CHILD_DIR", &d);
        unsafe { cmd.pre_exec(|| { libc::umask(0o022); Ok(()) }); }
        assert!(cmd.output().unwrap().status.success());
        let dir = d.join("waybar-notch");
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("clipboard.json")), 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn existing_public_history_and_directory_are_migrated() {
        let d = tmpdir("migrate");
        let dir = d.join("waybar-notch");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = dir.join("clipboard.json");
        std::fs::write(&p, r#"["old secret"]"#).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_history_at(&p).unwrap(), vec!["old secret".to_string()]);
        assert_eq!((mode(&dir), mode(&p)), (0o700, 0o600));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn symlink_and_nonregular_targets_are_rejected() {
        let d = tmpdir("reject");
        let dir = d.join("waybar-notch");
        std::fs::create_dir(&dir).unwrap();
        let victim = d.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let p = dir.join("clipboard.json");
        std::os::unix::fs::symlink(&victim, &p).unwrap();
        assert!(load_history_at(&p).is_err());
        assert!(save_history_at(&p, &["x".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        assert!(save_history_at(&p, &["x".to_string()]).is_err());
        // a symlinked data directory is refused as well
        let real = d.join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = d.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(save_history_at(&linked.join("clipboard.json"), &["x".to_string()]).is_err());
        assert!(std::fs::read_dir(&real).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_history_errors_without_echoing_content() {
        let d = tmpdir("malformed");
        let dir = d.join("waybar-notch");
        std::fs::create_dir(&dir).unwrap();
        let p = dir.join("clipboard.json");
        std::fs::write(&p, "TOPSECRET not json").unwrap();
        let e = load_history_at(&p).unwrap_err();
        assert!(!e.contains("TOPSECRET"), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn failed_clear_cannot_claim_success() {
        let d = tmpdir("failure");
        let dir = d.join("waybar-notch");
        let p = dir.join("clipboard.json");
        save_history_at(&p, &["keep me".to_string()]).unwrap();
        std::fs::remove_file(&p).unwrap();
        std::fs::create_dir(&p).unwrap();
        let failed = save_history_at(&p, &[]).is_err();
        assert!(failed);
        let (msg, ok) = clear_result(!failed, "Clipboard cleared".into(), true);
        assert!(!ok);
        assert_eq!(msg, "Clipboard history file could not be cleared");
        assert_eq!(clear_result(true, "Clipboard cleared".into(), true), ("Clipboard cleared".to_string(), true));
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod clip_service_tests {
    use super::*;
    use crate::testkit::{Pump, Rig};

    struct Env { dir: PathBuf, hist: PathBuf, rig: Rig, pump: Pump, svc: ClipService }

    impl Drop for Env {
        fn drop(&mut self) { let _ = self.svc.tx.send(ClipCommand::Shutdown); }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clip-svc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn config(dir: &Path, script: &str, clear: &[&str]) -> ClipConfig {
        ClipConfig {
            history: dir.join("data").join("clipboard.json"),
            watch: vec!["sh".into(), "-c".into(), script.into()],
            clear: clear.iter().map(|s| s.to_string()).collect(),
            restart_delay: Duration::from_secs(30),
            startup_gate: None,
        }
    }

    fn env_with(dir: PathBuf, cfg: ClipConfig) -> Env {
        let (rig, pump) = Rig::pumped();
        let hist = cfg.history.clone();
        let svc = clip_service(&rig.poster, cfg);
        Env { dir, hist, rig, pump, svc }
    }

    fn env(tag: &str, script: &str, clear: &[&str]) -> Env {
        let dir = tmp(tag);
        let cfg = config(&dir, script, clear);
        env_with(dir, cfg)
    }

    impl Env {
        fn saved(&self) -> Vec<String> { load_history_at(&self.hist).unwrap_or_default() }
        fn wait_saved(&self, want: &[&str]) {
            let end = Instant::now() + Duration::from_secs(10);
            while self.saved() != want {
                assert!(Instant::now() < end, "history file never became {want:?}, is {:?}", self.saved().len());
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn until(&mut self, what: &str, cond: impl Fn(&State) -> bool) { self.pump.until(&mut self.rig.st, what, cond); }
        fn drain(&mut self, ms: u64) { self.pump.dispatch(&mut self.rig.st, ms); }
        fn clear(&mut self) { clip_clear_with(&self.svc, &self.rig.poster); }
    }

    /// First watcher run emits 1, 2, 3; every later run emits `fresh`.
    fn staged(dir: &Path) -> String {
        format!("if [ -e {d}/ran ]; then printf 'fresh\\000'; else touch {d}/ran; printf '1\\0002\\0003\\000'; fi; sleep 30", d = dir.display())
    }

    fn filtered(source: &str) -> String { format!("{source} | ( {CLIP_FILTER} ); sleep 30") }

    #[test]
    fn nul_flood_yields_one_bounded_selection() {
        let mut e = env("flood", &filtered("yes x | head -n 100000 | tr '\\n' '\\000'"), &["true"]);
        let want = "x".repeat(CLIP_MAX_BYTES / 2);
        e.until("the capped selection", |st| st.sys.clip.len() == 1);
        assert_eq!(e.rig.st.sys.clip, [want.clone()]);
        e.drain(300);
        assert_eq!(e.rig.st.sys.clip, [want.clone()]);
        assert_eq!(e.saved(), [want]);
    }

    #[test]
    fn payload_size_boundaries_and_embedded_nuls() {
        for (n, tag) in [(19_999usize, "b1"), (20_000, "b2"), (20_001, "b3")] {
            let dir = tmp(tag);
            let src = dir.join("payload");
            std::fs::write(&src, vec![b'a'; n]).unwrap();
            let cfg = config(&dir, &filtered(&format!("cat {}", src.display())), &["true"]);
            let mut e = env_with(dir, cfg);
            e.until("the selection", |st| !st.sys.clip.is_empty());
            assert_eq!(e.rig.st.sys.clip[0].len(), n.min(CLIP_MAX_BYTES), "payload of {n} bytes");
            assert_eq!(e.saved()[0].len(), n.min(CLIP_MAX_BYTES));
        }
        // a multi-byte character cut by the cap becomes a replacement character, never invalid text
        let dir = tmp("utf8");
        let src = dir.join("payload");
        let mut p = vec![b'a'; CLIP_MAX_BYTES - 1];
        p.extend_from_slice("é".as_bytes());
        std::fs::write(&src, p).unwrap();
        let cfg = config(&dir, &filtered(&format!("cat {}", src.display())), &["true"]);
        let mut e = env_with(dir, cfg);
        e.until("the selection", |st| !st.sys.clip.is_empty());
        assert!(e.rig.st.sys.clip[0].ends_with('\u{FFFD}'));
        // embedded NULs are removed, not treated as delimiters
        let dir = tmp("nul");
        let src = dir.join("payload");
        std::fs::write(&src, b"ab\0cd").unwrap();
        let cfg = config(&dir, &filtered(&format!("cat {}", src.display())), &["true"]);
        let mut e = env_with(dir, cfg);
        e.until("the selection", |st| !st.sys.clip.is_empty());
        e.drain(200);
        assert_eq!(e.rig.st.sys.clip, ["abcd"]);
    }

    #[test]
    fn frames_split_across_reads_stay_intact() {
        let mut e = env("split", "printf 'a\\303'; sleep 0.3; printf '\\251b\\000c\\000'; sleep 30", &["true"]);
        e.until("both frames", |st| st.sys.clip.len() == 2);
        assert_eq!(e.rig.st.sys.clip, ["c", "a\u{e9}b"]);
    }

    #[test]
    fn stalled_ui_leaves_one_selection_outstanding() {
        let mut e = env("stall", "printf '1\\0002\\0003\\000'; sleep 30", &["true"]);
        e.drain(500); // applies the startup snapshot; its acknowledgement starts the watcher
        e.wait_saved(&["1"]);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(e.saved(), ["1"], "later selections wait for the UI to apply the first");
        e.until("all three", |st| st.sys.clip.len() == 3);
        assert_eq!(e.rig.st.sys.clip, ["3", "2", "1"]);
        e.wait_saved(&["3", "2", "1"]);
    }

    #[test]
    fn clear_with_a_pending_acknowledgement_finishes_and_only_fresh_text_follows() {
        let dir = tmp("clear-pending");
        let script = staged(&dir);
        let cfg = config(&dir, &script, &["true"]);
        let mut e = env_with(dir, cfg);
        e.drain(500); // applies the startup snapshot; its acknowledgement starts the watcher
        e.wait_saved(&["1"]); // the reader is now waiting for the UI, which has not run yet
        e.clear();
        e.until("the clear result", |st| st.sys.clip_status.is_some());
        let status = e.rig.st.sys.clip_status.clone().unwrap();
        assert!(status.success, "{}", status.message);
        e.until("fresh capture after the clear", |st| st.sys.clip == ["fresh"]);
        e.drain(300);
        assert_eq!(e.rig.st.sys.clip, ["fresh"]);
        e.wait_saved(&["fresh"]);
    }

    #[test]
    fn history_read_before_a_clear_cannot_return_after_it() {
        let dir = tmp("late-load");
        let gate = Arc::new(std::sync::Barrier::new(2));
        let mut cfg = config(&dir, "sleep 30", &["true"]);
        cfg.startup_gate = Some(gate.clone());
        save_history_at(&cfg.history, &["old secret".to_string()]).unwrap();
        let mut e = env_with(dir, cfg);
        e.clear();
        e.drain(100); // the clear bumps the generation while the coordinator is still before its file read
        assert!(e.rig.st.sys.clip_clearing);
        gate.wait();
        e.until("the clear result", |st| st.sys.clip_status.is_some());
        e.drain(300);
        assert!(e.rig.st.sys.clip.is_empty());
        assert!(e.saved().is_empty());
    }

    fn alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s.rsplit(") ").next().is_some_and(|rest| !rest.starts_with('Z')),
            Err(_) => false,
        }
    }

    #[test]
    fn watcher_descendants_cannot_hold_shutdown_open() {
        let dir = tmp("group");
        let script = format!("sleep 30 & echo $! > {d}/pid; printf '1\\000'; sleep 30", d = dir.display());
        let cfg = config(&dir, &script, &["true"]);
        let mut e = env_with(dir, cfg);
        e.until("the first selection", |st| st.sys.clip == ["1"]);
        let pid: u32 = std::fs::read_to_string(e.dir.join("pid")).unwrap().trim().parse().unwrap();
        assert!(alive(pid));
        e.clear();
        e.until("the clear result", |st| st.sys.clip_status.is_some());
        let end = Instant::now() + Duration::from_secs(3);
        while alive(pid) {
            assert!(Instant::now() < end, "the watcher's background child survived the clear");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn failed_clear_pauses_capture_and_a_retry_resumes_it() {
        let dir = tmp("retry");
        let script = staged(&dir);
        let ok = format!("test -e {}/ok", dir.display());
        let cfg = config(&dir, &script, &["sh", "-c", &ok]);
        let mut e = env_with(dir, cfg);
        e.until("the first run", |st| st.sys.clip.len() == 3);
        e.clear();
        e.until("the failure", |st| st.sys.clip_status.is_some());
        let status = e.rig.st.sys.clip_status.clone().unwrap();
        assert!(!status.success && status.message.contains("clipboard capture paused until Clear is retried"), "{}", status.message);
        e.drain(500);
        assert!(e.rig.st.sys.clip.is_empty() && e.saved().is_empty(), "capture stays paused");
        std::fs::write(e.dir.join("ok"), b"").unwrap();
        e.clear();
        e.until("the retry", |st| st.sys.clip_status.as_ref().is_some_and(|s| s.success));
        e.until("capture resumed", |st| st.sys.clip == ["fresh"]);
        e.wait_saved(&["fresh"]);
    }
}
