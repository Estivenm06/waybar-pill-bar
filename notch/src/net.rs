//! Wi-Fi / Bluetooth snapshots and actions. Everything slow (nmcli, bluetoothctl, NetworkManager D-Bus) runs on
//! worker threads; results land in `state.net` and the compact `state.status.wifi/bt` through the `Poster`.
//! Logic ported from the legacy GTK `main.rs`.

use crate::state::{BtStatus, ConnectedBtDevice, State, WifiStatus};
use crate::wayland::Poster;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WifiNet { pub ssid: String, pub signal: u8, pub secure: bool, pub active: bool, pub saved: bool }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WifiSnap { pub on: bool, pub available: bool, pub nets: Vec<WifiNet> }

impl WifiSnap {
    /// Layout-relevant fingerprint; signal is bucketed so jitter doesn't rebuild the list.
    pub fn signature(&self) -> String {
        let mut s = format!("{}{}|", self.on as u8, self.available as u8);
        for n in &self.nets {
            s.push_str(&format!("{}\x1f{}{}{}{}\x1e", n.ssid, n.active as u8, n.secure as u8, n.saved as u8, n.signal / 20));
        }
        s
    }

    /// Compact summary for the pill and Home chips.
    pub fn status(&self) -> WifiStatus {
        let active = self.nets.iter().find(|n| n.active);
        WifiStatus { available: self.available, on: self.on, ssid: active.map(|n| n.ssid.clone()), signal: active.map_or(0, |n| n.signal) }
    }

