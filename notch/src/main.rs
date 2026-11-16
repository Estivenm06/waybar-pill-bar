//! waybar-notch: an expanding "notch" for the top bar (GTK4 + layer-shell).
//! Collapsed: album art, title, equalizer. Hover: it springs open into
//! Home / Media / Calendar / Clipboard tabs.
mod app_status;
mod art_cache;
mod art_http;
mod bar_controller;
mod config;
mod desktop;
mod gcal;
mod image_loader;
mod lyrics;
mod notch_content;
mod notification_relay;
mod notify;
mod osd;
mod popups;
mod power;
mod private_files;
mod settings;
mod theme;
mod usage;
mod wifi;

use gtk::{cairo, gdk, glib, prelude::*};
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const BARS: usize = 6;
const WA_CONTENT_H: f64 = 420.0;   // preferred height of the hosted WhatsApp content, bounded by the output ceiling
const HOST_RESERVE_H: f64 = 560.0;   // layer-host height reserved up front so page changes never resize the window
const EQ_MIN: f64 = 0.4;   // shortest bar as a fraction of the drawing height, so a quiet band is still a bar, not a dot
const PLAYERS: &str = "spotify,%any";
const MAX_HIST: usize = 50;
const SPRING_K: f64 = 250.0;                     // stiffness
const SPRING_C: f64 = 31.6;                     // damping = 2*sqrt(K): critically damped, glides in with no overshoot
// (idle time before returning to Home, the leave-collapse delay and all sizes come from config::NotchSettings)
const SKELETON_SECS: u32 = 8;   // seconds with no player before the media UI turns into a skeleton

/// Whether a player is available: the skeleton is only for a player that stays missing, never for Paused/Stopped ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaPresence { Missing, Present, Skeleton }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MediaPresenceEvent { Media, NoMedia, IdleTimeout }

fn next_media_presence(current: MediaPresence, event: MediaPresenceEvent) -> MediaPresence {
    use MediaPresence::*;
    use MediaPresenceEvent as E;
    match (current, event) {
        (_, E::Media) => Present,
        (Skeleton, E::NoMedia) => Skeleton,
        (_, E::NoMedia) => Missing,
        (Missing, E::IdleTimeout) => Skeleton,
        (c, E::IdleTimeout) => c,
    }
}

/// Same match Omarchy uses to find its recorder; `-x` would fail on the truncated comm name.
fn recording_active() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| {
        e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
            && std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.starts_with(b"gpu-screen-recorder"))
    })
}

/// Hyprsunset's colour temperature below the 6000K identity value means Reading Mode is on
/// (same predicate as `omarchy toggle nightlight --status`). Unavailable or malformed output is "off".
fn nightlight_enabled() -> bool {
    let Ok(out) = Command::new("hyprctl").args(["hyprsunset", "temperature"]).stderr(Stdio::null()).output() else { return false };
    if !out.status.success() { return false; }
    let text = String::from_utf8_lossy(&out.stdout);
    let digits: String = text.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().is_ok_and(|t| t < 6000)
}

/// What the Controls tiles hand to Omarchy; each runs only after the notch is collapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureAction { Screenshot, RecordStart, RecordStop }

impl CaptureAction {
    fn args(self) -> &'static [&'static str] {
        match self {
            CaptureAction::Screenshot => &["screenshot"],
            CaptureAction::RecordStart => &["screenrecord"],
            CaptureAction::RecordStop => &["screenrecord", "--stop-recording"],
        }
    }
    fn failure(self) -> &'static str {
        match self { CaptureAction::Screenshot => "Screenshot command failed", _ => "Recording command failed" }
    }
}

enum Msg {
    Media { status: String, artist: String, title: String, art_url: String, len: f64, player: String },
    NoMedia,
    Weather(WeatherInfo),
    Art { url: String, path: std::path::PathBuf, image: image_loader::DecodedImage },
    Pos(f64),
    Bars(u64, [f64; BARS]),
    VisualizerEnded(u64),
    Clip(String),
    Stats(String, String, String, String, bool),
    OsdData(String, f64, bool),
    Toggle(String, bool),
    Desktop(desktop::DesktopSnapshot),
    CaptureActive(bool),
    CaptureFinished(CaptureAction, Result<(), String>),
    Relayout,
    UtilityPageChanged,
    Settings(settings::SettingsEvent),
    UtilResult(String),
    ClipCleared { message: String, success: bool },
    Net(&'static str, u8, String),
    NotificationRelayFailed(String),
    NotchContent(notch_content::Event),
    NotchContentFailed(String),
    TabSelected(&'static str),
    Gcal(gcal::Update),
    Usage(Box<usage::Usage>),
    PopupReq(String),
    PopupData(String, serde_json::Value),
    PopupRefresh(String),
    BtActionFinished,
    Lyrics(String, lyrics::Lyrics),
    PlayerRaised(u64, Result<(), String>),
}

static SEEKING: AtomicBool = AtomicBool::new(false);

// Notch size for the expanded view; `t`-driven open/close is separate. Springs animate between tab sizes.
#[derive(Clone, Copy)]
struct Size { w: f64, h: f64, vw: f64, vh: f64, tw: f64, th: f64 }

/// Where an ordinary close of the WhatsApp page leads once its content has hidden.
#[derive(Clone, Copy, PartialEq)]
enum CloseTo { Collapse, Home, Tab(&'static str) }

/// The one live WhatsApp content session (GTK owns it; QML only reports into it).
struct Ext {
    s: notch_content::Session,
    /// Follow mode keeps the notch on the output that was clicked while the page is up.
    pin: Option<String>,
    after: CloseTo,
    /// Output and slot rectangle of the newest layout sent; cleared while the content is suspended.
    last: Option<(String, [i32; 4])>,
}

struct Ui {
    content: notch_content::Bridge,
    ext: RefCell<Option<Ext>>,
    ext_timer: RefCell<Option<glib::SourceId>>,
    ext_pub_busy: Cell<bool>,
    notch_hover: Cell<bool>,
    started: std::time::Instant,
    wa_page: gtk::Box,
    wa_status: gtk::Label,
    popups: Rc<popups::Popups>,
    populate_bt: Rc<dyn Fn()>,
    populate_wifi: Rc<dyn Fn()>,
    settings: RefCell<config::NotchSettings>,
    desk: RefCell<Option<desktop::DesktopSnapshot>>,
    dest: RefCell<Option<String>>,
    dest_info: RefCell<Option<desktop::MonitorInfo>>,
    geom: Cell<desktop::Geometry>,
    host: Cell<(i32, i32)>,
    hidden: Cell<bool>,
    bar_tx: mpsc::Sender<bar_controller::SyncState>,
    cal: gtk::Calendar,
    agenda: gtk::Box,
    events: RefCell<Vec<gcal::Event>>,
    gstate: RefCell<String>,
    last_playing: Cell<Option<bool>>,
    osd: Rc<osd::Osd>,
    notes: Rc<notify::NotifyUi>,
    ai: usage::AiUi,
    stack: gtk::Stack,
    tab_btns: Rc<RefCell<Vec<(gtk::Button, &'static str)>>>,
    win: gtk::ApplicationWindow,
    notch: gtk::Box,
    c_art: gtk::Image,
    coll_stack: gtk::Stack,
    c_note: gtk::Label,
    c_title: gtk::Label,
    c_weather: gtk::Label,
    i_weather: gtk::Label,
    h_weather_temp: gtk::Label,
    h_weather_location: gtk::Label,
    h_weather_wind: gtk::Label,
    eq: gtk::DrawingArea,
    m_art: gtk::Picture,
    m_backdrop: gtk::Picture,
    m_title: gtk::Label,
    m_artist: gtk::Label,
    m_prog: gtk::Scale,
    m_t1: gtk::Label,
    m_t2: gtk::Label,
    m_play: gtk::Button,
    h_title: gtk::Label,
    h_artist: gtk::Label,
    h_art: gtk::Picture,
    h_play: gtk::Button,
    h_clip: gtk::Label,
    s_batt: gtk::Label,
    s_vol: gtk::Label,
    s_wifi: gtk::Label,
    s_bt: gtk::Label,
    coll: gtk::Box,
    exp: gtk::Box,
    tabs: gtk::Box,
    clip_box: gtk::Box,
    clip_title: gtk::Label,
    clip_clear: gtk::Button,
    prog: Cell<f64>,
    vel: Cell<f64>,
    target: Cell<f64>,
    size: Cell<Size>,
    animating: Cell<bool>,
    expanded: Arc<AtomicBool>,
    bars: RefCell<[f64; BARS]>,
    playing: Cell<bool>,
    media_presence: Cell<MediaPresence>,
    utility_host: gtk::Box,
    agenda_date: gtk::Label,
    utility_page: gtk::Box,
    wifi_page: gtk::Box,
    bt_page: gtk::Box,
    utility_title: gtk::Label,
    utility_kind: Cell<Option<&'static str>>,
    utility_kbd: Cell<bool>,
    len: Cell<f64>,
    art_url: RefCell<String>,
    art_worker: ArtWorker,
    lyr_current: gtk::Label,
    lyr_next: gtk::Label,
    lyr: RefCell<lyrics::Lyrics>,
    lyr_key: RefCell<String>,
    pos: Cell<f64>,
    hist: RefCell<Vec<String>>,
    collapse_id: RefCell<Option<glib::SourceId>>,
    reset_id: RefCell<Option<glib::SourceId>>,
    refresh_at: Cell<Option<std::time::Instant>>,
    cava: RefCell<Option<Child>>,
    workers: RefCell<Vec<std::sync::mpsc::Sender<bool>>>,
    viz_on: Cell<bool>,
    viz_gen: Cell<u64>,
    /// A capture helper has been accepted and has not exited yet (all capture tiles are disabled meanwhile).
    capture_pending: Cell<bool>,
    /// The compositor shows Omarchy's picker/freeze layer (`Msg::CaptureActive`).
    capture_active: Cell<bool>,
    /// Pointer entered while capturing: only a fresh enter after leaving may open the notch by hover.
    capture_hover_blocked: Cell<bool>,
    btn_ss: gtk::Button,
    btn_rec: gtk::Button,
    rec_update: Rc<dyn Fn()>,
    tile_wifi: gtk::Button,
    tile_bt: gtk::Button,
    settings_page: RefCell<Option<Rc<settings::SettingsPage>>>,
    settings_host: gtk::Box,
    art_accepted: RefCell<Option<std::path::PathBuf>>,
    art_prune_pending: Cell<bool>,
    tiles: Vec<(&'static str, gtk::Button)>,
    tile_settings: gtk::Button,
    cgrid: gtk::Grid,
    ctl_status: gtk::Label,
    ctl_status_timer: Rc<RefCell<Option<glib::SourceId>>>,
    detail_origin: Rc<Cell<&'static str>>,
    confirm_row: gtk::Box,
    clearing: Cell<bool>,
    media_tab: gtk::Box,
    m_cover_spacer: gtk::Box,
    media_background: gtk::Overlay,
    m_open_app: gtk::Button,
    player_instance: RefCell<String>,
    player_generation: Cell<u64>,
    player_raise_busy: Cell<bool>,
    strip: gtk::Box,
    h_backdrop: gtk::Picture,
    skel_id: RefCell<Option<glib::SourceId>>,
 tx: async_channel::Sender<Msg>,
}

// One damped-spring step. Integrated in <=4 ms substeps so the motion is frame-rate independent and stable.
fn spring(x: f64, v: f64, tg: f64, dt: f64) -> (f64, f64) {
    let n = ((dt / 0.004).ceil() as u32).max(1);
    let h = dt / n as f64;
    let (mut x, mut v) = (x, v);
    for _ in 0..n {
        v += (SPRING_K * (tg - x) - SPRING_C * v) * h;
        x += v * h;
    }
    (x, v)
}

fn sh_out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).stderr(Stdio::null()).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// Hand a detached child to a single waiter thread so it never lingers as a zombie.
fn reap_detached(mut child: Child) {
    std::thread::spawn(move || { let _ = child.wait(); });
}

fn spawn(cmd: &str, args: &[&str]) {
    if let Ok(c) = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn() { reap_detached(c); }
}

fn spawn_vec(c: &[String]) {
    if let Some((cmd, args)) = c.split_first() {
        if let Ok(c) = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn() { reap_detached(c); }
    }
}

/// Call `on_change` (debounced, on the GTK thread) whenever `path` is created, replaced or edited.
/// Watches the parent directory so atomic renames are seen; while that directory does not exist yet, the
/// nearest existing ancestor is watched and the monitor moves down as the path appears.
fn watch_file(path: std::path::PathBuf, on_change: impl Fn() + 'static) {
    struct W {
        path: std::path::PathBuf,
        mon: RefCell<Option<(std::path::PathBuf, gtk::gio::FileMonitor)>>,
        debounce: RefCell<Option<glib::SourceId>>,
        on_change: Box<dyn Fn()>,
    }
    fn attach(w: &Rc<W>) {
        let Some(parent) = w.path.parent() else { return };
        let Some(dir) = parent.ancestors().find(|d| d.is_dir()) else { return };
        if w.mon.borrow().as_ref().is_some_and(|(d, _)| d == dir) { return; }
        let Ok(mon) = gtk::gio::File::for_path(dir).monitor_directory(gtk::gio::FileMonitorFlags::NONE, gtk::gio::Cancellable::NONE) else { return };
        let weak = Rc::downgrade(w);
        mon.connect_changed(move |_, _, _, _| {
            let Some(w) = weak.upgrade() else { return };
            if let Some(id) = w.debounce.borrow_mut().take() { id.remove(); }
            let w2 = w.clone();
            *w.debounce.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(150), move || {
                w2.debounce.borrow_mut().take();
                attach(&w2);
                (w2.on_change)();
            }));
        });
        *w.mon.borrow_mut() = Some((dir.to_path_buf(), mon));
    }
    let w = Rc::new(W { path, mon: RefCell::new(None), debounce: RefCell::new(None), on_change: Box::new(on_change) });
    attach(&w);
    std::mem::forget(w); // lives for the process lifetime, like the other monitors
}

/// Re-triggerable one-shot CSS animation class.
fn flash_tile(b: &gtk::Button, cls: &'static str, ms: u64) {
    b.remove_css_class(cls);
    b.add_css_class(cls);
    let b = b.clone();
    glib::timeout_add_local_once(Duration::from_millis(ms), move || b.remove_css_class(cls));
}

/// Pulse `b` (class ctl-pending) until `read()` differs from its value now, or 15 s pass; then run `done`.
fn tile_await(b: &gtk::Button, read: impl Fn() -> bool + 'static, done: impl Fn() + 'static) {
    if b.has_css_class("ctl-pending") { return; }
    let was = read();
    b.add_css_class("ctl-pending");
    let (b, t) = (b.clone(), std::time::Instant::now());
    glib::timeout_add_local(Duration::from_millis(700), move || {
        if read() == was && t.elapsed() < Duration::from_secs(15) { return glib::ControlFlow::Continue; }
        b.remove_css_class("ctl-pending");
        done();
        glib::ControlFlow::Break
    });
}

/// Wi-Fi / Bluetooth tile reaction to a connect-state message.
fn net_tile(b: &gtk::Button, state: u8) {
    match state {
        osd::NET_CONNECTING => { b.remove_css_class("ctl-fail"); b.add_css_class("ctl-pending"); }
        osd::NET_CONNECTED => { b.remove_css_class("ctl-pending"); flash_tile(b, "ctl-flash", 450); }
        osd::NET_FAILED => { b.remove_css_class("ctl-pending"); flash_tile(b, "ctl-fail", 700); }
        _ => b.remove_css_class("ctl-pending"),
    }
}

// ── Wi-Fi page data (all nmcli work happens off the UI thread) ──────────────

struct WifiNet { ssid: String, signal: u8, secure: bool, active: bool, saved: bool }
struct WifiSnap { on: bool, available: bool, nets: Vec<WifiNet> }

impl WifiSnap {
    /// Layout-relevant fingerprint; signal is bucketed so jitter doesn't rebuild the list.
    fn signature(&self) -> String {
        let mut s = format!("{}{}|", self.on as u8, self.available as u8);
        for n in &self.nets {
            s.push_str(&format!("{}\x1f{}{}{}{}\x1e", n.ssid, n.active as u8, n.secure as u8, n.saved as u8, n.signal / 20));
        }
        s
    }
}

/// Split an `nmcli -t` line on unescaped ':' (nmcli escapes ':' and '\' with a backslash).
fn split_terse(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut esc = false;
    for ch in line.chars() {
        if esc { cur.push(ch); esc = false; }
        else if ch == '\\' { esc = true; }
        else if ch == ':' { out.push(std::mem::take(&mut cur)); }
        else { cur.push(ch); }
    }
    out.push(cur);
    out
}

fn wifi_snapshot(rescan: bool) -> WifiSnap {
    let radio = sh_out("nmcli", &["radio", "wifi"]);
    let available = matches!(radio.as_str(), "enabled" | "disabled");
    let on = radio == "enabled";
    let mut nets: Vec<WifiNet> = Vec::new();
    if on {
        // actual SSID bytes of saved Wi-Fi profiles; if NetworkManager cannot be queried, no Saved badges are shown
        let saved: Vec<Vec<u8>> = wifi::saved_ssids().unwrap_or_default();
        let out = sh_out("nmcli", &["-t", "-f", "ACTIVE,SSID,SIGNAL,SECURITY", "device", "wifi", "list",
            "--rescan", if rescan { "yes" } else { "no" }]);
        for l in out.lines() {
            let p = split_terse(l);
            if p.len() < 4 || p[1].is_empty() { continue; }
            let net = WifiNet { ssid: p[1].clone(), signal: p[2].parse().unwrap_or(0), secure: !p[3].is_empty(), active: p[0] == "yes", saved: saved.iter().any(|s| s == p[1].as_bytes()) };
            match nets.iter_mut().find(|n| n.ssid == net.ssid) {
                Some(n) => { if net.active || (!n.active && net.signal > n.signal) { *n = net; } }
                None => nets.push(net),
            }
        }
        nets.sort_by(|a, b| b.active.cmp(&a.active).then(b.signal.cmp(&a.signal)));
    }
    WifiSnap { on, available, nets }
}

/// Send a cached snapshot immediately, then (optionally) a freshly rescanned one.
fn wifi_snap_async(wtx: &async_channel::Sender<WifiSnap>, rescan: bool) {
    let wtx = wtx.clone();
    std::thread::spawn(move || {
        let _ = wtx.try_send(wifi_snapshot(false));
        if rescan { let _ = wtx.try_send(wifi_snapshot(true)); }
    });
}

/// `password` is for a new secured network; it goes to NetworkManager over D-Bus only. A failed attempt removes just the
/// profile this attempt created, so the next try prompts again.
fn wifi_connect(tx: &async_channel::Sender<Msg>, wtx: Option<&async_channel::Sender<WifiSnap>>, ssid: &str, connect: bool, password: Option<String>) {
    let (tx, wtx, ssid) = (tx.clone(), wtx.cloned(), ssid.to_string());
    std::thread::spawn(move || {
        if connect { let _ = tx.try_send(Msg::Net("wifi", osd::NET_CONNECTING, ssid.clone())); }
        let result = if !connect {
            wifi::disconnect(&ssid)
        } else if let Some(pw) = &password {
            wifi::connect_with_password(&ssid, pw)
        } else {
            wifi::connect_saved(&ssid)
        };
        if let Err(e) = &result { eprintln!("wifi: {e}"); }
        let state = match (result.is_ok(), connect) {
            (false, _) => osd::NET_FAILED,
            (true, true) => osd::NET_CONNECTED,
            (true, false) => osd::NET_DISCONNECTED,
        };
        let _ = tx.try_send(Msg::Net("wifi", state, ssid));
        if let Some(s) = &wtx { let _ = s.try_send(wifi_snapshot(false)); }
        let _ = tx.try_send(Msg::PopupRefresh("wifi".into()));
    });
}

fn wifi_radio(tx: &async_channel::Sender<Msg>, wtx: Option<&async_channel::Sender<WifiSnap>>, on: bool) {
    let (tx, wtx) = (tx.clone(), wtx.cloned());
    std::thread::spawn(move || {
        let name = if on { "Wi-Fi on" } else { "Wi-Fi off" };
        if on { let _ = tx.try_send(Msg::Net("wifi", osd::NET_CONNECTING, name.into())); }
        let ok = Command::new("nmcli").args(["radio", "wifi", if on { "on" } else { "off" }])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        let state = match (ok, on) {
            (false, _) => osd::NET_FAILED,
            (true, true) => osd::NET_CONNECTED,
            (true, false) => osd::NET_DISCONNECTED,
        };
        let _ = tx.try_send(Msg::Net("wifi", state, name.into()));
        if let Some(s) = &wtx { let _ = s.try_send(wifi_snapshot(false)); }
        let _ = tx.try_send(Msg::PopupRefresh("wifi".into()));
        if ok && on {
            // scan results trickle in after the radio comes up
            std::thread::sleep(Duration::from_millis(1500));
            if let Some(s) = &wtx { let _ = s.try_send(wifi_snapshot(true)); }
            std::thread::sleep(Duration::from_millis(3000));
            if let Some(s) = &wtx { let _ = s.try_send(wifi_snapshot(true)); }
        }
    });
}

struct BtDev {
    mac: String, name: String, connected: bool, paired: bool,
    icon: String, audio: bool, battery: Option<u8>,
    /// Exact playback sink of this device (preferring the actual default), if PipeWire exposes one.
    sink: Option<String>,
    active: bool,
    muted: Option<bool>,
}
struct BtSnap { on: bool, available: bool, audio_available: bool, devs: Vec<BtDev> }

impl BtSnap {
    fn signature(&self) -> String {
        let mut s = format!("{}{}{}|", self.on as u8, self.available as u8, self.audio_available as u8);
        for d in &self.devs {
            s.push_str(&format!("{}\x1f{}\x1f{}{}{}{}\x1f{}\x1f{:?}\x1f{:?}\x1f{}{:?}\x1e", d.mac, d.name, d.connected as u8, d.paired as u8,
                d.audio as u8, d.active as u8, d.icon, d.battery, d.sink, d.muted.is_some() as u8, d.muted));
        }
        s
    }
}

/// Headline and secondary line of the Wi-Fi status card.
fn wifi_card_text(s: &WifiSnap) -> (String, String) {
    if !s.available { return ("Wi-Fi unavailable".into(), "NetworkManager is not responding".into()); }
    if !s.on { return ("Wi-Fi is off".into(), "Turn on to see networks".into()); }
    match s.nets.iter().find(|n| n.active) {
        Some(n) => (n.ssid.clone(), format!("Signal {}%", n.signal)),
        None => ("Not connected".into(), "Wi-Fi is on".into()),
    }
}

/// Headline and secondary line of the Bluetooth status card.
fn bt_card_text(s: &BtSnap) -> (String, String) {
    if !s.available { return ("Bluetooth unavailable".into(), "No Bluetooth controller responding".into()); }
    if !s.on { return ("Bluetooth is off".into(), "Turn on to see devices".into()); }
    let n = s.devs.iter().filter(|d| d.connected).count();
    let audio: Vec<String> = s.devs.iter().filter(|d| d.connected && d.audio).map(|d| {
        let state = if !s.audio_available { "audio service unavailable" } else if d.active { "active" }
            else if d.sink.is_some() { "audio available" } else { "no audio output" };
        format!("{} · {state}", d.name)
    }).collect();
    let second = if audio.is_empty() { "Bluetooth is on".to_string() } else { audio.join(", ") };
    match n {
        0 => ("No devices connected".into(), "Bluetooth is on".into()),
        1 => ("1 device connected".into(), second),
        n => (format!("{n} devices connected"), second),
    }
}

fn bt_list(args: &[&str]) -> Vec<(String, String)> {
    sh_out("bluetoothctl", args).lines().filter_map(|l| {
        let mut p = l.trim().splitn(3, ' ');
        (p.next()? == "Device").then_some(())?;
        let mac = p.next()?.to_string();
        Some((mac, p.next().unwrap_or("").to_string()))
    }).collect()
}

const BT_AUDIO_UUIDS: [&str; 3] = [
    "0000110b-0000-1000-8000-00805f9b34fb", // Audio Sink
    "00001108-0000-1000-8000-00805f9b34fb", // Headset
    "0000111e-0000-1000-8000-00805f9b34fb", // Handsfree
];
const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const NO_SINK_MSG: &str = "Connected, but no audio output is available. Put the earbuds in pairing mode and connect their audio device.";

struct BtInfo { icon: String, audio_uuid: bool, sink_uuid: bool, battery: Option<u8> }

fn bt_parse_info(info: &str) -> BtInfo {
    let mut r = BtInfo { icon: String::new(), audio_uuid: false, sink_uuid: false, battery: None };
    for l in info.lines().map(str::trim) {
        if let Some(v) = l.strip_prefix("Icon:") { r.icon = v.trim().to_string(); }
        else if l.starts_with("UUID:") {
            let low = l.to_lowercase();
            if BT_AUDIO_UUIDS.iter().any(|u| low.contains(u)) { r.audio_uuid = true; }
            if low.contains(A2DP_SINK_UUID) { r.sink_uuid = true; }
        } else if let Some(v) = l.strip_prefix("Battery Percentage:") {
            r.battery = v.find('(').and_then(|a| v[a + 1..].split(')').next()).and_then(|n| n.trim().parse().ok());
        }
    }
    r
}

/// Playback sink of `mac` in the normalized output list: exact MAC match, preferring the actual default.
/// Returns (sink name, is default, muted).
fn bt_pick_sink(outputs: &serde_json::Value, mac: &str) -> Option<(String, bool, Option<bool>)> {
    let mac = mac.to_ascii_uppercase();
    let list = outputs["outputs"].as_array()?;
    let mut m = list.iter().filter(|o| o["bt_mac"].as_str() == Some(mac.as_str()));
    let first = m.next()?;
    let o = std::iter::once(first).chain(m).find(|o| o["default"].as_bool() == Some(true)).unwrap_or(first);
    Some((o["name"].as_str()?.to_string(), o["default"].as_bool().unwrap_or(false), o["mute"].as_bool()))
}

fn bt_join_outputs(devs: &mut [BtDev], outputs: &serde_json::Value) {
    for d in devs.iter_mut().filter(|d| d.connected) {
        if let Some((name, active, muted)) = bt_pick_sink(outputs, &d.mac) {
            d.sink = Some(name);
            d.active = active;
            d.muted = muted;
            d.audio = true;
        }
    }
}

/// One-line audio state of a connected audio device; `None` for non-audio or disconnected devices.
fn bt_audio_status(d: &BtDev, audio_available: bool) -> Option<String> {
    if !d.connected || !d.audio { return None; }
    let mut s = if !audio_available { "Connected · Audio service unavailable".to_string() }
        else if d.active { if d.muted == Some(true) { "Active audio output · Muted".into() } else { "Active audio output".into() } }
        else if d.sink.is_some() { "Connected · Audio available".into() }
        else { "Connected · Audio unavailable".into() };
    if !d.paired { s.push_str(" · Not paired"); }
    Some(s)
}

/// Extra action for a connected audio device that is not the active output.
fn bt_audio_action(d: &BtDev) -> Option<&'static str> {
    (d.connected && d.audio && !d.active).then_some(if d.paired { "use" } else { "pair" })
}

fn bt_glyph(icon: &str) -> &'static str {
    if icon.contains("headset") || icon.contains("headphone") { "\u{f025}" }
    else if icon.contains("speaker") { "\u{f028}" }
    else { "\u{f293}" }
}

fn bt_snapshot() -> BtSnap {
    let show = sh_out("bluetoothctl", &["show"]);
    let available = show.contains("Powered: yes") || show.contains("Powered: no");
    let on = show.contains("Powered: yes");
    if !on { return BtSnap { on, available, audio_available: false, devs: vec![] }; }
    let conn = bt_list(&["devices", "Connected"]);
    let paired = bt_list(&["devices", "Paired"]);
    let mut devs: Vec<BtDev> = bt_list(&["devices"]).into_iter().filter_map(|(mac, name)| {
        let connected = conn.iter().any(|(m, _)| *m == mac);
        if !connected && name == mac.replace(':', "-") { return None; }
        let paired = paired.iter().any(|(m, _)| *m == mac);
        Some(BtDev { mac, name, connected, paired, icon: String::new(), audio: false, battery: None, sink: None, active: false, muted: None })
    }).collect();
    devs.sort_by(|a, b| b.connected.cmp(&a.connected).then(b.paired.cmp(&a.paired))
        .then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let mut audio_available = false;
    if devs.iter().any(|d| d.connected) {
        for d in devs.iter_mut().filter(|d| d.connected) {
            let info = bt_parse_info(&sh_out("bluetoothctl", &["info", &d.mac]));
            d.audio = info.icon.starts_with("audio-") || info.audio_uuid;
            d.icon = info.icon;
            d.battery = info.battery;
        }
        let outputs = popups::gather_audio_outputs();
        audio_available = outputs["available"].as_bool().unwrap_or(false);
        bt_join_outputs(&mut devs, &outputs);
    }
    BtSnap { on, available, audio_available, devs }
}

fn bt_snap_async(btx: &async_channel::Sender<BtSnap>) {
    let btx = btx.clone();
    std::thread::spawn(move || { let _ = btx.try_send(bt_snapshot()); });
}

fn bt_state(ok: bool, on: bool) -> u8 {
    match (ok, on) {
        (false, _) => osd::NET_FAILED,
        (true, true) => osd::NET_CONNECTED,
        (true, false) => osd::NET_DISCONNECTED,
    }
}

/// Run bluetoothctl (bounded by `--timeout` when non-zero) and return stdout+stderr; it exits 0 even on failure.
fn bt_run(args: &[&str], timeout: u32) -> String {
    let mut c = Command::new("bluetoothctl");
    if timeout > 0 { c.args(["--timeout", &timeout.to_string()]); }
    match c.args(args).stdin(Stdio::null()).output() {
        Ok(o) => format!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
        Err(e) => e.to_string(),
    }
}

/// Most useful line of bluetoothctl output, without terminal escapes.
fn bt_diag(out: &str) -> String {
    let mut clean = String::new();
    let mut esc = false;
    for ch in out.chars() {
        if esc { if ch.is_ascii_alphabetic() { esc = false; } }
        else if ch == '\x1b' { esc = true; }
        else if ch != '\r' { clean.push(ch); }
    }
    let lines: Vec<&str> = clean.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.iter().rev().find(|l| l.contains("Failed") || l.contains("Error")).or(lines.last())
        .map(|l| l.to_string()).unwrap_or_else(|| "no response from bluetoothctl".into())
}

fn bt_info(mac: &str) -> String { sh_out("bluetoothctl", &["info", mac]) }
fn bt_connected(mac: &str) -> bool { bt_info(mac).contains("Connected: yes") }

fn bt_notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        match Command::new("/usr/bin/notify-send").args(["--app-name=Waybar", "--", &title, &body]).stdin(Stdio::null()).status() {
            Ok(s) if s.success() => {}
            r => eprintln!("bluetooth: notify-send failed ({r:?}): {body}"),
        }
    });
}

