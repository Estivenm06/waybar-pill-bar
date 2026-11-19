//! Media player state and workers: playerctl follow, position polling, cover pipeline, lyrics, the audio
//! spectrum for the equaliser and "raise the player window".
//!
//! `Media` is the UI-thread slice (`State::media`). Everything blocking runs on worker threads that mutate it
//! through the `Poster` handed to `start`. Entry points: `start`, `set_active`, `set_visualizer`, `play_pause`,
//! `next`, `prev`, `skip`, `seek_to`, `raise_player`.

use crate::art_cache;
use crate::art_http;
use crate::image_loader;
use crate::lyrics;
use crate::render::Image;
use crate::wayland::Poster;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::time::Duration;

/// Equaliser band count.
pub const BARS: usize = 6;
/// Resting level of an idle equaliser bar.
pub const BARS_IDLE: f32 = 0.12;
/// Field layout shared by the snapshot and the follower: status, artist, title, art URL, length (µs), instance.
const FORMAT: &str = "{{status}}\t{{artist}}\t{{title}}\t{{mpris:artUrl}}\t{{mpris:length}}\t{{playerInstance}}";
/// Safety net for events playerctl does not report (a player vanishing, a late start).
const SNAPSHOT_EVERY: Duration = Duration::from_secs(3);
/// Instance every transport command and the position poll target; empty = any player.
static TARGET: Mutex<String> = Mutex::new(String::new());
/// Seconds with no player before the media UI turns into a skeleton.
pub const SKELETON_SECS: u64 = 8;
/// Longest edge of a decoded cover.
pub const ART_MAX_EDGE: i32 = 384;
/// How long `Media::seeking` stays set after `seek_to`, so the progress bar does not jump back.
const SEEK_HOLD: Duration = Duration::from_millis(600);


/// Lyrics lifecycle for the current track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LyricsState { #[default] Idle, Loading, Ready, NotFound }

/// Media slice. Read-only for views; mutated by this module's workers.
#[derive(Default)]
pub struct Media {
    pub presence: MediaPresence,
    /// "Playing" / "Paused" / "Stopped" as reported by playerctl; empty without a player.
    pub status: String,
    pub playing: bool,
    pub artist: String,
    pub title: String,
    /// MPRIS player instance (e.g. `spotify`), empty without a player.
    pub player: String,
    /// Track length, seconds (0 when unknown).
    pub length: f64,
    /// Last polled position, seconds. Polled only while `set_active(true)`.
    pub position: f64,
    /// Cover, decoded and downscaled to `ART_MAX_EDGE`.
    pub art: Option<Image>,
    /// URL the player announced for the cover (empty = none).
    pub art_url: String,
    /// Local file backing `art`.
    pub art_path: Option<PathBuf>,
    /// Equaliser levels 0..1 (idle level when the visualizer is off).
    pub bars: [f32; BARS],
    pub lyrics: lyrics::Lyrics,
    pub lyrics_state: LyricsState,
    /// Current and next lyric rows for `position` (empty strings when none).
    pub lyric_current: String,
    pub lyric_next: String,
    /// True right after the user sought; views keep their own progress value meanwhile.
    pub seeking: bool,
    /// Set when play/pause flipped (`playing`, title); the OSD consumer takes it.
    pub playback_event: Option<(bool, String)>,
    /// A raise-window request is in flight.
    pub raise_busy: bool,
    /// Why the last raise failed.
    pub raise_error: Option<String>,
    /// Every MPRIS player currently known, in the order they are cycled through.
    pub players: Vec<PlayerInfo>,
    /// Player the user picked with the source chip; used while it is still running.
    pinned: Option<String>,
    last_tracks: Vec<Track>,
    lyr_key: String,
    last_playing: Option<bool>,
    player_gen: u64,
    skel_armed: bool,
    skel_gen: u64,
    seek_gen: u64,
}

/// One running MPRIS player (Spotify, a browser tab, mpv, VLC, ...).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInfo { pub instance: String, pub status: String, pub title: String }

/// "chromium.instance618584" -> "Chromium", "org.mozilla.firefox" -> "Firefox", "spotify" -> "Spotify".
pub fn player_label(instance: &str) -> String {
    let base = instance.split(".instance").next().unwrap_or(instance);
    let name = base.rsplit('.').next().unwrap_or(base);
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => "Player".into(),
    }
}

