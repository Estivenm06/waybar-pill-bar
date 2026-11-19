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
use std::sync::{Condvar, Mutex};
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
}

impl Sys {
    /// Add a copied text to the history (unless a clear is running) and persist it. True when the list changed.
    pub fn clip_push(&mut self, s: String) -> bool {
        if self.clip_clearing || !push_history(&mut self.clip, s) { return false; }
        if let Err(e) = save_history_at(&hist_path(), &self.clip) { eprintln!("clipboard history: {e}"); }
        true
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
    load_history(poster);
    power_worker(poster.clone());
    volume_worker(poster.clone());
    net_worker(poster.clone());
    clipboard_worker(poster.clone());
    stats_worker(poster.clone());
    usage_worker(poster.clone());
    controls_worker(poster.clone());
}

fn load_history(poster: &Poster) {
    let poster = poster.clone();
    std::thread::spawn(move || {
        let hist = load_history_at(&hist_path()).unwrap_or_else(|e| { eprintln!("clipboard history: {e}"); Vec::new() });
        poster.post(move |st: &mut State, _| {
            // copies that arrived before the file was read stay on top
            let mut merged = std::mem::take(&mut st.sys.clip);
            for h in hist { if !merged.contains(&h) { merged.push(h); } }
            merged.truncate(MAX_HIST);
            st.sys.clip = merged;
        });
    });
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
                poster.post(move |st: &mut State, _| st.status.bt = b);
            }
            GATE.sleep(Duration::from_secs(if GATE.active() { 5 } else { 12 }));
        }
    });
}

// ── clipboard ───────────────────────────────────────────────────────────────

fn clipboard_worker(poster: Poster) {
    std::thread::spawn(move || loop {
        // wl-paste runs the command on every clipboard change; NUL terminates each entry.
        let child = Command::new("wl-paste")
            .args(["--type", "text", "--watch", "sh", "-c", "cat; printf '\\0'"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = child {
            if let Some(mut out) = child.stdout.take() {
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(n) = out.read(&mut buf) {
                    if n == 0 { break; }
                    for &b in &buf[..n] {
                        if b == 0 {
                            let s = String::from_utf8_lossy(&acc).to_string();
                            acc.clear();
                            poster.post(move |st: &mut State, _| { st.sys.clip_push(s); });
                        } else if acc.len() < 20000 {
                            acc.push(b);
                        }
                    }
                }
            }
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_secs(2));
    });
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

/// Clear the in-memory history, the history file and the Wayland selection; copies still queued from before the
/// clear are dropped until the system clipboard has been cleared. The outcome lands in `sys.clip_status`.
pub fn clip_clear(poster: &Poster) {
    let p = poster.clone();
    poster.post(move |st: &mut State, _| {
        st.sys.clip_clearing = true;
        st.sys.clip_status = None;
        st.sys.clip.clear();
        let disk_cleared = match save_history_at(&hist_path(), &[]) {
            Ok(()) => true,
            Err(e) => { eprintln!("clipboard history: {e}"); false }
        };
        std::thread::spawn(move || {
            let (message, success) = if !on_path("wl-copy") {
                ("History cleared; system clipboard unavailable".to_string(), false)
            } else {
                match Command::new("wl-copy").arg("--clear").status() {
                    Ok(s) if s.success() => ("Clipboard cleared".to_string(), true),
                    Ok(s) => (format!("History cleared; wl-copy --clear failed ({s})"), false),
                    Err(e) => (format!("History cleared; wl-copy failed: {e}"), false),
                }
            };
            let (message, success) = clear_result(disk_cleared, message, success);
            p.post(move |st: &mut State, _| {
                st.sys.clip_clearing = false;
                st.sys.clip_status = Some(ClipStatus { message, success });
            });
        });
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