fn bt_sink_for(mac: &str) -> Result<Option<String>, String> {
    let out = popups::gather_audio_outputs();
    if !out["available"].as_bool().unwrap_or(false) { return Err("Audio server unavailable".into()); }
    Ok(bt_pick_sink(&out, mac).map(|(n, _, _)| n))
}

/// Pair when needed, trust and connect, with readback after each step.
fn bt_link_up(mac: &str) -> Result<(), String> {
    if !bt_info(mac).contains("Paired: yes") {
        let out = bt_run(&["pair", mac], 20);
        if !bt_info(mac).contains("Paired: yes") { return Err(format!("Pairing failed: {}", bt_diag(&out))); }
    }
    bt_run(&["trust", mac], 8);
    let out = bt_run(&["connect", mac], 15);
    if bt_connected(mac) { Ok(()) } else { Err(format!("Connection failed: {}", bt_diag(&out))) }
}

/// Is the connected device an audio device (icon, playback UUID or a real sink)?
fn bt_is_audio(mac: &str) -> bool {
    let i = bt_parse_info(&bt_info(mac));
    i.icon.starts_with("audio-") || i.audio_uuid || matches!(bt_sink_for(mac), Ok(Some(_)))
}

/// Wait (bounded) for the device's own sink, requesting the advertised A2DP profile once, then select it.
fn bt_route_audio(mac: &str) -> Result<(), String> {
    let info = bt_parse_info(&bt_info(mac));
    let mut sink = bt_sink_for(mac)?;
    if sink.is_none() && info.sink_uuid {
        let out = bt_run(&["connect", mac, A2DP_SINK_UUID], 15);
        eprintln!("bluetooth: requested audio sink profile for {mac}: {}", bt_diag(&out));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while sink.is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if !bt_connected(mac) { return Err("Device disconnected before audio became available".into()); }
        sink = bt_sink_for(mac)?;
    }
    popups::select_audio_output(&sink.ok_or_else(|| NO_SINK_MSG.to_string())?)
}

/// Terminal outcome of any connect/disconnect/use-audio: OSD, notification, fresh state, and busy-button reset.
fn bt_conclude(tx: &async_channel::Sender<Msg>, btx: Option<&async_channel::Sender<BtSnap>>, name: &str, connected: bool, r: Result<bool, String>) {
    let state = match &r {
        Ok(is_audio) => {
            if *is_audio { bt_notify("Bluetooth audio", &format!("Using {name} for audio")); }
            bt_state(true, connected)
        }
        Err(e) => {
            eprintln!("bluetooth {name}: {e}");
            bt_notify(if e == NO_SINK_MSG { "Bluetooth audio" } else { "Bluetooth" }, e);
            osd::NET_FAILED
        }
    };
    let _ = tx.try_send(Msg::Net("bt", state, name.to_string()));
    if let Some(s) = btx { let _ = s.try_send(bt_snapshot()); }
    let _ = tx.try_send(Msg::PopupRefresh("bluetooth".into()));
    let _ = tx.try_send(Msg::PopupRefresh("volume".into()));
    let _ = tx.try_send(Msg::BtActionFinished);
}

/// Connect (pairing first when needed, then selecting the device's audio output) or disconnect.
/// Audio success requires a real, selected sink, not just `Connected: yes`.
fn bt_connect(tx: &async_channel::Sender<Msg>, btx: Option<&async_channel::Sender<BtSnap>>, mac: &str, name: &str, connect: bool) {
    let (tx, btx, mac, name) = (tx.clone(), btx.cloned(), mac.to_string(), name.to_string());
    std::thread::spawn(move || {
        if !connect {
            bt_run(&["disconnect", &mac], 8);
            let r = if bt_connected(&mac) { Err("Disconnect failed".to_string()) } else { Ok(false) };
            bt_conclude(&tx, btx.as_ref(), &name, false, r);
            return;
        }
        let _ = tx.try_send(Msg::Net("bt", osd::NET_CONNECTING, name.clone()));
        let r = bt_link_up(&mac).and_then(|()| if bt_is_audio(&mac) { bt_route_audio(&mac).map(|()| true) } else { Ok(false) });
        bt_conclude(&tx, btx.as_ref(), &name, true, r);
    });
}

/// Select the audio output of an already connected, paired device without reconnecting or re-pairing.
pub(crate) fn bt_use_audio(tx: &async_channel::Sender<Msg>, btx: Option<&async_channel::Sender<BtSnap>>, mac: &str, name: &str) {
    let (tx, btx, mac, name) = (tx.clone(), btx.cloned(), mac.to_string(), name.to_string());
    std::thread::spawn(move || {
        let _ = tx.try_send(Msg::Net("bt", osd::NET_CONNECTING, name.clone()));
        let r = if bt_connected(&mac) { bt_route_audio(&mac).map(|()| true) } else { Err("Device is not connected".to_string()) };
        bt_conclude(&tx, btx.as_ref(), &name, true, r);
    });
}

fn bt_power(tx: &async_channel::Sender<Msg>, btx: Option<&async_channel::Sender<BtSnap>>, on: bool) {
    let (tx, btx) = (tx.clone(), btx.cloned());
    std::thread::spawn(move || {
        let name = if on { "Bluetooth on" } else { "Bluetooth off" };
        if on { let _ = tx.try_send(Msg::Net("bt", osd::NET_CONNECTING, name.into())); }
        let _ = Command::new("bluetoothctl").args(["power", if on { "on" } else { "off" }])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        let ok = sh_out("bluetoothctl", &["show"]).contains("Powered: yes") == on;
        let _ = tx.try_send(Msg::Net("bt", bt_state(ok, on), name.into()));
        if let Some(s) = &btx { let _ = s.try_send(bt_snapshot()); }
        let _ = tx.try_send(Msg::PopupRefresh("bluetooth".into()));
        if ok && on {
            std::thread::sleep(Duration::from_millis(1500));
            if let Some(s) = &btx { let _ = s.try_send(bt_snapshot()); }
            std::thread::sleep(Duration::from_millis(3000));
            if let Some(s) = &btx { let _ = s.try_send(bt_snapshot()); }
        }
    });
}

fn bt_scan(btx: &async_channel::Sender<BtSnap>) {
    let btx = btx.clone();
    std::thread::spawn(move || {
        let first = bt_snapshot();
        let on = first.on;
        let _ = btx.try_send(first);
        if !on { return; }
        let child = Command::new("bluetoothctl").args(["--timeout", "6", "scan", "on"])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(1500));
            let _ = btx.try_send(bt_snapshot());
        }
        if let Ok(mut c) = child { let _ = c.wait(); }
    });
}

struct DevicePage {
    root: gtk::Box,
    back: gtk::Button,
    refresh: gtk::Button,
    headline: gtk::Label,
    summary: gtk::Label,
    toggle: gtk::Button,
    list: gtk::Box,
}

struct HomePlayer {
    root: gtk::Overlay,
    strip: gtk::Box,
    art: gtk::Picture,
    backdrop: gtk::Picture,
    title: gtk::Label,
    artist: gtk::Label,
    play: gtk::Button,
}

/// Home media card: blurred cover + scrim behind a cover / title / artist / play row.
/// The overlay's default positioning cannot shrink a measuring child below its unconstrained
/// minimum height, so the foreground is explicitly allocated the card's full size.
fn home_player_card() -> HomePlayer {
    let strip = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    strip.set_margin_start(8);
    strip.set_margin_end(8);
    strip.set_margin_top(8);
    strip.set_margin_bottom(8);
    let h_art = gtk::Picture::new();
    h_art.set_content_fit(gtk::ContentFit::Cover);
    h_art.set_can_shrink(true);
    h_art.set_can_target(false);
    h_art.add_css_class("art");
    h_art.set_overflow(gtk::Overflow::Hidden);
    h_art.set_halign(gtk::Align::Fill);
    h_art.set_valign(gtk::Align::Fill);
    let h_cover_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    h_cover_spacer.set_size_request(48, 48);
    h_cover_spacer.set_can_target(false);
    let h_cover = gtk::Overlay::new();
    h_cover.set_halign(gtk::Align::Start);
    h_cover.set_valign(gtk::Align::Center);
    h_cover.set_child(Some(&h_cover_spacer));
    h_cover.add_overlay(&h_art);
    h_cover.set_measure_overlay(&h_art, false);
    h_cover.set_clip_overlay(&h_art, true);
    let hcol = gtk::Box::new(gtk::Orientation::Vertical, 2);
    hcol.set_hexpand(true);
    hcol.set_valign(gtk::Align::Center);
    let h_title = metadata_label("title", 0.0, Some("Nothing playing"));
    let h_artist = metadata_label("sub", 0.0, None);
    hcol.append(&h_title);
    hcol.append(&h_artist);
    let h_play = gtk::Button::with_label("\u{f04c}");
    h_play.add_css_class("ctl");
    h_play.add_css_class("home-play");
    h_play.set_size_request(32, 32);
    h_play.set_valign(gtk::Align::Center);
    h_play.set_tooltip_text(Some("Play / pause"));
    h_play.connect_clicked(|_| spawn("playerctl", &["-p", PLAYERS, "play-pause"]));
    strip.append(&h_cover);
    strip.append(&hcol);
    strip.append(&h_play);
    let h_player = gtk::Overlay::new();
    h_player.add_css_class("home-player");
    h_player.set_hexpand(true);
    h_player.set_valign(gtk::Align::Start);
    h_player.set_overflow(gtk::Overflow::Hidden);
    let h_base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    h_base.set_can_target(false);
    h_player.set_child(Some(&h_base));
    let h_backdrop = gtk::Picture::new();
    h_backdrop.set_content_fit(gtk::ContentFit::Cover);
    h_backdrop.set_can_shrink(true);
    h_backdrop.set_opacity(0.65);
    let h_scrim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    h_scrim.add_css_class("home-player-scrim");
    for w in [h_backdrop.upcast_ref::<gtk::Widget>(), h_scrim.upcast_ref()] {
        w.set_halign(gtk::Align::Fill);
        w.set_valign(gtk::Align::Fill);
        w.set_can_target(false);
    }
    strip.set_halign(gtk::Align::Fill);
    strip.set_valign(gtk::Align::Fill);
    h_player.add_overlay(&h_backdrop);
    h_player.add_overlay(&h_scrim);
    h_player.add_overlay(&strip);
    for w in [h_backdrop.upcast_ref::<gtk::Widget>(), h_scrim.upcast_ref()] {
        h_player.set_measure_overlay(w, false);
        h_player.set_clip_overlay(w, true);
    }
    h_player.set_measure_overlay(&strip, true);
    h_player.set_clip_overlay(&strip, true);
    let strip_weak = strip.downgrade();
    h_player.connect_get_child_position(move |o, child| {
        let strip = strip_weak.upgrade()?;
        (child == strip.upcast_ref::<gtk::Widget>()).then(|| gtk::gdk::Rectangle::new(0, 0, o.width(), o.height()))
    });
    HomePlayer { root: h_player, strip, art: h_art, backdrop: h_backdrop, title: h_title, artist: h_artist, play: h_play }
}

/// Shared Wi-Fi / Bluetooth page: header, status card with the radio action, and a scrolling list.
fn device_page(title: &str, icon: &str, toggle_class: &str) -> DevicePage {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("device-page");
    root.set_margin_start(20);
    root.set_margin_end(20);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let back = gtk::Button::with_label("\u{f060}");
    back.add_css_class("back-btn");
    back.set_tooltip_text(Some("Back"));
    let t = gtk::Label::new(Some(title));
    t.add_css_class("cap");
    t.set_hexpand(true);
    t.set_xalign(0.0);
    let refresh = gtk::Button::with_label("\u{f021}");
    refresh.add_css_class("small-btn");
    refresh.set_tooltip_text(Some("Refresh"));
    head.append(&back);
    head.append(&t);
    head.append(&refresh);

    let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    card.add_css_class("device-card");
    let ic = gtk::Label::new(Some(icon));
    ic.add_css_class("device-icon");
    ic.set_size_request(32, -1);
    ic.set_halign(gtk::Align::Center);
    ic.set_valign(gtk::Align::Center);
    ic.set_xalign(0.5);
    ic.connect_map(|l| {
        let l = l.clone();
        glib::idle_add_local_once(move || center_ink(&l));
    });
    let col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    col.set_hexpand(true);
    col.set_valign(gtk::Align::Center);
    let mk = |cls: &str, text: &str| {
        let l = gtk::Label::new(Some(text));
        l.add_css_class(cls);
        l.set_xalign(0.0);
        l.set_hexpand(true);
        l.set_width_chars(1);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l
    };
    let headline = mk("device-headline", title);
    let summary = mk("device-meta", "Checking\u{2026}");
    col.append(&headline);
    col.append(&summary);
    let toggle = gtk::Button::with_label("Turn off");
    toggle.add_css_class("small-btn");
    toggle.add_css_class(toggle_class);
    toggle.set_valign(gtk::Align::Center);
    card.append(&ic);
    card.append(&col);
    card.append(&toggle);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let sw = gtk::ScrolledWindow::new();
    sw.set_vexpand(true);
    sw.set_propagate_natural_height(false);
    sw.set_min_content_height(0);
    sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sw.set_child(Some(&list));
    root.append(&head);
    root.append(&card);
    root.append(&sw);
    DevicePage { root, back, refresh, headline, summary, toggle, list }
}

/// One list row: icon, ellipsized name over a meta line, optional badge, and the action button.
fn device_row(icon: &str, name: &str, meta: &str, badge: Option<&str>, connected: bool, action: &gtk::Button) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 8);
    row.add_css_class("device-row");
    row.add_css_class("wifi-row");
    if connected { row.add_css_class("device-connected"); }
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let ic = gtk::Label::new(Some(icon));
    ic.add_css_class("device-row-icon");
    ic.set_size_request(32, -1);
    ic.set_hexpand(false);
    ic.set_valign(gtk::Align::Center);
    ic.set_xalign(0.5);
    ic.connect_map(|l| {
        let l = l.clone();
        glib::idle_add_local_once(move || center_ink(&l));
    });
    let col = gtk::Box::new(gtk::Orientation::Vertical, 1);
    col.set_hexpand(true);
    col.set_valign(gtk::Align::Center);
    let name_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let nm = gtk::Label::new(Some(name));
    nm.add_css_class("device-name");
    nm.set_xalign(0.0);
    nm.set_hexpand(true);
    nm.set_width_chars(1);
    nm.set_ellipsize(gtk::pango::EllipsizeMode::End);
    nm.set_tooltip_text(Some(name));
    name_line.append(&nm);
    if let Some(b) = badge {
        let bl = gtk::Label::new(Some(b));
        bl.add_css_class("device-badge");
        name_line.append(&bl);
    }
    let mt = gtk::Label::new(Some(meta));
    mt.add_css_class("device-meta");
    mt.set_xalign(0.0);
    mt.set_width_chars(1);
    mt.set_wrap(true);
    mt.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    col.append(&name_line);
    col.append(&mt);
    action.add_css_class("small-btn");
    action.set_valign(gtk::Align::Center);
    top.append(&ic);
    top.append(&col);
    top.append(action);
    row.append(&top);
    row
}

/// Set or clear the Controls status line. Every update cancels the previous expiry, so an older timer can never
/// erase newer feedback. Empty text hides the label and frees its space.
fn set_control_status(label: &gtk::Label, timer: &Rc<RefCell<Option<glib::SourceId>>>, text: &str, ttl: Option<Duration>) {
    if let Some(id) = timer.borrow_mut().take() { id.remove(); }
    label.set_text(text);
    label.set_visible(!text.is_empty());
    let Some(ttl) = ttl.filter(|_| !text.is_empty()) else { return };
    let (weak_label, weak_timer) = (label.downgrade(), Rc::downgrade(timer));
    let id = glib::timeout_add_local_once(ttl, move || {
        let Some(timer) = weak_timer.upgrade() else { return };
        timer.borrow_mut().take(); // the source is finished; never remove it again
        if let Some(l) = weak_label.upgrade() {
            l.set_text("");
            l.set_visible(false);
        }
    });
    *timer.borrow_mut() = Some(id);
}

fn bt_row(d: &BtDev, audio_available: bool, tx: &async_channel::Sender<Msg>, btx: &async_channel::Sender<BtSnap>) -> gtk::Box {
    let btn = gtk::Button::with_label(if d.connected { "Disconnect" } else { "Connect" });
    let mut meta = bt_audio_status(d, audio_available)
        .unwrap_or_else(|| if d.connected { "Connected".into() } else if d.paired { "Paired".into() } else { "Not paired".into() });
    if d.connected { if let Some(b) = d.battery { meta.push_str(&format!(" · {b}%")); } }
    let badge = if d.connected && d.audio { if d.active { Some("Active") } else if d.sink.is_some() { Some("Audio") } else { None } } else { None };
    let row = device_row(bt_glyph(&d.icon), &d.name, &meta, badge, d.connected, &btn);
    let (tx, btx, mac, name, connected) = (tx.clone(), btx.clone(), d.mac.clone(), d.name.clone(), d.connected);
    let extra = bt_audio_action(d).map(|a| {
        let b = gtk::Button::with_label(if a == "pair" { "Pair for audio" } else { "Use for audio" });
        b.add_css_class("small-btn");
        b.set_halign(gtk::Align::End);
        (a, b)
    });
    if let Some((_, b)) = &extra { row.append(b); }
    {
        let (row, extra_btn) = (row.clone(), extra.as_ref().map(|(_, b)| b.clone()));
        let (tx, btx, mac, name) = (tx.clone(), btx.clone(), mac.clone(), name.clone());
        btn.connect_clicked(move |b| {
            b.set_sensitive(false);
            b.set_label(if connected { "Disconnecting…" } else { "Connecting…" });
            if let Some(e) = &extra_btn { e.set_sensitive(false); }
            row.add_css_class("busy");
            bt_connect(&tx, Some(&btx), &mac, &name, !connected);
        });
    }
    if let Some((a, eb)) = extra {
        let (row, main_btn) = (row.clone(), btn.clone());
        eb.connect_clicked(move |b| {
            b.set_sensitive(false);
            b.set_label(if a == "pair" { "Pairing…" } else { "Switching…" });
            main_btn.set_sensitive(false);
            row.add_css_class("busy");
            if a == "pair" { bt_connect(&tx, Some(&btx), &mac, &name, true); } else { bt_use_audio(&tx, Some(&btx), &mac, &name); }
        });
    }
    row
}

fn render_bt(list: &gtk::Box, snap: &BtSnap, animate: bool, tx: &async_channel::Sender<Msg>, btx: &async_channel::Sender<BtSnap>) {
    while let Some(c) = list.first_child() { list.remove(&c); }
    let mut items: Vec<gtk::Widget> = Vec::new();
    let header = |t: &str| { let l = gtk::Label::new(Some(t)); l.add_css_class("section-header"); l.add_css_class("wifi-row"); l.upcast::<gtk::Widget>() };
    if !snap.available || !snap.on || snap.devs.is_empty() {
        let l = gtk::Label::new(Some(if !snap.available { "Bluetooth unavailable" } else if snap.on { "No named Bluetooth devices found" } else { "Bluetooth is off" }));
        l.add_css_class("device-empty");
        l.add_css_class("wifi-row");
        items.push(l.upcast());
    } else {
        if snap.devs.iter().any(|d| d.connected) {
            items.push(header("Connected"));
            for d in snap.devs.iter().filter(|d| d.connected) { items.push(bt_row(d, snap.audio_available, tx, btx).upcast()); }
        }
        if snap.devs.iter().any(|d| !d.connected && d.paired) {
            items.push(header("Paired Devices"));
            for d in snap.devs.iter().filter(|d| !d.connected && d.paired) { items.push(bt_row(d, snap.audio_available, tx, btx).upcast()); }
        }
        if snap.devs.iter().any(|d| !d.connected && !d.paired) {
            items.push(header("Available Devices"));
            for d in snap.devs.iter().filter(|d| !d.connected && !d.paired) { items.push(bt_row(d, snap.audio_available, tx, btx).upcast()); }
        }
    }
    for (i, w) in items.into_iter().enumerate() {
        list.append(&w);
        if animate {
            glib::timeout_add_local_once(Duration::from_millis(40 * i as u64 + 16), move || w.add_css_class("shown"));
        } else {
            w.add_css_class("shown");
        }
    }
}

fn wifi_row(net: &WifiNet, tx: &async_channel::Sender<Msg>, wtx: &async_channel::Sender<WifiSnap>, win: &gtk::ApplicationWindow) -> gtk::Box {
    let btn = gtk::Button::with_label(if net.active { "Disconnect" } else { "Connect" });
    let meta = format!("Signal {}% \u{b7} {}", net.signal, if net.secure { "Secured" } else { "Open" });
    let badge = if net.active { Some("Connected") } else if net.saved { Some("Saved") } else { None };
    let icon = if net.active { "\u{f00c}" } else if net.secure { "\u{f023}" } else { "\u{f1eb}" };
    let row = device_row(icon, &net.ssid, &meta, badge, net.active, &btn);

    // inline password prompt for secured networks with no saved profile
    let pw_rev = gtk::Revealer::new();
    pw_rev.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    pw_rev.set_transition_duration(180);
    let pw_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let ent = gtk::PasswordEntry::new();
    ent.set_show_peek_icon(true);
    ent.set_placeholder_text(Some("Password…"));
    ent.set_hexpand(true);
    let join = gtk::Button::with_label("Join");
    join.add_css_class("small-btn");
    join.add_css_class("wifi-toggle");
    pw_box.append(&ent);
    pw_box.append(&join);
    pw_rev.set_child(Some(&pw_box));
    row.append(&pw_rev);

    let needs_pw = net.secure && !net.saved && !net.active;
    let (ssid, active) = (net.ssid.clone(), net.active);
    {
        let (tx, wtx, win, row, pw_rev, ent) = (tx.clone(), wtx.clone(), win.clone(), row.clone(), pw_rev.clone(), ent.clone());
        btn.connect_clicked(move |b| {
            if active || !needs_pw {
                b.set_sensitive(false);
                b.set_label(if active { "Disconnecting…" } else { "Connecting…" });
                row.add_css_class("busy");
                wifi_connect(&tx, Some(&wtx), &ssid, !active, None);
            } else {
                let open = !pw_rev.reveals_child();
                pw_rev.set_reveal_child(open);
                b.set_label(if open { "Cancel" } else { "Connect" });
                win.set_keyboard_mode(if open { KeyboardMode::OnDemand } else { KeyboardMode::None });
                if open { ent.grab_focus(); } else { ent.set_text(""); }
            }
        });
    }
    if needs_pw {
        let (tx, wtx, win, row_c, pw_rev, ent_c, btn, ssid) =
            (tx.clone(), wtx.clone(), win.clone(), row.clone(), pw_rev.clone(), ent.clone(), btn.clone(), net.ssid.clone());
        let submit = std::rc::Rc::new(move || {
            let ent = &ent_c;
            let pw = ent.text().to_string();
            ent.set_text("");
            if pw.is_empty() { ent.add_css_class("error"); return; }
            pw_rev.set_reveal_child(false);
            win.set_keyboard_mode(KeyboardMode::None);
            btn.set_sensitive(false);
            btn.set_label("Connecting…");
            row_c.add_css_class("busy");
            wifi_connect(&tx, Some(&wtx), &ssid, true, Some(pw));
        });
        let s2 = submit.clone();
        ent.connect_activate(move |_| s2());
        join.connect_clicked(move |_| submit());
    }
    row
}

fn render_wifi(list: &gtk::Box, snap: &WifiSnap, animate: bool, tx: &async_channel::Sender<Msg>, wtx: &async_channel::Sender<WifiSnap>, win: &gtk::ApplicationWindow) {
    while let Some(c) = list.first_child() { list.remove(&c); }
    let mut items: Vec<gtk::Widget> = Vec::new();
    let header = |t: &str| { let l = gtk::Label::new(Some(t)); l.add_css_class("section-header"); l.add_css_class("wifi-row"); l.upcast::<gtk::Widget>() };
    if !snap.available || !snap.on || snap.nets.is_empty() {
        let l = gtk::Label::new(Some(if !snap.available { "Wi-Fi unavailable" } else if snap.on { "No Wi-Fi networks found" } else { "Wi-Fi is off" }));
        l.add_css_class("device-empty");
        l.add_css_class("wifi-row");
        items.push(l.upcast());
    } else {
        let mut rest = &snap.nets[..];
        if snap.nets[0].active {
            items.push(header("Connected"));
            items.push(wifi_row(&snap.nets[0], tx, wtx, win).upcast());
            rest = &snap.nets[1..];
        }
        if !rest.is_empty() {
            items.push(header("Available Networks"));
            for n in rest { items.push(wifi_row(n, tx, wtx, win).upcast()); }
        }
    }
    for (i, w) in items.into_iter().enumerate() {
        list.append(&w);
        if animate {
            glib::timeout_add_local_once(Duration::from_millis(40 * i as u64 + 16), move || w.add_css_class("shown"));
        } else {
            w.add_css_class("shown");
        }
    }
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
}

fn hist_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{}/.local/share", std::env::var("HOME").unwrap_or_default()));
    std::path::PathBuf::from(base).join("waybar-notch").join("clipboard.json")
}

/// Largest clipboard history file we are willing to read back.
const MAX_HISTORY_BYTES: u64 = 8 * 1024 * 1024;

/// Reads the clipboard history. A missing file is an empty history; the content is never echoed in errors.
fn load_history_at(path: &std::path::Path) -> Result<Vec<String>, String> {
    match private_files::read_private_bytes(path, MAX_HISTORY_BYTES)? {
        None => Ok(Vec::new()),
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| format!("{} is not valid clipboard history", path.display())),
    }
}

static HIST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomically replaces the history file; the content only ever exists in 0600 files.
fn save_history_at(path: &std::path::Path, history: &[String]) -> Result<(), String> {
    let text = serde_json::to_string(history).map_err(|e| e.to_string())?;
    private_files::write_private_file(path, text.as_bytes(), ".clipboard.json.tmp", &HIST_SEQ)
}

/// Outcome shown after a clear: it is only a full success when the history file was cleared as well.
fn clear_result(disk_cleared: bool, message: String, success: bool) -> (String, bool) {
    if disk_cleared { (message, success) } else { ("Clipboard history file could not be cleared".to_string(), false) }
}

/// Sharp rounded cover + blurred backdrop, both 256x256 immutable paintables built from one texture.
fn album_paintables(texture: &gtk::gdk::Texture) -> (Option<gtk::gdk::Paintable>, Option<gtk::gdk::Paintable>) {
    use gtk::graphene::{Rect, Size};
    const N: f32 = 256.0;
    let (tw, th) = (texture.width() as f32, texture.height() as f32);
    if tw <= 0.0 || th <= 0.0 { return (None, None); }
    // center-cover crop of the texture over a square of `size` at (x, y)
    let cover = |snap: &gtk::Snapshot, x: f32, y: f32, size: f32| {
        let s = (size / tw).max(size / th);
        let (w, h) = (tw * s, th * s);
        snap.append_texture(texture, &Rect::new(x + (size - w) / 2.0, y + (size - h) / 2.0, w, h));
    };
    let bounds = Rect::new(0.0, 0.0, N, N);
    let sharp = gtk::Snapshot::new();
    sharp.push_rounded_clip(&gtk::gsk::RoundedRect::from_rect(bounds, 24.0));
    cover(&sharp, 0.0, 0.0, N);
    sharp.pop();
    let back = gtk::Snapshot::new();
    back.push_clip(&bounds);
    back.push_blur(10.0);
    cover(&back, -24.0, -24.0, N + 48.0);
    back.pop();
    back.pop();
    let size = Size::new(N, N);
    (sharp.to_paintable(Some(&size)), back.to_paintable(Some(&size)))
}

/// Reusable FFT workspace for one capture reader; `frame` allocates nothing.
struct Spectrum {
    raw: [u8; Self::N * 2],
    win: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    bounds: [(usize, usize); BARS],
    prev: [f32; BARS],
}

impl Spectrum {
    const N: usize = 1024;

    fn new() -> Self {
        let n = Self::N;
        let rate = 44100.0f32;
        // log-spaced band edges (Hz)
        let edges = [50.0f32, 140.0, 350.0, 900.0, 2300.0, 5500.0, 14000.0];
        let mut bounds = [(0, 0); BARS];
        for (k, b) in bounds.iter_mut().enumerate() {
            let lo = ((edges[k] * n as f32 / rate) as usize).max(1);
            let hi = ((edges[k + 1] * n as f32 / rate) as usize).max(lo + 1).min(n / 2);
            *b = (lo, hi);
        }
        Self {
            raw: [0; Self::N * 2],
            win: (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect(),
            re: vec![0.0; n],
            im: vec![0.0; n],
            bounds,
            prev: [0.0; BARS],
        }
    }

    /// Un-smoothed band levels (0..1) of the samples currently in `raw`.
    fn analyze(&mut self) -> [f32; BARS] {
        let n = Self::N;
        for i in 0..n {
            self.re[i] = i16::from_le_bytes([self.raw[2 * i], self.raw[2 * i + 1]]) as f32 / 32768.0 * self.win[i];
        }
        self.im.fill(0.0);
        fft(&mut self.re, &mut self.im);
        let mut out = [0.0f32; BARS];
        for (k, &(lo, hi)) in self.bounds.iter().enumerate() {
            let cnt = (hi - lo) as f32;
            let rms = ((lo..hi).map(|i| self.re[i] * self.re[i] + self.im[i] * self.im[i]).sum::<f32>() / cnt).sqrt();
            let amp = rms / (n as f32 / 4.0);                         // ~sine amplitude after the Hann window
            let db = 20.0 * amp.max(1e-6).log10() + 5.5 * k as f32;   // +5.5 dB/band: music is bass-heavy
            out[k] = ((db + 64.0) / 54.0).clamp(0.0, 1.0).powf(1.25); // compress: calmer mid heights
        }
        out
    }

    fn frame(&mut self) -> [f64; BARS] {
        let v = self.analyze();
        let mut bars = [0.0f64; BARS];
        for k in 0..BARS {
            // ease both directions so the bars glide instead of flicker
            self.prev[k] += (v[k] - self.prev[k]) * if v[k] > self.prev[k] { 0.35 } else { 0.12 };
            bars[k] = self.prev[k].max(0.08) as f64;
        }
        bars
    }
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// In-place radix-2 FFT (length must be a power of two).
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 { j ^= bit; bit >>= 1; }
        j ^= bit;
        if i < j { re.swap(i, j); im.swap(i, j); }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for i in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (i + k, i + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr; im[b] = im[a] - ti;
                re[a] += tr; im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        len <<= 1;
    }
}

/// The seven dates (Sunday first) of the week containing `today`.
fn week_days(today: chrono::NaiveDate) -> [chrono::NaiveDate; 7] {
    use chrono::Datelike;
    let start = today - chrono::Duration::days(today.weekday().num_days_from_sunday() as i64);
    std::array::from_fn(|i| start + chrono::Duration::days(i as i64))
}

/// Fill the Home week strip: weekday header row plus the seven days (Sunday first) of the week containing `today`.
fn fill_home_week(grid: &gtk::Grid, title: &gtk::Label, today: chrono::NaiveDate) {
    use chrono::Datelike;
    while let Some(c) = grid.first_child() { grid.remove(&c); }
    title.set_text(&today.format("%B %Y").to_string());
    for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
        let l = gtk::Label::new(Some(d));
        l.add_css_class("mc-dow");
        l.set_hexpand(true);
        grid.attach(&l, i as i32, 0, 1, 1);
    }
    for (n, d) in week_days(today).into_iter().enumerate() {
        let n = n as i32;
        let l = gtk::Label::new(Some(&d.day().to_string()));
        l.add_css_class("mc-day");
        l.set_halign(gtk::Align::Center);
        if d.month() != today.month() || d.year() != today.year() { l.add_css_class("mc-dim"); }
        if d == today { l.add_css_class("mc-today"); }
        grid.attach(&l, n, 1, 1, 1);
    }
}