/// Which player to show: the user's pick, else the one that is playing (staying on the current one while it still
/// plays, so two players never flip-flop), else the current/any paused one.
fn pick_player(tracks: &[Track], pinned: Option<&str>, current: &str) -> Option<usize> {
    let find = |n: &str| tracks.iter().position(|t| t.player == n);
    if let Some(i) = pinned.and_then(find) { return Some(i); }
    let playing = |t: &Track| t.status == "Playing";
    if let Some(i) = find(current).filter(|&i| playing(&tracks[i])) { return Some(i); }
    if let Some(i) = tracks.iter().position(playing) { return Some(i); }
    find(current).or_else(|| tracks.iter().position(|t| !t.title.is_empty())).or(if tracks.is_empty() { None } else { Some(0) })
}

impl Media {
    /// Apply a fresh list of every player: pick the one to show, or fall back to "no media".
    fn on_snapshot(&mut self, tracks: Vec<Track>) {
        self.players = tracks.iter().map(|t| PlayerInfo { instance: t.player.clone(), status: t.status.clone(), title: t.title.clone() }).collect();
        if self.pinned.as_deref().is_some_and(|p| !tracks.iter().any(|t| t.player == p)) { self.pinned = None; }
        let chosen = pick_player(&tracks, self.pinned.as_deref(), &self.player);
        self.last_tracks = tracks;
        match chosen {
            Some(i) => { let t = self.last_tracks[i].clone(); self.on_media(t); }
            None => self.on_no_media(),
        }
    }

    /// Source chip click: show the next running player and keep following it.
    pub fn cycle_player(&mut self) {
        if self.players.len() < 2 { return; }
        let i = self.players.iter().position(|p| p.instance == self.player).map_or(0, |i| i + 1) % self.players.len();
        self.pinned = Some(self.players[i].instance.clone());
        let tracks = std::mem::take(&mut self.last_tracks);
        self.on_snapshot(tracks);
    }

    /// Whether the player's window can be raised (valid MPRIS name).
    pub fn can_raise(&self) -> bool { player_service_name(&self.player).is_some() }

    /// Progress 0..1, 0 when the length is unknown.
    pub fn fraction(&self) -> f64 {
        if self.length > 0.0 { (self.position / self.length).clamp(0.0, 1.0) } else { 0.0 }
    }

    fn refresh_lyric_lines(&mut self) {
        let (c, n) = lyric_pair(&self.lyrics, self.position);
        if self.lyric_current != c { self.lyric_current = c.to_string(); }
        if self.lyric_next != n { self.lyric_next = n.to_string(); }
    }

    fn clear_lyrics(&mut self) {
        self.lyr_key.clear();
        self.lyrics = lyrics::Lyrics::None;
        self.lyrics_state = LyricsState::Idle;
        self.lyric_current.clear();
        self.lyric_next.clear();
    }

    fn clear_art(&mut self) {
        self.art = None;
        self.art_path = None;
    }

    fn on_no_media(&mut self) {
        self.last_playing = None;
        self.presence = next_media_presence(self.presence, MediaPresenceEvent::NoMedia);
        self.status.clear();
        self.playing = false;
        self.title.clear();
        self.artist.clear();
        self.length = 0.0;
        self.position = 0.0;
        self.clear_art();
        self.art_url.clear();
        self.clear_lyrics();
        self.set_player("");
        if self.presence == MediaPresence::Missing && !self.skel_armed { self.arm_skeleton(); }
    }

    fn set_player(&mut self, instance: &str) {
        if self.player != instance {
            self.player = instance.to_string();
            *TARGET.lock().unwrap_or_else(|e| e.into_inner()) = instance.to_string();
            self.last_playing = None;
            self.player_gen += 1;
            self.raise_busy = false;
            self.raise_error = None;
        }
    }