    /// Headline and secondary line of the Wi-Fi status card.
    pub fn card_text(&self) -> (String, String) {
        if !self.available { return ("Wi-Fi unavailable".into(), "NetworkManager is not responding".into()); }
        if !self.on { return ("Wi-Fi is off".into(), "Turn on to see networks".into()); }
        match self.nets.iter().find(|n| n.active) {
            Some(n) => (n.ssid.clone(), format!("Signal {}%", n.signal)),
            None => ("Not connected".into(), "Wi-Fi is on".into()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BtDev {
    pub mac: String, pub name: String, pub connected: bool, pub paired: bool,
    pub icon: String, pub audio: bool, pub battery: Option<u8>,
    /// Exact playback sink of this device (preferring the actual default), if PipeWire exposes one.
    pub sink: Option<String>,
    pub active: bool,
    pub muted: Option<bool>,
}

impl BtDev {
    /// One-line audio state of a connected audio device; `None` for non-audio or disconnected devices.
    pub fn audio_status(&self, audio_available: bool) -> Option<String> {
        if !self.connected || !self.audio { return None; }
        let mut s = if !audio_available { "Connected · Audio service unavailable".to_string() }
            else if self.active { if self.muted == Some(true) { "Active audio output · Muted".into() } else { "Active audio output".into() } }
            else if self.sink.is_some() { "Connected · Audio available".into() }
            else { "Connected · Audio unavailable".into() };
        if !self.paired { s.push_str(" · Not paired"); }
        Some(s)
    }

    /// Extra action for a connected audio device that is not the active output: `"use"` or `"pair"`.
    pub fn audio_action(&self) -> Option<&'static str> {
        (self.connected && self.audio && !self.active).then_some(if self.paired { "use" } else { "pair" })
    }

    /// Nerd Font glyph for the device class.
    pub fn glyph(&self) -> &'static str { bt_glyph(&self.icon) }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BtSnap { pub on: bool, pub available: bool, pub audio_available: bool, pub devs: Vec<BtDev> }

impl BtSnap {
    pub fn signature(&self) -> String {
        let mut s = format!("{}{}{}|", self.on as u8, self.available as u8, self.audio_available as u8);
        for d in &self.devs {
            s.push_str(&format!("{}\x1f{}\x1f{}{}{}{}\x1f{}\x1f{:?}\x1f{:?}\x1f{}{:?}\x1e", d.mac, d.name, d.connected as u8, d.paired as u8,
                d.audio as u8, d.active as u8, d.icon, d.battery, d.sink, d.muted.is_some() as u8, d.muted));
        }
        s
    }

    pub fn status(&self) -> BtStatus {
        BtStatus { available: self.available, on: self.on, connected: self.devs.iter().filter(|d| d.connected).map(|d| ConnectedBtDevice { mac: d.mac.clone(), name: d.name.clone(), battery: d.battery }).collect() }
    }

    /// Headline and secondary line of the Bluetooth status card.
    pub fn card_text(&self) -> (String, String) {
        if !self.available { return ("Bluetooth unavailable".into(), "No Bluetooth controller responding".into()); }
        if !self.on { return ("Bluetooth is off".into(), "Turn on to see devices".into()); }
        let n = self.devs.iter().filter(|d| d.connected).count();
        let audio: Vec<String> = self.devs.iter().filter(|d| d.connected && d.audio).map(|d| {
            let state = if !self.audio_available { "audio service unavailable" } else if d.active { "active" }
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetKind { Wifi, Bt }

/// Connect-state of the last Wi-Fi/Bluetooth action; the OSD and the Controls tiles react to these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetState { Disconnected, Connecting, Connected, Failed }

#[derive(Clone, Debug, PartialEq)]
pub struct NetEvent {
    pub kind: NetKind,
    pub state: NetState,
    /// SSID / device name / "Wi-Fi on" etc.
    pub name: String,
    /// Increments with every event, so consumers can tell a repeat from a stale one.
    pub seq: u64,
}

#[derive(Default)]
pub struct Net {
    pub wifi: WifiSnap,
    pub bt: BtSnap,
    /// One-line result of the last connect/power action, for the device pages.
    pub status_line: Option<String>,
    /// Set while a connect/pair/scan is running.
    pub busy: bool,
    /// Latest connect-state change (see `NetEvent`).
    pub event: Option<NetEvent>,
    ops: u32,
    seq: u64,
}

impl Net {
    fn emit(&mut self, kind: NetKind, state: NetState, name: &str) {
        self.seq += 1;
        self.event = Some(NetEvent { kind, state, name: name.to_string(), seq: self.seq });
    }
    fn begin(&mut self) { self.ops += 1; self.busy = true; }
    fn end(&mut self) { self.ops = self.ops.saturating_sub(1); self.busy = self.ops > 0; }
}

fn state_of(ok: bool, on: bool) -> NetState {
    match (ok, on) {
        (false, _) => NetState::Failed,
        (true, true) => NetState::Connected,
        (true, false) => NetState::Disconnected,
    }
}

pub(crate) fn sh_out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).stderr(Stdio::null()).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn spawn(f: impl FnOnce() + Send + 'static) { std::thread::spawn(f); }

// ── Wi-Fi ───────────────────────────────────────────────────────────────────

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

fn radio_state() -> (bool, bool) {
    let radio = sh_out("nmcli", &["radio", "wifi"]);
    (matches!(radio.as_str(), "enabled" | "disabled"), radio == "enabled")
}

/// Merge `nmcli -t -f ACTIVE,SSID,SIGNAL,SECURITY device wifi list` output: one row per SSID (the active
/// or strongest AP), active first, then by signal.
fn parse_wifi_list(out: &str, saved: &[Vec<u8>]) -> Vec<WifiNet> {
    let mut nets: Vec<WifiNet> = Vec::new();
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
    nets
}

pub fn wifi_snapshot(rescan: bool) -> WifiSnap {
    let (available, on) = radio_state();
    let mut nets = Vec::new();
    if on {
        // actual SSID bytes of saved Wi-Fi profiles; if NetworkManager cannot be queried, no Saved badges are shown
        let saved = crate::wifi::saved_ssids().unwrap_or_default();
        let out = sh_out("nmcli", &["-t", "-f", "ACTIVE,SSID,SIGNAL,SECURITY", "device", "wifi", "list",
            "--rescan", if rescan { "yes" } else { "no" }]);
        nets = parse_wifi_list(&out, &saved);
    }
    WifiSnap { on, available, nets }
}

/// Active network from a terse `ACTIVE,SSID,SIGNAL` listing.
fn parse_active(out: &str) -> Option<(String, u8)> {
    out.lines().find_map(|l| {
        let p = split_terse(l);
        (p.len() >= 3 && p[0] == "yes" && !p[1].is_empty()).then(|| (p[1].clone(), p[2].parse().unwrap_or(0)))
    })
}

/// Cheap status for the pill: radio + active network, no D-Bus and no rescan.
pub fn wifi_status() -> WifiStatus {
    let (available, on) = radio_state();
    if !on { return WifiStatus { available, on, ssid: None, signal: 0 }; }
    let active = parse_active(&sh_out("nmcli", &["-t", "-f", "ACTIVE,SSID,SIGNAL", "device", "wifi", "list", "--rescan", "no"]));
    WifiStatus { available, on, ssid: active.as_ref().map(|a| a.0.clone()), signal: active.map_or(0, |a| a.1) }
}

fn post_wifi(poster: &Poster, snap: WifiSnap) {
    poster.post(move |st: &mut State, _| {
        st.status.wifi = snap.status();
        st.net.wifi = snap;
    });
}

/// Post a cached snapshot immediately, then (optionally) a freshly rescanned one.
pub fn refresh_wifi(poster: &Poster, rescan: bool) {
    let poster = poster.clone();
    spawn(move || {
        post_wifi(&poster, wifi_snapshot(false));
        if rescan { post_wifi(&poster, wifi_snapshot(true)); }
    });
}

fn begin(poster: &Poster, kind: NetKind, connecting: Option<String>, line: Option<String>) {
    poster.post(move |st: &mut State, _| {
        st.net.begin();
        st.net.status_line = line;
        if let Some(name) = connecting { st.net.emit(kind, NetState::Connecting, &name); }
    });
}

fn conclude(poster: &Poster, kind: NetKind, state: NetState, name: String, line: Option<String>) {
    poster.post(move |st: &mut State, _| {
        st.net.end();
        st.net.status_line = line;
        st.net.emit(kind, state, &name);
    });
}

/// `password` is for a new secured network; it goes to NetworkManager over D-Bus only. A failed attempt removes just the
/// profile this attempt created, so the next try prompts again.
pub fn wifi_connect(poster: &Poster, ssid: &str, password: Option<String>, connect: bool) {
    let (poster, ssid) = (poster.clone(), ssid.to_string());
    spawn(move || {
        begin(&poster, NetKind::Wifi, connect.then(|| ssid.clone()), connect.then(|| format!("Connecting to {ssid}…")));
        let result = if !connect {
            crate::wifi::disconnect(&ssid)
        } else if let Some(pw) = &password {
            crate::wifi::connect_with_password(&ssid, pw)
        } else {
            crate::wifi::connect_saved(&ssid)
        };
        if let Err(e) = &result { eprintln!("wifi: {e}"); }
        let line = match (&result, connect) {
            (Err(e), _) => Some(e.clone()),
            (Ok(()), true) => Some(format!("Connected to {ssid}")),
            (Ok(()), false) => Some(format!("Disconnected from {ssid}")),
        };
        conclude(&poster, NetKind::Wifi, state_of(result.is_ok(), connect), ssid, line);
        post_wifi(&poster, wifi_snapshot(false));
    });
}

pub fn wifi_radio(poster: &Poster, on: bool) {
    let poster = poster.clone();
    spawn(move || {
        let name = if on { "Wi-Fi on" } else { "Wi-Fi off" };
        begin(&poster, NetKind::Wifi, on.then(|| name.to_string()), None);
        let ok = Command::new("nmcli").args(["radio", "wifi", if on { "on" } else { "off" }])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        conclude(&poster, NetKind::Wifi, state_of(ok, on), name.to_string(), (!ok).then(|| format!("Could not turn {}", name.to_lowercase())));
        post_wifi(&poster, wifi_snapshot(false));
        if ok && on {
            // scan results trickle in after the radio comes up
            std::thread::sleep(Duration::from_millis(1500));
            post_wifi(&poster, wifi_snapshot(true));
            std::thread::sleep(Duration::from_millis(3000));
            post_wifi(&poster, wifi_snapshot(true));
        }
    });
}

// ── Bluetooth ───────────────────────────────────────────────────────────────

fn bt_list(args: &[&str]) -> Vec<(String, String)> {
    parse_bt_devices(&sh_out("bluetoothctl", args))
}

fn parse_bt_devices(out: &str) -> Vec<(String, String)> {
    out.lines().filter_map(|l| {
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
            r.battery = v.find('(').and_then(|a| v[a + 1..].split(')').next()).and_then(|n| n.trim().parse::<u8>().ok()).filter(|b| *b <= 100);
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

pub fn bt_glyph(icon: &str) -> &'static str {
    if icon.contains("headset") || icon.contains("headphone") { "\u{f025}" }
    else if icon.contains("speaker") { "\u{f028}" }
    else { "\u{f293}" }
}

pub fn bt_snapshot() -> BtSnap {
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
        Some(BtDev { mac, name, connected, paired, ..BtDev::default() })
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
        let outputs = crate::audio::gather_audio_outputs();
        audio_available = outputs["available"].as_bool().unwrap_or(false);
        bt_join_outputs(&mut devs, &outputs);
    }
    BtSnap { on, available, audio_available, devs }
}

/// Status for the pill: controller power + connected devices with their live battery
/// (one `bluetoothctl info` per connected device only; paired-but-disconnected devices are never queried).
pub fn bt_status() -> BtStatus {
    let show = sh_out("bluetoothctl", &["show"]);
    let on = show.contains("Powered: yes");
    let available = on || show.contains("Powered: no");
    let connected = if on {
        bt_list(&["devices", "Connected"]).into_iter()
            .map(|(mac, name)| { let battery = bt_parse_info(&bt_info(&mac)).battery; ConnectedBtDevice { mac, name, battery } })
            .collect()
    } else { Vec::new() };
    BtStatus { available, on, connected }
}

/// Apply a pill-status update: the compact status, and the battery of matching connected detail rows
/// (rows no longer connected lose theirs so a stale percentage never survives a disconnect).
pub fn apply_bt_status(st: &mut State, b: BtStatus) {
    for d in st.net.bt.devs.iter_mut() {
        d.battery = if d.connected { b.connected.iter().find(|c| c.mac.eq_ignore_ascii_case(&d.mac)).map_or(d.battery, |c| c.battery) } else { None };
    }
    st.status.bt = b;
}

fn post_bt(poster: &Poster, snap: BtSnap) {
    poster.post(move |st: &mut State, _| {
        st.status.bt = snap.status();
        st.net.bt = snap;
    });
}

/// Post a fresh Bluetooth snapshot.
pub fn refresh_bt(poster: &Poster) {
    let poster = poster.clone();
    spawn(move || post_bt(&poster, bt_snapshot()));
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

/// Desktop notification through the same `notify-send` the legacy code used.
pub fn bt_notify(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    spawn(move || {
        match Command::new("/usr/bin/notify-send").args(["--app-name=Waybar", "--", &title, &body]).stdin(Stdio::null()).status() {
            Ok(s) if s.success() => {}
            r => eprintln!("bluetooth: notify-send failed ({r:?}): {body}"),
        }
    });
}

fn bt_sink_for(mac: &str) -> Result<Option<String>, String> {
    let out = crate::audio::gather_audio_outputs();
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
    let deadline = Instant::now() + Duration::from_secs(10);
    while sink.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if !bt_connected(mac) { return Err("Device disconnected before audio became available".into()); }
        sink = bt_sink_for(mac)?;
    }
    crate::audio::select_audio_output(&sink.ok_or_else(|| NO_SINK_MSG.to_string())?)
}

/// Terminal outcome of any connect/disconnect/use-audio: notification, state event, fresh snapshot.
fn bt_conclude(poster: &Poster, name: &str, connected: bool, r: Result<bool, String>) {
    let (state, line) = match &r {
        Ok(is_audio) => {
            if *is_audio { bt_notify("Bluetooth audio", &format!("Using {name} for audio")); }
            let line = if *is_audio { format!("Using {name} for audio") } else if connected { format!("Connected to {name}") } else { format!("Disconnected from {name}") };
            (state_of(true, connected), Some(line))
        }
        Err(e) => {
            eprintln!("bluetooth {name}: {e}");
            bt_notify(if e == NO_SINK_MSG { "Bluetooth audio" } else { "Bluetooth" }, e);
            (NetState::Failed, Some(e.clone()))
        }
    };
    conclude(poster, NetKind::Bt, state, name.to_string(), line);
    post_bt(poster, bt_snapshot());
}

/// Connect (pairing first when needed, then selecting the device's audio output) or disconnect.
/// Audio success requires a real, selected sink, not just `Connected: yes`.
pub fn bt_connect(poster: &Poster, mac: &str, name: &str, connect: bool) {
    let (poster, mac, name) = (poster.clone(), mac.to_string(), name.to_string());
    spawn(move || {
        begin(&poster, NetKind::Bt, connect.then(|| name.clone()), connect.then(|| format!("Connecting to {name}…")));
        if !connect {
            bt_run(&["disconnect", &mac], 8);
            let r = if bt_connected(&mac) { Err("Disconnect failed".to_string()) } else { Ok(false) };
            bt_conclude(&poster, &name, false, r);
            return;
        }
        let r = bt_link_up(&mac).and_then(|()| if bt_is_audio(&mac) { bt_route_audio(&mac).map(|()| true) } else { Ok(false) });
        bt_conclude(&poster, &name, true, r);
    });
}

/// Select the audio output of an already connected, paired device without reconnecting or re-pairing.
pub fn bt_use_audio(poster: &Poster, mac: &str, name: &str) {
    let (poster, mac, name) = (poster.clone(), mac.to_string(), name.to_string());
    spawn(move || {
        begin(&poster, NetKind::Bt, Some(name.clone()), Some(format!("Connecting to {name}…")));
        let r = if bt_connected(&mac) { bt_route_audio(&mac).map(|()| true) } else { Err("Device is not connected".to_string()) };
        bt_conclude(&poster, &name, true, r);
    });
}

pub fn bt_power(poster: &Poster, on: bool) {
    let poster = poster.clone();
    spawn(move || {
        let name = if on { "Bluetooth on" } else { "Bluetooth off" };
        begin(&poster, NetKind::Bt, on.then(|| name.to_string()), None);
        let _ = Command::new("bluetoothctl").args(["power", if on { "on" } else { "off" }])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        let ok = sh_out("bluetoothctl", &["show"]).contains("Powered: yes") == on;
        conclude(&poster, NetKind::Bt, state_of(ok, on), name.to_string(), (!ok).then(|| format!("Could not turn {}", name.to_lowercase())));
        post_bt(&poster, bt_snapshot());
        if ok && on {
            std::thread::sleep(Duration::from_millis(1500));
            post_bt(&poster, bt_snapshot());
            std::thread::sleep(Duration::from_millis(3000));
            post_bt(&poster, bt_snapshot());
        }
    });
}

/// Scan for ~6 s, posting a fresh snapshot as devices show up. `busy` stays set meanwhile.
pub fn bt_scan(poster: &Poster) {
    let poster = poster.clone();
    spawn(move || {
        let first = bt_snapshot();
        let on = first.on;
        post_bt(&poster, first);
        if !on { return; }
        begin(&poster, NetKind::Bt, None, Some("Scanning…".into()));
        let child = Command::new("bluetoothctl").args(["--timeout", "6", "scan", "on"])
            .stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(1500));
            post_bt(&poster, bt_snapshot());
        }
        if let Ok(mut c) = child { let _ = c.wait(); }
        poster.post(|st: &mut State, _| {
            st.net.end();
            st.net.status_line = None;
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi(on: bool, available: bool, active: bool) -> WifiSnap {
        WifiSnap { on, available, nets: vec![WifiNet { ssid: "Home".into(), signal: 71, secure: true, active, saved: true }] }
    }

    #[test]
    fn unavailable_wifi_differs_from_off_and_is_labelled() {
        let (off, gone) = (wifi(false, true, false), wifi(false, false, false));
        assert_ne!(off.signature(), gone.signature());
        assert_eq!(off.card_text().0, "Wi-Fi is off");
        assert_eq!(gone.card_text().0, "Wi-Fi unavailable");
        assert_eq!(wifi(true, true, true).card_text(), ("Home".to_string(), "Signal 71%".to_string()));
        assert_eq!(wifi(true, true, false).card_text().0, "Not connected");
    }

    #[test]
    fn bluetooth_pairing_and_connection_rebuild_the_list() {
        let dev = |paired, connected| BtSnap { on: true, available: true, audio_available: true, devs: vec![bt("AA", "Buds", connected, paired)] };
        let (new, paired, conn) = (dev(false, false), dev(true, false), dev(true, true));
        assert_ne!(new.signature(), paired.signature());
        assert_ne!(paired.signature(), conn.signature());
        assert_eq!(conn.card_text().0, "1 device connected");
        assert_eq!(paired.card_text().0, "No devices connected");
        let gone = BtSnap { on: false, available: false, audio_available: false, devs: vec![] };
        let off = BtSnap { on: false, available: true, audio_available: false, devs: vec![] };
        assert_ne!(gone.signature(), off.signature());
        assert_eq!(gone.card_text().0, "Bluetooth unavailable");
    }

    fn bt(mac: &str, name: &str, connected: bool, paired: bool) -> BtDev {
        BtDev { mac: mac.into(), name: name.into(), connected, paired, ..BtDev::default() }
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
        assert_eq!(devs[0].audio_status(true).as_deref(), Some("Active audio output · Muted"));
        assert_eq!(devs[0].audio_action(), None);
        assert!(devs[1].sink.is_none() && !devs[1].active);
        assert_eq!(devs[1].audio_status(true).as_deref(), Some("Connected · Audio unavailable · Not paired"));
        assert_eq!(devs[1].audio_action(), Some("pair"));
        assert_eq!(devs[1].audio_status(false).as_deref(), Some("Connected · Audio service unavailable · Not paired"));
        assert_eq!(devs[2].audio_status(true), None);
        assert_eq!(devs[2].audio_action(), None);
        // sink exists but another output is default
        let mut inactive = bt("AA:AA:AA:AA:AA:AA", "Buds", true, true);
        bt_join_outputs(std::slice::from_mut(&mut inactive), &serde_json::json!({"outputs": [
            {"name": "x", "bt_mac": "AA:AA:AA:AA:AA:AA", "default": false, "mute": false}]}));
        assert_eq!(inactive.audio_status(true).as_deref(), Some("Connected · Audio available"));
        assert_eq!(inactive.audio_action(), Some("use"));
    }

    #[test]
    fn terse_lines_split_on_unescaped_colons() {
        assert_eq!(split_terse(r"yes:Home\: 5G:71:WPA2"), ["yes", "Home: 5G", "71", "WPA2"]);
        assert_eq!(split_terse(r"no:a\\b::"), ["no", r"a\b", "", ""]);
    }

    #[test]
    fn wifi_list_merges_duplicates_and_sorts_active_first() {
        let out = "no:Mesh:40:WPA2\nno:Mesh:80:WPA2\nno::90:\nyes:Home:55:WPA3\nno:Cafe:70:\nbad line\n";
        let nets = parse_wifi_list(out, &[b"Cafe".to_vec()]);
        let names: Vec<_> = nets.iter().map(|n| (n.ssid.as_str(), n.signal, n.secure, n.saved)).collect();
        assert_eq!(names, [("Home", 55, true, false), ("Mesh", 80, true, false), ("Cafe", 70, false, true)]);
        assert!(nets[0].active);
        assert_eq!(parse_active(out), Some(("Home".to_string(), 55)));
        assert_eq!(parse_active("no:Mesh:40\n"), None);
    }

    #[test]
    fn snapshots_summarise_into_status() {
        let w = wifi(true, true, true).status();
        assert_eq!((w.on, w.ssid.as_deref(), w.signal), (true, Some("Home"), 71));
        assert_eq!(wifi(true, true, false).status().ssid, None);
        let s = BtSnap { on: true, available: true, audio_available: false, devs: vec![bt("A", "Buds", true, true), bt("B", "Mouse", false, true)] };
        assert_eq!(s.status().connected, [ConnectedBtDevice { mac: "A".into(), name: "Buds".into(), battery: None }]);
        assert_eq!(bt_parse_info("Battery Percentage: 0x00 (0)").battery, Some(0));
        assert_eq!(bt_parse_info("Battery Percentage: 0x64 (100)").battery, Some(100));
        assert_eq!(bt_parse_info("Battery Percentage: 0x65 (101)").battery, None);
        assert_eq!(bt_parse_info("Battery Percentage: junk").battery, None);
        let mut st = State::new();
        st.net.bt = BtSnap { on: true, available: true, audio_available: false, devs: vec![BtDev { mac: "A".into(), connected: true, battery: Some(10), ..Default::default() }, BtDev { mac: "B".into(), connected: false, battery: Some(5), ..Default::default() }] };
        apply_bt_status(&mut st, BtStatus { available: true, on: true, connected: vec![ConnectedBtDevice { mac: "A".into(), name: "x".into(), battery: Some(55) }] });
        assert_eq!((st.net.bt.devs[0].battery, st.net.bt.devs[1].battery), (Some(55), None));
    }

    #[test]
    fn bluetoothctl_parsing() {
        let devs = parse_bt_devices("Device AA:BB:CC:DD:EE:FF Sony WH-1000XM5\nDevice 11:22:33:44:55:66\nAgent registered\n");
        assert_eq!(devs, [("AA:BB:CC:DD:EE:FF".to_string(), "Sony WH-1000XM5".to_string()), ("11:22:33:44:55:66".to_string(), String::new())]);
        let info = bt_parse_info("\tIcon: audio-headset\n\tUUID: Audio Sink       (0000110b-0000-1000-8000-00805f9b34fb)\n\tBattery Percentage: 0x57 (87)\n");
        assert_eq!((info.icon.as_str(), info.audio_uuid, info.sink_uuid, info.battery), ("audio-headset", true, true, Some(87)));
        assert_eq!(bt_glyph("audio-headphones"), "\u{f025}");
        assert_eq!(bt_glyph("input-mouse"), "\u{f293}");
        assert_eq!(bt_diag("\x1b[0;91mFailed to pair: org.bluez.Error.AuthenticationFailed\x1b[0m\r\n[bluetooth]# "), "Failed to pair: org.bluez.Error.AuthenticationFailed");
        assert_eq!(bt_diag("Failed to connect: x\nTrailing"), "Failed to connect: x");
        assert_eq!(bt_diag("  \n"), "no response from bluetoothctl");
    }

    #[test]
    fn busy_tracks_overlapping_actions() {
        let mut n = Net::default();
        n.begin();
        n.begin();
        n.end();
        assert!(n.busy);
        n.end();
        assert!(!n.busy);
        n.emit(NetKind::Wifi, NetState::Failed, "x");
        n.emit(NetKind::Bt, NetState::Connected, "y");
        assert_eq!(n.event.as_ref().map(|e| (e.kind, e.seq)), Some((NetKind::Bt, 2)));
    }
}