/// Delay before the next Google Calendar poll: long after success, short while setup/network needs recovery.
fn gcal_poll_delay(u: &gcal::Update) -> Duration {
    Duration::from_secs(match u {
        gcal::Update::Events(_) => 600,
        gcal::Update::NoConfig | gcal::Update::NeedAuth | gcal::Update::Error(_) => 30,
    })
}

/// Google Calendar poll: every 10 minutes after success, every 30 s while not configured, signed out or failing.
fn gcal_thread(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || loop {
        let u = gcal::fetch();
        let wait = gcal_poll_delay(&u);
        let _ = tx.try_send(Msg::Gcal(u));
        std::thread::sleep(wait);
    });
}

/// Let every scroller under `root` report its content size (bounded by the budgets) instead of a minimal request.
fn prepare_page_measure(root: &gtk::Widget, max_w: i32, max_h: i32) {
    if let Some(sw) = root.downcast_ref::<gtk::ScrolledWindow>() {
        sw.set_propagate_natural_width(true);
        sw.set_propagate_natural_height(true);
        sw.set_min_content_width(0);
        sw.set_min_content_height(0);
        sw.set_max_content_width(max_w);
        sw.set_max_content_height(max_h);
    }
    let mut c = root.first_child();
    while let Some(w) = c {
        prepare_page_measure(&w, max_w, max_h);
        c = w.next_sibling();
    }
}

/// Natural height of a page at `width`. Scrollers report their child's height-for-width at the
/// width the viewport will actually give it (minus scroller margins), not a stale allocation.
fn page_natural_height(page: &gtk::Widget, width: i32) -> i32 {
    if let Some(child) = page.downcast_ref::<gtk::ScrolledWindow>().and_then(|sw| sw.child()) {
        let w = (width - page.margin_start() - page.margin_end()).max(1);
        return child.measure(gtk::Orientation::Vertical, w).1 + page.margin_top() + page.margin_bottom();
    }
    page.measure(gtk::Orientation::Vertical, width).1
}

/// Natural size bounded to `[floor, ceiling]`; the ceiling wins on outputs smaller than the floor.
fn fit_extent(natural: f64, floor: f64, ceiling: f64) -> f64 {
    natural.max(floor).min(ceiling).max(1.0)
}

/// Song metadata: natural width is bounded so text length never widens the notch; wraps instead.
fn metadata_label(class: &str, xalign: f32, text: Option<&str>) -> gtk::Label {
    let l = gtk::Label::new(text);
    l.add_css_class(class);
    l.set_hexpand(true);
    l.set_halign(gtk::Align::Fill);
    l.set_vexpand(false);
    l.set_width_chars(1);
    l.set_max_width_chars(1);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_ellipsize(gtk::pango::EllipsizeMode::None);
    l.set_xalign(xalign);
    l.set_justify(if xalign == 0.0 { gtk::Justification::Left } else { gtk::Justification::Center });
    l
}

/// Lyric line: wraps at word (or character, for long words) boundaries to the width it is given.
fn lyric_label(class: &str, align: gtk::Align) -> gtk::Label {
    let l = gtk::Label::new(Some(" "));
    l.add_css_class(class);
    l.set_hexpand(true);
    l.set_vexpand(false);
    l.set_valign(align);
    l.set_xalign(0.5);
    l.set_justify(gtk::Justification::Center);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_ellipsize(gtk::pango::EllipsizeMode::None);
    l.set_width_chars(1);
    l.set_max_width_chars(1);
    l
}

fn lerp(a: f64, b: f64, t: f64) -> f64 { a + (b - a) * t }

const MEDIA_MAX_H: f64 = 440.0;

/// Square cover side: at most 128 and the page width minus margins, shrunk to the height the fixed controls leave,
/// but never below 64 (beyond that the player region scrolls instead).
fn media_cover_side(page_width: i32, page_budget: i32, fixed_height: i32) -> i32 {
    (page_width.max(1) - 16).min(128).min((page_budget - fixed_height).max(64)).max(1)
}

/// (current, next) lyric rows for `pos`. Synced: before the first stamp the current row is blank; at the last
/// stamp next is blank. Plain lyrics have no timing, so they stay a static two-row excerpt.
fn lyric_pair(l: &lyrics::Lyrics, pos: f64) -> (&str, &str) {
    fn at(v: &[(f64, String)], i: usize) -> &str { v.get(i).map_or("", |(_, t)| t.as_str()) }
    match l {
        lyrics::Lyrics::Synced(v) => match lyrics::current(v, pos) {
            None => ("", at(v, 0)),
            Some(i) => (at(v, i), at(v, i + 1)),
        },
        lyrics::Lyrics::Plain(v) => (v.first().map_or("", |s| s.as_str()), v.get(1).map_or("", |s| s.as_str())),
        lyrics::Lyrics::None => ("", ""),
    }
}

/// Hyprland address (`0x` + hex) of the single mapped window that belongs to the player process or, failing a
/// PID match, to its desktop entry. Ambiguity yields None so the wrong window is never focused.
fn player_window_address<'a>(clients: &'a serde_json::Value, pid: Option<u32>, desktop_entry: Option<&str>) -> Option<&'a str> {
    let arr = clients.as_array()?;
    let mapped = || arr.iter().filter(|c| c["mapped"].as_bool().unwrap_or(false));
    let addr = |c: &'a serde_json::Value| -> Option<&'a str> {
        let a = c["address"].as_str()?;
        let hex = a.strip_prefix("0x")?;
        (!hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(a)
    };
    if let Some(pid) = pid {
        let mut m = mapped().filter(|c| c["pid"].as_u64() == Some(pid as u64));
        if let Some(first) = m.next() {
            return if m.next().is_none() { addr(first) } else { None };
        }
    }
    let de = desktop_entry.filter(|s| !s.is_empty())?;
    let mut m = mapped().filter(|c| ["class", "initialClass"].iter().any(|k| c[*k].as_str().is_some_and(|s| s.eq_ignore_ascii_case(de))));
    let first = m.next()?;
    if m.next().is_some() { return None; }
    addr(first)
}

fn player_service_name(instance: &str) -> Option<String> {
    let name = format!("org.mpris.MediaPlayer2.{instance}");
    let ok = zbus::names::WellKnownName::try_from(name.as_str()).is_ok();
    ok.then_some(name)
}

/// Bring the player's app forward: MPRIS Raise (when offered) plus a precise Hyprland focus of its mapped window.
/// Blocking; call off the GTK thread. Never launches anything.
fn raise_player_app(instance: &str) -> Result<(), String> {
    use gtk::gio;
    let name = player_service_name(instance).ok_or_else(|| format!("Invalid player name: {instance}"))?;
    let conn = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).map_err(|e| e.to_string())?;
    let call = |dest: &str, path: &str, iface: &str, method: &str, params: Option<&glib::Variant>| {
        conn.call_sync(Some(dest), path, iface, method, params, None, gio::DBusCallFlags::NO_AUTO_START, 1500, None::<&gio::Cancellable>)
    };
    const BUS: (&str, &str, &str) = ("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus");
    const MPRIS: (&str, &str) = ("/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2");
    let owner = call(BUS.0, BUS.1, BUS.2, "GetNameOwner", Some(&(name.as_str(),).to_variant()))
        .map_err(|e| format!("Player is not running: {e}"))?
        .child_value(0).get::<String>().ok_or("Player is not running")?;
    let mut err: Option<String> = None;
    let (mut can_raise, mut entry) = (false, None::<String>);
    match call(&owner, MPRIS.0, "org.freedesktop.DBus.Properties", "GetAll", Some(&(MPRIS.1,).to_variant())) {
        Ok(v) => {
            let d = glib::VariantDict::new(Some(&v.child_value(0)));
            can_raise = d.lookup::<bool>("CanRaise").ok().flatten().unwrap_or(false);
            entry = d.lookup::<String>("DesktopEntry").ok().flatten();
        }
        Err(e) => err = Some(format!("Player did not answer: {e}")),
    }
    let mut done = false;
    if can_raise {
        match call(&owner, MPRIS.0, MPRIS.1, "Raise", None) {
            Ok(_) => done = true,
            Err(e) => err = Some(format!("Raise failed: {e}")),
        }
    }
    let pid = call(BUS.0, BUS.1, BUS.2, "GetConnectionUnixProcessID", Some(&(owner.as_str(),).to_variant()))
        .ok().and_then(|v| v.child_value(0).get::<u32>());
    let clients = desktop::hypr_req("j/clients").and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    if let Some(addr) = clients.as_ref().and_then(|c| player_window_address(c, pid, entry.as_deref())) {
        match desktop::hypr_req(&format!("dispatch hl.dsp.focus({{ window = \"address:{addr}\" }})")) {
            Some(r) if r.trim() == "ok" => done = true,
            Some(r) => err = Some(format!("Compositor refused focus: {}", r.trim())),
            None => err = Some("Compositor focus request failed".into()),
        }
    }
    if done { Ok(()) } else { Err(err.unwrap_or_else(|| "This player cannot open its window".into())) }
}