    fn arm_skeleton(&mut self) {
        let Some(ctl) = CTL.get() else { return };
        self.skel_armed = true;
        self.skel_gen += 1;
        let gen_ = self.skel_gen;
        let poster = ctl.poster.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(SKELETON_SECS));
            poster.post(move |st, _| {
                let m = &mut st.media;
                if m.skel_gen != gen_ || !m.skel_armed { return; }
                m.skel_armed = false;
                m.presence = next_media_presence(m.presence, MediaPresenceEvent::IdleTimeout);
            });
        });
    }

    fn on_media(&mut self, track: Track) {
        self.presence = next_media_presence(self.presence, MediaPresenceEvent::Media);
        self.skel_armed = false;
        self.skel_gen += 1;
        self.set_player(&track.player);
        self.length = track.len;
        let key = format!("{}\u{1f}{}", track.artist, track.title);
        if self.lyr_key != key {
            self.lyr_key = key.clone();
            self.lyrics = lyrics::Lyrics::None;
            self.lyrics_state = LyricsState::Loading;
            self.lyric_current.clear();
            self.lyric_next.clear();
            if let Some(ctl) = CTL.get() {
                let (poster, a, t, len) = (ctl.poster.clone(), track.artist.clone(), track.title.clone(), track.len);
                std::thread::spawn(move || {
                    let l = lyrics::fetch(&a, &t, len);
                    poster.post(move |st, _| st.media.on_lyrics(key, l));
                });
            }
        }
        self.artist = track.artist;
        let playing = track.status == "Playing";
        self.status = track.status;
        self.playing = playing;
        if self.last_playing.replace(playing).is_some_and(|o| o != playing) {
            self.playback_event = Some((playing, track.title.clone()));
        }
        self.title = track.title;
        if self.art_url != track.art_url {
            self.art_url = track.art_url.clone();
            self.clear_art();
            if let Some(ctl) = CTL.get() { ctl.art.submit(track.art_url); }
        }
    }

    fn on_lyrics(&mut self, key: String, l: lyrics::Lyrics) {
        if self.lyr_key != key { return; }   // lyrics for a track that already changed
        self.lyrics_state = if matches!(l, lyrics::Lyrics::None) { LyricsState::NotFound } else { LyricsState::Ready };
        self.lyrics = l;
        self.refresh_lyric_lines();
    }

    fn on_art(&mut self, url: String, path: PathBuf, image: image_loader::DecodedImage) {
        if self.art_url != url { return; }   // stale cover from a previous track
        let Some(img) = image.into_image() else { return };
        if let Some(ctl) = CTL.get() { ctl.art.accept(&path); }
        self.art = Some(img);
        self.art_path = Some(path);
    }

    fn on_position(&mut self, p: f64) {
        self.position = p;
        self.refresh_lyric_lines();
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Track { status: String, artist: String, title: String, art_url: String, len: f64, player: String }

// ------------------------------------------------------------------ shared control block

struct Ctl {
    poster: Poster,
    art: ArtWorker,
    /// Wakes/parks the position poller.
    active: Mutex<mpsc::Sender<bool>>,
    viz: Mutex<Option<Child>>,
}

static CTL: OnceLock<Ctl> = OnceLock::new();
static VIZ_GEN: AtomicU64 = AtomicU64::new(0);

/// Starts the playerctl follower, the (dormant) position poller and the cover worker. Call once.
pub fn start(poster: &Poster) {
    let (atx, arx) = mpsc::channel();
    let p = poster.clone();
    let art = ArtWorker::new(Box::new(move |url, path, image| {
        p.post(move |st, _| st.media.on_art(url, path, image));
    }));
    if CTL.set(Ctl { poster: poster.clone(), art, active: Mutex::new(atx), viz: Mutex::new(None) }).is_err() { return; }
    media_thread(poster.clone());
    position_thread(poster.clone(), arx);
}

/// Expanded notch: poll the playback position every second (immediately on activation). Collapsed: dormant.
pub fn set_active(active: bool) {
    if let Some(c) = CTL.get() { let _ = c.active.lock().unwrap_or_else(|e| e.into_inner()).send(active); }
}

fn sh_out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).stderr(Stdio::null()).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn spawn_quiet(cmd: &str, args: &[&str]) {
    if let Ok(mut c) = Command::new(cmd).args(args).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        std::thread::spawn(move || { let _ = c.wait(); });
    }
}

fn player_cmd(args: &[&str]) {
    let target = TARGET.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut a = vec!["-p", if target.is_empty() { "%any" } else { target.as_str() }];
    a.extend_from_slice(args);
    spawn_quiet("playerctl", &a);
}

pub fn play_pause() { player_cmd(&["play-pause"]); }
pub fn next() { player_cmd(&["next"]); }
pub fn prev() { player_cmd(&["previous"]); }
/// Relative seek by whole seconds (negative = back), e.g. the +-30 s buttons.
pub fn skip(secs: i32) {
    let arg = format!("{}{}", secs.abs(), if secs < 0 { '-' } else { '+' });
    player_cmd(&["position", &arg]);
}

/// Jumps to `secs`, holds `Media::seeking` for a moment and refreshes the position.
pub fn seek_to(poster: &Poster, secs: f64) {
    let secs = secs.max(0.0);
    let p = poster.clone();
    poster.post(move |st, _| {
        st.media.seeking = true;
        st.media.on_position(secs);
        st.media.seek_gen += 1;
        let gen_ = st.media.seek_gen;
        std::thread::spawn(move || {
            player_cmd(&["position", &format!("{secs:.2}")]);
            std::thread::sleep(SEEK_HOLD);
            p.post(move |st, _| { if st.media.seek_gen == gen_ { st.media.seeking = false; } });
        });
    });
}