fn media_thread(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || loop {
        let _ = tx.try_send(Msg::NoMedia);
        let child = Command::new("playerctl")
            .args(["-p", PLAYERS, "metadata", "-F", "--format",
                "{{status}}\t{{artist}}\t{{title}}\t{{mpris:artUrl}}\t{{mpris:length}}\t{{playerInstance}}"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let p: Vec<&str> = line.split('\t').collect();
                    if p.len() < 5 || p[0].is_empty() {
                        let _ = tx.try_send(Msg::NoMedia);
                        continue;
                    }
                    let _ = tx.try_send(Msg::Media {
                        status: p[0].into(), artist: p[1].into(), title: p[2].into(),
                        art_url: p[3].into(), len: p[4].parse::<f64>().unwrap_or(0.0) / 1e6,
                        player: p.get(5).copied().unwrap_or("").into(),
                    });
                }
            }
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

/// Block until the notch is expanded (`true`), then run `work` immediately and every `period` until collapsed.
/// Collapsed means fully dormant: the thread waits on the channel instead of waking on a timer.
fn activity_worker(rx: std::sync::mpsc::Receiver<bool>, period: Duration, mut work: impl FnMut() -> bool + Send + 'static) {
    std::thread::spawn(move || {
        let mut active = false;
        loop {
            if !active {
                match rx.recv() { Ok(a) => active = a, Err(_) => return }
                continue;
            }
            if !work() { return; }
            match rx.recv_timeout(period) {
                Ok(a) => active = a,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

fn position_thread(tx: async_channel::Sender<Msg>, rx: std::sync::mpsc::Receiver<bool>) {
    activity_worker(rx, Duration::from_millis(1000), move || {
        if let Ok(p) = sh_out("playerctl", &["-p", PLAYERS, "position"]).parse::<f64>() {
            return tx.try_send(Msg::Pos(p)).is_ok() || !tx.is_closed();
        }
        true
    });
}

/// Cover files that must survive pruning: the one being fetched and the one on screen.
#[derive(Default)]
struct ArtPins { current: Option<std::path::PathBuf>, accepted: Option<std::path::PathBuf> }

impl ArtPins {
    fn list(&self) -> Vec<std::path::PathBuf> { self.current.iter().chain(self.accepted.iter()).cloned().collect() }
}

const ART_TIMEOUT: Duration = Duration::from_secs(6);
static ART_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

enum ArtFail { Final, Retry }

/// Streams one http(s) cover into the new, private file `tmp`, never writing more than `art_cache::MAX_ART_BYTES`.
fn art_download(agent: &ureq::Agent, url: &str, tmp: &std::path::Path, timeout: Duration) -> Result<(), ArtFail> {
    use std::os::unix::fs::OpenOptionsExt;
    let body = art_http::get(agent, url, timeout).map_err(|e| match e { art_http::GetError::Final => ArtFail::Final, art_http::GetError::Retry => ArtFail::Retry })?;
    if body.content_length().is_some_and(|n| n > art_cache::MAX_ART_BYTES) { return Err(ArtFail::Final); }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(tmp).map_err(|_| ArtFail::Retry)?;
    match art_cache::copy_limited(&mut body.into_reader(), &mut f) {
        Ok(0) => Err(ArtFail::Retry),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(ArtFail::Final),
        Err(_) => Err(ArtFail::Retry),
    }
}

/// Returns the cached path of `url`'s cover, downloading it when needed. `None` when the cover is unusable, too
/// large, or superseded by a newer request waiting in `pending`.
fn fetch_art_in(agent: &ureq::Agent, dir: &std::path::Path, url: &str, pending: &async_channel::Receiver<String>, timeout: Duration) -> Option<String> {
    let path = art_cache::path_for(dir, url);
    if art_cache::is_usable(&path) { return Some(path.to_string_lossy().into_owned()); }
    let deadline = std::time::Instant::now() + timeout;
    for attempt in 0..3 {
        if !pending.is_empty() { return None; }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() { return None; }
        let tmp = std::path::PathBuf::from(format!("{}.part.{}.{}", path.display(), std::process::id(), ART_SEQ.fetch_add(1, Ordering::Relaxed)));
        match art_download(agent, url, &tmp, remaining) {
            Ok(()) if pending.is_empty() && std::time::Instant::now() < deadline && std::fs::rename(&tmp, &path).is_ok() => return Some(path.to_string_lossy().into_owned()),
            Ok(()) if !pending.is_empty() => { let _ = std::fs::remove_file(&tmp); return None; }
            Ok(()) | Err(ArtFail::Retry) => { let _ = std::fs::remove_file(&tmp); }
            Err(ArtFail::Final) => { let _ = std::fs::remove_file(&tmp); return None; }
        }
        if attempt < 2 {
            if !pending.is_empty() { return None; }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() { return None; }
            std::thread::sleep(left.min(Duration::from_millis(600)));
        }
    }
    None
}

const ART_MAX_EDGE: i32 = 384;

fn art_is_remote(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// One background thread downloads covers: one transfer at a time, and only the newest requested URL waits.
struct ArtWorker {
    tx: async_channel::Sender<String>,
    rx: async_channel::Receiver<String>,
    dir: std::path::PathBuf,
    pins: Arc<std::sync::Mutex<ArtPins>>,
}

impl ArtWorker {
    fn new(tx: async_channel::Sender<Msg>) -> Self {
        Self::with(tx, std::path::PathBuf::from(runtime_dir()), ART_TIMEOUT, art_http::agent(ART_TIMEOUT))
    }

    fn with(tx: async_channel::Sender<Msg>, dir: std::path::PathBuf, timeout: Duration, agent: ureq::Agent) -> Self {
        let (ptx, prx) = async_channel::bounded::<String>(1);
        let pins = Arc::new(std::sync::Mutex::new(ArtPins::default()));
        let (wrx, wdir, wpins) = (prx.clone(), dir.clone(), pins.clone());
        let snapshot = move || wpins.lock().unwrap_or_else(|e| e.into_inner()).list();
        std::thread::spawn(move || {
            while let Ok(url) = wrx.recv_blocking() {
                if url.is_empty() { continue; }
                if !art_is_remote(&url) {
                    // Local artwork: validated and decoded here, never on the GTK thread.
                    let Some(path) = image_loader::local_path(&url) else { continue };
                    let Ok(image) = image_loader::load_file(&path, ART_MAX_EDGE) else { continue };
                    if wrx.is_empty() { let _ = tx.try_send(Msg::Art { url, path, image }); }
                    continue;
                }
                art_cache::prune(&wdir, art_cache::KEEP, &snapshot());
                let Some(path) = fetch_art_in(&agent, &wdir, &url, &wrx, timeout) else { continue };
                let path = std::path::PathBuf::from(path);
                let image = match image_loader::load_file(&path, ART_MAX_EDGE) {
                    Ok(image) => image,
                    Err(_) => { let _ = std::fs::remove_file(&path); continue; }   // undecodable cover: never cache or show it
                };
                let mut pins = snapshot();
                pins.push(path.clone());
                art_cache::prune(&wdir, art_cache::KEEP, &pins);
                if wrx.is_empty() { let _ = tx.try_send(Msg::Art { url, path, image }); }
            }
        });
        ArtWorker { tx: ptx, rx: prx, dir, pins }
    }

    /// Replaces any waiting request with `url` (empty = no cover wanted); never starts a second transfer.
    fn submit(&self, url: String) {
        let current = art_is_remote(&url).then(|| art_cache::path_for(&self.dir, &url));
        self.pins.lock().unwrap_or_else(|e| e.into_inner()).current = current;
        while self.rx.try_recv().is_ok() {}
        let _ = self.tx.try_send(url);
    }

    /// The cover now on screen stays pinned until the next one is accepted.
    fn accept(&self, path: &std::path::Path) {
        self.pins.lock().unwrap_or_else(|e| e.into_inner()).accepted = Some(path.to_path_buf());
    }
}

/// Polls AI usage every 2 s while the notch is expanded; the tracker only reads new log bytes.
fn usage_thread(tx: async_channel::Sender<Msg>, rx: std::sync::mpsc::Receiver<bool>) {
    let mut tracker = usage::Tracker::default();
    let mut last: Option<usage::Usage> = None;
    activity_worker(rx, Duration::from_secs(2), move || {
        let u = tracker.poll(&usage::Sources::detect(), chrono::Local::now());
        if last.as_ref() != Some(&u) {
            if tx.try_send(Msg::Usage(Box::new(u.clone()))).is_err() { return false; }
            last = Some(u);
        }
        true
    });
}

#[derive(Clone)]
struct WeatherInfo { compact: String, location: String, wind: String }

/// Parse `City  ·  Temp +25°C  ·  Wind ↑5km/h`; malformed output yields None rather than partial fields.
fn parse_weather_status(icon: &str, status: &str) -> Option<WeatherInfo> {
    let parts: Vec<&str> = status.split('·').map(str::trim).collect();
    let [city, temp, wind] = parts[..] else { return None };
    let temp = temp.strip_prefix("Temp ")?.trim();
    let wind = wind.strip_prefix("Wind ")?.trim();
    if city.is_empty() || temp.is_empty() || wind.is_empty() { return None; }
    let temp = temp.strip_prefix('+').unwrap_or(temp);
    Some(WeatherInfo { compact: format!("{icon} {temp}"), location: city.to_string(), wind: format!("Wind {wind}") })
}

fn weather_snapshot(icon: &str, status: Option<&str>, previous: Option<WeatherInfo>) -> WeatherInfo {
    status.and_then(|s| parse_weather_status(icon, s)).or(previous)
        .unwrap_or_else(|| WeatherInfo { compact: icon.to_string(), location: "Weather unavailable".into(), wind: String::new() })
}

fn weather_thread(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        const CLOUD: &str = "\u{f0c2}";
        if tx.send_blocking(Msg::Weather(weather_snapshot(CLOUD, None, None))).is_err() { return; }
        let mut last: Option<WeatherInfo> = None;
        loop {
            let run = |c: &str| std::process::Command::new(c).output().ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());
            let icon = run("omarchy-weather-icon").unwrap_or_else(|| CLOUD.to_string());
            let status = run("omarchy-weather-status");
            let snap = weather_snapshot(&icon, status.as_deref(), last.take());
            last = Some(snap.clone());
            if tx.send_blocking(Msg::Weather(snap)).is_err() { return; }
            std::thread::sleep(Duration::from_secs(600));
        }
    });
}

fn stats_thread(tx: async_channel::Sender<Msg>) {
    std::thread::spawn(move || {
        let p = power::snapshot();
        let batt = if !p.available {
            "Power \u{2014}".to_string()
        } else if p.batteries.is_empty() {
            if p.ac_online == Some(true) { "AC".to_string() } else { "No battery".to_string() }
        } else {
            let charging = p.batteries.len() == 1 && p.batteries[0].state == power::BatteryState::Charging;
            let glyph = if charging { "\u{f0e7}" } else { "\u{f240}" };
            match p.percentage { Some(v) => format!("{glyph} {}%", v.round() as u32), None => format!("{glyph} \u{2014}") }
        };
        let vol = popups::audio_status();
        let wifi_on = sh_out("nmcli", &["radio", "wifi"]).trim() == "enabled";
        let wifi = if !wifi_on { "\u{f092e} off".to_string() } else {
            sh_out("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"]).lines()
                .find_map(|l| l.strip_prefix("yes:").map(String::from))
                .map(|n| format!("\u{f1eb} {n}")).unwrap_or("\u{f1eb} offline".into())
        };
        let bt_on = sh_out("bluetoothctl", &["show"]).contains("Powered: yes");
        let bt = format!("\u{f293} {}", if bt_on { "on" } else { "off" });
        let _ = tx.try_send(Msg::Stats(batt, vol, wifi, bt, bt_on));
    });
}

fn clipboard_thread(tx: async_channel::Sender<Msg>) {
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
                            let _ = tx.try_send(Msg::Clip(s));
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

// ---------------------------------------------------------------- UI logic
impl Ui {
    fn animations(&self) -> bool {
        self.settings.borrow().behavior.animations
    }

    /// True while the user (or an animation still settling) holds the notch or a popup on its current output.
    fn interaction_active(&self) -> bool {
        self.expanded.load(Ordering::Relaxed) || self.prog.get() > 0.02
    }

    fn apply(&self, t: f64) {
        let e = t; // t is the spring position: 0 = collapsed, 1 = expanded (may overshoot a hair)
        let g = self.geom.get();
        let s = self.size.get();
        let w = lerp(g.coll_w, s.w, e).max(g.coll_w);
        let h = lerp(g.coll_h, s.h, e).max(g.coll_h);
        self.notch.set_size_request(w as i32, h as i32);
        // The layer window keeps one size (no per-frame resize); only the clickable region follows the notch.
        if let Some(surf) = self.win.surface() {
            let host_w = if self.win.width() > 0 { self.win.width() } else { self.host.get().0 };
            let x = (host_w - w as i32) / 2;
            surf.set_input_region(Some(&cairo::Region::create_rectangle(&cairo::RectangleInt::new(x, 0, w.ceil() as i32, h.ceil() as i32))));
        }
        let tc = t.clamp(0.0, 1.0);
        // Strict hand-over: the small row is gone before the big card appears (and vice versa).
        let co = (1.0 - tc / 0.22).clamp(0.0, 1.0);
        let eo = ((tc - 0.5) / 0.4).clamp(0.0, 1.0);
        // fully transparent regions are unmapped so they cost no rendering; remap before the hand-over starts
        self.coll.set_child_visible(co > 0.0);
        self.exp.set_child_visible(eo > 0.0);
        self.media_background.set_child_visible(eo > 0.0);
        self.coll.set_opacity(co);
        self.exp.set_opacity(eo);
        // decorative album tint only: keeps the shared translucent glass visible behind the panel
        self.media_background.set_opacity(eo * 0.25);
        self.coll.set_can_target(co > 0.5);
        self.exp.set_can_target(eo > 0.5);
    }

    fn set_expanded(self: &Rc<Self>, expand: bool) {
        if expand && (self.capture_pending.get() || self.capture_active.get()) { return; }
        if !expand { self.ext_drop("collapsed", Some("home")); }
        // Hovering again keeps the tab you left on; staying away for a while sends it back to Home.
        if let Some(id) = self.reset_id.borrow_mut().take() { id.remove(); }
        let reset_secs = self.settings.borrow().behavior.home_reset_secs;
        if !expand && reset_secs > 0 {
            let u = self.clone();
            *self.reset_id.borrow_mut() = Some(glib::timeout_add_seconds_local_once(reset_secs, move || {
                u.reset_id.borrow_mut().take();
                if !u.expanded.load(Ordering::Relaxed) && u.stack.visible_child_name().as_deref() != Some("settings") { u.show_tab("home"); }
            }));
        }
        if !expand { self.clear_control_status(); }
        self.target.set(if expand { 1.0 } else { 0.0 });
        self.expanded.store(expand, Ordering::Relaxed);
        for w in self.workers.borrow().iter() { let _ = w.send(expand); }
        self.sync_visualizer();
        if expand {
            let _ = self.tx.try_send(Msg::Pos(f64::NAN)); // refresh position promptly
            // Stats and usage cost several shell calls and rebuild the AI page on the GTK thread;
            // re-expanding within 15 s reuses the last snapshot so the open animation stays clear of that work.
            let stale = self.refresh_at.get().map_or(true, |t| t.elapsed() > Duration::from_secs(15));
            if stale {
                self.refresh_at.set(Some(std::time::Instant::now()));
                stats_thread(self.tx.clone());
            }
        }
        self.sync_utility();
        self.kick();
    }

    fn schedule_collapse(self: &Rc<Self>, ms: u64) {
        self.cancel_collapse();
        let u = self.clone();
        let id = glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            u.collapse_id.borrow_mut().take();
            let page = u.stack.visible_child_name();
            if page.as_deref() == Some("settings") { return; } // editing: stays open until Back
            // a slider drag that left the notch must not close the page under the pointer
            if page.as_deref() == Some("utility") && u.popups.is_interacting() { u.schedule_collapse(ms); return; }
            // a live editor or pointer in the WhatsApp content keeps the page; otherwise hide it before collapsing
            if u.ext.borrow().as_ref().is_some_and(|e| e.s.holds_open()) { u.schedule_collapse(ms); return; }
            if u.ext_close(CloseTo::Collapse) { return; }
            u.set_expanded(false);
        });
        *self.collapse_id.borrow_mut() = Some(id);
    }

    fn show_tab(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        for (b, n) in self.tab_btns.borrow().iter() {
            if *n == name { b.add_css_class("active") } else { b.remove_css_class("active") }
        }
    }

    /// Fit the card to the visible page: width and height follow that page's own content, bounded by the
    /// output-derived ceilings; long bodies scroll inside their ScrolledWindow.
    fn set_expanded_size(self: &Rc<Self>) {
        let Some(page) = self.stack.visible_child() else { return };
        let g = self.geom.get();
        // a formerly tall/wide page must not keep the card large
        self.stack.set_size_request(-1, -1);
        let sel = self.stack.visible_child_name();
        let is_media = sel.as_deref() == Some("media");
        let is_wa = sel.as_deref() == Some("whatsapp");
        let prev_size = (self.size.get().tw, self.size.get().th);
        let apply_compact = |compact: bool| {
            if compact { self.notch.add_css_class("compact-layout") } else { self.notch.remove_css_class("compact-layout") }
            let (em, pm) = if compact { (12, 8) } else { (16, 12) };
            self.exp.set_margin_start(em);
            self.exp.set_margin_end(em);
            for p in [&self.utility_page, &self.wifi_page, &self.bt_page] {
                p.set_margin_start(pm);
                p.set_margin_end(pm);
            }
        };
        let content_w = |s: &Self| {
            let margins = (s.exp.margin_start() + s.exp.margin_end()) as f64;
            let nat = (page.measure(gtk::Orientation::Horizontal, -1).1).max(s.tabs.measure(gtk::Orientation::Horizontal, -1).1) as f64;
            fit_extent(nat + margins, g.coll_w.max(360.0), g.exp_w)
        };
        let was_compact = self.notch.has_css_class("compact-layout");
        apply_compact(was_compact);
        let vpad = (self.exp.margin_top() + self.exp.margin_bottom() + self.exp.spacing()) as f64;
        let tabs_h0 = self.tabs.measure(gtk::Orientation::Vertical, -1).1 as f64;
        prepare_page_measure(&page, (g.exp_w as i32).max(1), ((g.exp_h_max - vpad - tabs_h0) as i32).max(1));
        let mut tw = content_w(self);
        if (tw <= 520.0) != was_compact {
            apply_compact(tw <= 520.0);
            tw = content_w(self);
        }
        // the WhatsApp page is as wide as the configured card; its content is hosted, not measured
        if is_wa { tw = g.exp_w; }
        let pw = ((tw as i32) - self.exp.margin_start() - self.exp.margin_end()).max(1);
        let chrome = self.tabs.measure(gtk::Orientation::Vertical, pw).1 as f64 + vpad;
        let ceiling = if is_media { MEDIA_MAX_H.min(g.exp_h_max) } else { g.exp_h_max };
        let ph = if is_wa {
            WA_CONTENT_H
        } else if is_media {
            // compact player: the cover takes whatever the fixed controls leave of the output-bounded budget
            let budget = (ceiling.floor() - chrome) as i32;
            self.m_cover_spacer.set_size_request(0, 0);
            let fixed = page_natural_height(&page, pw);
            let side = media_cover_side(pw, budget, fixed);
            self.m_cover_spacer.set_size_request(side, side);
            page_natural_height(&page, pw) as f64
        } else {
            page_natural_height(&page, pw) as f64
        };
        let th = fit_extent(ph + chrome, chrome + 40.0, ceiling);
        self.stack.set_size_request(-1, (th - chrome).max(0.0) as i32);
        let mut s = self.size.get();
        s.tw = tw;
        s.th = th;
        if self.prog.get() < 0.01 || !self.animations() { s = Size { w: s.tw, h: s.th, vw: 0.0, vh: 0.0, ..s }; }
        self.size.set(s);
        let host = g.host(HOST_RESERVE_H.max(th));
        if host != self.host.get() {
            self.host.set(host);
            self.win.set_default_size(host.0, host.1);
            self.win.set_size_request(host.0, host.1);
        }
        if self.ext.borrow().is_some() {
            if prev_size != (tw, th) { self.ext_invalidate(); }
            self.ext_schedule_publish();
        }
        self.kick();
    }

    /// Re-derive sizes from the current settings and the destination output's logical size.
    fn relayout(self: &Rc<Self>) {
        let logical = self.dest_info.borrow().as_ref().map(|m| (m.logical_width, m.logical_height));
        let g = desktop::Geometry::compute(&self.settings.borrow().layout, logical);
        self.geom.set(g);
        self.ext_invalidate();
        self.win.set_margin(Edge::Top, g.top_margin);
        self.coll.set_size_request(-1, g.coll_h as i32);
        self.osd.set_collapsed(g.coll_w, g.coll_h);
        self.set_expanded_size();
    }

    /// Jump to the end state of the open/close spring (animations off, or a forced collapse).
    fn snap(self: &Rc<Self>) {
        self.prog.set(self.target.get());
        self.vel.set(0.0);
        let s = self.size.get();
        self.size.set(Size { w: s.tw, h: s.th, vw: 0.0, vh: 0.0, ..s });
        self.apply(self.prog.get());
        self.settled();
    }

    /// The open/close motion ended: pick up any destination change that waited for the interaction.
    fn settled(self: &Rc<Self>) {
        self.ext_schedule_publish();
        if !self.interaction_active() { self.reconcile(); }
    }

    // One frame loop drives the open/close spring and the size spring; it stops once both settle.
    fn kick(self: &Rc<Self>) {
        if !self.animations() { return self.snap(); }
        if self.animating.replace(true) { return; }
        let u = self.clone();
        let last = Cell::new(0i64);
        let stats = Cell::new((0u32, 0i64, 0i64)); // frames, worst dt (us), total dt (us)
        self.notch.add_tick_callback(move |_, clock| {
            let now = clock.frame_time();
            let dt = if last.get() == 0 { 16_000 } else { now - last.get() };
            last.set(now);
            let (f, w, t) = stats.get();
            stats.set((f + 1, w.max(dt), t + dt));
            let dt = (dt as f64 / 1e6).min(0.033);
            let tg = u.target.get();
            let (mut nt, mut nv) = spring(u.prog.get(), u.vel.get(), tg, dt);
            let t_done = (nt - tg).abs() < 0.0015 && nv.abs() < 0.02;
            if t_done { nt = tg; nv = 0.0; }
            u.prog.set(nt);
            u.vel.set(nv);
            let s = u.size.get();
            let (w, vw) = spring(s.w, s.vw, s.tw, dt);
            let (h, vh) = spring(s.h, s.vh, s.th, dt);
            let s_done = (w - s.tw).abs() < 0.5 && vw.abs() < 1.0 && (h - s.th).abs() < 0.5 && vh.abs() < 1.0;
            u.size.set(if s_done { Size { w: s.tw, h: s.th, vw: 0.0, vh: 0.0, ..s } } else { Size { w, h, vw, vh, ..s } });
            u.apply(nt);
            if t_done && s_done {
                u.animating.set(false);
                if std::env::var_os("NOTCH_DEBUG").is_some() {
                    let (f, w, t) = stats.get();
                    eprintln!("anim {}: {f} frames, avg {:.1} ms, worst {:.1} ms", if tg > 0.5 { "open" } else { "close" }, t as f64 / f as f64 / 1000.0, w as f64 / 1000.0);
                }
                u.settled();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    // ---------------------------------------------------------------- destination output

    /// Collapse everything at once and give back input focus (the output is going away, or the bar is being hidden).
    fn force_idle(self: &Rc<Self>) {
        self.ext_drop("idle", Some("home"));
        self.cancel_collapse();
        self.popups.close();
        self.win.set_keyboard_mode(KeyboardMode::None);
        self.clear_control_status();
        self.expanded.store(false, Ordering::Relaxed);
        for w in self.workers.borrow().iter() { let _ = w.send(false); }
        self.sync_visualizer();
        self.sync_utility();
        self.target.set(0.0);
        self.prog.set(0.0);
        self.vel.set(0.0);
        self.apply(0.0);
    }

    /// Build the Settings UI on first use (it is large and mostly never opened) and keep it for later visits.
    fn ensure_settings_page(self: &Rc<Self>) -> Rc<settings::SettingsPage> {
        if let Some(p) = self.settings_page.borrow().clone() { return p; }
        let (stack, win) = (self.stack.clone(), self.win.clone());
        let page = settings::SettingsPage::new(self.tx.clone(), self.settings.borrow().clone(), Rc::new(move || {
            win.set_keyboard_mode(KeyboardMode::None);
            stack.set_visible_child_name("controls");
        }));
        if let Some(snap) = self.desk.borrow().as_ref() {
            page.set_outputs(snap.monitors.iter().map(|m| m.name.clone()).collect());
        }
        self.settings_host.append(&page.root);
        *self.settings_page.borrow_mut() = Some(page.clone());
        page
    }

    /// Coalesced, serial cache trim; runs in an idle callback and never during an open/close animation.
    /// Pins come from the state at prune time, so a late download can never delete the cover now on screen.
    fn schedule_art_prune(self: &Rc<Self>) {
        if self.art_prune_pending.replace(true) { return; }
        let u = self.clone();
        glib::idle_add_local_once(move || {
            if u.animating.get() {
                u.art_prune_pending.set(false);
                let u2 = u.clone();
                glib::timeout_add_local_once(Duration::from_millis(500), move || u2.schedule_art_prune());
                return;
            }
            u.art_prune_pending.set(false);
            let dir = std::path::PathBuf::from(runtime_dir());
            let mut pins = Vec::new();
            let url = u.art_url.borrow().clone();
            if !url.is_empty() && !url.starts_with("file://") { pins.push(art_cache::path_for(&dir, &url)); }
            if let Some(p) = u.art_accepted.borrow().clone() { pins.push(p); }
            art_cache::prune(&dir, art_cache::KEEP, &pins);
        });
    }

    fn suppressed(&self) -> bool {
        self.hidden.get() || self.dest.borrow().is_none()
    }

    fn refresh_visibility(self: &Rc<Self>) {
        let sup = self.suppressed();
        self.sync_visualizer();
        self.osd.set_suppressed(sup);
        self.notes.set_hidden(sup);
        if sup {
            self.win.set_visible(false);
        } else if !self.win.is_visible() {
            self.win.set_visible(true);
            self.apply(self.prog.get());
        }
    }

    /// Move every notch surface to `info` (or hide them when no output exists). The GDK monitor can show up after
    /// the compositor reports the output; the move then waits for `watch_gdk_monitors` to call `reconcile` again.
    fn place(self: &Rc<Self>, info: Option<desktop::MonitorInfo>) {
        let old_name = self.dest.borrow().clone();
        if info == *self.dest_info.borrow() && info.is_some() == old_name.is_some() { return; }
        let Some(m) = info else {
            *self.dest.borrow_mut() = None;
            *self.dest_info.borrow_mut() = None;
            return;
        };
        if old_name.as_deref() != Some(m.name.as_str()) {
            let Some(gm) = desktop::gdk_monitor_by_name(&m.name) else { return };
            self.win.set_visible(false);
            self.win.set_monitor(Some(&gm));
            self.osd.set_monitor(&gm);
            self.notes.set_monitor(&gm);
        }
        *self.dest.borrow_mut() = Some(m.name.clone());
        *self.dest_info.borrow_mut() = Some(m.clone());
        self.relayout();
    }

    /// Re-evaluate where the notch, popups, OSD, notifications and the side pills belong.
    fn reconcile(self: &Rc<Self>) {
        let Some(snap) = self.desk.borrow().clone() else { return };
        let cur = self.dest.borrow().clone();
        if cur.as_deref().is_some_and(|c| snap.monitor(c).is_none()) { self.force_idle(); }
        let (prefs_clock, prefs_font) = {
            let p = self.settings.borrow();
            (p.behavior.clock_format, p.appearance.font_family.clone())
        };
        let (prefs_mode_hide, sel) = {
            let prefs = self.settings.borrow();
            let ext_pin = self.ext.borrow().as_ref().and_then(|e| e.pin.clone());
            let pin = if ext_pin.is_some() { ext_pin.as_deref() }
                else if cur.as_deref().is_some_and(|c| snap.monitor(c).is_some()) && self.interaction_active() { cur.as_deref() } else { None };
            (prefs.behavior.hide_on_fullscreen, desktop::select_output(&snap, &prefs, pin, cur.as_deref()))
        };
        let hidden = prefs_mode_hide && sel.as_deref().is_some_and(|n| snap.fullscreen_outputs.contains(n));
        if hidden && !self.hidden.get() { self.force_idle(); }
        self.hidden.set(hidden);
        let _ = self.bar_tx.send(bar_controller::SyncState {
            outputs: snap.monitors.iter().map(|m| m.name.clone()).collect(),
            active: sel.clone(),
            hidden,
            clock_12h: prefs_clock.uses_12h(),
            font_family: prefs_font,
        });
        self.place(sel.as_deref().and_then(|n| snap.monitor(n)).cloned());
        self.refresh_visibility();
    }

    /// Rebuild the Calendar tab's agenda (events of the selected day, Meet links) and mark days that have events.
    fn refresh_agenda(self: &Rc<Self>) {
        use chrono::NaiveDate;
        if self.stack.visible_child_name().as_deref() == Some("calendar") { let _ = self.tx.try_send(Msg::Relayout); }
        {
            let (y, m, d) = (self.cal.year(), self.cal.month() as u32 + 1, self.cal.day() as u32);
            if let Some(sel) = chrono::NaiveDate::from_ymd_opt(y, m, d) { self.agenda_date.set_text(&sel.format("%A, %-d %B %Y").to_string()); }
        }
        while let Some(c) = self.agenda.first_child() { self.agenda.remove(&c); }
        self.cal.clear_marks();
        let hint = |t: &str| {
            let l = gtk::Label::new(Some(t));
            l.add_css_class("sub");
            l.set_xalign(0.0);
            l.set_hexpand(true);
            l.set_halign(gtk::Align::Fill);
            l.set_width_chars(1);
            l.set_wrap(true);
            l.set_max_width_chars(1);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_ellipsize(gtk::pango::EllipsizeMode::None);
            l
        };
        let state = self.gstate.borrow().clone();
        if state == "noconfig" {
            self.agenda.append(&hint(&format!("Google Calendar is not connected. Run ~/.config/waybar/scripts/google-setup.py (or press Connect after saving an OAuth client to {})", gcal::config_path())));
            return;
        }
        if state == "needauth" {
            let b = gtk::Button::with_label("Connect Google Calendar");
            b.add_css_class("small-btn");
            b.set_halign(gtk::Align::Fill);
            if let Some(l) = b.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                l.set_wrap(true);
                l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
                l.set_max_width_chars(1);
            }
            let tx = self.tx.clone();
            b.connect_clicked(move |b| {
                b.set_sensitive(false);
                b.set_label("Waiting for browser…");
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let open = |u: &str| { if let Ok(c) = Command::new("xdg-open").arg(u).stdout(Stdio::null()).stderr(Stdio::null()).spawn() { reap_detached(c); } };
                    let u = match gcal::authorize(&open) { Ok(()) => gcal::fetch(), Err(e) => gcal::Update::Error(e) };
                    let _ = tx.try_send(Msg::Gcal(u));
                });
            });
            self.agenda.append(&b);
            return;
        }
        let events = self.events.borrow();
        if events.is_empty() && !state.is_empty() {
            self.agenda.append(&hint(&state));
            return;
        }
        let (y, m) = (self.cal.year(), self.cal.month() as u32 + 1);
        for d in 1..=31u32 {
            if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                if events.iter().any(|e| e.on_day(date)) { self.cal.mark_day(d); }
            }
        }
        let Some(sel) = NaiveDate::from_ymd_opt(y, m, self.cal.day() as u32) else { return };
        let mut any = false;
        for e in events.iter().filter(|e| e.on_day(sel)) {
            any = true;
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("ev-row");
            let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
            bar.add_css_class("ev-bar");
            if e.link.is_some() { bar.add_css_class("ev-bar-meet"); }
            let txt = gtk::Box::new(gtk::Orientation::Vertical, 1);
            txt.set_hexpand(true);
            txt.set_valign(gtk::Align::Center);
            let title = gtk::Label::new(Some(&e.title));
            title.add_css_class("ev-title");
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.set_max_width_chars(1);
            title.set_hexpand(true);
            title.set_tooltip_text(Some(&e.title));
            let when = gtk::Label::new(Some(&if e.all_day { "All day".to_string() } else { e.start.format("%H:%M").to_string() }));
            when.add_css_class("ev-time");
            when.set_xalign(0.0);
            txt.append(&title);
            txt.append(&when);
            row.append(&bar);
            row.append(&txt);
            if let Some(link) = &e.link {
                let j = gtk::Button::with_label("Join");
                j.add_css_class("join-btn");
                j.set_valign(gtk::Align::Center);
                let link = link.clone();
                j.connect_clicked(move |_| spawn("xdg-open", &[&link]));
                row.append(&j);
            }
            self.agenda.append(&row);
        }
        if !any {
            let l = gtk::Label::new(Some("Nothing planned"));
            l.add_css_class("ev-empty");
            l.set_xalign(0.0);
            l.set_wrap(true);
            l.set_max_width_chars(1);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            self.agenda.append(&l);
        }
    }

    // ---------------------------------------------------------------- hosted WhatsApp content

    fn now_ms(&self) -> u64 { self.started.elapsed().as_millis() as u64 }

    fn ext_send(&self, conn: u64, frame: serde_json::Value) {
        self.content.send(conn, frame);
    }

    /// Run `f` on the live session when `conn` is still its connection; events of any other connection are stale.
    fn ext_with<R>(&self, conn: u64, f: impl FnOnce(&mut Ext) -> R) -> Option<R> {
        self.ext.borrow_mut().as_mut().filter(|e| e.s.conn == conn).map(f)
    }

    fn ext_event(self: &Rc<Self>, ev: notch_content::Event) {
        use notch_content::{Event, Phase};
        match ev {
            Event::Open { conn, output } => self.ext_open(conn, output),
            Event::Mapped { conn, serial } => {
                if self.ext_with(conn, |e| e.s.on_mapped(serial)) == Some(true) { self.wa_status.set_visible(false); }
            }
            Event::Interaction { conn, hovered, busy } => {
                let Some(was_held) = self.ext_with(conn, |e| { let held = e.s.holds_open(); e.s.on_interaction(hovered, busy); held }) else { return };
                if hovered || busy {
                    self.cancel_collapse();
                } else if was_held && !self.notch_hover.get() {
                    // only leaving the content arms the collapse; the first report must not cut the travel grace short
                    let ms = self.settings.borrow().behavior.collapse_delay_ms;
                    self.schedule_collapse(ms as u64);
                }
            }
            Event::Close { conn, home } => {
                if self.ext_with(conn, |_| ()).is_some() { self.ext_close(if home { CloseTo::Home } else { CloseTo::Collapse }); }
            }
            Event::Hidden { conn } => {
                if self.ext_with(conn, |e| e.s.phase == Phase::Closing) == Some(true) { self.ext_finish(); }
            }
            Event::Gone { conn } => match self.ext_with(conn, |e| e.s.phase) {
                Some(Phase::Closing) => self.ext_finish(),
                Some(Phase::Open) => {
                    self.ext_drop("connection lost", Some("home"));
                    self.set_expanded(false);
                }
                None => {}
            },
        }
    }

    fn ext_open(self: &Rc<Self>, conn: u64, output: String) {
        let bridge = &self.content;
        // a replacement session supersedes the old one without bouncing through Home
        self.ext_drop("replaced", None);
        let reject = |why: &str| {
            bridge.send(conn, serde_json::json!({"op": "error", "message": why}));
            bridge.disconnect(conn);
        };
        let Some(snap) = self.desk.borrow().clone() else { return reject("the compositor state is not known yet") };
        if snap.monitor(&output).is_none() { return reject(&format!("output {output} is not connected")); }
        if self.capture_pending.get() || self.capture_active.get() { return reject("a screenshot or recording is in progress"); }
        let (follow, hide_fs) = {
            let p = self.settings.borrow();
            (p.monitor.mode == config::MonitorMode::Follow, p.behavior.hide_on_fullscreen)
        };
        if hide_fs && snap.fullscreen_outputs.contains(&output) { return reject("the notch is hidden by a fullscreen window"); }
        if self.hidden.get() { return reject("the notch is hidden"); }
        *self.ext.borrow_mut() = Some(Ext {
            s: notch_content::Session::new(conn, self.now_ms()),
            pin: follow.then(|| output.clone()),
            after: CloseTo::Collapse,
            last: None,
        });
        self.wa_status.set_visible(true);
        self.cancel_collapse();
        self.detail_origin.set("home");
        self.stack.set_visible_child_name("whatsapp");
        self.reconcile();
        if self.suppressed() {
            self.ext_fail(conn, "the notch has no output to show on");
            return;
        }
        self.set_expanded(true);
        self.schedule_collapse(3000); // grace for the pointer to travel from the bar; entering either window cancels it
        self.ext_start_timer();
        self.ext_schedule_publish();
    }

    /// Tell the content why it is going away, close its connection, and leave the WhatsApp page.
    fn ext_drop(self: &Rc<Self>, reason: &str, leave: Option<&'static str>) {
        let Some(e) = self.ext.borrow_mut().take() else { return };
        let conn = e.s.conn;
        if std::env::var_os("NOTCH_DEBUG").is_some() { eprintln!("content session {conn} dropped: {reason}"); }
        self.ext_send(conn, serde_json::json!({"op": "hide", "session": conn, "reason": reason}));
        self.content.disconnect(conn);
        if let Some(id) = self.ext_timer.borrow_mut().take() { id.remove(); }
        if let Some(page) = leave {
            if self.stack.visible_child_name().as_deref() == Some("whatsapp") { self.show_tab(page); }
        }
        if e.pin.is_some() {
            let u = self.clone();
            glib::idle_add_local_once(move || u.reconcile()); // the interaction pin is gone
        }
    }

    /// Start the bounded close (hide, then wait for `hidden`). False when no content session is live.
    fn ext_close(self: &Rc<Self>, to: CloseTo) -> bool {
        let now = self.now_ms();
        let conn = {
            let mut g = self.ext.borrow_mut();
            let Some(e) = g.as_mut() else { return false };
            e.after = to;
            if !e.s.begin_close(now) { return true; }
            if std::env::var_os("NOTCH_DEBUG").is_some() { eprintln!("content session {} closing", e.s.conn); }
            e.s.conn
        };
        self.ext_send(conn, serde_json::json!({"op": "hide", "session": conn, "reason": "close"}));
        true
    }

    /// The content hid (or the bound ran out): disconnect and carry on to where the close was heading.
    fn ext_finish(self: &Rc<Self>) {
        let Some(after) = self.ext.borrow().as_ref().map(|e| e.after) else { return };
        match after {
            CloseTo::Tab(t) => { self.ext_drop("closed", Some(t)); }
            _ => { self.ext_drop("closed", Some("home")); }
        }
        if after == CloseTo::Collapse { self.set_expanded(false); }
    }

    /// A failed open or lost content: tell QML (it notifies the user), return Home and collapse.
    fn ext_fail(self: &Rc<Self>, conn: u64, why: &str) {
        if self.ext_with(conn, |_| ()).is_none() { return; }
        self.ext_send(conn, serde_json::json!({"op": "error", "message": why}));
        self.ext_drop("error", Some("home"));
        self.set_expanded(false);
    }

    fn ext_start_timer(self: &Rc<Self>) {
        if let Some(id) = self.ext_timer.borrow_mut().take() { id.remove(); }
        let u = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(50), move || {
            use notch_content::Tick;
            let t = u.ext.borrow().as_ref().map(|e| (e.s.conn, e.s.tick(u.now_ms())));
            match t {
                Some((_, Tick::Idle)) => glib::ControlFlow::Continue,
                None => { u.ext_timer.borrow_mut().take(); glib::ControlFlow::Break }
                Some((conn, tick)) => {
                    u.ext_timer.borrow_mut().take(); // this source ends by returning Break
                    if tick == Tick::MapTimeout { u.ext_fail(conn, "WhatsApp did not appear in the notch in time"); } else { u.ext_finish(); }
                    glib::ControlFlow::Break
                }
            }
        });
        *self.ext_timer.borrow_mut() = Some(id);
    }

    /// Slot rectangle in output-local logical pixels, from the real allocation and the real layer host position.
    fn ext_slot_rect(&self) -> Option<[i32; 4]> {
        let b = self.wa_page.compute_bounds(&self.win)?;
        let (w, h) = (b.width().round() as i32, b.height().round() as i32);
        let win_w = self.win.width();
        if w <= 0 || h <= 0 || win_w <= 0 || !self.win.is_visible() { return None; }
        let out_w = self.dest_info.borrow().as_ref()?.logical_width.round() as i32;
        let x = (out_w - win_w) / 2 + b.x().round() as i32;
        let y = self.geom.get().top_margin + b.y().round() as i32;
        Some([x, y, w, h])
    }

    /// Hide the content while the slot moves; the next settled layout brings it back.
    fn ext_invalidate(&self) {
        let sent = {
            let mut g = self.ext.borrow_mut();
            let Some(e) = g.as_mut() else { return };
            if e.last.take().is_none() { return; }
            e.s.suspended();
            e.s.conn
        };
        self.ext_send(sent, serde_json::json!({"op": "suspend", "session": sent}));
    }

    /// Wait for the slot to settle (animation over, the same allocation on consecutive frames), then publish it once.
    fn ext_schedule_publish(self: &Rc<Self>) {
        if self.ext.borrow().is_none() || self.ext_pub_busy.replace(true) { return; }
        let u = self.clone();
        let (prev, stable, frames) = (Cell::new(None::<[i32; 4]>), Cell::new(0u32), Cell::new(0u32));
        self.win.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            if u.ext.borrow().is_none() || frames.get() > 600 {
                u.ext_pub_busy.set(false);
                return glib::ControlFlow::Break;
            }
            let settled = !u.animating.get() && u.expanded.load(Ordering::Relaxed) && u.prog.get() > 0.99;
            let rect = if settled { u.ext_slot_rect() } else { None };
            let Some(r) = rect else { stable.set(0); prev.set(None); return glib::ControlFlow::Continue };
            if prev.get() == Some(r) { stable.set(stable.get() + 1) } else { stable.set(0); prev.set(Some(r)); }
            if stable.get() < 2 { return glib::ControlFlow::Continue; }
            u.ext_pub_busy.set(false);
            u.ext_publish(r);
            glib::ControlFlow::Break
        });
    }

    fn ext_publish(self: &Rc<Self>, r: [i32; 4]) {
        let Some((out, ow, oh)) = self.dest_info.borrow().as_ref().map(|m| (m.name.clone(), m.logical_width.round() as i32, m.logical_height.round() as i32)) else { return };
        let Some(conn) = self.ext.borrow().as_ref().map(|e| e.s.conn) else { return };
        if r[0] < 0 || r[1] < 0 || r[0] + r[2] > ow || r[1] + r[3] > oh {
            return self.ext_fail(conn, "the notch content falls outside its output");
        }
        let serial = {
            let mut g = self.ext.borrow_mut();
            let Some(e) = g.as_mut() else { return };
            if e.last.as_ref() == Some(&(out.clone(), r)) { return; }
            e.last = Some((out.clone(), r));
            e.s.layout_sent()
        };
        let t = theme::current();
        let (size, radius, anim) = {
            let s = self.settings.borrow();
            (s.appearance.font_size, s.appearance.corner_radius, s.behavior.animations)
        };
        self.ext_send(conn, serde_json::json!({
            "op": "layout", "version": notch_content::VERSION, "session": conn, "serial": serial, "output": out,
            "rect": {"x": r[0], "y": r[1], "width": r[2], "height": r[3]},
            "theme": {
                "background": theme::hex(t.bg), "foreground": theme::hex(t.fg), "foreground_hi": theme::hex(t.fg_hi()),
                "accent": theme::hex(t.accent), "alert": theme::hex(t.red), "font_family": t.font,
                "font_size": size, "radius": radius, "animations": anim,
            },
        }));
    }

    fn cancel_collapse(&self) {
        if let Some(id) = self.collapse_id.borrow_mut().take() {
            id.remove();
        }
    }

    /// Start the no-player countdown once; it only ever turns a *missing* player into the skeleton.
    fn arm_skeleton(self: &Rc<Self>) {
        if self.media_presence.get() != MediaPresence::Missing || self.skel_id.borrow().is_some() { return; }
        let u = self.clone();
        *self.skel_id.borrow_mut() = Some(glib::timeout_add_seconds_local_once(SKELETON_SECS, move || {
            u.skel_id.borrow_mut().take();
            let next = next_media_presence(u.media_presence.get(), MediaPresenceEvent::IdleTimeout);
            u.media_presence.set(next);
            if next == MediaPresence::Skeleton { u.set_skeleton(true); }
        }));
    }

    /// Collapsed notch shows the clock unless a player is available.
    fn update_idle(&self) {
        let idle = self.media_presence.get() != MediaPresence::Present;
        self.coll_stack.set_visible_child_name(if idle { "idle" } else { "media" });
    }

    /// Show the embedded Audio/Displays controls only while the notch is expanded on the utility page.
    fn sync_utility(self: &Rc<Self>) {
        let want = self.expanded.load(Ordering::Relaxed) && !self.suppressed()
            && self.stack.visible_child_name().as_deref() == Some("utility");
        match (want, self.utility_kind.get()) {
            (true, Some(kind)) => {
                self.popups.embed(kind, &self.utility_host);
                self.win.set_keyboard_mode(KeyboardMode::OnDemand);
                self.utility_kbd.set(true);
            }
            _ => {
                self.popups.close_embedded();
                if self.utility_kbd.replace(false) { self.win.set_keyboard_mode(KeyboardMode::None); }
            }
        }
    }

    /// Skeleton look for the Media tab and Home mini player; controls are disabled while it shows.
    fn set_skeleton(&self, on: bool) {
        for w in [self.media_tab.upcast_ref::<gtk::Widget>(), self.strip.upcast_ref()] {
            if on { w.add_css_class("skel") } else { w.remove_css_class("skel") }
        }
        self.media_tab.set_sensitive(!on);
        self.h_play.set_sensitive(!on);
        if on {
            self.m_title.set_text(&"\u{a0}".repeat(18));
            self.m_artist.set_text(&"\u{a0}".repeat(12));
            self.h_title.set_text(&"\u{a0}".repeat(18));
            self.h_artist.set_text(&"\u{a0}".repeat(12));
            *self.lyr_key.borrow_mut() = String::new();
            self.lyr_clear();
            *self.lyr.borrow_mut() = lyrics::Lyrics::None;
            self.set_player("");
            self.h_art.set_paintable(None::<&gtk::gdk::Paintable>);
            self.h_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
            self.h_title.set_tooltip_text(None);
            self.h_artist.set_tooltip_text(None);
            self.m_art.set_paintable(None::<&gtk::gdk::Paintable>);
            self.m_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
            *self.art_url.borrow_mut() = String::new();
        }
        self.update_idle();
    }

    fn set_playing(self: &Rc<Self>, playing: bool) {
        if self.playing.replace(playing) == playing {
            return;
        }
        self.sync_visualizer();
    }

    /// Capture/animate the EQ only while it can be seen: playing, notch collapsed, not suppressed.
    fn sync_visualizer(self: &Rc<Self>) {
        let want = self.playing.get() && !self.expanded.load(Ordering::Relaxed) && !self.suppressed();
        if self.viz_on.replace(want) == want {
            return;
        }
        self.viz_gen.set(self.viz_gen.get() + 1); // frames from the previous reader are now stale
        if want {
            self.start_cava(self.viz_gen.get());
        } else {
            self.stop_visualizer();
        }
    }

    fn stop_visualizer(&self) {
        if let Some(mut c) = self.cava.borrow_mut().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        *self.bars.borrow_mut() = [0.12; BARS];
        self.eq.queue_draw();
    }

    /// Real spectrum: capture the default sink's monitor with `parec`, FFT it, send band levels.
    fn start_cava(self: &Rc<Self>, vgen: u64) {
        let spawned = Command::new("parec")
            .args(["-d", "@DEFAULT_SINK@.monitor", "--format=s16le", "--rate=44100", "--channels=1", "--latency-msec=30"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        match spawned {
            Ok(mut child) => {
                let out = child.stdout.take();
                *self.cava.borrow_mut() = Some(child);
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    if let Some(mut out) = out {
                        let mut sp = Spectrum::new();
                        while out.read_exact(&mut sp.raw).is_ok() {
                            let bars = sp.frame();
                            if tx.try_send(Msg::Bars(vgen, bars)).is_err() { return; }
                        }
                    }
                    let _ = tx.try_send(Msg::VisualizerEnded(vgen));
                });
            }
            Err(e) => {
                eprintln!("audio visualizer unavailable: {e}");
                self.viz_on.set(false);
                self.stop_visualizer();
            }
        }
    }

    fn handle(self: &Rc<Self>, m: Msg) {
        // content that can change the active page's natural size; the pump coalesces these into one sizing pass
        if self.expanded.load(Ordering::Relaxed)
            && matches!(m, Msg::Media { .. } | Msg::NoMedia | Msg::Usage(_) | Msg::Clip(_) | Msg::UtilResult(_) | Msg::ClipCleared { .. })
        {
            let _ = self.tx.try_send(Msg::Relayout);
        }
        match m {
            Msg::NotificationRelayFailed(why) => {
                eprintln!("notification relay failed: {why}");
                if let Some(a) = gtk::gio::Application::default() { a.quit(); }
            }
            Msg::NotchContentFailed(why) => {
                eprintln!("notch content transport failed: {why}");
                if let Some(a) = gtk::gio::Application::default() { a.quit(); }
            }
            Msg::NotchContent(ev) => self.ext_event(ev),
            Msg::TabSelected(name) => { if !self.ext_close(CloseTo::Tab(name)) { self.show_tab(name); } }
            Msg::Gcal(u) => {
                match u {
                    gcal::Update::Events(ev) => { *self.events.borrow_mut() = ev; self.gstate.borrow_mut().clear(); }
                    gcal::Update::NoConfig => *self.gstate.borrow_mut() = "noconfig".into(),
                    gcal::Update::NeedAuth => *self.gstate.borrow_mut() = "needauth".into(),
                    gcal::Update::Error(e) => *self.gstate.borrow_mut() = format!("Google Calendar: {e}"),
                }
                self.refresh_agenda();
            }
            Msg::CaptureActive(on) => {
                self.capture_active.set(on);
                if on { self.ext_drop("capture", Some("home")); }
                self.win.set_layer(if on { Layer::Top } else { Layer::Overlay });
            }
            Msg::CaptureFinished(action, res) => self.finish_capture(action, res),
            Msg::Desktop(snap) => {
                if let Some(p) = self.settings_page.borrow().as_ref() { p.set_outputs(snap.monitors.iter().map(|m| m.name.clone()).collect()); }
                *self.desk.borrow_mut() = Some(snap);
                self.reconcile();
            }
            Msg::Relayout => self.set_expanded_size(),
            Msg::UtilityPageChanged => {
                let name = self.stack.visible_child_name();
                let name = name.as_deref();
                // any other page replaces the WhatsApp content at once
                if name != Some("whatsapp") { self.ext_drop("page changed", Some("home")); }
                let is_tab = |n: &str| self.tab_btns.borrow().iter().any(|(_, t)| *t == n);
                // Wi-Fi, Bluetooth and the utility pages keep their origin tab (Home or Controls) selected
                let sel = if matches!(name, Some("utility") | Some("wifi") | Some("bluetooth") | Some("whatsapp")) { Some(self.detail_origin.get()) } else { name.filter(|n| is_tab(n)) };
                if name != Some("controls") { self.clear_control_status(); }
                if name == Some("settings") { self.ensure_settings_page().on_show(); }
                if let Some(sel) = sel {
                    for (b, n) in self.tab_btns.borrow().iter() {
                        if *n == sel { b.add_css_class("active") } else { b.remove_css_class("active") }
                    }
                }
                let media = name == Some("media");
                self.media_background.set_visible(media);
                if media { self.notch.add_css_class("music-view") } else { self.notch.remove_css_class("music-view") }
                let was_home = self.notch.has_css_class("home-view");
                let home = name == Some("home");
                if home { self.notch.add_css_class("home-view") } else { self.notch.remove_css_class("home-view") }
                self.sync_utility();
                self.set_expanded_size();
                if was_home != home {
                    let u = self.clone();
                    glib::timeout_add_local_once(Duration::from_millis(120), move || u.relayout());
                }
            }
            Msg::NoMedia => {
                self.last_playing.set(None);
                self.media_presence.set(next_media_presence(self.media_presence.get(), MediaPresenceEvent::NoMedia));
                self.set_playing(false);
                self.c_title.set_text("No media");
                self.m_title.set_text("Nothing playing");
                self.m_artist.set_text("");
                self.h_title.set_text("Nothing playing");
                self.h_title.set_tooltip_text(None);
                self.h_artist.set_text("");
                self.h_artist.set_tooltip_text(None);
                self.h_art.set_paintable(None::<&gtk::gdk::Paintable>);
                self.h_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
                self.m_prog.set_value(0.0);
                self.m_t1.set_text("0:00");
                self.m_t2.set_text("0:00");
                self.c_art.set_visible(false);
                self.c_note.set_visible(true);
                self.m_art.set_paintable(None::<&gtk::gdk::Paintable>);
                self.m_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
                self.art_url.borrow_mut().clear();
                *self.lyr_key.borrow_mut() = String::new();
                self.lyr_clear();
                *self.lyr.borrow_mut() = lyrics::Lyrics::None;
                self.set_player("");
                self.arm_skeleton();
                self.update_idle();
            }
            Msg::Media { status, artist, title, art_url, len, player } => {
                self.media_presence.set(next_media_presence(self.media_presence.get(), MediaPresenceEvent::Media));
                if let Some(id) = self.skel_id.borrow_mut().take() { id.remove(); }
                self.set_skeleton(false);
                self.set_player(&player);
                self.len.set(len);
                let key = format!("{artist}\u{1f}{title}");
                if *self.lyr_key.borrow() != key {
                    *self.lyr_key.borrow_mut() = key.clone();
                    self.lyr_status("Loading lyrics…");
                    let (tx, a, t) = (self.tx.clone(), artist.clone(), title.clone());
                    std::thread::spawn(move || { let _ = tx.try_send(Msg::Lyrics(key, lyrics::fetch(&a, &t, len))); });
                }
                let short: String = if title.chars().count() > 22 {
                    title.chars().take(21).collect::<String>() + "…"
                } else { title.clone() };
                self.c_title.set_text(&short);
                self.m_title.set_text(&title);
                self.m_artist.set_text(&artist);
                self.h_title.set_text(&title);
                self.h_title.set_tooltip_text(Some(&title));
                self.h_artist.set_text(&artist);
                self.h_artist.set_tooltip_text(Some(&artist));
                self.m_t2.set_text(&fmt_time(len));
                let playing = status == "Playing";
                self.m_play.set_label(if playing { "\u{f04c}" } else { "\u{f04b}" });
                self.h_play.set_label(if playing { "\u{f04c}" } else { "\u{f04b}" });
                self.set_playing(playing);
                if self.last_playing.replace(Some(playing)).map_or(false, |o| o != playing) {
                    self.osd.show_media(playing, &title);
                }
                if *self.art_url.borrow() != art_url {
                    *self.art_url.borrow_mut() = art_url.clone();
                    self.m_art.set_paintable(None::<&gtk::gdk::Paintable>);
                    self.m_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
                    self.h_art.set_paintable(None::<&gtk::gdk::Paintable>);
                    self.h_backdrop.set_paintable(None::<&gtk::gdk::Paintable>);
                    self.c_art.set_visible(false);
                    self.c_note.set_visible(true);
                    self.art_worker.submit(art_url);
                }
            }
            Msg::Art { url, path, image } => {
                self.schedule_art_prune();
                if *self.art_url.borrow() != url { return; }   // stale cover from a previous track
                *self.art_accepted.borrow_mut() = Some(path.clone());
                self.art_worker.accept(&path);
                let tex: gtk::gdk::Texture = image_loader::texture(image).upcast();
                self.c_art.set_paintable(Some(&tex));
                self.c_art.set_visible(true);
                self.c_note.set_visible(false);
                let (cover, back) = album_paintables(&tex);
                self.h_art.set_paintable(cover.as_ref());
                self.m_art.set_paintable(cover.as_ref());
                self.h_backdrop.set_paintable(back.as_ref());
                self.m_backdrop.set_paintable(back.as_ref());
            }
            Msg::Pos(p) => {
                if p.is_nan() {
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        if let Ok(v) = sh_out("playerctl", &["-p", PLAYERS, "position"]).parse::<f64>() {
                            let _ = tx.try_send(Msg::Pos(v));
                        }
                    });
                } else {
                    self.show_pos(p);
                }
            }
            Msg::Lyrics(key, l) => {
                if *self.lyr_key.borrow() != key { return; }   // lyrics for a track that already changed
                match l {
                    lyrics::Lyrics::None => self.lyr_status("No lyrics found for this track"),
                    l => self.lyr_set(l),
                }
            }
            Msg::PlayerRaised(generation, res) => {
                if generation != self.player_generation.get() { return; }   // player changed or vanished while raising
                self.player_raise_busy.set(false);
                let inst = self.player_instance.borrow().clone();
                self.m_open_app.set_sensitive(player_service_name(&inst).is_some());
                match res {
                    Ok(()) => self.m_open_app.set_tooltip_text(Some(&format!("Open {inst}"))),
                    Err(e) => {
                        eprintln!("raise player {inst}: {e}");
                        self.m_open_app.set_tooltip_text(Some(&e));
                    }
                }
            }
            Msg::Bars(vgen, v) => {
                if vgen != self.viz_gen.get() || !self.viz_on.get() { return; }
                *self.bars.borrow_mut() = v;
                if self.prog.get() < 0.3 { self.eq.queue_draw(); }
            }
            Msg::VisualizerEnded(vgen) => {
                if vgen != self.viz_gen.get() { return; }
                // capture ended by itself: reap it and wait for the next activity transition instead of retrying
                self.viz_on.set(false);
                self.stop_visualizer();
            }
            Msg::Weather(w) => {
                self.c_weather.set_text(&w.compact);
                self.i_weather.set_text(&w.compact);
                self.h_weather_temp.set_text(&w.compact);
                self.h_weather_location.set_text(&w.location);
                self.h_weather_wind.set_text(&w.wind);
                self.h_weather_wind.set_visible(!w.wind.is_empty());
                for l in [&self.h_weather_temp, &self.h_weather_location, &self.h_weather_wind] { l.set_tooltip_text(Some(&l.text())); }
                let _ = self.tx.try_send(Msg::Relayout);
            }
            Msg::Stats(b, v, w, bt, bt_on) => {
                for (l, t) in [(&self.s_batt, &b), (&self.s_vol, &v), (&self.s_wifi, &w), (&self.s_bt, &bt)] {
                    l.set_text(t);
                    l.set_tooltip_text(Some(t));
                }
                self.s_wifi.add_css_class("stat-on");
                if bt_on { self.s_bt.add_css_class("stat-on"); } else { self.s_bt.remove_css_class("stat-on"); }
            }
            Msg::OsdData(kind, level, muted) => self.osd.show(&kind, level, muted),
            Msg::Toggle(glyph, on) => self.osd.show_toggle(&glyph, on),
            Msg::Net(kind, state, name) => {
                net_tile(if kind == "wifi" { &self.tile_wifi } else { &self.tile_bt }, state);
                self.osd.show_net(kind, state, &name)
            }
            Msg::Usage(u) => self.ai.update(&u),
            Msg::PopupReq(k) if k == "reload-theme" => {
                theme::reload();
                reload_main_css();
                let u = self.clone();
                glib::timeout_add_local_once(Duration::from_millis(120), move || u.relayout());
            }
            Msg::PopupReq(k) if k.starts_with("osd-") => {
                // read the level off the UI thread, then show the notch-style overlay
                let (tx, kind) = (self.tx.clone(), k[4..].to_string());
                std::thread::spawn(move || {
                    let (level, muted) = if kind == "brightness" {
                        (sh_out("omarchy-brightness-display", &["--no-osd"]).trim_end_matches('%').parse::<f64>().unwrap_or(0.0), false)
                    } else {
                        (sh_out("pamixer", &["--get-volume"]).parse::<f64>().unwrap_or(0.0), sh_out("pamixer", &["--get-mute"]) == "true")
                    };
                    let _ = tx.try_send(Msg::OsdData(kind, level, muted));
                });
            }
            Msg::PopupReq(_) if self.suppressed() => {} // fullscreen on the destination (or no output): nothing to show on
            Msg::PopupReq(k) => {
                if matches!(k.as_str(), "calendar" | "home" | "ai" | "media" | "clipboard" | "controls" | "settings") {
                    // clicking the clock again while its calendar is showing closes it right away
                    if k == "calendar" && self.expanded.load(Ordering::Relaxed)
                        && self.stack.visible_child_name().as_deref() == Some("calendar") && self.collapse_id.borrow().is_some() {
                        self.cancel_collapse();
                        self.set_expanded(false);
                        return;
                    }
                    // the clock opens the notch on its calendar tab; it folds back unless hovered
                    self.cancel_collapse();
                    // a hidden optional tab redirects to Home; Settings is always reachable
                    let page = if self.tab_allowed(&k) { k.as_str() } else { "home" };
                    self.show_tab(page);
                    self.set_expanded(true);
                    self.schedule_collapse(3000);
                } else if k == "close" {
                    self.cancel_collapse();
                    self.win.set_keyboard_mode(KeyboardMode::None);
                    self.set_expanded(false);
                } else if let Some(kind) = ["wifi", "bluetooth", "volume", "battery", "display"].into_iter().find(|n| *n == k) {
                    // the same detail page again closes; another one switches inside the open notch
                    if self.open_detail_kind() == Some(kind) {
                        self.cancel_collapse();
                        self.set_expanded(false);
                        return;
                    }
                    match kind {
                        "wifi" | "bluetooth" => self.open_device(kind, "home"),
                        _ => self.open_utility(kind, "home"),
                    }
                    self.schedule_collapse(3000); // grace for the pointer to travel from the bar; entering the notch cancels it
                }
            }
            Msg::PopupData(k, v) => self.popups.on_data(k, v),
            Msg::PopupRefresh(k) => self.popups.refresh(&k),
            Msg::BtActionFinished => (self.populate_bt)(),
            Msg::Settings(ev) => match ev {
                settings::SettingsEvent::Disk(s, err) => {
                    let s = *s;
                    if err.is_none() && *self.settings.borrow() != s { self.apply_settings(s.clone()); }
                    let shown = if err.is_some() { self.settings.borrow().clone() } else { s };
                    if let Some(p) = self.settings_page.borrow().clone() { p.on_external(shown, err); }
                }
                settings::SettingsEvent::Doc(d) => { if let Some(p) = self.settings_page.borrow().clone() { p.on_doc(*d); } }
                settings::SettingsEvent::Saved(sv) => {
                    if let settings::Saved::Ok(n) | settings::Saved::AppsFailed(n, _) = &sv { self.apply_settings((**n).clone()); }
                    if let Some(p) = self.settings_page.borrow().clone() { p.on_saved(&sv); }
                }
            },
            Msg::UtilResult(s) => set_control_status(&self.ctl_status, &self.ctl_status_timer, &s, None),
            Msg::ClipCleared { message, success } => {
                self.clearing.set(false);
                // only a visible Controls page shows it; a success confirmation expires, it is never replayed on a later visit
                if self.expanded.load(Ordering::Relaxed) && self.stack.visible_child_name().as_deref() == Some("controls") {
                    set_control_status(&self.ctl_status, &self.ctl_status_timer, &message, success.then_some(Duration::from_secs(3)));
                }
            }
            Msg::Clip(s) => {
                if self.clearing.get() { return; } // a clear is in progress: queued pre-clear copies must not refill the history
                let s = s.trim_end_matches('\n').to_string();
                if s.trim().is_empty() { return; }
                {
                    let mut h = self.hist.borrow_mut();
                    h.retain(|x| *x != s);
                    h.insert(0, s);
                    h.truncate(MAX_HIST);
                    if let Err(e) = save_history_at(&hist_path(), &h) { eprintln!("clipboard history: {e}"); }
                }
                if self.expanded.load(Ordering::Relaxed) { refresh_clip(self); }
            }
        }
    }

    fn show_pos(&self, p: f64) {
        let len = self.len.get();
        self.pos.set(p);
        self.lyr_update(p);
        self.m_t1.set_text(&fmt_time(p));
        if !SEEKING.load(Ordering::Relaxed) {
            self.m_prog.set_value(if len > 0.0 { (p / len).clamp(0.0, 1.0) } else { 0.0 });
        }
    }

    fn lyr_show(&self, cur: &str, next: &str) {
        let mut changed = false;
        for (l, t) in [(&self.lyr_current, cur), (&self.lyr_next, next)] {
            let shown = if t.is_empty() { " " } else { t };
            if l.text().as_str() != shown {
                l.set_text(shown);
                l.set_tooltip_text(if t.trim().is_empty() { None } else { Some(t) });
                changed = true;
            }
        }
        if changed && self.stack.visible_child_name().as_deref() == Some("media") {
            let _ = self.tx.try_send(Msg::Relayout);
        }
    }

    fn lyr_clear(&self) {
        self.lyr_show("", "");
    }

    fn lyr_status(&self, text: &str) {
        *self.lyr.borrow_mut() = lyrics::Lyrics::None;
        self.lyr_show(text, "");
    }

    fn lyr_set(&self, l: lyrics::Lyrics) {
        *self.lyr.borrow_mut() = l;
        self.lyr_update(self.pos.get());
    }

    /// Show the current and next line for `pos`; text changes only when the pair does.
    fn lyr_update(&self, pos: f64) {
        let l = self.lyr.borrow();
        if matches!(&*l, lyrics::Lyrics::None) { return; }   // status text owns the rows
        let (cur, next) = lyric_pair(&l, pos);
        self.lyr_show(cur, next);
    }

    fn set_player(&self, player: &str) {
        if *self.player_instance.borrow() == player { return; }
        *self.player_instance.borrow_mut() = player.to_string();
        self.player_generation.set(self.player_generation.get().wrapping_add(1));
        self.player_raise_busy.set(false);
        let ok = player_service_name(player).is_some();
        self.m_open_app.set_sensitive(ok);
        self.m_open_app.set_tooltip_text(Some(&if ok { format!("Open {player}") } else { "Open playing app".into() }));
    }
}