/// Brings the player's window forward (MPRIS Raise + Hyprland focus). Result lands in `raise_error`/`raise_busy`.
pub fn raise_player(poster: &Poster) {
    let p = poster.clone();
    poster.post(move |st, _| {
        let m = &mut st.media;
        if m.raise_busy || !m.can_raise() { return; }
        m.raise_busy = true;
        m.raise_error = None;
        let (gen_, inst) = (m.player_gen, m.player.clone());
        std::thread::spawn(move || {
            let res = raise_player_app(&inst);
            p.post(move |st, _| {
                let m = &mut st.media;
                if m.player_gen != gen_ { return; }   // player changed or vanished while raising
                m.raise_busy = false;
                if let Err(e) = res { eprintln!("raise player {inst}: {e}"); m.raise_error = Some(e); }
            });
        });
    });
}

// ------------------------------------------------------------------ equaliser capture

/// Starts (`true`) or stops the equaliser capture. Typical policy: on while playing and the notch is collapsed.
pub fn set_visualizer(poster: &Poster, on: bool) {
    let Some(ctl) = CTL.get() else { return };
    let mut slot = ctl.viz.lock().unwrap_or_else(|e| e.into_inner());
    let gen_ = VIZ_GEN.fetch_add(1, Ordering::SeqCst) + 1;   // frames from the previous reader are now stale
    if let Some(mut c) = slot.take() { let _ = c.kill(); let _ = c.wait(); }
    if !on {
        poster.post(|st, _| st.media.bars = [BARS_IDLE; BARS]);
        return;
    }
    let spawned = Command::new("parec")
        .args(["-d", "@DEFAULT_SINK@.monitor", "--format=s16le", "--rate=44100", "--channels=1", "--latency-msec=30"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    match spawned {
        Ok(mut child) => {
            let out = child.stdout.take();
            *slot = Some(child);
            let p = poster.clone();
            std::thread::spawn(move || {
                if let Some(mut out) = out {
                    let mut sp = Spectrum::new();
                    while out.read_exact(&mut sp.raw).is_ok() {
                        if VIZ_GEN.load(Ordering::SeqCst) != gen_ { return; }
                        let bars = sp.frame();
                        p.post(move |st, _| { if VIZ_GEN.load(Ordering::SeqCst) == gen_ { st.media.bars = bars; } });
                    }
                }
                // capture ended by itself: settle to the idle level
                if VIZ_GEN.load(Ordering::SeqCst) == gen_ {
                    p.post(move |st, _| { if VIZ_GEN.load(Ordering::SeqCst) == gen_ { st.media.bars = [BARS_IDLE; BARS]; } });
                }
            });
        }
        Err(e) => {
            eprintln!("audio visualizer unavailable: {e}");
            poster.post(|st, _| st.media.bars = [BARS_IDLE; BARS]);
        }
    }
}


// ------------------------------------------------------------------ workers

fn parse_tracks(text: &str) -> Vec<Track> {
    text.lines().filter_map(|line| {
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 6 || p[0].is_empty() || p[5].is_empty() { return None; }
        Some(Track {
            status: p[0].into(), artist: p[1].into(), title: p[2].into(),
            art_url: p[3].into(), len: p[4].parse::<f64>().unwrap_or(0.0) / 1e6, player: p[5].into(),
        })
    }).collect()
}

/// Follows every MPRIS player. playerctl's follower only says "something changed"; the full picture is re-read from a
/// one-shot snapshot of all players, so the shown player can be chosen among them.
fn media_thread(poster: Poster) {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || loop {
        let child = Command::new("playerctl")
            .args(["-a", "metadata", "-F", "--format", "{{playerInstance}}"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                for _ in BufReader::new(out).lines().map_while(Result::ok) { if tx.send(()).is_err() { return; } }
            }
            let _ = child.wait();
        }
        if tx.send(()).is_err() { return; }
        std::thread::sleep(Duration::from_secs(1));
    });
    std::thread::spawn(move || {
        let mut last: Option<Vec<Track>> = None;
        loop {
            match rx.recv_timeout(SNAPSHOT_EVERY) {
                Ok(()) => { std::thread::sleep(Duration::from_millis(120)); while rx.try_recv().is_ok() {} }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            let tracks = parse_tracks(&sh_out("playerctl", &["-a", "metadata", "--format", FORMAT]));
            if last.as_ref() == Some(&tracks) { continue; }
            last = Some(tracks.clone());
            poster.post(move |st, _| st.media.on_snapshot(tracks));
        }
    });
}

/// Block until activated (`true`), then run `work` immediately and every `period` until deactivated.
/// Inactive means fully dormant: the thread waits on the channel instead of waking on a timer.
fn activity_worker(rx: mpsc::Receiver<bool>, period: Duration, mut work: impl FnMut() -> bool + Send + 'static) {
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
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

fn position_thread(poster: Poster, rx: mpsc::Receiver<bool>) {
    activity_worker(rx, Duration::from_millis(1000), move || {
        if let Ok(p) = sh_out("playerctl", &["-p", &{ let t = TARGET.lock().unwrap_or_else(|e| e.into_inner()).clone(); if t.is_empty() { "%any".to_string() } else { t } }, "position"]).parse::<f64>() {
            poster.post(move |st, _| st.media.on_position(p));
        }
        true
    });
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
}

/// Whether a player is available: the skeleton is only for a player that stays missing, never for Paused/Stopped ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaPresence {
    #[default] Missing, Present, Skeleton }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaPresenceEvent { Media, NoMedia, IdleTimeout }

pub fn next_media_presence(current: MediaPresence, event: MediaPresenceEvent) -> MediaPresence {
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



fn hypr_req(cmd: &str) -> Option<String> {
    let rt = runtime_dir();
    let try_sig = |sig: &str| UnixStream::connect(format!("{rt}/hypr/{sig}/.socket.sock")).ok();
    let mut s = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty()).and_then(|s| try_sig(&s)).or_else(|| {
        let mut ents: Vec<_> = std::fs::read_dir(format!("{rt}/hypr")).ok()?.flatten().collect();
        ents.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
        ents.iter().find_map(|e| try_sig(&e.file_name().to_string_lossy()))
    })?;
    s.set_read_timeout(Some(Duration::from_millis(800))).ok()?;
    s.set_write_timeout(Some(Duration::from_millis(800))).ok()?;
    s.write_all(cmd.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

/// Bring the player's app forward: MPRIS Raise (when offered) plus a precise Hyprland focus of its mapped window.
/// Blocking; call off the UI thread. Never launches anything.
fn raise_player_app(instance: &str) -> Result<(), String> {
    use std::collections::HashMap;
    use zbus::OwnedVariant;
    let name = player_service_name(instance).ok_or_else(|| format!("Invalid player name: {instance}"))?;
    let conn = zbus::block_on(zbus::Connection::session()).map_err(|e| e.to_string())?;
    const BUS: (&str, &str, &str) = ("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus");
    const MPRIS: (&str, &str) = ("/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2");
    let owner: String = zbus::block_on(conn.call_method(Some(BUS.0), BUS.1, Some(BUS.2), "GetNameOwner", &(name.as_str(),)))
        .and_then(|m| m.body().deserialize())
        .map_err(|e| format!("Player is not running: {e}"))?;
    let mut err: Option<String> = None;
    let (mut can_raise, mut entry) = (false, None::<String>);
    let props = zbus::block_on(conn.call_method(Some(owner.as_str()), MPRIS.0, Some("org.freedesktop.DBus.Properties"), "GetAll", &(MPRIS.1,)))
        .and_then(|m| m.body().deserialize::<HashMap<String, OwnedVariant>>());
    match props {
        Ok(d) => {
            can_raise = d.get("CanRaise").and_then(|v| bool::try_from(v).ok()).unwrap_or(false);
            entry = d.get("DesktopEntry").and_then(|v| String::try_from(v.try_clone().ok()?).ok());
        }
        Err(e) => err = Some(format!("Player did not answer: {e}")),
    }
    let mut done = false;
    if can_raise {
        match zbus::block_on(conn.call_method(Some(owner.as_str()), MPRIS.0, Some(MPRIS.1), "Raise", &())) {
            Ok(_) => done = true,
            Err(e) => err = Some(format!("Raise failed: {e}")),
        }
    }
    let pid: Option<u32> = zbus::block_on(conn.call_method(Some(BUS.0), BUS.1, Some(BUS.2), "GetConnectionUnixProcessID", &(owner.as_str(),)))
        .ok().and_then(|m| m.body().deserialize().ok());
    let clients = hypr_req("j/clients").and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    if let Some(addr) = clients.as_ref().and_then(|c| player_window_address(c, pid, entry.as_deref())) {
        match hypr_req(&format!("dispatch hl.dsp.focus({{ window = \"address:{addr}\" }})")) {
            Some(r) if r.trim() == "ok" => done = true,
            Some(r) => err = Some(format!("Compositor refused focus: {}", r.trim())),
            None => err = Some("Compositor focus request failed".into()),
        }
    }
    if done { Ok(()) } else { Err(err.unwrap_or_else(|| "This player cannot open its window".into())) }
}

/// Hyprland address (`0x` + hex) of the single mapped window that belongs to the player process or, failing a
/// PID match, to its desktop entry. Ambiguity yields None so the wrong window is never focused.
pub fn player_window_address<'a>(clients: &'a serde_json::Value, pid: Option<u32>, desktop_entry: Option<&str>) -> Option<&'a str> {
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

pub fn player_service_name(instance: &str) -> Option<String> {
    let name = format!("org.mpris.MediaPlayer2.{instance}");
    let ok = zbus::names::WellKnownName::try_from(name.as_str()).is_ok();
    ok.then_some(name)
}

/// (current, next) lyric rows for `pos`. Synced: before the first stamp the current row is blank; at the last
/// stamp next is blank. Plain lyrics have no timing, so they stay a static two-row excerpt.
pub fn lyric_pair(l: &lyrics::Lyrics, pos: f64) -> (&str, &str) {
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

pub fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
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

    fn frame(&mut self) -> [f32; BARS] {
        let v = self.analyze();
        let mut bars = [0.0f32; BARS];
        for k in 0..BARS {
            // ease both directions so the bars glide instead of flicker
            self.prev[k] += (v[k] - self.prev[k]) * if v[k] > self.prev[k] { 0.35 } else { 0.12 };
            bars[k] = self.prev[k].max(0.08);
        }
        bars
    }
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
fn fetch_art_in(agent: &ureq::Agent, dir: &std::path::Path, url: &str, pending: &Slot, timeout: Duration) -> Option<String> {
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


/// Bytes of an embedded cover (`data:image/...;base64,...`, what mpv, VLC and similar players announce); `None` for
/// anything else, a malformed payload or one above the cover size limit.
fn data_url_bytes(url: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let head = url.get(..5)?;
    if !head.eq_ignore_ascii_case("data:") { return None; }
    let (meta, payload) = url[5..].split_once(',')?;
    let meta = meta.to_ascii_lowercase();
    if !meta.starts_with("image/") || !meta.ends_with(";base64") { return None; }
    if payload.len() as u64 > art_cache::MAX_ART_BYTES * 4 / 3 + 4 { return None; }
    let bytes = base64::engine::general_purpose::STANDARD.decode(payload.trim()).ok()?;
    (!bytes.is_empty() && bytes.len() as u64 <= art_cache::MAX_ART_BYTES).then_some(bytes)
}

fn art_is_remote(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Latest-wins request slot: submitting replaces whatever still waits.
#[derive(Default)]
struct Slot { inner: Mutex<SlotInner>, cv: Condvar }
#[derive(Default)]
struct SlotInner { url: Option<String>, closed: bool }

impl Slot {
    fn put(&self, url: String) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).url = Some(url);
        self.cv.notify_all();
    }
    fn is_empty(&self) -> bool { self.inner.lock().unwrap_or_else(|e| e.into_inner()).url.is_none() }
    fn close(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.cv.notify_all();
    }
    /// Blocks for the next request; `None` once closed.
    fn take(&self) -> Option<String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if g.closed { return None; }
            if let Some(u) = g.url.take() { return Some(u); }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

type Deliver = Box<dyn Fn(String, PathBuf, image_loader::DecodedImage) + Send + Sync>;

/// One background thread downloads covers: one transfer at a time, and only the newest requested URL waits.
struct ArtWorker {
    slot: Arc<Slot>,
    dir: PathBuf,
    pins: Arc<Mutex<ArtPins>>,
}

impl Drop for ArtWorker {
    fn drop(&mut self) { self.slot.close(); }
}

impl ArtWorker {
    fn new(deliver: Deliver) -> Self {
        Self::with(deliver, PathBuf::from(runtime_dir()), ART_TIMEOUT, art_http::agent(ART_TIMEOUT))
    }

    fn with(deliver: Deliver, dir: PathBuf, timeout: Duration, agent: ureq::Agent) -> Self {
        let slot = Arc::new(Slot::default());
        let pins = Arc::new(Mutex::new(ArtPins::default()));
        let (wslot, wdir, wpins) = (slot.clone(), dir.clone(), pins.clone());
        let snapshot = move || wpins.lock().unwrap_or_else(|e| e.into_inner()).list();
        std::thread::spawn(move || {
            while let Some(url) = wslot.take() {
                if url.is_empty() { continue; }
                if url.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("data:")) {
                    // Embedded artwork: decoded into a private cache file, then loaded like any cover.
                    let Some(bytes) = data_url_bytes(&url) else { continue };
                    let path = art_cache::path_for(&wdir, &url);
                    if !art_cache::is_usable(&path) {
                        use std::io::Write;
                        use std::os::unix::fs::OpenOptionsExt;
                        let tmp = PathBuf::from(format!("{}.part.{}.{}", path.display(), std::process::id(), ART_SEQ.fetch_add(1, Ordering::Relaxed)));
                        let written = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)
                            .and_then(|mut f| f.write_all(&bytes)).and_then(|()| std::fs::rename(&tmp, &path));
                        if written.is_err() { let _ = std::fs::remove_file(&tmp); continue; }
                    }
                    let Ok(image) = image_loader::load_file(&path, ART_MAX_EDGE) else { let _ = std::fs::remove_file(&path); continue };
                    let mut pins = snapshot();
                    pins.push(path.clone());
                    art_cache::prune(&wdir, art_cache::KEEP, &pins);
                    if wslot.is_empty() { deliver(url, path, image); }
                    continue;
                }
                if !art_is_remote(&url) {
                    // Local artwork: validated and decoded here, never on the UI thread.
                    let Some(path) = image_loader::local_path(&url) else { continue };
                    let Ok(image) = image_loader::load_file(&path, ART_MAX_EDGE) else { continue };
                    if wslot.is_empty() { deliver(url, path, image); }
                    continue;
                }
                art_cache::prune(&wdir, art_cache::KEEP, &snapshot());
                let Some(path) = fetch_art_in(&agent, &wdir, &url, &wslot, timeout) else { continue };
                let path = PathBuf::from(path);
                let image = match image_loader::load_file(&path, ART_MAX_EDGE) {
                    Ok(image) => image,
                    Err(_) => { let _ = std::fs::remove_file(&path); continue; }   // undecodable cover: never cache or show it
                };
                let mut pins = snapshot();
                pins.push(path.clone());
                art_cache::prune(&wdir, art_cache::KEEP, &pins);
                if wslot.is_empty() { deliver(url, path, image); }
            }
        });
        ArtWorker { slot, dir, pins }
    }

    /// Replaces any waiting request with `url` (empty = no cover wanted); never starts a second transfer.
    fn submit(&self, url: String) {
        let current = (art_is_remote(&url) || url.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("data:"))).then(|| art_cache::path_for(&self.dir, &url));
        self.pins.lock().unwrap_or_else(|e| e.into_inner()).current = current;
        self.slot.put(url);
    }

    /// The cover now on screen stays pinned until the next one is accepted.
    fn accept(&self, path: &Path) {
        self.pins.lock().unwrap_or_else(|e| e.into_inner()).accepted = Some(path.to_path_buf());
    }
}


#[cfg(test)]
mod tests {
    use super::MediaPresence::*;
    use super::MediaPresenceEvent as E;
    use super::*;

    #[test]
    fn fft_finds_the_sine_bin() {
        let n = 1024;
        let k = 40; // bin index of a 1.7 kHz tone at 44.1 kHz
        let mut re: Vec<f32> = (0..n).map(|i| (2.0 * std::f32::consts::PI * k as f32 * i as f32 / n as f32).sin()).collect();
        let mut im = vec![0.0f32; n];
        fft(&mut re, &mut im);
        let peak = (1..n / 2).max_by(|a, b| (re[*a].hypot(im[*a])).total_cmp(&re[*b].hypot(im[*b]))).unwrap();
        assert_eq!(peak, k);
    }

    #[test]
    fn spectrum_reuse_does_not_leak_previous_frame() {
        let tone = |hz: f32| -> Vec<u8> {
            (0..1024).flat_map(|i| (((2.0 * std::f32::consts::PI * hz * i as f32 / 44100.0).sin() * 12000.0) as i16).to_le_bytes()).collect()
        };
        let (a, b) = (tone(200.0), tone(3000.0));
        let mut reused = Spectrum::new();
        reused.raw.copy_from_slice(&a);
        reused.analyze();
        reused.raw.copy_from_slice(&b);
        let second = reused.analyze();
        let mut fresh = Spectrum::new();
        fresh.raw.copy_from_slice(&b);
        assert_eq!(second, fresh.analyze());
    }

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

    use lyrics::Lyrics;
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

}

#[cfg(test)]
mod art_worker_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
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

    type Delivered = (String, String, image_loader::DecodedImage);

    fn worker_with(dir: std::path::PathBuf, timeout: Duration, agent: ureq::Agent) -> (ArtWorker, mpsc::Receiver<(String, std::path::PathBuf, image_loader::DecodedImage)>) {
        let (tx, rx) = mpsc::channel();
        let w = ArtWorker::with(Box::new(move |u, p, i| { let _ = tx.send((u, p, i)); }), dir, timeout, agent);
        (w, rx)
    }

    fn next_art(rx: &mpsc::Receiver<(String, std::path::PathBuf, image_loader::DecodedImage)>, within: Duration) -> Option<Delivered> {
        rx.recv_timeout(within).ok().map(|(u, p, i)| (u, p.to_string_lossy().into_owned(), i))
    }

    fn names(d: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect()
    }

    #[test]
    fn small_cover_is_downloaded_privately_and_delivered() {
        use std::os::unix::fs::MetadataExt;
        let srv = serve(Arc::new(|_, s, _| ok(s, IMG)));
        let d = tmpdir("small");
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(5), agent(5));
        let url = format!("http://127.0.0.1:{}/c.png", srv.port);
        w.submit(url.clone());
        let (u, p, img) = next_art(&rx, Duration::from_secs(10)).expect("cover delivered");
        assert_eq!((img.width, img.height), (1, 1));
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
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(5), agent(5));
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
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(10), agent(10));
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
        let rx = Slot::default();
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
        let rx = Slot::default();
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
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(30), agent(30));
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
        let rx = Slot::default();
        assert!(fetch_art_in(&agent(1), &d, "ftp://127.0.0.1/x", &rx, Duration::from_secs(1)).is_none());
        assert!(names(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undecodable_cover_is_neither_delivered_nor_kept() {
        let srv = serve(Arc::new(|_, s, _| ok(s, b"\x89PNG synthetic cover bytes")));
        let d = tmpdir("undecodable");
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(5), agent(5));
        w.submit(format!("http://127.0.0.1:{}/bad.png", srv.port));
        assert!(next_art(&rx, Duration::from_secs(2)).is_none());
        assert!(names(&d).is_empty(), "{:?}", names(&d));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn production_agent_never_contacts_loopback_servers() {
        let srv = serve(Arc::new(|_, s, _| ok(s, IMG)));
        let d = tmpdir("policy");
        let (w, rx) = worker_with(d.clone(), Duration::from_secs(3), art_http::agent(Duration::from_secs(3)));
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
        let (w, rx) = worker_with(d.join("cache"), Duration::from_secs(3), agent(3));
        w.submit(format!("file://{}", fifo.display()));
        assert!(next_art(&rx, Duration::from_millis(500)).is_none());
        w.submit(format!("file://{}", png.to_string_lossy().replace(' ', "%20")));
        let (u, p, img) = next_art(&rx, Duration::from_secs(5)).expect("local cover delivered");
        assert!(u.starts_with("file://"));
        assert_eq!(std::path::Path::new(&p), png.as_path());
        assert_eq!((img.width, img.height), (1, 1));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tr(player: &str, status: &str, title: &str) -> Track {
        Track { status: status.into(), artist: String::new(), title: title.into(), art_url: String::new(), len: 0.0, player: player.into() }
    }

    #[test]
    fn embedded_covers_decode_and_junk_is_refused() {
        assert_eq!(data_url_bytes("data:image/png;base64,aGk="), Some(b"hi".to_vec()));
        assert_eq!(data_url_bytes("DATA:image/jpeg;base64,aGk="), Some(b"hi".to_vec()));
        for bad in ["data:text/html;base64,aGk=", "data:image/png,aGk=", "data:image/png;base64,***", "data:image/png;base64,", "http://x/y", "file:///a"] {
            assert!(data_url_bytes(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_playing_player_wins_whatever_its_name() {
        let t = [tr("spotify", "Paused", "a"), tr("firefox.instance1", "Playing", "b"), tr("mpv", "Paused", "c")];
        assert_eq!(pick_player(&t, None, ""), Some(1));
        assert_eq!(pick_player(&t, None, "spotify"), Some(1), "a paused current player yields to one that plays");
    }

    #[test]
    fn current_player_keeps_the_slot_while_it_plays_and_pin_wins() {
        let t = [tr("a", "Playing", "x"), tr("b", "Playing", "y")];
        assert_eq!(pick_player(&t, None, "b"), Some(1));
        assert_eq!(pick_player(&t, Some("a"), "b"), Some(0));
        assert_eq!(pick_player(&t, Some("gone"), "b"), Some(1), "a pin on a vanished player is ignored");
    }

    #[test]
    fn all_paused_keeps_current_else_first_with_a_title() {
        let t = [tr("a", "Stopped", ""), tr("b", "Paused", "y"), tr("c", "Paused", "z")];
        assert_eq!(pick_player(&t, None, "c"), Some(2));
        assert_eq!(pick_player(&t, None, ""), Some(1));
        assert_eq!(pick_player(&[], None, ""), None);
    }

    #[test]
    fn labels_and_parsing_handle_browsers_and_odd_lines() {
        assert_eq!(player_label("chromium.instance618584"), "Chromium");
        assert_eq!(player_label("org.mozilla.firefox"), "Firefox");
        assert_eq!(player_label("spotify"), "Spotify");
        let v = parse_tracks("Playing\tArt\tSong\thttp://x\t180000000\tmpv\n\t\t\t\t\t\nbad line\nPaused\t\tOnly title\t\t\tvlc\n");
        assert_eq!(v.iter().map(|t| t.player.as_str()).collect::<Vec<_>>(), ["mpv", "vlc"]);
        assert_eq!(v[0].len, 180.0);
        assert_eq!(v[1].artist, "");
    }
}