fn refresh_clip(ui: &Rc<Ui>) {
    let last = ui.hist.borrow().first().map(|e| {
        let one: String = e.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        if one.chars().count() > 50 { one.chars().take(49).collect::<String>() + "…" } else { one }
    });
    ui.h_clip.set_text(&match last { Some(t) => format!("\u{f0ea}  {t}"), None => "\u{f0ea}  Nothing copied yet".into() });
    while let Some(c) = ui.clip_box.first_child() {
        ui.clip_box.remove(&c);
    }
    let hist = ui.hist.borrow();
    ui.clip_title.set_text(&if hist.is_empty() { "Clipboard".to_string() } else { format!("Clipboard \u{b7} {}", hist.len()) });
    ui.clip_clear.set_sensitive(!hist.is_empty());
    if hist.is_empty() {
        let l = gtk::Label::new(Some("Nothing copied yet"));
        l.add_css_class("sub");
        l.set_margin_top(14);
        l.set_halign(gtk::Align::Center);
        ui.clip_box.append(&l);
    }
    for entry in hist.iter() {
        let one: String = entry.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let preview = if one.chars().count() > 56 { one.chars().take(55).collect::<String>() + "…" } else { one };
        let b = gtk::Button::with_label(&preview);
        b.add_css_class("clip-row");
        b.set_tooltip_text(Some(&entry.chars().take(400).collect::<String>()));
        if let Some(l) = b.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
            l.set_xalign(0.0);
            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
            l.set_max_width_chars(1);
            l.set_hexpand(true);
        }
        let text = entry.clone();
        b.connect_clicked(move |_| {
            if let Ok(mut c) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
                if let Some(mut si) = c.stdin.take() { let _ = si.write_all(text.as_bytes()); }
                std::thread::spawn(move || { let _ = c.wait(); });
            }
        });
        ui.clip_box.append(&b);
    }
}

fn draw_bars(ui: &Ui, cr: &cairo::Context, w: i32, h: i32) {
    let bars = ui.bars.borrow();
    let n = bars.len().max(1) as f64;
    let gap = 3.0;
    let bw = (w as f64 - gap * (n - 1.0)) / n;
    let (cr_, cg_, cb_) = if ui.playing.get() { theme::accent() } else { theme::fg() };
    let (r, g, b) = (cr_ as f64 / 255.0, cg_ as f64 / 255.0, cb_ as f64 / 255.0);
    cr.set_source_rgba(r, g, b, if ui.playing.get() { 1.0 } else { 0.35 });
    for (i, v) in bars.iter().enumerate() {
        let bh = v.max(EQ_MIN) * h as f64;
        let x = i as f64 * (bw + gap);
        let y = (h as f64 - bh) / 2.0;
        let r = bw / 2.0;
        cr.new_sub_path();
        cr.arc(x + r, y + r, r, std::f64::consts::PI, 0.0);
        cr.arc(x + r, y + bh - r, r, 0.0, std::f64::consts::PI);
        cr.close_path();
    }
    let _ = cr.fill();
}

fn build(app: &gtk::Application, shell_pid: i32) {
    let expanded = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::unbounded::<Msg>();
    let (settings, _) = config::load_settings();
    let geom0 = desktop::Geometry::compute(&settings.layout, None);
    let host0 = geom0.host(480.0);
    let win = gtk::ApplicationWindow::new(app);
    win.init_layer_shell();
    // Top until the first valid layer query proves no screenshot freeze exists; then Overlay is restored.
    win.set_layer(Layer::Top);
    win.set_namespace(Some("waybar-notch"));
    win.set_anchor(Edge::Top, true);
    win.set_margin(Edge::Top, geom0.top_margin);
    win.set_exclusive_zone(-1);
    win.set_keyboard_mode(KeyboardMode::None);

    // sizing/background widget + two overlay layers (collapsed row, expanded tabs)
    let notch = gtk::Box::new(gtk::Orientation::Vertical, 0);
    notch.set_widget_name("notch");
    notch.add_css_class("home-view");
    notch.set_overflow(gtk::Overflow::Hidden);
    notch.set_size_request(geom0.coll_w as i32, geom0.coll_h as i32);
    notch.set_halign(gtk::Align::Center);
    notch.set_valign(gtk::Align::Start);
    win.set_default_size(host0.0, host0.1);
    let overlay = gtk::Overlay::new();
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    overlay.set_child(Some(&spacer));
    notch.append(&overlay);

    // collapsed row
    let coll = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    coll.set_valign(gtk::Align::Start);
    coll.set_size_request(-1, geom0.coll_h as i32);
    coll.set_margin_start(12);
    coll.set_margin_end(12);
    let c_art = gtk::Image::new();
    c_art.set_pixel_size(20);
    c_art.add_css_class("cart");
    c_art.set_overflow(gtk::Overflow::Hidden);
    c_art.set_visible(false);
    c_art.set_valign(gtk::Align::Center);
    let c_note = gtk::Label::new(Some("\u{f001}"));
    c_note.add_css_class("note");
    c_note.set_valign(gtk::Align::Center);
    let c_title = gtk::Label::new(None); // kept for state; the collapsed notch shows art + bars only
    c_title.set_visible(false);
    let eq = gtk::DrawingArea::new();
    eq.set_content_width(36);
    eq.set_content_height(16);
    eq.set_valign(gtk::Align::Center);
    eq.set_halign(gtk::Align::End);
    eq.set_hexpand(false);
    let c_weather = gtk::Label::new(None);
    c_weather.add_css_class("c-weather");
    c_weather.set_halign(gtk::Align::Center);
    c_weather.set_valign(gtk::Align::Center);
    c_weather.set_hexpand(true);
    let i_weather = gtk::Label::new(None);
    i_weather.add_css_class("c-weather");
    let media_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    media_row.append(&c_art); media_row.append(&c_note); media_row.append(&c_title); media_row.append(&c_weather); media_row.append(&eq);
    let i_day = gtk::Label::new(None);  i_day.add_css_class("idle-day");
    let i_time = gtk::Label::new(None); i_time.add_css_class("idle-time");
    let idle_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    idle_row.set_halign(gtk::Align::Center);
    idle_row.set_valign(gtk::Align::Center);
    idle_row.append(&i_day); idle_row.append(&i_time); idle_row.append(&i_weather);
    let coll_stack = gtk::Stack::new();
    coll_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    coll_stack.set_transition_duration(220);
    coll_stack.set_hexpand(true);
    coll_stack.add_named(&media_row, Some("media"));
    coll_stack.add_named(&idle_row, Some("idle"));
    coll_stack.set_visible_child_name("idle"); // no player known at startup
    coll.append(&coll_stack);

    // expanded content
    let exp = gtk::Box::new(gtk::Orientation::Vertical, 6);
    exp.set_halign(gtk::Align::Fill);
    exp.set_valign(gtk::Align::Fill);
    exp.set_margin_top(12);
    exp.set_margin_bottom(12);
    exp.set_margin_start(16);
    exp.set_margin_end(16);
    exp.set_opacity(0.0);
    exp.set_can_target(false);
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(160);
    stack.set_vexpand(true);
    // Size to the visible page only; homogeneous would make every page as big as the tallest/widest one.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    // -- home: greeting + clock, quick stats, now-playing strip, last clipboard entry
    let home = gtk::Box::new(gtk::Orientation::Vertical, 10);
    home.add_css_class("home-dashboard");
    let greet = gtk::Label::new(None);
    greet.add_css_class("home-greeting");
    greet.add_css_class("cap");
    greet.set_xalign(0.0);
    let clock = gtk::Label::new(None);
    clock.add_css_class("clock");
    clock.add_css_class("home-clock");
    clock.set_xalign(0.0);
    let date = gtk::Label::new(None);
    date.add_css_class("sub");
    date.add_css_class("home-date");
    date.set_xalign(0.0);
    date.set_width_chars(1);
    date.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 2);
    left.set_hexpand(true);
    for w in [&greet, &clock, &date] { left.append(w); }
    let stats = gtk::Grid::new();
    stats.set_column_homogeneous(true);
    stats.set_column_spacing(4);
    stats.set_row_spacing(4);
    let mkstat = |t: &str| {
        let l = gtk::Label::new(Some(t));
        l.add_css_class("stat");
        l.set_xalign(0.0);
        l.set_halign(gtk::Align::Fill);
        l.set_width_chars(1);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l
    };
    let s_batt = mkstat("\u{f240} …");
    let s_vol = mkstat("\u{f028} …");
    let s_wifi = mkstat("\u{f1eb} …");
    s_wifi.set_max_width_chars(10); // cap long SSIDs so the home width does not change with the network name
    s_wifi.set_width_chars(1);
    s_wifi.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let s_bt = mkstat("\u{f293} …");
    let chip = |l: &gtk::Label, tip: &str| {
        let b = gtk::Button::new();
        b.set_child(Some(l));
        b.add_css_class("stat-button");
        b.set_tooltip_text(Some(tip));
        b.set_hexpand(true);
        b
    };
    let c_batt = chip(&s_batt, "Open battery details");
    let c_wifi = chip(&s_wifi, "Open Wi-Fi details");
    let c_bt = chip(&s_bt, "Open Bluetooth details");
    let c_vol = chip(&s_vol, "Open audio details");
    stats.attach(&c_batt, 0, 0, 1, 1);
    stats.attach(&c_vol, 1, 0, 1, 1);
    stats.attach(&c_wifi, 0, 1, 1, 1);
    stats.attach(&c_bt, 1, 1, 1, 1);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    top.append(&left);
    let weather_col = gtk::Box::new(gtk::Orientation::Vertical, 2);
    weather_col.add_css_class("home-weather");
    weather_col.set_valign(gtk::Align::Center);
    weather_col.set_halign(gtk::Align::Fill);
    weather_col.set_hexpand(true);
    let mk_weather = |cls: &str, text: &str| {
        let l = gtk::Label::new(Some(text));
        l.add_css_class(cls);
        l.set_xalign(0.0);
        l.set_halign(gtk::Align::Start);
        l.set_width_chars(1);
        l.set_max_width_chars(20);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l
    };
    let h_weather_temp = mk_weather("home-weather-temp", "\u{f0c2}");
    let h_weather_location = mk_weather("home-weather-location", "Weather unavailable");
    let h_weather_wind = mk_weather("home-weather-wind", "");
    h_weather_wind.set_visible(false);
    for w in [&h_weather_temp, &h_weather_location, &h_weather_wind] { weather_col.append(w); }
    top.append(&weather_col);
    let HomePlayer { root: h_player, strip, art: h_art, backdrop: h_backdrop, title: h_title, artist: h_artist, play: h_play } = home_player_card();
    let h_play_ref = h_play.clone();
    let h_clip = gtk::Label::new(Some("\u{f0ea}  Nothing copied yet"));
    h_clip.add_css_class("chip");
    h_clip.set_xalign(0.0);
    h_clip.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let lcol = gtk::Box::new(gtk::Orientation::Vertical, 6);
    lcol.set_hexpand(true);
    lcol.append(&top);
    lcol.append(&stats);
    lcol.append(&h_player);
    // Home week strip (below the player)
    let mini = gtk::Box::new(gtk::Orientation::Vertical, 8);
    mini.add_css_class("home-calendar");
    mini.set_hexpand(true);
    mini.set_valign(gtk::Align::Start);
    let mini_title = gtk::Label::new(None);
    mini_title.add_css_class("mc-title");
    mini_title.set_xalign(0.0);
    mini_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let mini_grid = gtk::Grid::new();
    mini_grid.set_hexpand(true);
    mini_grid.set_column_homogeneous(true);
    mini_grid.set_row_homogeneous(true);
    mini_grid.set_column_spacing(2);
    mini_grid.set_row_spacing(2);
    mini.append(&mini_title);
    mini.append(&mini_grid);
    home.append(&lcol);
    home.append(&mini);
    let home_sc = gtk::ScrolledWindow::new();
    home_sc.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    home_sc.set_propagate_natural_height(true);
    home_sc.set_min_content_height(0);
    home_sc.set_child(Some(&home));
    stack.add_named(&home_sc, Some("home"));

    // -- media: page body only; the artwork backdrop sits behind the whole notch (media_background)
    let media = gtk::Box::new(gtk::Orientation::Vertical, 0);
    media.add_css_class("media-tab");
    media.set_hexpand(true);
    let media_background = gtk::Overlay::new();
    media_background.set_halign(gtk::Align::Fill);
    media_background.set_valign(gtk::Align::Fill);
    media_background.set_can_target(false);
    media_background.set_visible(false);
    let media_base = gtk::Box::new(gtk::Orientation::Vertical, 0);
    media_base.add_css_class("media-base");
    media_base.set_hexpand(true);
    media_base.set_vexpand(true);
    media_background.set_child(Some(&media_base));
    let m_backdrop = gtk::Picture::new();
    m_backdrop.set_content_fit(gtk::ContentFit::Cover);
    m_backdrop.set_can_shrink(true);
    m_backdrop.set_opacity(0.65);
    let scrim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scrim.add_css_class("media-scrim");
    for w in [m_backdrop.upcast_ref::<gtk::Widget>(), scrim.upcast_ref()] {
        w.set_halign(gtk::Align::Fill);
        w.set_valign(gtk::Align::Fill);
        w.set_can_target(false);
    }
    media_base.set_can_target(false);
    media_background.add_overlay(&m_backdrop);
    media_background.add_overlay(&scrim);
    for w in [m_backdrop.upcast_ref::<gtk::Widget>(), scrim.upcast_ref()] {
        media_background.set_measure_overlay(w, false);
        media_background.set_clip_overlay(w, true);
    }
    // cover: the wrapper measures only the spacer, so artwork pixel size can never inflate the layout
    let m_art = gtk::Picture::new();
    m_art.set_content_fit(gtk::ContentFit::Cover);
    m_art.set_can_shrink(true);
    m_art.set_halign(gtk::Align::Fill);
    m_art.set_valign(gtk::Align::Fill);
    m_art.add_css_class("art");
    m_art.add_css_class("art-big");
    m_art.set_overflow(gtk::Overflow::Hidden);
    let cover_gloss = gtk::Box::new(gtk::Orientation::Vertical, 0);
    cover_gloss.add_css_class("cover-gloss");
    cover_gloss.set_can_target(false);
    cover_gloss.set_overflow(gtk::Overflow::Hidden);
    let m_cover_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    m_cover_spacer.set_size_request(128, 128);
    let m_cover = gtk::Overlay::new();
    m_cover.add_css_class("media-cover");
    m_cover.set_halign(gtk::Align::Center);
    m_cover.set_valign(gtk::Align::Center);
    m_cover.set_child(Some(&m_cover_spacer));
    m_cover.add_overlay(&m_art);
    m_cover.add_overlay(&cover_gloss);
    for w in [m_art.upcast_ref::<gtk::Widget>(), cover_gloss.upcast_ref()] {
        m_cover.set_measure_overlay(w, false);
        m_cover.set_clip_overlay(w, true);
    }
    let m_open_app = gtk::Button::from_icon_name("external-link-symbolic");
    m_open_app.add_css_class("media-open-app");
    m_open_app.set_size_request(28, 28);
    m_open_app.set_halign(gtk::Align::End);
    m_open_app.set_valign(gtk::Align::Start);
    m_open_app.set_tooltip_text(Some("Open playing app"));
    m_open_app.set_sensitive(false);
    let cover_row = gtk::Overlay::new();
    cover_row.set_hexpand(true);
    cover_row.set_margin_top(8);
    cover_row.set_margin_bottom(8);
    cover_row.set_child(Some(&m_cover));
    cover_row.add_overlay(&m_open_app);
    cover_row.set_measure_overlay(&m_open_app, false);
    let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
    col.set_hexpand(true);
    col.set_halign(gtk::Align::Fill);
    col.set_margin_start(8);
    col.set_margin_end(8);
    col.set_margin_top(6);
    col.set_margin_bottom(6);
    let meta = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let m_title = metadata_label("title", 0.5, Some("Nothing playing"));
    let m_artist = metadata_label("sub", 0.5, None);
    meta.append(&m_title);
    meta.append(&m_artist);
    let m_prog = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    m_prog.set_draw_value(false);
    m_prog.add_css_class("seek");
    m_prog.set_hexpand(true);
    m_prog.set_margin_top(0);
    {
        // user seek: debounce drags, then jump to fraction * track length
        let seq = Rc::new(Cell::new(0u32));
        m_prog.connect_change_value(move |_, _, v| {
            SEEKING.store(true, Ordering::Relaxed);
            let n = seq.get().wrapping_add(1);
            seq.set(n);
            let g = seq.clone();
            let frac = v.clamp(0.0, 1.0);
            glib::timeout_add_local_once(Duration::from_millis(150), move || {
                if g.get() != n { return; }
                let us: f64 = sh_out("playerctl", &["-p", PLAYERS, "metadata", "mpris:length"]).trim().parse().unwrap_or(0.0);
                if us > 0.0 {
                    let secs = format!("{:.2}", frac * us / 1e6);
                    spawn("playerctl", &["-p", PLAYERS, "position", &secs]);
                }
                glib::timeout_add_local_once(Duration::from_millis(600), || SEEKING.store(false, Ordering::Relaxed));
            });
            glib::Propagation::Proceed
        });
    }
    let times = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let m_t1 = gtk::Label::new(Some("0:00"));
    let m_t2 = gtk::Label::new(Some("0:00"));
    m_t1.add_css_class("sub");
    m_t2.add_css_class("sub");
    m_t1.set_hexpand(true);
    m_t1.set_xalign(0.0);
    times.append(&m_t1);
    times.append(&m_t2);
    let ctl = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    ctl.set_hexpand(true);
    ctl.set_homogeneous(true);
    let mk = |icon: &str, cmd: &'static str, tip: &str, size: i32| {
        let b = gtk::Button::with_label(icon);
        b.add_css_class("ctl");
        b.set_tooltip_text(Some(tip));
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
        b.set_size_request(size, size);
        b.connect_clicked(move |_| spawn("playerctl", &["-p", PLAYERS, cmd]));
        b
    };
    let mk_seek = |icon: &str, arg: &'static str, tip: &str| {
        let b = gtk::Button::with_label(icon);
        b.add_css_class("ctl");
        b.set_tooltip_text(Some(tip));
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
        b.set_size_request(40, 40);
        b.connect_clicked(move |_| spawn("playerctl", &["-p", PLAYERS, "position", arg]));
        b
    };
    let m_play = mk("\u{f04c}", "play-pause", "Play / pause", 48);
    m_play.add_css_class("ctl-play");
    ctl.append(&mk_seek("\u{f04a}", "30-", "Back 30 seconds"));
    ctl.append(&mk("\u{f048}", "previous", "Previous track", 40));
    ctl.append(&m_play);
    ctl.append(&mk("\u{f051}", "next", "Next track", 40));
    ctl.append(&mk_seek("\u{f04e}", "30+", "Forward 30 seconds"));
    for w in [cover_row.upcast_ref::<gtk::Widget>(), meta.upcast_ref(), m_prog.upcast_ref(),
        times.upcast_ref(), ctl.upcast_ref()] { col.append(w); }
    // current/next lyric lines wrap to the notch width; the Media page scrolls when they are long
    let lyr_current = lyric_label("lyric-cur", gtk::Align::End);
    let lyr_next = lyric_label("lyric-next", gtk::Align::Start);
    let lyrics_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    lyrics_box.add_css_class("lyrics");
    lyrics_box.set_hexpand(true);
    lyrics_box.set_margin_start(12);
    lyrics_box.set_margin_end(12);
    lyrics_box.append(&lyr_current);
    lyrics_box.append(&lyr_next);
    let media_fg = gtk::ScrolledWindow::new();
    media_fg.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    media_fg.set_propagate_natural_height(true);
    media_fg.set_min_content_height(0);
    media_fg.set_hexpand(true);
    media_fg.set_vexpand(true);
    media.append(&col);
    media.append(&lyrics_box);
    media_fg.set_child(Some(&media));
    stack.add_named(&media_fg, Some("media"));

    // -- calendar: month card above the selected day's agenda
    let calbox = gtk::Box::new(gtk::Orientation::Vertical, 12);
    calbox.add_css_class("calendar-tab");
    calbox.set_hexpand(true);
    calbox.set_valign(gtk::Align::Start);
    calbox.set_margin_start(8);
    calbox.set_margin_end(8);
    calbox.set_margin_top(6);
    calbox.set_margin_bottom(6);
    let cal = gtk::Calendar::new();
    cal.set_show_week_numbers(false);
    cal.set_hexpand(true);
    cal.set_valign(gtk::Align::Start);
    let calcard = gtk::Box::new(gtk::Orientation::Vertical, 8);
    calcard.add_css_class("cal-card");
    calcard.set_hexpand(true);
    calcard.set_valign(gtk::Align::Start);
    let side = gtk::Box::new(gtk::Orientation::Vertical, 6);
    side.add_css_class("day-card");
    side.set_hexpand(true);
    side.set_valign(gtk::Align::Start);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let c_clock = gtk::Label::new(None);
    c_clock.add_css_class("cal-clock");
    c_clock.set_xalign(0.0);
    c_clock.set_hexpand(true);
    c_clock.set_max_width_chars(1);
    c_clock.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let today = gtk::Button::with_label("Today");
    today.add_css_class("today-btn");
    today.set_valign(gtk::Align::Center);
    if let Some(l) = today.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    }
    {
        let cal = cal.clone();
        today.connect_clicked(move |_| cal.set_date(Some(&glib::DateTime::now_local().unwrap())));
    }
    head.append(&c_clock);
    head.append(&today);
    calcard.append(&head);
    calcard.append(&cal);
    let agenda_head = gtk::Label::new(Some("Agenda"));
    agenda_head.add_css_class("sub");
    agenda_head.set_xalign(0.0);
    let c_date = gtk::Label::new(None);
    c_date.add_css_class("day-date");
    c_date.set_xalign(0.0);
    c_date.set_hexpand(true);
    c_date.set_wrap(true);
    c_date.set_max_width_chars(1);
    c_date.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    sep.add_css_class("day-sep");
    side.append(&agenda_head);
    side.append(&c_date);
    side.append(&sep);
    let agenda = gtk::Box::new(gtk::Orientation::Vertical, 6);
    side.append(&agenda);
    calbox.append(&calcard);
    calbox.append(&side);
    let cal_sw = gtk::ScrolledWindow::new();
    cal_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    cal_sw.set_propagate_natural_height(true);
    cal_sw.set_min_content_height(0);
    cal_sw.set_hexpand(true);
    cal_sw.set_vexpand(true);
    cal_sw.set_child(Some(&calbox));
    stack.add_named(&cal_sw, Some("calendar"));

    // -- clipboard
    let clipv = gtk::Box::new(gtk::Orientation::Vertical, 6);
    clipv.set_margin_start(20);
    clipv.set_margin_end(20);
    clipv.set_margin_top(10);
    clipv.set_vexpand(true);
    let chead = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    chead.set_margin_start(6);
    chead.set_margin_end(6);
    chead.set_margin_top(4);
    let ctitle = gtk::Label::new(Some("Clipboard"));
    ctitle.add_css_class("cap");
    ctitle.set_hexpand(true);
    ctitle.set_xalign(0.0);
    let clear = gtk::Button::with_label("Clear");
    clear.add_css_class("small-btn");
    chead.append(&ctitle);
    chead.append(&clear);
    let clip_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let sw = gtk::ScrolledWindow::new();
    sw.set_vexpand(true);
    sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sw.set_child(Some(&clip_box));
    clipv.append(&chead);
    clipv.append(&sw);
    stack.add_named(&clipv, Some("clipboard"));
    let (ai_page, ai) = usage::build();
    let ai_scroll = gtk::ScrolledWindow::new();
    ai_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    ai_scroll.set_hexpand(true);
    ai_scroll.set_vexpand(true);
    ai_scroll.set_min_content_height(0);
    ai_scroll.set_propagate_natural_width(false);
    ai_scroll.set_propagate_natural_height(false);
    ai_scroll.set_child(Some(&ai_page));
    stack.add_named(&ai_scroll, Some("ai"));

    // -- controls page (stateful tiles: polling for DND/nightlight/recording)
    // Tile checks that only matter while Controls is on screen register here; one owned timer runs them then.
    let ctl_updates: Rc<RefCell<Vec<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(Vec::new()));
    let controls_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    controls_page.set_margin_start(12);
    controls_page.set_margin_end(12);
    controls_page.set_margin_top(8);
    controls_page.set_margin_bottom(8);
    controls_page.set_vexpand(true);

    let cgrid = gtk::Grid::new();
    cgrid.set_row_spacing(10);
    cgrid.set_column_spacing(10);
    cgrid.set_column_homogeneous(true);
    cgrid.set_row_homogeneous(true);
    cgrid.set_hexpand(true);
    cgrid.set_vexpand(true);

    // Returns (button, icon_label)
    let mk_tile = |icon: &str, label: &str| -> (gtk::Button, gtk::Label) {
        let b = gtk::Button::new();
        b.add_css_class("ctl-tile");
        b.set_hexpand(true);
        b.set_vexpand(true);
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 4);
        vbox.set_halign(gtk::Align::Fill);
        vbox.set_valign(gtk::Align::Center);
        let il = gtk::Label::new(Some(icon));
        il.add_css_class("ctl-icon");
        let tl = gtk::Label::new(Some(label));
        tl.add_css_class("ctl-lbl");
        tl.set_hexpand(true);
        tl.set_width_chars(1);
        tl.set_max_width_chars(10);
        tl.set_wrap(true);
        tl.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        tl.set_xalign(0.5);
        tl.set_justify(gtk::Justification::Center);
        vbox.append(&il);
        vbox.append(&tl);
        b.set_child(Some(&vbox));
        (b, il)
    };

    // Screenshot — fire and forget with delay
    let (btn_ss, _) = mk_tile("\u{f030}", "Screenshot");

    // Screen Record — stateful: red dot when recording
    let (btn_rec, icon_rec) = mk_tile("\u{f03d}", "Record");
    btn_rec.add_css_class("ctl-red");
    let rec_update: Rc<dyn Fn()> = {
        let b = btn_rec.clone();
        let il = icon_rec.clone();
        let update: Rc<dyn Fn()> = Rc::new(move || {
            if recording_active() { b.add_css_class("ctl-active"); b.add_css_class("ctl-rec"); il.set_text("\u{f111}"); }
            else { b.remove_css_class("ctl-active"); b.remove_css_class("ctl-rec"); il.set_text("\u{f03d}"); }
        });
        update();
        ctl_updates.borrow_mut().push(update.clone());
        update
    };

    // Reading Mode (nightlight) — stateful: active when hyprsunset running
    let (btn_nl, icon_nl) = mk_tile("\u{f06e}", "Reading Mode");
    btn_nl.add_css_class("ctl-warm");
    {
        let b = btn_nl.clone();
        let il = icon_nl.clone();
        let prev: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
        let tx = tx.clone();
        // one off-thread read at a time; the widget is only touched when the state actually changed
        let busy = Rc::new(Cell::new(false));
        let update: Rc<dyn Fn()> = Rc::new(move || {
            if busy.replace(true) { return; }
            let (rtx, rrx) = async_channel::bounded::<bool>(1);
            std::thread::spawn(move || { let _ = rtx.try_send(nightlight_enabled()); });
            let (busy, b, il, prev, tx) = (busy.clone(), b.clone(), il.clone(), prev.clone(), tx.clone());
            glib::MainContext::default().spawn_local(async move {
                let on = rrx.recv().await.unwrap_or(false);
                busy.set(false);
                if prev.get() == Some(on) { return; }
                il.set_text(if on { "\u{f06e}" } else { "\u{f070}" });
                if on { b.add_css_class("ctl-active"); } else { b.remove_css_class("ctl-active"); }
                if prev.replace(Some(on)).is_some() {
                    let _ = tx.try_send(Msg::Toggle(if on { "\u{f06e}" } else { "\u{f070}" }.into(), on));
                }
            });
        });
        update();
        {
            let update2 = update.clone();
            glib::timeout_add_seconds_local(3, move || { update2(); glib::ControlFlow::Continue });
        }
        btn_nl.connect_clicked({
            let update = update.clone();
            move |btn| {
                flash_tile(btn, "ctl-flash", 450);
                tile_await(btn, nightlight_enabled, { let u = update.clone(); move || u() });
                if let Ok(c) = Command::new("sh").arg("-c").arg("omarchy toggle nightlight").spawn() { reap_detached(c); }
            }
        });
    }

    // Do Not Disturb — stateful: reads ~/.local/state/omarchy/notifications.json
 let (btn_dnd, icon_dnd) = mk_tile("\u{f0f3}", "Do Not Disturb");
 btn_dnd.add_css_class("ctl-dnd");
 {
     let b = btn_dnd.clone();
     let il = icon_dnd.clone();
     let dnd_path = format!("{}/.local/state/omarchy/notifications.json",
         std::env::var("HOME").unwrap_or_default());
     let read_dnd = move |p: &str| -> bool {
         std::fs::read_to_string(p).ok()
             .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
             .and_then(|v| v["dnd"].as_bool()).unwrap_or(false)
     };
     let prev: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
     let update = {
         let b = b.clone(); let il = il.clone(); let p = dnd_path.clone(); let rd = read_dnd.clone();
         let tx = tx.clone();
         move || {
             let dnd_on = rd(&p);
             il.set_text(if dnd_on { "\u{f0f4}" } else { "\u{f0f3}" });
             if dnd_on { b.add_css_class("ctl-active"); } else { b.remove_css_class("ctl-active"); }
             // announce changes (not the first read) with a notch toast; notifications are silenced while on
             if prev.replace(Some(dnd_on)).map_or(false, |o| o != dnd_on) {
                 let _ = tx.try_send(Msg::Toggle(if dnd_on { "\u{f0f4}" } else { "\u{f0f3}" }.into(), dnd_on));
             }
         }
     };
     update();
     watch_file(std::path::PathBuf::from(&dnd_path), { let u = update.clone(); move || u() });
    btn_dnd.connect_clicked(move |btn| {
        flash_tile(btn, "ctl-flash", 450);
         notify::set_dnd(!notify::dnd_on());
         let u = update.clone();
         glib::timeout_add_local_once(Duration::from_millis(100), move || u());
     });
 }

    // -- wifi sub-page
    let detail_origin: Rc<Cell<&'static str>> = Rc::new(Cell::new("controls"));
    let DevicePage { root: wifi_page, back: wifi_back, refresh: wifi_refresh, headline: wifi_toggle_label, summary: wifi_summary, toggle: wifi_toggle_btn, list: wifi_list } =
        device_page("Wi-Fi", "\u{f1eb}", "wifi-toggle");
    stack.add_named(&wifi_page, Some("wifi"));

    let (wtx, wrx) = async_channel::unbounded::<WifiSnap>();
    let wifi_on = std::rc::Rc::new(std::cell::Cell::new(true));
    let wifi_sig = std::rc::Rc::new(RefCell::new(String::new()));

    // opening the page: forget the fingerprint so the list re-animates, show cached results, then rescan
    let populate_wifi: Rc<dyn Fn()> = {
        let wtx = wtx.clone();
        let wifi_sig = wifi_sig.clone();
        let win = win.clone();
        Rc::new(move || { win.set_keyboard_mode(KeyboardMode::None); wifi_sig.borrow_mut().clear(); wifi_snap_async(&wtx, true); })
    };

    // snapshot receiver (runs on the UI thread, never blocks) + 3s refresh while the page is visible
    {
        let (list, label, sum, btn, refresh) = (wifi_list.clone(), wifi_toggle_label.clone(), wifi_summary.clone(), wifi_toggle_btn.clone(), wifi_refresh.clone());
        let (tx, wtx, stack) = (tx.clone(), wtx.clone(), stack.clone()); let win_a = win.clone();
        let (wifi_on, wifi_sig) = (wifi_on.clone(), wifi_sig.clone());
        let pending: Rc<RefCell<Option<WifiSnap>>> = Rc::new(RefCell::new(None));
        let retry = Rc::new(Cell::new(false));
        let apply = Rc::new(move |s: WifiSnap| {
            wifi_on.set(s.on);
            let (h, m) = wifi_card_text(&s);
            label.set_text(&h);
            sum.set_text(&m);
            btn.set_label(if s.on { "Turn off" } else { "Turn on" });
            btn.set_sensitive(s.available);
            refresh.set_sensitive(s.available);
            let new_sig = s.signature();
            let old_sig = wifi_sig.borrow().clone();
            if old_sig != new_sig {
                let animate = old_sig.is_empty() || old_sig.as_bytes()[0] != new_sig.as_bytes()[0];
                render_wifi(&list, &s, animate, &tx, &wtx, &win_a);
                *wifi_sig.borrow_mut() = new_sig;
                if list.is_mapped() { let _ = tx.try_send(Msg::Relayout); }
            }
        });
        let (stack2, win2) = (stack.clone(), win.clone());
        let typing = Rc::new(move || win2.keyboard_mode() == KeyboardMode::OnDemand && stack2.visible_child_name().as_deref() == Some("wifi"));
        glib::MainContext::default().spawn_local(async move {
            while let Ok(mut s) = wrx.recv().await {
                while let Ok(n) = wrx.try_recv() { s = n; }
                // never rebuild the list under a password the user is typing; apply once typing ends
                if typing() {
                    *pending.borrow_mut() = Some(s);
                    if !retry.replace(true) {
                        let (pending, retry, apply, typing) = (pending.clone(), retry.clone(), apply.clone(), typing.clone());
                        glib::timeout_add_local(Duration::from_millis(250), move || {
                            if typing() { return glib::ControlFlow::Continue; }
                            retry.set(false);
                            if let Some(p) = pending.borrow_mut().take() { apply(p); }
                            glib::ControlFlow::Break
                        });
                    }
                } else { pending.borrow_mut().take(); apply(s); }
            }
        });
    }

    {
        let (tx_w, wtx_w, wifi_on, label, sum) = (tx.clone(), wtx.clone(), wifi_on.clone(), wifi_toggle_label.clone(), wifi_summary.clone());
        wifi_toggle_btn.connect_clicked(move |b| {
            let turn_on = !wifi_on.get();
            wifi_on.set(turn_on);
            label.set_text(if turn_on { "Turning on\u{2026}" } else { "Wi-Fi is off" });
            sum.set_text("");
            b.set_label(if turn_on { "Turn off" } else { "Turn on" });
            wifi_radio(&tx_w, Some(&wtx_w), turn_on);
        });
    }

    {
        let wtx = wtx.clone();
        wifi_refresh.connect_clicked(move |_| wifi_snap_async(&wtx, true));
    }
    wifi_back.connect_clicked({
        let (stack, win, origin) = (stack.clone(), win.clone(), detail_origin.clone());
        move |_| { win.set_keyboard_mode(KeyboardMode::None); stack.set_visible_child_name(origin.get()); }
    });

    // -- bluetooth sub-page
    let DevicePage { root: bt_page, back: bt_back, refresh: bt_refresh, headline: bt_toggle_label, summary: bt_summary, toggle: bt_toggle_btn, list: bt_list } =
        device_page("Bluetooth", "\u{f293}", "bt-toggle");
    stack.add_named(&bt_page, Some("bluetooth"));

    let (btx, brx) = async_channel::unbounded::<BtSnap>();
    let bt_on = std::rc::Rc::new(std::cell::Cell::new(true));
    let bt_sig = std::rc::Rc::new(RefCell::new(String::new()));

    // opening the page: forget the fingerprint so the list re-animates
    let populate_bt: Rc<dyn Fn()> = {
        let btx = btx.clone();
        let bt_sig = bt_sig.clone();
        Rc::new(move || { bt_sig.borrow_mut().clear(); bt_snap_async(&btx); })
    };

    {
        let (list, label, sum, btn, refresh) = (bt_list.clone(), bt_toggle_label.clone(), bt_summary.clone(), bt_toggle_btn.clone(), bt_refresh.clone());
        let (tx, btx) = (tx.clone(), btx.clone());
        let (bt_on, bt_sig) = (bt_on.clone(), bt_sig.clone());
        let apply = Rc::new(move |s: BtSnap| {
            bt_on.set(s.on);
            let (h, m) = bt_card_text(&s);
            label.set_text(&h);
            sum.set_text(&m);
            btn.set_label(if s.on { "Turn off" } else { "Turn on" });
            btn.set_sensitive(s.available);
            refresh.set_sensitive(s.available);
            let new_sig = s.signature();
            let old_sig = bt_sig.borrow().clone();
            if old_sig != new_sig {
                let animate = old_sig.is_empty() || old_sig.as_bytes()[0] != new_sig.as_bytes()[0];
                render_bt(&list, &s, animate, &tx, &btx);
                *bt_sig.borrow_mut() = new_sig;
                if list.is_mapped() { let _ = tx.try_send(Msg::Relayout); }
            }
        });
        glib::MainContext::default().spawn_local(async move {
            while let Ok(mut s) = brx.recv().await {
                while let Ok(n) = brx.try_recv() { s = n; }
                apply(s);
            }
        });
    }

    {
        let (tx_b, btx_b, bt_on, label, sum) = (tx.clone(), btx.clone(), bt_on.clone(), bt_toggle_label.clone(), bt_summary.clone());
        bt_toggle_btn.connect_clicked(move |b| {
            let turn_on = !bt_on.get();
            bt_on.set(turn_on);
            label.set_text(if turn_on { "Turning on\u{2026}" } else { "Bluetooth is off" });
            sum.set_text("");
            b.set_label(if turn_on { "Turn off" } else { "Turn on" });
            bt_power(&tx_b, Some(&btx_b), turn_on);
        });
    }

    {
        let btx = btx.clone();
        bt_refresh.connect_clicked(move |_| bt_scan(&btx));
    }
    bt_back.connect_clicked({
        let (stack, win, origin) = (stack.clone(), win.clone(), detail_origin.clone());
        move |_| { win.set_keyboard_mode(KeyboardMode::None); stack.set_visible_child_name(origin.get()); }
    });

    // One owned 3s refresh per detail page, installed only while that page is displayed and the notch is expanded.
    {
        let (wtx, btx, win, expanded, ctl_updates) = (wtx.clone(), btx.clone(), win.clone(), expanded.clone(), ctl_updates.clone());
        let slot: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        stack.connect_visible_child_name_notify(move |st| {
            if let Some(id) = slot.borrow_mut().take() { id.remove(); }
            let name = st.visible_child_name();
            if win.keyboard_mode() == KeyboardMode::OnDemand && !matches!(name.as_deref(), Some("wifi") | Some("settings") | Some("utility")) {
                win.set_keyboard_mode(KeyboardMode::None);
            }
            let (wtx, btx, expanded) = (wtx.clone(), btx.clone(), expanded.clone());
            if name.as_deref() == Some("controls") {
                let ups = ctl_updates.clone();
                for u in ups.borrow().iter() { u(); }
                let id = glib::timeout_add_seconds_local(2, move || {
                    if expanded.load(Ordering::Relaxed) { for u in ups.borrow().iter() { u(); } }
                    glib::ControlFlow::Continue
                });
                *slot.borrow_mut() = Some(id);
                return;
            }
            let wifi = match name.as_deref() { Some("wifi") => true, Some("bluetooth") => false, _ => return };
            let id = glib::timeout_add_seconds_local(3, move || {
                if expanded.load(Ordering::Relaxed) {
                    if wifi { wifi_snap_async(&wtx, false) } else { bt_snap_async(&btx) }
                }
                glib::ControlFlow::Continue
            });
            *slot.borrow_mut() = Some(id);
        });
    }

    // Wi-Fi — opens notch wifi sub-page
    let (btn_wifi, icon_wifi) = mk_tile("\u{f1eb}", "Wi-Fi");
    {
        let b = btn_wifi.clone();
        let il = icon_wifi.clone();
        let update: Rc<dyn Fn()> = Rc::new(move || {
            let on = sh_out("nmcli", &["radio", "wifi"]).trim() == "enabled";
            il.set_text(if on { "\u{f1eb}" } else { "\u{f092e}" });
            if on { b.add_css_class("ctl-active"); } else { b.remove_css_class("ctl-active"); }
        });
        update();
        ctl_updates.borrow_mut().push(update);
    }

    // Bluetooth — opens notch bluetooth sub-page
    let (btn_bt, icon_bt) = mk_tile("\u{f293}", "Bluetooth");
    {
        let b = btn_bt.clone();
        let il = icon_bt.clone();
        let update: Rc<dyn Fn()> = Rc::new(move || {
            let on = sh_out("bluetoothctl", &["show"]).contains("Powered: yes");
            il.set_text(if on { "\u{f293}" } else { "\u{f294}" });
            if on { b.add_css_class("ctl-active"); } else { b.remove_css_class("ctl-active"); }
        });
        update();
        ctl_updates.borrow_mut().push(update);
    }

    // -- utilities added next to the original six, and the Settings tile
    let (btn_lock, _) = mk_tile("\u{f023}", "Lock");
    let (btn_clr, _) = mk_tile("\u{f1f8}", "Clear clipboard");
    let (btn_audio, _) = mk_tile("\u{f028}", "Audio");
    let (btn_disp, _) = mk_tile("\u{f108}", "Displays");
    let (btn_set, _) = mk_tile("\u{f013}", "Settings");
    if !on_path("omarchy") {
        btn_lock.set_sensitive(false);
        btn_lock.set_tooltip_text(Some("The `omarchy` command was not found"));
    }
    let ctl_status = gtk::Label::new(None);
    ctl_status.add_css_class("sub");
    ctl_status.set_xalign(0.0);
    ctl_status.set_wrap(true);
    ctl_status.set_visible(false);
    {
        let tx = tx.clone();
        ctl_status.connect_visible_notify({ let tx = tx.clone(); move |_| { let _ = tx.try_send(Msg::Relayout); } });
        ctl_status.connect_label_notify(move |_| { let _ = tx.try_send(Msg::Relayout); });
    }
    let ctl_status_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    let confirm_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let confirm_lbl = gtk::Label::new(Some("Delete all clipboard history?"));
    confirm_lbl.add_css_class("sub");
    confirm_lbl.set_hexpand(true);
    confirm_lbl.set_xalign(0.0);
    let confirm_yes = gtk::Button::with_label("Clear");
    let confirm_no = gtk::Button::with_label("Cancel");
    for b in [&confirm_yes, &confirm_no] { b.add_css_class("small-btn"); }
    confirm_row.append(&confirm_lbl);
    confirm_row.append(&confirm_yes);
    confirm_row.append(&confirm_no);
    confirm_row.set_visible(false);
    {
        let c = confirm_row.clone();
        confirm_no.connect_clicked(move |_| c.set_visible(false));
        let c = confirm_row.clone();
        btn_clr.connect_clicked(move |_| c.set_visible(true));
    }
    let tile_list: Vec<(&'static str, gtk::Button)> = vec![
        ("screenshot", btn_ss.clone()), ("record", btn_rec.clone()), ("nightlight", btn_nl.clone()), ("dnd", btn_dnd.clone()),
        ("wifi", btn_wifi.clone()), ("bluetooth", btn_bt.clone()), ("lock", btn_lock.clone()), ("clear-clipboard", btn_clr.clone()),
        ("audio", btn_audio.clone()), ("display", btn_disp.clone()),
    ];
    // The Settings UI is built on first entry (`Ui::ensure_settings_page`); until then this host stays empty.
    let settings_host = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stack.add_named(&settings_host, Some("settings"));

    // -- Audio / Displays page: hosts the shared real controls from `popups` inside the notch
    let utility_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    utility_page.add_css_class("utility-page");
    utility_page.set_margin_start(20);
    utility_page.set_margin_end(20);
    utility_page.set_margin_top(12);
    utility_page.set_margin_bottom(12);
    let utility_head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let utility_back = gtk::Button::with_label("\u{f060}");
    utility_back.add_css_class("back-btn");
    let utility_title = gtk::Label::new(Some("Audio"));
    utility_title.add_css_class("cap");
    utility_title.set_hexpand(true);
    utility_title.set_xalign(0.0);
    utility_head.append(&utility_back);
    utility_head.append(&utility_title);
    let utility_host = gtk::Box::new(gtk::Orientation::Vertical, 8);
    utility_host.set_margin_end(6);
    let utility_sw = gtk::ScrolledWindow::new();
    utility_sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    utility_sw.set_propagate_natural_width(false);
    utility_sw.set_propagate_natural_height(false);
    utility_sw.set_vexpand(true);
    utility_sw.set_min_content_height(0);
    utility_sw.set_child(Some(&utility_host));
    utility_page.append(&utility_head);
    utility_page.append(&utility_sw);
    stack.add_named(&utility_page, Some("utility"));
    utility_back.connect_clicked({
        let (stack, win, origin) = (stack.clone(), win.clone(), detail_origin.clone());
        move |_| { win.set_keyboard_mode(KeyboardMode::None); stack.set_visible_child_name(origin.get()); }
    });
    {
        let st = stack.clone();
        btn_set.connect_clicked(move |_| st.set_visible_child_name("settings"));
        let (win2, was_settings, txr) = (win.clone(), Rc::new(Cell::new(false)), tx.clone());
        let mut anim_tiles: Vec<gtk::Button> = tile_list.iter().map(|(_, b)| b.clone()).collect();
        anim_tiles.push(btn_set.clone());
        let stack2 = stack.clone();
        stack2.connect_visible_child_name_notify(move |s| {
            let name = s.visible_child_name();
            if name.as_deref() == Some("settings") {
                // editing needs the keyboard: on-demand, never exclusive
                win2.set_keyboard_mode(KeyboardMode::OnDemand);
                was_settings.set(true);
                let _ = txr.try_send(Msg::Relayout);
                return;
            }
            if was_settings.replace(false) { win2.set_keyboard_mode(KeyboardMode::None); let _ = txr.try_send(Msg::Relayout); }
            if name.as_deref() != Some("controls") { return; }
            for (i, t) in anim_tiles.iter().enumerate() {
                t.add_css_class("ctl-hide");
                let t = t.clone();
                glib::timeout_add_local_once(Duration::from_millis(45 * i as u64 + 16), move || {
                    t.remove_css_class("ctl-hide");
                    flash_tile(&t, "ctl-in", 320);
                });
            }
        });
    }
    let cgrid_scroll = gtk::ScrolledWindow::new();
    cgrid_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    cgrid_scroll.set_vexpand(true);
    cgrid_scroll.set_min_content_height(0);
    cgrid_scroll.set_child(Some(&cgrid));
    controls_page.append(&cgrid_scroll);
    controls_page.append(&confirm_row);
    controls_page.append(&ctl_status);
    stack.add_named(&controls_page, Some("controls"));
    // -- tab row
    let tabs = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    tabs.add_css_class("tabs");
    tabs.set_halign(gtk::Align::Center);
    tabs.set_valign(gtk::Align::End);
    let tab_btns: Rc<RefCell<Vec<(gtk::Button, &'static str)>>> = Rc::new(RefCell::new(vec![]));
    for (icon, name) in [("\u{f015}", "home"), ("\u{f001}", "media"), ("\u{f073}", "calendar"), ("\u{f0ea}", "clipboard"), ("\u{f06a9}", "ai"), ("\u{f013}", "controls")] {
        let b = gtk::Button::with_label(icon);
        b.add_css_class("tab");
        b.add_css_class(&format!("tab-{name}"));
        b.set_halign(gtk::Align::Fill);
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.connect_map(|l| {
                let l = l.clone();
                glib::idle_add_local_once(move || center_ink(&l));
            });
        }
        if name == "home" { b.add_css_class("active"); }
        tabs.append(&b);
        tab_btns.borrow_mut().push((b.clone(), name));
        // the handler releases hosted WhatsApp content first, then switches page and highlight
        let tx = tx.clone();
        b.connect_clicked(move |_| { let _ = tx.try_send(Msg::TabSelected(name)); });
    }
    // clicking the home page's mini player / mini calendar jumps to that tab
    {
        let g = gtk::GestureClick::new();
        let tab_btns = tab_btns.clone();
        let (player, play) = (h_player.clone(), h_play_ref.clone());
        g.connect_released(move |_, _, x, y| {
            let mut w = player.pick(x, y, gtk::PickFlags::DEFAULT);
            while let Some(cur) = w {
                if cur == *play.upcast_ref::<gtk::Widget>() { return; }
                w = cur.parent();
            }
            if let Some((b, _)) = tab_btns.borrow().iter().find(|(_, n)| *n == "media") { if b.is_visible() { b.emit_clicked(); } }
        });
        h_player.add_controller(g);
    }
    {
        let g = gtk::GestureClick::new();
        let tab_btns = tab_btns.clone();
        g.connect_released(move |_, _, _, _| {
            if let Some((b, _)) = tab_btns.borrow().iter().find(|(_, n)| *n == "calendar") { if b.is_visible() { b.emit_clicked(); } }
        });
        mini.add_controller(g);
    }
    // WhatsApp detail page: an empty slot. The plugin's QML window is placed exactly over it and draws no backplate.
    let wa_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    wa_page.add_css_class("wa-page");
    wa_page.set_vexpand(true);
    wa_page.set_hexpand(true);
    let wa_status = gtk::Label::new(Some("Opening WhatsApp\u{2026}"));
    wa_status.add_css_class("sub");
    wa_status.set_vexpand(true);
    wa_status.set_valign(gtk::Align::Center);
    wa_status.set_halign(gtk::Align::Center);
    wa_page.append(&wa_status);
    stack.add_named(&wa_page, Some("whatsapp"));
    exp.append(&stack);
    exp.append(&tabs);

    overlay.add_overlay(&media_background);
    overlay.set_measure_overlay(&media_background, false);
    overlay.set_clip_overlay(&media_background, true);
    overlay.add_overlay(&coll);
    overlay.add_overlay(&exp);
    overlay.set_measure_overlay(&coll, false);
    overlay.set_measure_overlay(&exp, false);
    win.set_child(Some(&notch));

    let hist: Vec<String> = load_history_at(&hist_path()).unwrap_or_else(|e| { eprintln!("clipboard history: {e}"); Vec::new() });

    let popups = popups::Popups::new(tx.clone());
    let osd = osd::Osd::new(app);
    // Quickshell owns org.freedesktop.Notifications and relays over private sockets; both must bind or there is no UI.
    // Bounded ingress (the shell never has more than 32 unacknowledged events); the GTK consumer acknowledges each one.
    let (events_tx, events_rx) = async_channel::bounded::<notification_relay::Event>(32);
    let cmds = match notification_relay::start(shell_pid, tx.clone(), events_tx) {
        Ok(cmds) => cmds,
        Err(e) => { eprintln!("notification relay: cannot listen: {e}"); app.quit(); return; }
    };
    let content = match notch_content::start(shell_pid, tx.clone()) {
        Ok(b) => b,
        Err(e) => { eprintln!("notch content: cannot listen: {e}"); app.quit(); return; }
    };
    let notes = notify::NotifyUi::new(app, cmds);
    notes.run_events(events_rx);
    let ui = Rc::new(Ui {
        content, ext: RefCell::new(None), ext_timer: RefCell::new(None), ext_pub_busy: Cell::new(false), notch_hover: Cell::new(false),
        started: std::time::Instant::now(), wa_page: wa_page.clone(), wa_status: wa_status.clone(),
        popups, populate_bt: populate_bt.clone(), populate_wifi: populate_wifi.clone(), osd, notes, ai, cal, agenda, events: RefCell::new(Vec::new()), gstate: RefCell::new(String::new()), stack: stack.clone(), tab_btns: tab_btns.clone(),
        settings: RefCell::new(settings), desk: RefCell::new(None), dest: RefCell::new(None), dest_info: RefCell::new(None),
        geom: Cell::new(geom0), host: Cell::new(host0), hidden: Cell::new(false), bar_tx: bar_controller::start_sync_sender(),
        tabs: tabs.clone(),
        win: win.clone(), notch: notch.clone(), c_art, c_note, c_title, c_weather, i_weather, h_weather_temp, h_weather_location, h_weather_wind, eq: eq.clone(), m_art, m_backdrop, m_title, m_artist, m_prog, m_t1, m_t2,
        m_cover_spacer, media_background: media_background.clone(), m_open_app: m_open_app.clone(),
        player_instance: RefCell::new(String::new()), player_generation: Cell::new(0), player_raise_busy: Cell::new(false),
        coll_stack: coll_stack.clone(),
        m_play, h_title, h_artist, h_art, h_play, h_clip, s_batt, s_vol, s_wifi, s_bt, coll, exp, clip_box, clip_title: ctitle.clone(), clip_clear: clear.clone(), prog: Cell::new(0.0), vel: Cell::new(0.0), target: Cell::new(0.0),
        animating: Cell::new(false), expanded: expanded.clone(), bars: RefCell::new([0.12; BARS]),
        size: Cell::new(Size { w: geom0.coll_w, h: geom0.coll_h, tw: geom0.coll_w, th: geom0.coll_h, vw: 0.0, vh: 0.0 }),
        playing: Cell::new(false), media_presence: Cell::new(MediaPresence::Missing), len: Cell::new(0.0), last_playing: Cell::new(None),
        art_url: RefCell::new(String::new()), art_worker: ArtWorker::new(tx.clone()), hist: RefCell::new(hist), collapse_id: RefCell::new(None), reset_id: RefCell::new(None),
        refresh_at: Cell::new(None),
        lyr_current: lyr_current.clone(), lyr_next: lyr_next.clone(), lyr: RefCell::new(lyrics::Lyrics::None),
        lyr_key: RefCell::new(String::new()), pos: Cell::new(0.0),
 cava: RefCell::new(None), workers: RefCell::new(Vec::new()), viz_on: Cell::new(false), viz_gen: Cell::new(0), capture_pending: Cell::new(false), capture_active: Cell::new(false), capture_hover_blocked: Cell::new(false), btn_ss: btn_ss.clone(), btn_rec: btn_rec.clone(), rec_update: rec_update.clone(), tx: tx.clone(),
 tile_wifi: btn_wifi.clone(), tile_bt: btn_bt.clone(),
 settings_page: RefCell::new(None), settings_host: settings_host.clone(), art_accepted: RefCell::new(None), art_prune_pending: Cell::new(false), tiles: tile_list, tile_settings: btn_set.clone(), cgrid: cgrid.clone(),
 ctl_status: ctl_status.clone(), ctl_status_timer: ctl_status_timer.clone(), detail_origin: detail_origin.clone(), confirm_row: confirm_row.clone(), clearing: Cell::new(false),
        media_tab: media.clone(), strip: strip.clone(), h_backdrop: h_backdrop.clone(), skel_id: RefCell::new(None),
        utility_host: utility_host.clone(), utility_title: utility_title.clone(), utility_kind: Cell::new(None), utility_kbd: Cell::new(false),
        utility_page: utility_page.clone(), wifi_page: wifi_page.clone(), bt_page: bt_page.clone(), agenda_date: c_date.clone(),
    });
    ui.apply(0.0);
    {
        let u = ui.clone();
        ui.m_open_app.connect_clicked(move |b| {
            if u.player_raise_busy.replace(true) { return; }
            let inst = u.player_instance.borrow().clone();
            if inst.is_empty() { u.player_raise_busy.set(false); return; }
            let generation = u.player_generation.get();
            b.set_sensitive(false);
            let tx = u.tx.clone();
            std::thread::spawn(move || { let _ = tx.try_send(Msg::PlayerRaised(generation, raise_player_app(&inst))); });
        });
    }
    {
        let txu = tx.clone();
        stack.connect_visible_child_name_notify(move |_| { let _ = txu.try_send(Msg::UtilityPageChanged); });
        let (u, key) = (ui.clone(), gtk::EventControllerKey::new());
        key.connect_key_pressed(move |_, k, _, _| {
            if k == gdk::Key::Escape && u.stack.visible_child_name().as_deref() == Some("whatsapp") {
                if !u.ext_close(CloseTo::Home) { u.show_tab("home"); }
                glib::Propagation::Stop
            } else if k == gdk::Key::Escape && matches!(u.stack.visible_child_name().as_deref(), Some("utility") | Some("wifi") | Some("bluetooth")) {
                u.win.set_keyboard_mode(KeyboardMode::None);
                u.stack.set_visible_child_name(u.detail_origin.get());
                glib::Propagation::Stop
            } else { glib::Propagation::Proceed }
        });
        win.add_controller(key);
    }
    {
        let u = ui.clone();
        confirm_yes.connect_clicked(move |_| { u.confirm_row.set_visible(false); u.clear_clipboard(); });
        let u = ui.clone();
        btn_lock.connect_clicked(move |_| u.lock_screen());
        let u = ui.clone();
        btn_audio.connect_clicked(move |_| u.open_utility("volume", "controls"));
        let u = ui.clone();
        btn_disp.connect_clicked(move |_| u.open_utility("display", "controls"));
        // Wi-Fi / Bluetooth detail pages: opening only navigates and refreshes, it never changes radio state or connects
        { let u = ui.clone(); btn_wifi.connect_clicked(move |_| u.open_device("wifi", "controls")); }
        { let u = ui.clone(); c_wifi.connect_clicked(move |_| u.open_device("wifi", "home")); }
        { let u = ui.clone(); btn_bt.connect_clicked(move |_| u.open_device("bluetooth", "controls")); }
        { let u = ui.clone(); c_bt.connect_clicked(move |_| u.open_device("bluetooth", "home")); }
        { let u = ui.clone(); c_batt.connect_clicked(move |_| u.open_utility("battery", "home")); }
        { let u = ui.clone(); c_vol.connect_clicked(move |_| u.open_utility("volume", "home")); }
        // a click on the collapsed notch always opens it, whatever the hover setting
        let g = gtk::GestureClick::new();
        let u = ui.clone();
        g.connect_released(move |_, _, _, _| {
            if !u.expanded.load(Ordering::Relaxed) { u.cancel_collapse(); u.set_expanded(true); }
        });
        ui.coll.add_controller(g);
        ui.sync_settings();
    }
    if let Some(d) = gdk::Display::default() {
        // the compositor can report an output before GDK has its monitor: retry the pending move then
        let u = ui.clone();
        d.monitors().connect_items_changed(move |_, _, _, _| u.reconcile());
    }
    // Screenshot / Record: collapse the notch first, then hand over to Omarchy (see `Ui::start_capture`)
    {
        let u = ui.clone();
        btn_ss.connect_clicked(move |_| u.start_capture(CaptureAction::Screenshot));
        let u = ui.clone();
        btn_rec.connect_clicked(move |_| {
            u.start_capture(if recording_active() { CaptureAction::RecordStop } else { CaptureAction::RecordStart });
        });
    }

    {
        let u = ui.clone();
        eq.set_draw_func(move |_, cr, w, h| draw_bars(&u, cr, w, h));
    }
    {
        let u = ui.clone();
        clear.connect_clicked(move |_| u.clear_clipboard());
    }
    {
        let u = ui.clone();
        ui.cal.connect_day_selected(move |_| u.refresh_agenda());
        let u = ui.clone();
        ui.cal.connect_notify_local(Some("month"), move |_, _| u.refresh_agenda());
        ui.refresh_agenda();
        gcal_thread(tx.clone());
    }

    // hover: real enter/leave events (no polling). Leaving waits a beat before collapsing.
    let motion = gtk::EventControllerMotion::new();
    {
        let u = ui.clone();
        motion.connect_enter(move |_, _, _| {
            u.notch_hover.set(true);
            u.cancel_collapse();
            if u.settings.borrow().behavior.open_on_hover && !u.capture_hover_blocked.get() { u.set_expanded(true); }
        });
        let u = ui.clone();
        motion.connect_leave(move |_| { u.notch_hover.set(false); u.capture_hover_blocked.set(false); let ms = u.settings.borrow().behavior.collapse_delay_ms; u.schedule_collapse(ms as u64) });
    }
    notch.add_controller(motion);

    // clocks (1 s) and the fake equalizer (only when cava is missing)
    let tick = {
        let (greet, clock, date, c_clock, i_day, i_time) = (greet.clone(), clock.clone(), date.clone(), c_clock.clone(), i_day.clone(), i_time.clone());
        let (mini_grid, mini_title, shown) = (mini_grid.clone(), mini_title.clone(), Cell::new(0u32));
        let uu = ui.clone();
        move || {
            use chrono::{Local, Timelike};
            let n = Local::now();
            if shown.get() != chrono::Datelike::ordinal(&n) + 1000 * chrono::Datelike::year(&n) as u32 {
                shown.set(chrono::Datelike::ordinal(&n) + 1000 * chrono::Datelike::year(&n) as u32);
                fill_home_week(&mini_grid, &mini_title, n.date_naive());
                if uu.stack.visible_child_name().as_deref() == Some("home") { let _ = uu.tx.try_send(Msg::Relayout); }
            }
            let g = match n.hour() { 5..=11 => "Good morning", 12..=17 => "Good afternoon", _ => "Good evening" };
            greet.set_text(g);
            let hm = uu.settings.borrow().behavior.clock_format.chrono_pattern();
            clock.set_text(&n.format(hm).to_string());
            let full = n.format("%A, %-d %B").to_string();
            date.set_text(&full);
            date.set_tooltip_text(Some(&full));
            c_clock.set_text(&n.format(hm).to_string());
            i_day.set_text(&n.format("%a").to_string().to_uppercase());
            i_time.set_text(&n.format(hm).to_string());
        }
    };
    tick();
    glib::timeout_add_seconds_local(1, move || { tick(); glib::ControlFlow::Continue });
    {
        let tx = tx.clone();
        ui.stack.connect_transition_running_notify(move |s| {
            if !s.is_transition_running() { let _ = tx.try_send(Msg::Relayout); }
        });
    }

    // message pump: wakes only when a message is queued; one sizing pass per drained batch of at most 64 messages,
    // after which the main loop gets a turn (one-millisecond timeout) before the next batch
    {
        const PUMP_BATCH: usize = 64;
        let u = ui.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(first) = rx.recv().await {
                let mut relayout = false;
                let mut taken = 0;
                let mut m = Some(first);
                while let Some(msg) = m.take() {
                    taken += 1;
                    if matches!(msg, Msg::Relayout) { relayout = true } else { u.handle(msg) }
                    m = if taken < PUMP_BATCH { rx.try_recv().ok() } else { None };
                }
                if relayout { u.set_expanded_size(); }
                if taken >= PUMP_BATCH { glib::timeout_future(Duration::from_millis(1)).await; }
            }
        });
    }

    media_thread(tx.clone());
    weather_thread(tx.clone());
    desktop::start_watcher(tx.clone());
    settings::watch_config(tx.clone());
    let (usage_act, usage_rx) = std::sync::mpsc::channel::<bool>();
    let (pos_act, pos_rx) = std::sync::mpsc::channel::<bool>();
    *ui.workers.borrow_mut() = vec![usage_act, pos_act];
    usage_thread(tx.clone(), usage_rx);
    position_thread(tx.clone(), pos_rx);
    clipboard_thread(tx.clone());
    popups::Popups::start_fifo(tx);
    ui.popups.warm_up();
    refresh_clip(&ui);

    // keep the app (and its state) alive for the process lifetime
    win.present();
    ui.set_expanded_size();
    ui.apply(0.0);
    ui.schedule_art_prune();
    // Invisible warm-up: one open/close cycle at startup so the first real hover is as smooth as the rest.
    {
        let (u, w) = (ui.clone(), win.clone());
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            w.set_opacity(0.0);
            u.set_expanded(true);
            let (u2, w2) = (u.clone(), w.clone());
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                u2.set_expanded(false);
                let w3 = w2.clone();
                glib::timeout_add_local_once(Duration::from_millis(600), move || w3.set_opacity(1.0));
            });
        });
    }
    std::mem::forget(ui);
}

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

impl Ui {
    fn tab_allowed(&self, name: &str) -> bool {
        matches!(name, "home" | "controls" | "settings") || self.settings.borrow().tabs.iter().any(|t| t == name)
    }

    /// Tab buttons follow the normalized `tabs` order; hidden ones disappear.
    fn apply_tabs(&self) {
        let order = self.settings.borrow().tabs.clone();
        let btns = self.tab_btns.borrow();
        let mut prev: Option<gtk::Button> = None;
        for name in &order {
            if let Some((b, _)) = btns.iter().find(|(_, n)| n == name) {
                self.tabs.reorder_child_after(b, prev.as_ref());
                b.set_visible(true);
                prev = Some(b.clone());
            }
        }
        for (b, n) in btns.iter() { if !order.iter().any(|o| o == n) { b.set_visible(false); } }
        drop(btns);
        if let Some(cur) = self.stack.visible_child_name() {
            if !self.tab_allowed(&cur) && matches!(cur.as_str(), "media" | "calendar" | "clipboard" | "ai") { self.show_tab("home"); }
        }
    }

    /// Visible utilities in the chosen order, Settings always last.
    fn place_tiles(&self) {
        while let Some(c) = self.cgrid.first_child() { self.cgrid.remove(&c); }
        let util = self.settings.borrow().utilities.clone();
        let mut i = 0;
        for id in &util {
            if let Some((_, b)) = self.tiles.iter().find(|(t, _)| t == id) {
                self.cgrid.attach(b, i % 3, i / 3, 1, 1);
                i += 1;
            }
        }
        self.cgrid.attach(&self.tile_settings, i % 3, i / 3, 1, 1);
    }

    /// Idempotent parts of the preferences that live in widgets.
    fn sync_settings(self: &Rc<Self>) {
        let s = self.settings.borrow().clone();
        let anim = s.behavior.animations;
        self.osd.set_animations(anim);
        self.notes.set_animations(anim);
        if anim { self.win.remove_css_class("no-anim") } else { self.win.add_css_class("no-anim") }
        let tt = if anim { gtk::StackTransitionType::Crossfade } else { gtk::StackTransitionType::None };
        self.stack.set_transition_type(tt);
        self.coll_stack.set_transition_type(tt);
        if s.behavior.home_reset_secs == 0 { if let Some(id) = self.reset_id.borrow_mut().take() { id.remove(); } }
        self.apply_tabs();
        self.place_tiles();
    }

    fn apply_settings(self: &Rc<Self>, new: config::NotchSettings) {
        let old = self.settings.replace(new.clone());
        if old == new { return; }
        theme::configure(&new.appearance);
        reload_main_css();
        self.sync_settings();
        if old.appearance != new.appearance || old.layout != new.layout {
            self.relayout();
            // fonts re-measure after the stylesheet has been applied to the widgets
            let u = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(120), move || u.relayout());
        }
        if !new.behavior.animations { self.snap(); }
        if old.monitor != new.monitor || old.behavior.hide_on_fullscreen != new.behavior.hide_on_fullscreen
            || old.behavior.clock_format != new.behavior.clock_format || old.appearance.font_family != new.appearance.font_family {
            self.reconcile();
        }
    }

    /// Clears the in-memory history, the history file and the Wayland selection; copies still queued from before the
    /// clear are dropped until the system clipboard has been cleared.
    fn clear_clipboard(self: &Rc<Self>) {
        self.clearing.set(true);
        set_control_status(&self.ctl_status, &self.ctl_status_timer, "", None);
        self.hist.borrow_mut().clear();
        let disk_cleared = match save_history_at(&hist_path(), &[]) {
            Ok(()) => true,
            Err(e) => { eprintln!("clipboard history: {e}"); false }
        };
        refresh_clip(self);
        let tx = self.tx.clone();
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
            let _ = tx.try_send(Msg::ClipCleared { message, success });
        });
    }

    fn lock_screen(self: &Rc<Self>) {
        self.popups.close();
        self.cancel_collapse();
        self.set_expanded(false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match Command::new("omarchy").args(["system", "lock"]).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => { let _ = tx.try_send(Msg::UtilResult(format!("Lock failed: {}", String::from_utf8_lossy(&o.stderr).trim()))); }
                Err(e) => { let _ = tx.try_send(Msg::UtilResult(format!("Lock failed: {e}"))); }
            }
        });
    }

    /// Drop any Controls status text and its pending expiry (leaving Controls, collapsing, forced idle).
    fn set_capture_tiles_busy(&self, busy: bool) {
        for b in [&self.btn_ss, &self.btn_rec] {
            if busy { b.add_css_class("ctl-pending") } else { b.remove_css_class("ctl-pending") }
            b.set_sensitive(!busy);
        }
    }

    /// Accept one capture action: collapse the whole notch at once, wait for two collapsed frames to be presented,
    /// then run the Omarchy helper off the GTK thread. Only one helper (or picker) at a time.
    fn start_capture(self: &Rc<Self>, action: CaptureAction) {
        if self.capture_pending.get() || self.capture_active.get() { return; }
        self.capture_pending.set(true);
        self.capture_hover_blocked.set(true);
        if let Some(id) = self.reset_id.borrow_mut().take() { id.remove(); }
        self.btn_rec.remove_css_class("ctl-fail");
        self.set_capture_tiles_busy(true);
        self.force_idle();
        self.notch.queue_draw();
        if !self.notch.is_mapped() {
            self.finish_capture(action, Err("the notch is not visible".into()));
            return;
        }
        // `waiting` is cleared by whichever of {two frames, safety timeout} comes first
        let waiting = Rc::new(Cell::new(true));
        let tick_id: Rc<RefCell<Option<gtk::TickCallbackId>>> = Rc::new(RefCell::new(None));
        let frames = Cell::new(0u32);
        let (u, w) = (self.clone(), waiting.clone());
        let id = self.notch.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            if frames.get() < 2 { return glib::ControlFlow::Continue; }
            w.set(false);
            let u = u.clone();
            glib::idle_add_local_once(move || u.run_capture(action));
            glib::ControlFlow::Break
        });
        *tick_id.borrow_mut() = Some(id);
        let u = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            if !waiting.replace(false) { return; }
            if let Some(id) = tick_id.borrow_mut().take() { id.remove(); }
            u.finish_capture(action, Err("the collapsed notch was never presented".into()));
        });
    }

    fn run_capture(self: &Rc<Self>, action: CaptureAction) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // stdout must not be piped: a background recorder inherits it and would hold the pipe open
            let r = match Command::new("omarchy").args(action.args()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit()).status() {
                Ok(s) if s.success() => Ok(()),
                Ok(s) => Err(s.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.try_send(Msg::CaptureFinished(action, r));
        });
    }

    fn finish_capture(self: &Rc<Self>, action: CaptureAction, res: Result<(), String>) {
        self.capture_pending.set(false);
        self.set_capture_tiles_busy(false);
        (self.rec_update)();
        if let Err(e) = res {
            let tile = if action == CaptureAction::Screenshot { &self.btn_ss } else { &self.btn_rec };
            flash_tile(tile, "ctl-fail", 700);
            set_control_status(&self.ctl_status, &self.ctl_status_timer, &format!("{}: {e}", action.failure()), None);
        }
    }

    fn clear_control_status(&self) {
        set_control_status(&self.ctl_status, &self.ctl_status_timer, "", None);
    }

    /// Open the Wi-Fi or Bluetooth page inside the notch. Only navigates and refreshes; never changes radio state.
    fn open_device(self: &Rc<Self>, kind: &'static str, origin: &'static str) {
        let populate = match kind { "wifi" => self.populate_wifi.clone(), "bluetooth" => self.populate_bt.clone(), _ => return };
        self.cancel_collapse();
        self.detail_origin.set(origin);
        self.stack.set_visible_child_name(kind);
        populate();
        self.set_expanded(true);
    }

    /// Page currently expanded as a bar-routable detail view ("wifi", "bluetooth", "volume", "display", "battery").
    fn open_detail_kind(&self) -> Option<&'static str> {
        if !self.expanded.load(Ordering::Relaxed) { return None; }
        match self.stack.visible_child_name().as_deref() {
            Some("wifi") => Some("wifi"),
            Some("bluetooth") => Some("bluetooth"),
            Some("utility") => self.utility_kind.get(),
            _ => None,
        }
    }

    /// Open Audio, Displays or Battery inside the expanded notch (the shared controls live in `popups`).
    /// `origin` is the tab that Back / Escape return to.
    fn open_utility(self: &Rc<Self>, kind: &'static str, origin: &'static str) {
        let title = match kind { "volume" => "Audio", "display" => "Displays", "battery" => "Battery & Power", _ => return };
        self.detail_origin.set(origin);
        self.cancel_collapse();
        self.utility_kind.set(Some(kind));
        self.utility_title.set_text(title);
        self.stack.set_visible_child_name("utility");
        self.set_expanded(true);
        self.sync_utility();
    }
}

thread_local! { static MAIN_PROV: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) }; }

/// Nerd Font glyphs overflow their advance width, so GTK's advance-centered label sits off-center.
/// Shift the label by its measured ink-vs-logical offset so the visible glyph is centered.
fn center_ink(l: &gtk::Label) {
    let (ink, logical) = l.layout().extents();
    let off = (((ink.x() + ink.width() / 2) - (logical.x() + logical.width() / 2)) as f64 / gtk::pango::SCALE as f64).round() as i32;
    l.set_margin_start(if off < 0 { -2 * off } else { 0 });
    l.set_margin_end(if off > 0 { 2 * off } else { 0 });
}
/// Notch stylesheet: its own rules plus the shared control rules scoped to the utility page.
fn main_css() -> String {
    theme::notch_css(&format!("{}\n{}", include_str!("style.css"), popups::embedded_css()))
}

fn reload_main_css() {
    MAIN_PROV.with(|p| if let Some(p) = p.borrow().as_ref() { p.load_from_string(&main_css()); });
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut relay_pid = None;
    match args.first().map(String::as_str) {
        Some("--gen-waybar-theme") => {
            let family = config::read_document().map(|d| config::NotchSettings::from_json(&d).appearance.font_family);
            return match family.and_then(|f| theme::write_waybar_theme(f.as_deref())) {
                Ok(()) => glib::ExitCode::SUCCESS,
                Err(e) => { eprintln!("{e}"); glib::ExitCode::FAILURE }
            };
        }
        Some("--waybar-supervisor") if args.len() == 1 => return bar_controller::run_supervisor(),
        Some("--notification-relay") => {
            let pid = args.get(1).filter(|_| args.len() == 2).and_then(|p| p.parse::<i32>().ok()).filter(|p| *p > 0);
            let Some(pid) = pid else {
                eprintln!("usage: waybar-notch --notification-relay <shell-pid>");
                return glib::ExitCode::FAILURE;
            };
            relay_pid = Some(pid);
        }
        Some("--waybar-app-status") => {
            return match args.get(1).filter(|_| args.len() == 2).map(|id| app_status::read_status(id)) {
                Some(Ok(line)) => { print!("{line}"); glib::ExitCode::SUCCESS }
                Some(Err(e)) => { eprintln!("waybar-app-status: {e}"); glib::ExitCode::FAILURE }
                None => { eprintln!("usage: waybar-notch --waybar-app-status <app-id>"); glib::ExitCode::FAILURE }
            };
        }
        Some("--google-login") => {
            let print_url = args.iter().any(|a| a == "--print-url");
            let open = move |u: &str| {
                if print_url {
                    println!("AUTH_URL {u}");
                    let _ = std::io::stdout().flush();
                } else {
                    if let Ok(c) = Command::new("xdg-open").arg(u).stdout(Stdio::null()).stderr(Stdio::null()).spawn() { reap_detached(c); }
                }
            };
            return match gcal::authorize(&open) {
                Ok(()) => glib::ExitCode::SUCCESS,
                Err(e) if e == gcal::ERR_NO_CONFIG => { eprintln!("{e}"); glib::ExitCode::from(2) }
                Err(e) => { eprintln!("{e}"); glib::ExitCode::FAILURE }
            };
        }
        _ => {}
    }
    let Some(shell_pid) = relay_pid else {
        eprintln!("usage: waybar-notch --notification-relay <shell-pid>");
        return glib::ExitCode::FAILURE;
    };
    theme::configure(&config::load_settings().0.appearance);
    let app = gtk::Application::builder().application_id("dev.waybar.notch").build();
    app.connect_startup(|_| {
        let prov = gtk::CssProvider::new();
        prov.load_from_string(&main_css());
        if let Some(d) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&d, &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        MAIN_PROV.with(|p| *p.borrow_mut() = Some(prov));
    });
    app.connect_activate(move |a| build(a, shell_pid));
    // the private relay flag must not reach GTK's own argument parser
    app.run_with_args(&[std::env::args().next().unwrap_or_default()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weather_parsing() {
        let w = parse_weather_status("", "Example City  ·  Temp +25°C  ·  Wind ↑5km/h").unwrap();
        assert_eq!((w.compact.as_str(), w.location.as_str(), w.wind.as_str()), (" 25°C", "Example City", "Wind ↑5km/h"));
        let w = parse_weather_status("i", "Éxample São  ·  Temp -3°C  ·  Wind ↓12km/h").unwrap();
        assert_eq!((w.compact.as_str(), w.location.as_str(), w.wind.as_str()), ("i -3°C", "Éxample São", "Wind ↓12km/h"));
        for bad in ["Weather unavailable", "A · Temp 1°C", "A · Temp  · Wind 1", " · Temp 1 · Wind 2", "A · T 1 · Wind 2", "A · Temp 1 · W 2", "A · Temp 1 · Wind 2 · x"] {
            assert!(parse_weather_status("i", bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn weather_snapshot_retains_previous() {
        let ok = "Example City  ·  Temp +25°C  ·  Wind ↑5km/h";
        let s = weather_snapshot("a", Some(ok), None);
        let r = weather_snapshot("b", None, Some(s.clone()));
        assert_eq!((r.compact.as_str(), r.location.as_str(), r.wind.as_str()), ("a 25°C", "Example City", "Wind ↑5km/h"));
        let r = weather_snapshot("b", Some("garbage"), Some(s));
        assert_eq!(r.location, "Example City");
        let f = weather_snapshot("c", None, None);
        assert_eq!((f.compact.as_str(), f.location.as_str(), f.wind.as_str()), ("c", "Weather unavailable", ""));
        let n = weather_snapshot("d", Some(ok), Some(f));
        assert_eq!(n.compact, "d 25°C");
    }

    #[test]
    fn fit_extent_bounds() {
        assert_eq!(fit_extent(120.0, 80.0, 400.0), 120.0);
        assert_eq!(fit_extent(40.0, 80.0, 400.0), 80.0);
        assert_eq!(fit_extent(900.0, 80.0, 400.0), 400.0);
        assert_eq!(fit_extent(120.0, 80.0, 50.0), 50.0);
    }

    fn rows(n: usize) -> gtk::Box {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for _ in 0..n { let l = gtk::Label::new(Some("row")); l.set_height_request(20); b.append(&l); }
        b
    }

    /// Needs a display: `cargo test -- --ignored` under a (nested) compositor.
    #[test]
    #[ignore]
    fn scroller_page_fits_content_and_shrinks_again() {
        gtk::init().expect("display");
        let content = rows(1);
        let sw = gtk::ScrolledWindow::new();
        sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        sw.set_child(Some(&content));
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let footer = gtk::Label::new(Some("footer"));
        page.append(&sw);
        page.append(&footer);
        let height = |cap: i32| {
            prepare_page_measure(page.upcast_ref(), 400, cap);
            page.measure(gtk::Orientation::Vertical, 400).1
        };
        let small = height(300);
        assert!(small < 80, "{small}");
        for _ in 0..29 { content.append(&gtk::Label::new(Some("x"))); }
        let big = height(300);
        assert!(big > small && big <= 300 + footer.measure(gtk::Orientation::Vertical, 400).1, "{big}");
        while let Some(c) = content.last_child() { if content.first_child() == Some(c.clone()) { break; } content.remove(&c); }
        assert_eq!(height(300), small);
    }

    const DNS_ERR: &str = "error sending request for url (https://oauth2.googleapis.com/token): error trying to connect: dns error: failed to lookup address information: Temporary failure in name resolution";

    /// Wrap `content` in the production-style scroller and allocate it directly at `w` x min(natural, `cap`)
    /// (no toplevel, so a compositor cannot resize it). Returns (scroller, natural height).
    fn show_scrolled(content: &gtk::Box, w: i32, cap: i32) -> (gtk::ScrolledWindow, i32) {
        if let Some(vp) = content.parent().and_then(|p| p.downcast::<gtk::Viewport>().ok()) { vp.set_child(gtk::Widget::NONE); }
        let sw = gtk::ScrolledWindow::new();
        sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        sw.set_child(Some(content));
        prepare_page_measure(sw.upcast_ref(), w, cap);
        let nat = page_natural_height(sw.upcast_ref(), w);
        sw.allocate(w, nat.min(cap), -1, None);
        (sw, nat)
    }

    fn wrapped_label(text: &str) -> gtk::Label {
        let l = gtk::Label::new(Some(text));
        l.set_xalign(0.0);
        l.set_hexpand(true);
        l.set_width_chars(1);
        l.set_max_width_chars(1);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l
    }

    /// Needs a display. Wrapped error / lyric text is measured at the real viewport width and stays reachable.
    #[test]
    #[ignore]
    fn wrapped_text_fits_scrolls_and_shrinks() {
        gtk::init().expect("display");
        for w in [360, 520] {
            // agenda error
            let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let err = wrapped_label(DNS_ERR);
            col.append(&err);
            let (sw, nat) = show_scrolled(&col, w, 400);
            assert!(err.layout().line_count() > 1, "w{w}");
            assert!(!err.layout().is_ellipsized());
            let (_, lh) = err.layout().pixel_size();
            assert!(nat >= lh, "w{w}: nat {nat} < text {lh}");
            let adj = sw.vadjustment();
            assert!(adj.upper() - adj.page_size() <= 0.5, "fits below cap, w{w}");

            // swap to short text: fitted height shrinks
            err.set_text("Nothing planned");
            let (_, nat_short) = show_scrolled(&col, w, 400);
            assert!(nat_short < nat, "w{w}: {nat_short} !< {nat}");

            // long lyrics beyond the cap scroll and the last line is reachable
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
            let cur = lyric_label("lyric-cur", gtk::Align::End);
            let nxt = lyric_label("lyric-next", gtk::Align::Start);
            let long = "word ".repeat(400);
            cur.set_text(&long);
            nxt.set_text("Supercalifragilisticexpialidocious_and_a_very_long_unbroken_word_that_cannot_break_naturally\nsecond line");
            box_.append(&cur);
            box_.append(&nxt);
            let (sw, nat) = show_scrolled(&box_, w, 200);
            assert!(nat > 200, "w{w}: {nat}");
            for l in [&cur, &nxt] {
                assert!(l.layout().line_count() > 1);
                assert!(!l.layout().is_ellipsized());
                assert!(l.width() <= w, "label {} wider than {w}", l.width());
            }
            let adj = sw.vadjustment();
            assert!(adj.upper() - adj.page_size() > 0.0);
            adj.set_value(adj.upper() - adj.page_size());
            sw.allocate(w, sw.height(), -1, None);
            let b = nxt.compute_bounds(&sw).expect("bounds");
            assert!(b.y() + b.height() <= sw.height() as f32 + 1.0, "last line below viewport: y{} h{} sw{} val{} upper{} page{}", b.y(), b.height(), sw.height(), adj.value(), adj.upper(), adj.page_size());
            cur.set_text("short");
            nxt.set_text("short");
            let (_, nat2) = show_scrolled(&box_, w, 200);
            assert!(nat2 < nat);
        }
    }

    /// Needs a display. The Home foreground must be allocated exactly the card minus its margins,
    /// wrap long metadata, and shrink back for short text.
    #[test]
    #[ignore]
    fn home_player_foreground_stays_inside_card() {
        gtk::init().expect("display");
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&main_css());
        gtk::style_context_add_provider_for_display(&gtk::gdk::Display::default().unwrap(), &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        let long = "A very long unbroken-ish song title that must wrap inside the card ".repeat(4);
        for w in [312, 472] {
            let p = home_player_card();
            let settle = |p: &HomePlayer| -> i32 {
                let h = p.root.measure(gtk::Orientation::Vertical, w).1;
                p.root.allocate(w, h, -1, None);
                h
            };
            p.title.set_text("Short");
            p.artist.set_text("Artist");
            let short = settle(&p);
            p.title.set_text(&long);
            p.artist.set_text(&long);
            let tall = settle(&p);
            assert!(tall > short, "w{w}: {tall} !> {short}");
            for l in [&p.title, &p.artist] { assert!(!l.layout().is_ellipsized()); assert!(l.layout().line_count() > 1); }
            assert_eq!(p.root.width(), w);
            assert_eq!((p.strip.width(), p.strip.height()), (w - 16, tall - 16), "w{w}");
            let card = gtk::graphene::Rect::new(8.0, 8.0, (w - 16) as f32, (tall - 16) as f32);
            for wd in [p.title.upcast_ref::<gtk::Widget>(), p.artist.upcast_ref(), p.play.upcast_ref()] {
                let b = wd.compute_bounds(&p.root).expect("bounds");
                assert!(b.width() > 0.0 && b.height() > 0.0);
                assert!(b.x() >= card.x() - 1.0 && b.y() >= card.y() - 1.0, "w{w}: {b:?}");
                assert!(b.x() + b.width() <= card.x() + card.width() + 1.0 && b.y() + b.height() <= card.y() + card.height() + 1.0, "w{w}: {b:?}");
            }
            p.title.set_text("Short");
            p.artist.set_text("Artist");
            assert_eq!(settle(&p), short, "w{w}");
            assert_eq!(p.root.width(), w);
        }
    }

    #[test]
    fn week_strip_is_the_sunday_first_week() {
        use chrono::NaiveDate;
        let d = |y, m, dd| NaiveDate::from_ymd_opt(y, m, dd).unwrap();
        assert_eq!(week_days(d(2025, 6, 1))[0], d(2025, 6, 1)); // Sunday starts its own week
        assert_eq!(week_days(d(2025, 6, 4)), [d(2025, 6, 1), d(2025, 6, 2), d(2025, 6, 3), d(2025, 6, 4), d(2025, 6, 5), d(2025, 6, 6), d(2025, 6, 7)]);
        let w = week_days(d(2025, 12, 31)); // crosses the year boundary
        assert_eq!((w[0], w[3], w[4], w[6]), (d(2025, 12, 28), d(2025, 12, 31), d(2026, 1, 1), d(2026, 1, 3)));
    }

    #[test]
    fn calendar_failure_retries_quickly_and_success_waits() {
        assert_eq!(gcal_poll_delay(&gcal::Update::Error(DNS_ERR.into())), Duration::from_secs(30));
        assert_eq!(gcal_poll_delay(&gcal::Update::NeedAuth), Duration::from_secs(30));
        assert_eq!(gcal_poll_delay(&gcal::Update::Events(vec![])), Duration::from_secs(600));
    }

    #[test]
    fn fft_finds_the_sine_bin() {
        let n = 1024;
        let k = 40; // bin index of a 1.7 kHz tone at 44.1 kHz
        let mut re: Vec<f32> = (0..n).map(|i| (2.0 * std::f32::consts::PI * k as f32 * i as f32 / n as f32).sin()).collect();
        let mut im = vec![0.0f32; n];
        super::fft(&mut re, &mut im);
        let peak = (1..n / 2).max_by(|a, b| (re[*a].hypot(im[*a])).total_cmp(&re[*b].hypot(im[*b]))).unwrap();
        assert_eq!(peak, k);
    }

    #[test]
    fn spectrum_reuse_does_not_leak_previous_frame() {
        let tone = |hz: f32| -> Vec<u8> {
            (0..1024).flat_map(|i| (((2.0 * std::f32::consts::PI * hz * i as f32 / 44100.0).sin() * 12000.0) as i16).to_le_bytes()).collect()
        };
        let (a, b) = (tone(200.0), tone(3000.0));
        let mut reused = super::Spectrum::new();
        reused.raw.copy_from_slice(&a);
        reused.analyze();
        reused.raw.copy_from_slice(&b);
        let second = reused.analyze();
        let mut fresh = super::Spectrum::new();
        fresh.raw.copy_from_slice(&b);
        assert_eq!(second, fresh.analyze());
    }

    use super::{bt_audio_action, bt_audio_status, bt_card_text, bt_join_outputs, next_media_presence, wifi_card_text, BtDev, BtSnap, MediaPresence::*, MediaPresenceEvent as E, WifiNet, WifiSnap};

    #[test]
    fn idle_timeout_never_hides_an_available_player() {
        assert_eq!(next_media_presence(Present, E::IdleTimeout), Present);
    }

    #[test]
    fn missing_player_skeletons_then_media_restores() {
        let s = next_media_presence(Missing, E::IdleTimeout);
        assert_eq!(s, Skeleton);
        assert_eq!(next_media_presence(s, E::Media), Present);
    }

    #[test]
    fn stale_timeout_after_media_returns_is_harmless() {
        let s = next_media_presence(Present, E::NoMedia);
        let s = next_media_presence(s, E::Media);
        assert_eq!(next_media_presence(s, E::IdleTimeout), Present);
    }

    #[test]
    fn repeated_absence_keeps_skeleton() {
        assert_eq!(next_media_presence(Skeleton, E::NoMedia), Skeleton);
        assert_eq!(next_media_presence(Missing, E::NoMedia), Missing);
    }

    use super::{lyric_pair, player_window_address};
    use crate::lyrics::Lyrics;
    use serde_json::json;

    #[test]
    fn synced_pair_follows_timestamps_and_keeps_blank_lines() {
        let l = Lyrics::Synced(vec![(1.0, "first".into()), (3.0, "".into()), (5.0, "last".into())]);
        assert_eq!(lyric_pair(&l, 0.0), ("", "first"));
        assert_eq!(lyric_pair(&l, 1.0), ("first", ""));
        assert_eq!(lyric_pair(&l, 3.0), ("", "last"));
        assert_eq!(lyric_pair(&l, 5.0), ("last", ""));
        assert_eq!(lyric_pair(&l, 100.0), ("last", ""));
        assert_eq!(lyric_pair(&l, 1.0), ("first", ""));
    }

    #[test]
    fn plain_pair_is_static_and_short_inputs_pad_blank() {
        let l = Lyrics::Plain(vec!["one".into(), "two".into(), "three".into()]);
        assert_eq!(lyric_pair(&l, 0.0), ("one", "two"));
        assert_eq!(lyric_pair(&l, 99.0), ("one", "two"));
        assert_eq!(lyric_pair(&Lyrics::Plain(vec!["solo".into()]), 5.0), ("solo", ""));
        assert_eq!(lyric_pair(&Lyrics::Plain(vec![]), 5.0), ("", ""));
        assert_eq!(lyric_pair(&Lyrics::Synced(vec![]), 5.0), ("", ""));
        assert_eq!(lyric_pair(&Lyrics::None, 5.0), ("", ""));
    }

    #[test]
    fn window_match_prefers_unique_pid_and_rejects_ambiguity() {
        let c = json!([
            {"address": "0xa1", "mapped": true, "pid": 10, "class": "x", "initialClass": "x"},
            {"address": "0xb2", "mapped": true, "pid": 20, "class": "Spotify", "initialClass": "spotify"},
            {"address": "0xc3", "mapped": false, "pid": 30, "class": "x", "initialClass": "x"},
        ]);
        assert_eq!(player_window_address(&c, Some(10), Some("spotify")), Some("0xa1"));
        assert_eq!(player_window_address(&c, Some(99), Some("spotify")), Some("0xb2"));
        assert_eq!(player_window_address(&c, None, Some("SPOTIFY")), Some("0xb2"));
        assert_eq!(player_window_address(&c, Some(30), None), None);
        let dup = json!([
            {"address": "0xa1", "mapped": true, "pid": 10, "class": "app"},
            {"address": "0xa2", "mapped": true, "pid": 10, "class": "app"},
        ]);
        assert_eq!(player_window_address(&dup, Some(10), Some("app")), None);
        assert_eq!(player_window_address(&dup, None, Some("app")), None);
    }

    #[test]
    fn window_match_rejects_malformed_addresses() {
        for bad in ["a1", "0x", "0xzz", "0x1; exec", ""] {
            let c = json!([{"address": bad, "mapped": true, "pid": 1, "class": "app"}]);
            assert_eq!(player_window_address(&c, Some(1), None), None, "{bad}");
        }
    }

    fn wifi(on: bool, available: bool, active: bool) -> WifiSnap {
        WifiSnap { on, available, nets: vec![WifiNet { ssid: "Home".into(), signal: 71, secure: true, active, saved: true }] }
    }

    #[test]
    fn unavailable_wifi_differs_from_off_and_is_labelled() {
        let (off, gone) = (wifi(false, true, false), wifi(false, false, false));
        assert_ne!(off.signature(), gone.signature());
        assert_eq!(wifi_card_text(&off).0, "Wi-Fi is off");
        assert_eq!(wifi_card_text(&gone).0, "Wi-Fi unavailable");
        assert_eq!(wifi_card_text(&wifi(true, true, true)), ("Home".to_string(), "Signal 71%".to_string()));
        assert_eq!(wifi_card_text(&wifi(true, true, false)).0, "Not connected");
    }

    #[test]
    fn bluetooth_pairing_and_connection_rebuild_the_list() {
        let dev = |paired, connected| BtSnap { on: true, available: true, audio_available: true, devs: vec![bt("AA", "Buds", connected, paired)] };
        let (new, paired, conn) = (dev(false, false), dev(true, false), dev(true, true));
        assert_ne!(new.signature(), paired.signature());
        assert_ne!(paired.signature(), conn.signature());
        assert_eq!(bt_card_text(&conn).0, "1 device connected");
        assert_eq!(bt_card_text(&paired).0, "No devices connected");
        let gone = BtSnap { on: false, available: false, audio_available: false, devs: vec![] };
        let off = BtSnap { on: false, available: true, audio_available: false, devs: vec![] };
        assert_ne!(gone.signature(), off.signature());
        assert_eq!(bt_card_text(&gone).0, "Bluetooth unavailable");
    }

    fn bt(mac: &str, name: &str, connected: bool, paired: bool) -> BtDev {
        BtDev { mac: mac.into(), name: name.into(), connected, paired, icon: String::new(), audio: false, battery: None, sink: None, active: false, muted: None }
    }

    #[test]
    fn bluetooth_audio_state_is_honest() {
        let outs = serde_json::json!({"outputs": [
            {"name": "bluez_output.AA_AA_AA_AA_AA_AA.1", "bt_mac": "AA:AA:AA:AA:AA:AA", "default": false, "mute": false},
            {"name": "bluez_output.AA_AA_AA_AA_AA_AA.2", "bt_mac": "AA:AA:AA:AA:AA:AA", "default": true, "mute": true},
            {"name": "alsa", "bt_mac": null, "default": false, "mute": false}]});
        let mut buds = bt("AA:AA:AA:AA:AA:AA", "Buds", true, true);
        buds.audio = true;
        let mut t21 = bt("BB:BB:BB:BB:BB:BB", "Buds", true, false); // same alias, different MAC, no sink
        t21.audio = true;
        let mouse = bt("CC:CC:CC:CC:CC:CC", "Mouse", true, true);
        let mut devs = vec![buds, t21, mouse];
        bt_join_outputs(&mut devs, &outs);
        assert!(devs[0].active && devs[0].sink.as_deref() == Some("bluez_output.AA_AA_AA_AA_AA_AA.2"));
        assert_eq!(bt_audio_status(&devs[0], true).as_deref(), Some("Active audio output · Muted"));
        assert_eq!(bt_audio_action(&devs[0]), None);
        assert!(devs[1].sink.is_none() && !devs[1].active);
        assert_eq!(bt_audio_status(&devs[1], true).as_deref(), Some("Connected · Audio unavailable · Not paired"));
        assert_eq!(bt_audio_action(&devs[1]), Some("pair"));
        assert_eq!(bt_audio_status(&devs[1], false).as_deref(), Some("Connected · Audio service unavailable · Not paired"));
        assert_eq!(bt_audio_status(&devs[2], true), None);
        assert_eq!(bt_audio_action(&devs[2]), None);
        // sink exists but another output is default
        let mut inactive = bt("AA:AA:AA:AA:AA:AA", "Buds", true, true);
        bt_join_outputs(std::slice::from_mut(&mut inactive), &serde_json::json!({"outputs": [
            {"name": "x", "bt_mac": "AA:AA:AA:AA:AA:AA", "default": false, "mute": false}]}));
        assert_eq!(bt_audio_status(&inactive, true).as_deref(), Some("Connected · Audio available"));
        assert_eq!(bt_audio_action(&inactive), Some("use"));
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hist-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(p: &std::path::Path) -> u32 { std::fs::metadata(p).unwrap().mode() & 0o777 }

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
        let p = std::path::Path::new(&d).join("waybar-notch").join("clipboard.json");
        save_history_at(&p, &["synthetic".to_string()]).unwrap();
        use std::os::unix::fs::MetadataExt;
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
        cmd.args(["--exact", "history_tests::history_umask_child", "--nocapture"]).env("HIST_CHILD_DIR", &d);
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
mod art_worker_tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    const IMG: &[u8] = image_loader::TINY_PNG;

    fn agent(secs: u64) -> ureq::Agent { art_http::loopback_agent(Duration::from_secs(secs)) }

    #[derive(Default)]
    struct Shared { log: std::sync::Mutex<Vec<String>>, active: AtomicUsize, max_active: AtomicUsize, release: AtomicBool, stop: AtomicBool }

    type Handler = Arc<dyn Fn(&str, &mut TcpStream, &Shared) + Send + Sync>;

    struct Server { port: u16, st: Arc<Shared> }
    impl Drop for Server {
        fn drop(&mut self) {
            self.st.release.store(true, Ordering::SeqCst);
            self.st.stop.store(true, Ordering::SeqCst);
        }
    }

    fn serve(h: Handler) -> Server {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        let st = Arc::new(Shared::default());
        let st2 = st.clone();
        std::thread::spawn(move || {
            while !st2.stop.load(Ordering::SeqCst) {
                match TcpListener::accept(&l) {
                    Ok((mut s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let (h, st3) = (h.clone(), st2.clone());
                        std::thread::spawn(move || {
                            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                            let mut head = Vec::new();
                            let mut b = [0u8; 256];
                            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                                match s.read(&mut b) { Ok(n) if n > 0 => head.extend_from_slice(&b[..n]), _ => return }
                            }
                            let head = String::from_utf8_lossy(&head).to_string();
                            let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                            st3.log.lock().unwrap().push(path.clone());
                            let a = st3.active.fetch_add(1, Ordering::SeqCst) + 1;
                            st3.max_active.fetch_max(a, Ordering::SeqCst);
                            h(&path, &mut s, &st3);
                            st3.active.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Server { port, st }
    }

    fn ok(s: &mut TcpStream, body: &[u8]) {
        let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = s.write_all(body);
    }

    fn wait_release(st: &Shared) {
        let end = Instant::now() + Duration::from_secs(15);
        while !st.release.load(Ordering::SeqCst) && Instant::now() < end { std::thread::sleep(Duration::from_millis(10)); }
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("art-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn next_art(rx: &async_channel::Receiver<Msg>, within: Duration) -> Option<(String, String, image_loader::DecodedImage)> {
        let end = Instant::now() + within;
        while Instant::now() < end {
            match rx.try_recv() {
                Ok(Msg::Art { url, path, image }) => return Some((url, path.to_string_lossy().into_owned(), image)),
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        None
    }

    fn names(d: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect()
    }

    #[test]
    fn small_cover_is_downloaded_privately_and_delivered() {
        use std::os::unix::fs::MetadataExt;
        let srv = serve(Arc::new(|_, s, _| ok(s, IMG)));
        let d = tmpdir("small");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(5), agent(5));
        let url = format!("http://127.0.0.1:{}/c.png", srv.port);
        w.submit(url.clone());
        let (u, p, img) = next_art(&rx, Duration::from_secs(10)).expect("cover delivered");
        assert_eq!((img.width, img.height, img.alpha), (1, 1, true));
        assert_eq!(u, url);
        assert_eq!(std::fs::read(&p).unwrap(), IMG);
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        assert_eq!(names(&d).len(), 1, "{:?}", names(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn advertised_oversize_is_rejected_without_files_or_retries() {
        let srv = serve(Arc::new(|_, s, _| {
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", art_cache::MAX_ART_BYTES + 1);
        }));
        let d = tmpdir("advertised");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(5), agent(5));
        w.submit(format!("http://127.0.0.1:{}/big", srv.port));
        assert!(next_art(&rx, Duration::from_secs(3)).is_none());
        assert!(names(&d).is_empty(), "{:?}", names(&d));
        assert_eq!(srv.st.log.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chunked_oversize_is_cut_off_and_leaves_nothing() {
        let srv = serve(Arc::new(|_, s, _| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            let chunk = vec![1u8; 65536];
            for _ in 0..160 { // 10 MiB, more than the limit
                if write!(s, "{:x}\r\n", chunk.len()).is_err() || s.write_all(&chunk).is_err() || s.write_all(b"\r\n").is_err() { return; }
            }
            let _ = s.write_all(b"0\r\n\r\n");
        }));
        let d = tmpdir("chunked");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(10), agent(10));
        w.submit(format!("http://127.0.0.1:{}/chunked", srv.port));
        assert!(next_art(&rx, Duration::from_secs(4)).is_none());
        assert!(names(&d).is_empty(), "{:?}", names(&d));
        assert_eq!(srv.st.log.lock().unwrap().len(), 1, "a size violation must not be retried");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stalled_response_times_out_within_the_bound() {
        let srv = serve(Arc::new(|_, s, st| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nab");
            wait_release(st);
        }));
        let d = tmpdir("stall");
        let (_tx, rx) = async_channel::bounded::<String>(1);
        let start = Instant::now();
        let r = fetch_art_in(&agent(1), &d, &format!("http://127.0.0.1:{}/stall", srv.port), &rx, Duration::from_secs(1));
        let took = start.elapsed();
        assert!(r.is_none());
        assert!(took < Duration::from_secs(2), "took {took:?}");
        assert_eq!(srv.st.log.lock().unwrap().len(), 1, "the budget is shared by all attempts");
        assert!(names(&d).is_empty(), "{:?}", names(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn transient_failure_is_retried_within_the_total_budget() {
        let srv = serve(Arc::new(|_, s, st| {
            if st.log.lock().unwrap().len() == 1 {
                let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            } else {
                ok(s, IMG);
            }
        }));
        let d = tmpdir("retry");
        let (_tx, rx) = async_channel::bounded::<String>(1);
        let p = fetch_art_in(&agent(2), &d, &format!("http://127.0.0.1:{}/r", srv.port), &rx, Duration::from_secs(2)).expect("retry succeeds");
        assert_eq!(std::fs::read(&p).unwrap(), IMG);
        assert_eq!(srv.st.log.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn only_one_transfer_runs_and_only_the_latest_pending_url_follows() {
        let srv = serve(Arc::new(|path, s, st| {
            if path == "/0" { wait_release(st); }
            ok(s, IMG);
        }));
        let d = tmpdir("latest");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(30), agent(30));
        let url = |n: usize| format!("http://127.0.0.1:{}/{n}", srv.port);
        w.submit(url(0));
        let end = Instant::now() + Duration::from_secs(10);
        while srv.st.log.lock().unwrap().is_empty() && Instant::now() < end { std::thread::sleep(Duration::from_millis(10)); }
        for n in 1..=9 { w.submit(url(n)); }
        srv.st.release.store(true, Ordering::SeqCst);
        let (u, p, _) = next_art(&rx, Duration::from_secs(10)).expect("latest cover delivered");
        assert_eq!(u, url(9));
        assert!(std::path::Path::new(&p).exists());
        assert!(next_art(&rx, Duration::from_millis(500)).is_none(), "no stale cover may follow");
        assert_eq!(*srv.st.log.lock().unwrap(), vec!["/0".to_string(), "/9".to_string()]);
        assert_eq!(srv.st.max_active.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unsupported_scheme_is_not_downloaded() {
        let d = tmpdir("scheme");
        let (_tx, rx) = async_channel::bounded::<String>(1);
        assert!(fetch_art_in(&agent(1), &d, "ftp://127.0.0.1/x", &rx, Duration::from_secs(1)).is_none());
        assert!(names(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undecodable_cover_is_neither_delivered_nor_kept() {
        let srv = serve(Arc::new(|_, s, _| ok(s, b"\x89PNG synthetic cover bytes")));
        let d = tmpdir("undecodable");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(5), agent(5));
        w.submit(format!("http://127.0.0.1:{}/bad.png", srv.port));
        assert!(next_art(&rx, Duration::from_secs(2)).is_none());
        assert!(names(&d).is_empty(), "{:?}", names(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn production_agent_never_contacts_loopback_servers() {
        let srv = serve(Arc::new(|_, s, _| ok(s, IMG)));
        let d = tmpdir("policy");
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.clone(), Duration::from_secs(3), art_http::agent(Duration::from_secs(3)));
        w.submit(format!("http://127.0.0.1:{}/c.png", srv.port));
        assert!(next_art(&rx, Duration::from_secs(2)).is_none());
        assert!(srv.st.log.lock().unwrap().is_empty(), "the policy must refuse before any connection");
        assert!(names(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn local_cover_is_decoded_off_thread_and_special_files_are_skipped() {
        let d = tmpdir("local");
        let png = d.join("cover file.png");
        std::fs::write(&png, IMG).unwrap();
        let fifo = d.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = async_channel::unbounded();
        let w = ArtWorker::with(tx, d.join("cache"), Duration::from_secs(3), agent(3));
        w.submit(format!("file://{}", fifo.display()));
        assert!(next_art(&rx, Duration::from_millis(500)).is_none());
        w.submit(format!("file://{}", png.to_string_lossy().replace(' ', "%20")));
        let (u, p, img) = next_art(&rx, Duration::from_secs(5)).expect("local cover delivered");
        assert!(u.starts_with("file://"));
        assert_eq!(std::path::Path::new(&p), png.as_path());
        assert_eq!((img.width, img.height), (1, 1));
        let _ = std::fs::remove_dir_all(&d);
    }
}
