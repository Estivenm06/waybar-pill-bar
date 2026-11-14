//! Side-pill control. The GTK notch owns the destination output and tells the Waybar supervisor
//! (`waybar-notch --waybar-supervisor`, run by `waybar.service`) over a private datagram socket which output's
//! pills to show. The supervisor keeps one persistent Waybar worker per enabled output (all started hidden) and only
//! ever sends show/hide signals to those workers, so a focus change never reloads or restarts Waybar.
use crate::config::{self, NotchSettings};
use crate::desktop::{self, DesktopSnapshot};
use crate::app_status;
use gtk4::glib;
use gtk4::gio::{self, prelude::SocketExtManual};
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{kill, killpg, SigSet, Signal};
use nix::unistd::{getpgid, Pid};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ wire format (notch -> supervisor)

/// Everything the supervisor needs to know; sent whole each time so it never has to merge deltas.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncState {
    pub outputs: Vec<String>,
    pub active: Option<String>,
    /// True fullscreen on the active output (and hide-on-fullscreen enabled).
    pub hidden: bool,
    /// Effective clock convention for the right pill (`Locale` already resolved).
    pub clock_12h: bool,
    /// Saved notch font family shared with the pills; `None` follows the system font.
    pub font_family: Option<String>,
}

impl SyncState {
    pub fn to_json(&self) -> String {
        json!({"op": "sync", "outputs": self.outputs, "active": self.active, "hidden": self.hidden,
               "clock_12h": self.clock_12h, "font_family": self.font_family}).to_string()
    }

    pub fn from_json(text: &str) -> Option<SyncState> {
        let v: Value = serde_json::from_str(text).ok()?;
        if v["op"].as_str()? != "sync" { return None; }
        let outputs = v["outputs"].as_array()?.iter().filter_map(|o| o.as_str().map(str::to_string)).collect();
        let font_family = match &v["font_family"] {
            Value::Null => None,
            Value::String(f) if config::valid_font_family(f) => Some(f.clone()),
            _ => return None,
        };
        Some(SyncState {
            outputs, active: v["active"].as_str().map(str::to_string), hidden: v["hidden"].as_bool()?,
            clock_12h: v["clock_12h"].as_bool()?, font_family,
        })
    }
}

/// `$XDG_RUNTIME_DIR/waybar-notch` (the supervisor creates it 0700).
pub fn runtime_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(base).join("waybar-notch"))
}

pub fn control_socket() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join("bar-control.sock"))
}

/// Background sender for the GTK side: pushes each new state at once and re-sends the latest every second, so a
/// restarted supervisor recovers without waiting for the next focus change.
pub fn start_sync_sender() -> mpsc::Sender<SyncState> {
    let (tx, rx) = mpsc::channel::<SyncState>();
    std::thread::spawn(move || {
        let Some(path) = control_socket() else { return };
        let Ok(sock) = UnixDatagram::unbound() else { return };
        let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
        let mut last: Option<SyncState> = None;
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(s) => last = Some(s),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            if let Some(s) = &last {
                let _ = sock.send_to(s.to_json().as_bytes(), &path); // no supervisor yet: nothing to tell
            }
        }
    });
    tx
}

// ------------------------------------------------------------------ visibility planning (pure)

/// What the notch asked for.
#[derive(Clone, Debug, Default, PartialEq)]
struct Desired {
    outputs: Vec<String>,
    active: Option<String>,
    hidden: bool,
    clock_12h: bool,
    font_family: Option<String>,
}

impl Desired {
    fn prefs(&self) -> Prefs { (self.clock_12h, self.font_family.clone()) }
}

/// Appearance the Waybar workers are generated from: (12-hour clock, font family).
type Prefs = (bool, Option<String>);

/// What the supervisor knows about one worker's visibility.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Applied {
    /// Signal handlers installed and layer surface present: safe to signal.
    ready: bool,
    /// Last visibility applied. Workers start hidden (`start_hidden`).
    shown: bool,
}

const FRESH: Applied = Applied { ready: false, shown: false };

fn want_visible(d: &Desired, manual_hidden: bool, output: &str) -> bool {
    !manual_hidden && !d.hidden && d.active.as_deref() == Some(output)
}

/// Visibility changes to send now (`true` = show). Records them in `applied`, so identical state sends nothing.
fn plan_visibility(d: &Desired, manual_hidden: bool, applied: &mut BTreeMap<String, Applied>) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for (output, a) in applied.iter_mut() {
        if !a.ready { continue; }
        let want = want_visible(d, manual_hidden, output);
        if a.shown != want {
            a.shown = want;
            out.push((output.clone(), want));
        }
    }
    out
}

/// Pure fallback when the notch does not report: same selection policy, no interaction pin.
fn desired_from_snapshot(snap: &DesktopSnapshot, prefs: &NotchSettings, previous: Option<&str>) -> Desired {
    let active = desktop::select_output(snap, prefs, None, previous);
    let hidden = prefs.behavior.hide_on_fullscreen && active.as_deref().is_some_and(|a| snap.fullscreen_outputs.contains(a));
    Desired {
        outputs: snap.monitors.iter().map(|m| m.name.clone()).collect(), active, hidden,
        clock_12h: prefs.behavior.clock_format.uses_12h(), font_family: prefs.appearance.font_family.clone(),
    }
}

// ------------------------------------------------------------------ process helpers

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const SYNC_STALE: Duration = Duration::from_secs(3);
const STOP_DEADLINE: Duration = Duration::from_secs(2);
const RESTART_DELAY: Duration = Duration::from_secs(1);

fn waybar_bin() -> PathBuf {
    let p = PathBuf::from("/usr/bin/waybar");
    if p.exists() { p } else { PathBuf::from("waybar") }
}

/// SIGUSR1 (10) and SIGUSR2 (12) both caught.
fn sigcgt_has_usr(status: &str) -> bool {
    let Some(mask) = status.lines().find_map(|l| l.strip_prefix("SigCgt:")) else { return false };
    let Ok(mask) = u64::from_str_radix(mask.trim(), 16) else { return false };
    let bit = |sig: i32| mask & (1u64 << (sig - 1)) != 0;
    bit(Signal::SIGUSR1 as i32) && bit(Signal::SIGUSR2 as i32)
}

fn layer_present(layers: &Value, output: &str, pid: u32) -> bool {
    layers[output]["levels"].as_object().is_some_and(|levels| {
        levels.values().filter_map(Value::as_array).flatten()
            .any(|l| l["namespace"].as_str() == Some("waybar") && l["pid"].as_u64() == Some(pid as u64))
    })
}

fn worker_ready(output: &str, pid: u32) -> bool {
    let caught = std::fs::read_to_string(format!("/proc/{pid}/status")).map(|s| sigcgt_has_usr(&s)).unwrap_or(false);
    caught && desktop::hypr_req("j/layers").and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| layer_present(&v, output, pid))
}

/// Owner of this process (`/proc/self` belongs to the effective user).
fn my_uid() -> u32 {
    std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(u32::MAX)
}

fn pid_of(child: &Child) -> Pid {
    Pid::from_raw(child.id() as i32)
}

/// SIGTERM every group, wait up to one shared deadline, SIGKILL the rest and reap the leaders.
fn stop_groups(mut children: Vec<Child>) {
    for c in &children {
        let _ = killpg(pid_of(c), Signal::SIGTERM);
    }
    let deadline = Instant::now() + STOP_DEADLINE;
    loop {
        children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        if children.is_empty() || Instant::now() >= deadline { break; }
        std::thread::sleep(Duration::from_millis(40));
    }
    for c in &mut children {
        let _ = killpg(pid_of(c), Signal::SIGKILL);
        let _ = c.wait();
    }
}

/// Worker groups left behind by a supervisor that died without cleaning up. Only processes of this user whose
/// executable is waybar and whose `-c` file lives in this private directory qualify.
fn reap_stale_workers(dir: &Path) {
    let me = my_uid();
    let Ok(procs) = std::fs::read_dir("/proc") else { return };
    let mut groups: Vec<Pid> = Vec::new();
    for e in procs.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        if e.metadata().map(|m| m.uid()) .ok() != Some(me) { continue; }
        let exe_ok = std::fs::read_link(format!("/proc/{pid}/exe")).ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().trim_end_matches(" (deleted)").to_string()))
            .is_some_and(|n| n == "waybar");
        if !exe_ok { continue; }
        let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let args: Vec<String> = cmd.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let ours = args.windows(2).any(|w| {
            w[0] == "-c" && Path::new(&w[1]).parent() == Some(dir)
                && Path::new(&w[1]).file_name().is_some_and(|n| n.to_string_lossy().starts_with("worker-"))
        });
        if ours && getpgid(Some(Pid::from_raw(pid))).ok() == Some(Pid::from_raw(pid)) {
            groups.push(Pid::from_raw(pid));
        }
    }
    for g in &groups { let _ = killpg(*g, Signal::SIGTERM); }
    let deadline = Instant::now() + STOP_DEADLINE;
    while Instant::now() < deadline && groups.iter().any(|g| Path::new(&format!("/proc/{g}")).exists()) {
        std::thread::sleep(Duration::from_millis(40));
    }
    for g in groups.iter().filter(|g| Path::new(&format!("/proc/{g}")).exists()) { let _ = killpg(*g, Signal::SIGKILL); }
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).inspect_err(|_| { let _ = std::fs::remove_file(&tmp); })
}

// ------------------------------------------------------------------ the supervisor

enum Event {
    Signal(Signal),
    Ready(String, u32),
    NotReady(String, u32),
    Snapshot(Result<DesktopSnapshot, String>, Box<NotchSettings>),
    Sync(SyncState),
    Clients(Result<Value, String>),
    Fatal(String),
}

struct Worker {
    child: Child,
    cfg: PathBuf,
    check_inflight: bool,
}

struct Supervisor {
    dir: PathBuf,
    base_config: PathBuf,
    style: PathBuf,
    workers: BTreeMap<String, Worker>,
    applied: BTreeMap<String, Applied>,
    restart_at: BTreeMap<String, Instant>,
    desired: Desired,
    manual_hidden: bool,
    last_sync: Option<Instant>,
    next_slot: u64,
    tx: async_channel::Sender<Event>,
    tracker: Option<app_status::Tracker>,
    /// Outputs whose Waybar worker still has to be told to re-read the app status cache.
    status_pending: BTreeSet<String>,
    last_proc_scan: Instant,
    fatal: Option<String>,
    fallback_inflight: bool,
    hypr_sig: Option<String>,
    /// Appearance the running workers were generated with (set by the first worker or a successful reload).
    worker_prefs: Option<Prefs>,
    /// Appearance whose theme preparation failed; not retried on identical heartbeats.
    failed_prefs: Option<Prefs>,
}

impl Supervisor {
    fn base_ok(&self) -> Result<(), String> {
        let text = std::fs::read_to_string(&self.base_config).map_err(|e| format!("cannot read {}: {e}", self.base_config.display()))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => {}
            Ok(_) => return Err(format!("{} is not a JSON object", self.base_config.display())),
            Err(e) => return Err(format!("{} is not valid JSON: {e}", self.base_config.display())),
        }
        if !self.style.is_file() { return Err(format!("missing {}", self.style.display())); }
        Ok(())
    }

    fn start_worker(&mut self, output: &str) -> Result<(), String> {
        self.base_ok()?;
        self.next_slot += 1;
        let cfg = self.dir.join(format!("worker-{}.json", self.next_slot));
        let (clock_12h, _) = self.worker_prefs.get_or_insert_with(|| self.desired.prefs()).clone();
        let fmt = if clock_12h { config::ClockFormat::H12 } else { config::ClockFormat::H24 }.waybar_pattern();
        // Waybar merges the included base recursively without overriding existing members, so only `format` is replaced.
        let doc = json!({
            "output": output, "start_hidden": true, "on-sigusr1": "hide", "on-sigusr2": "show",
            // Waybar's stock invisible mode drops the exclusive zone; keep it so the
            // work area stays stable while workspaces/focus change.
            "modes": {"invisible": {"layer": "top", "exclusive": true, "passthrough": true, "visible": false}},
            "clock": {"format": fmt},
            "include": [self.base_config],
        });
        write_atomic(&cfg, &serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)
            .map_err(|e| format!("cannot write {}: {e}", cfg.display()))?;
        let mut cmd = Command::new(waybar_bin());
        cmd.arg("-c").arg(&cfg).arg("-s").arg(&self.style)
            .env_remove("GDK_SCALE").env_remove("GDK_DPI_SCALE")
            .process_group(0);
        // The supervisor blocks USR1/USR2/TERM/INT (one sigwait thread owns them) and Command spawns inherit that
        // mask; Waybar would never see its show/hide signals. Clearing the mask in the child is the only unsafe step.
        // SAFETY: the closure runs between fork and exec and only calls pthread_sigmask, which is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| SigSet::empty().thread_set_mask().map_err(std::io::Error::from));
        }
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none() {
            if let Some(sig) = &self.hypr_sig { cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig); }
        }
        let child = cmd.spawn().map_err(|e| {
            let _ = std::fs::remove_file(&cfg);
            format!("cannot start waybar for {output}: {e}")
        })?;
        let pid = child.id();
        self.workers.insert(output.to_string(), Worker { child, cfg, check_inflight: true });
        self.applied.insert(output.to_string(), FRESH);
        self.status_pending.insert(output.to_string());
        self.spawn_readiness(output.to_string(), pid);
        Ok(())
    }

    fn spawn_readiness(&self, output: String, pid: u32) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + READY_TIMEOUT;
            while Instant::now() < deadline {
                if worker_ready(&output, pid) {
                    let _ = tx.send_blocking(Event::Ready(output, pid));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = tx.send_blocking(Event::NotReady(output, pid));
        });
    }

    fn stop_outputs(&mut self, outputs: &[String]) {
        let mut children = Vec::new();
        for o in outputs {
            self.applied.remove(o);
            self.restart_at.remove(o);
            if let Some(w) = self.workers.remove(o) {
                let _ = std::fs::remove_file(&w.cfg);
                children.push(w.child);
            }
        }
        stop_groups(children);
    }

    /// Workers exist exactly for the desired outputs.
    fn reconcile_workers(&mut self) {
        let gone: Vec<String> = self.workers.keys().filter(|o| !self.desired.outputs.contains(o)).cloned().collect();
        if !gone.is_empty() { self.stop_outputs(&gone); }
        self.restart_at.retain(|o, _| self.desired.outputs.contains(o));
        for o in self.desired.outputs.clone() {
            if self.workers.contains_key(&o) || self.restart_at.contains_key(&o) { continue; }
            if let Err(e) = self.start_worker(&o) {
                eprintln!("waybar-supervisor: {e}");
                self.restart_at.insert(o, Instant::now() + RESTART_DELAY * 5);
            }
        }
    }

    fn send_visibility(&mut self) {
        for (output, show) in plan_visibility(&self.desired, self.manual_hidden, &mut self.applied) {
            let Some(w) = self.workers.get_mut(&output) else { continue };
            if !matches!(w.child.try_wait(), Ok(None)) { continue; }
            let sig = if show { Signal::SIGUSR2 } else { Signal::SIGUSR1 };
            if let Err(e) = kill(pid_of(&w.child), sig) {
                eprintln!("waybar-supervisor: cannot signal worker for {output}: {e}");
            }
        }
    }

    fn set_desired(&mut self, d: Desired) {
        if d == self.desired && self.workers.len() == d.outputs.len() {
            self.send_visibility();
            return;
        }
        let prefs = d.prefs();
        self.desired = d;
        if !self.workers.is_empty() && self.worker_prefs.as_ref().is_some_and(|w| *w != prefs)
            && self.failed_prefs.as_ref() != Some(&prefs) {
            if !self.reload() { self.failed_prefs = Some(prefs); }
            self.send_visibility();
            return;
        }
        self.reconcile_workers();
        self.send_visibility();
    }

    fn on_event(&mut self, ev: Event) -> bool {
        match ev {
            Event::Signal(Signal::SIGUSR1) => {
                self.manual_hidden = !self.manual_hidden;
                self.send_visibility();
            }
            Event::Signal(Signal::SIGUSR2) => { self.reload(); }
            Event::Signal(Signal::SIGCHLD) => {
                // reap only children this supervisor owns (workers and ensure helpers), then reconcile
                if let Some(t) = self.tracker.as_mut() { t.reap(); }
                self.tick();
            }
            Event::Signal(_) => return false,
            Event::Sync(s) => self.on_sync(s),
            Event::Clients(Ok(clients)) => {
                let res = self.tracker.as_mut().map(|t| t.update_clients(clients));
                self.after_status(res);
            }
            Event::Clients(Err(e)) => eprintln!("waybar-supervisor: {e}"),
            Event::Fatal(e) => {
                eprintln!("waybar-supervisor: {e}");
                self.fatal = Some(e);
                return false;
            }
            Event::Ready(output, pid) => {
                if let Some(w) = self.workers.get_mut(&output).filter(|w| w.child.id() == pid) {
                    w.check_inflight = false;
                    if let Some(a) = self.applied.get_mut(&output) { a.ready = true; }
                    self.send_visibility();
                    self.signal_status();
                }
            }
            Event::NotReady(output, pid) => {
                if let Some(w) = self.workers.get_mut(&output).filter(|w| w.child.id() == pid) {
                    w.check_inflight = false;
                    eprintln!("waybar-supervisor: worker for {output} is not ready after {}s; keeping it hidden", READY_TIMEOUT.as_secs());
                }
            }
            Event::Snapshot(res, prefs) => {
                self.fallback_inflight = false;
                if self.last_sync.is_some_and(|t| t.elapsed() < SYNC_STALE) { return true; }
                match res {
                    Ok(snap) => {
                        let d = desired_from_snapshot(&snap, &prefs, self.desired.active.as_deref());
                        self.set_desired(d);
                    }
                    Err(e) => eprintln!("waybar-supervisor: {e}"),
                }
            }
        }
        true
    }

    fn after_status(&mut self, res: Option<Result<bool, String>>) {
        match res {
            Some(Ok(true)) => {
                self.status_pending.extend(self.workers.keys().cloned());
                self.signal_status();
            }
            Some(Err(e)) => eprintln!("waybar-supervisor: app status: {e}"),
            _ => {}
        }
    }

    /// Ask ready workers to re-run their app status modules (SIGRTMIN+offset); the rest stay pending until Ready.
    fn signal_status(&mut self) {
        let Some(off) = self.tracker.as_ref().and_then(|t| t.signal_offset()) else { self.status_pending.clear(); return };
        let ready: Vec<String> = self.status_pending.iter().filter(|o| self.applied.get(*o).is_some_and(|a| a.ready)).cloned().collect();
        for o in ready {
            self.status_pending.remove(&o);
            let Some(w) = self.workers.get(&o) else { continue };
            if !matches!(w.child.id(), pid if pid > 0) { continue; }
            // SAFETY: plain kill(2) on a child pid we own; SIGRTMIN() only reads libc's runtime value.
            let r = unsafe { libc::kill(w.child.id() as i32, libc::SIGRTMIN() + off) };
            if r != 0 { eprintln!("waybar-supervisor: cannot signal app status to {o}: {}", std::io::Error::last_os_error()); }
        }
    }

    /// Rebuild the app tracker from the current configuration; a broken file keeps the previous good tracker.
    fn rebuild_tracker(&mut self) {
        let doc = match std::fs::read_to_string(config::path()) {
            Ok(t) => match serde_json::from_str::<Value>(&t) {
                Ok(v) if v.is_object() => v,
                Ok(_) => { eprintln!("waybar-supervisor: {} is not a JSON object; keeping app status as is", config::path().display()); return }
                Err(e) => { eprintln!("waybar-supervisor: {}: {e}; keeping app status as is", config::path().display()); return }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(e) => { eprintln!("waybar-supervisor: {}: {e}; keeping app status as is", config::path().display()); return }
        };
        let bar = match std::fs::read_to_string(&self.base_config).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => { eprintln!("waybar-supervisor: bar config: {e}; keeping app status as is"); return }
        };
        let Some(dir) = app_status::app_status_dir() else { return };
        match app_status::Tracker::new(dir, &doc, &bar) {
            Ok(mut t) => {
                if let Some(mut old) = self.tracker.take() {
                    old.shutdown();
                    if let Some(c) = old.clients().cloned() { let _ = t.update_clients(c); }
                }
                let _ = t.refresh();
                self.tracker = Some(t);
                self.status_pending.extend(self.workers.keys().cloned());
            }
            Err(e) => eprintln!("waybar-supervisor: app status: {e}; keeping the previous tracker"),
        }
    }

    fn on_sync(&mut self, s: SyncState) {
        self.last_sync = Some(Instant::now());
        self.set_desired(Desired {
            outputs: s.outputs, active: s.active, hidden: s.hidden, clock_12h: s.clock_12h, font_family: s.font_family,
        });
    }

    /// Re-read config and style: replace every owned worker (hidden first), keep the manual-hide flag.
    /// Returns false (workers untouched) when the base config/style or the generated theme cannot be prepared.
    fn reload(&mut self) -> bool {
        if let Err(e) = self.base_ok() {
            eprintln!("waybar-supervisor: reload refused, keeping the running bars: {e}");
            return false;
        }
        if let Err(e) = crate::theme::write_waybar_theme(self.desired.font_family.as_deref()) {
            eprintln!("waybar-supervisor: cannot prepare the pill theme, keeping the running bars: {e}");
            return false;
        }
        self.failed_prefs = None;
        self.worker_prefs = Some(self.desired.prefs());
        self.rebuild_tracker();
        let outputs: Vec<String> = self.workers.keys().cloned().collect();
        for (o, a) in self.applied.iter_mut() {
            if a.shown && self.workers.contains_key(o) {
                if let Some(w) = self.workers.get(o) { let _ = kill(pid_of(&w.child), Signal::SIGUSR1); }
                a.shown = false;
            }
        }
        self.stop_outputs(&outputs);
        self.reconcile_workers();
        self.send_visibility();
        true
    }

    fn tick(&mut self) {
        // crashed workers come back after a delay while their output still exists
        let dead: Vec<String> = self.workers.iter_mut()
            .filter_map(|(o, w)| match w.child.try_wait() { Ok(None) => None, _ => Some(o.clone()) }).collect();
        for o in dead {
            eprintln!("waybar-supervisor: worker for {o} exited; restarting");
            if let Some(w) = self.workers.remove(&o) { let _ = std::fs::remove_file(&w.cfg); }
            self.applied.remove(&o);
            self.restart_at.insert(o, Instant::now() + RESTART_DELAY);
        }
        let due: Vec<String> = self.restart_at.iter().filter(|(_, t)| Instant::now() >= **t).map(|(o, _)| o.clone()).collect();
        if !due.is_empty() {
            for o in &due { self.restart_at.remove(o); }
            self.reconcile_workers();
        }
        // process-only apps and background-app upkeep (window events cover everything else)
        if self.last_proc_scan.elapsed() >= Duration::from_secs(2) {
            self.last_proc_scan = Instant::now();
            let res = self.tracker.as_mut().map(|t| t.refresh());
            self.after_status(res);
        }
        // a worker whose readiness check timed out gets another look
        let retry: Vec<(String, u32)> = self.workers.iter()
            .filter(|(o, w)| !w.check_inflight && !self.applied.get(*o).is_some_and(|a| a.ready))
            .map(|(o, w)| (o.clone(), w.child.id())).collect();
        for (o, pid) in retry {
            if let Some(w) = self.workers.get_mut(&o) { w.check_inflight = true; }
            self.spawn_readiness(o, pid);
        }
        // the notch stopped reporting (not started yet, or crashed): decide from the compositor directly
        let stale = self.last_sync.map_or(true, |t| t.elapsed() > SYNC_STALE);
        if stale && !self.fallback_inflight {
            self.fallback_inflight = true;
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let (prefs, _) = config::load_settings();
                let _ = tx.send_blocking(Event::Snapshot(desktop::query_snapshot(), Box::new(prefs)));
            });
        }
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.tracker.as_mut() { t.shutdown(); }
        let outputs: Vec<String> = self.workers.keys().cloned().collect();
        self.stop_outputs(&outputs);
    }
}

fn private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let md = std::fs::metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if md.uid() != my_uid() { return Err(format!("{} is not owned by this user", dir.display())); }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))
}

/// `waybar-notch --waybar-supervisor`: runs until SIGTERM/SIGINT.
pub fn run_supervisor() -> glib::ExitCode {
    match supervise() {
        Ok(()) => glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("waybar-supervisor: {e}");
            glib::ExitCode::FAILURE
        }
    }
}

fn supervise() -> Result<(), String> {
    let dir = runtime_dir().ok_or("XDG_RUNTIME_DIR is not set")?;
    private_dir(&dir)?;
    let lock_path = dir.join("supervisor.lock");
    let lock_file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock)
        .map_err(|_| format!("another supervisor owns {}", lock_path.display()))?;

    // signals are handled by one thread (sigwait); they must be blocked before any other thread exists
    let mut sigs = SigSet::empty();
    for s in [Signal::SIGUSR1, Signal::SIGUSR2, Signal::SIGTERM, Signal::SIGINT, Signal::SIGCHLD] { sigs.add(s); }
    sigs.thread_block().map_err(|e| format!("cannot block signals: {e}"))?;
    let (tx, rx) = async_channel::unbounded::<Event>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(sig) = sigs.wait() {
                if tx.send_blocking(Event::Signal(sig)).is_err() { break; }
            }
        });
    }

    let sock_path = dir.join("bar-control.sock");
    let _ = std::fs::remove_file(&sock_path); // stale: we hold the lock, so nobody else listens on it
    let sock = UnixDatagram::bind(&sock_path).map_err(|e| format!("cannot bind {}: {e}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    sock.set_nonblocking(true).map_err(|e| e.to_string())?;

    reap_stale_workers(&dir);

    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    let waybar_dir = Path::new(&home).join(".config/waybar");
    let sup = Supervisor {
        dir: dir.clone(),
        base_config: waybar_dir.join("config.jsonc"),
        style: waybar_dir.join("style.css"),
        workers: BTreeMap::new(),
        applied: BTreeMap::new(),
        restart_at: BTreeMap::new(),
        desired: Desired::default(),
        manual_hidden: false,
        last_sync: None,
        next_slot: 0,
        tx: tx.clone(),
        tracker: None,
        status_pending: BTreeSet::new(),
        last_proc_scan: Instant::now(),
        fatal: None,
        fallback_inflight: false,
        hypr_sig: desktop::hypr_sig(),
        worker_prefs: None,
        failed_prefs: None,
    };

    // generated pill CSS first, from the saved font, so the first workers already use it
    {
        let (prefs, _) = config::load_settings();
        if let Err(e) = crate::theme::write_waybar_theme(prefs.appearance.font_family.as_deref()) {
            eprintln!("waybar-supervisor: cannot prepare the pill theme: {e}");
        }
    }

    let main_loop = glib::MainLoop::new(None, false);
    let ctx = glib::MainContext::default();
    let sup = std::rc::Rc::new(std::cell::RefCell::new(sup));
    sup.borrow_mut().rebuild_tracker();
    {
        // compositor window events -> app status
        let (ctx_tx, ctx_rx) = async_channel::unbounded::<Result<Value, String>>();
        app_status::watch_clients(ctx_tx);
        let tx = tx.clone();
        ctx.spawn_local(async move {
            while let Ok(r) = ctx_rx.recv().await { if tx.try_send(Event::Clients(r)).is_err() { break; } }
        });
    }
    // control socket: wake only when a datagram (or hang-up) arrives; the source parses and queues, never touches `sup`
    let fd = sock.try_clone().map_err(|e| format!("cannot duplicate control socket: {e}"))?;
    let gsock = gio::Socket::from_fd(std::os::fd::OwnedFd::from(fd)).map_err(|e| format!("cannot wrap control socket: {e}"))?;
    let source = {
        let tx = tx.clone();
        gsock.create_source(
            glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
            gio::Cancellable::NONE, Some("bar-control"), glib::Priority::DEFAULT,
            move |_, cond| {
                if cond.intersects(glib::IOCondition::HUP | glib::IOCondition::ERR) {
                    let _ = tx.try_send(Event::Fatal("control socket failed".into()));
                    return glib::ControlFlow::Break;
                }
                let mut buf = [0u8; 4096];
                while let Ok(n) = sock.recv(&mut buf) {
                    if let Some(s) = SyncState::from_json(&String::from_utf8_lossy(&buf[..n])) { let _ = tx.try_send(Event::Sync(s)); }
                }
                glib::ControlFlow::Continue
            },
        )
    };
    source.attach(Some(&ctx));
    {
        let (sup, main_loop) = (sup.clone(), main_loop.clone());
        ctx.spawn_local(async move {
            while let Ok(ev) = rx.recv().await {
                if !sup.borrow_mut().on_event(ev) {
                    sup.borrow_mut().shutdown();
                    main_loop.quit();
                    break;
                }
            }
        });
    }
    {
        // heartbeat staleness, worker restarts, readiness retries and the process-only status scan
        let sup = sup.clone();
        glib::timeout_add_seconds_local(1, move || { sup.borrow_mut().tick(); glib::ControlFlow::Continue });
    }
    main_loop.run();
    source.destroy();
    drop(gsock);
    sup.borrow_mut().shutdown();
    let _ = std::fs::remove_file(&sock_path);
    match sup.borrow_mut().fatal.take() { Some(e) => Err(e), None => Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(outputs: &[&str], active: Option<&str>, hidden: bool) -> Desired {
        Desired { outputs: outputs.iter().map(|s| s.to_string()).collect(), active: active.map(str::to_string), hidden, ..Default::default() }
    }
    fn workers(names: &[&str], ready: bool) -> BTreeMap<String, Applied> {
        names.iter().map(|n| (n.to_string(), Applied { ready, shown: false })).collect()
    }
    fn plan(d: &Desired, manual: bool, a: &mut BTreeMap<String, Applied>) -> Vec<(String, bool)> {
        plan_visibility(d, manual, a)
    }

    #[test]
    fn only_the_active_worker_is_shown_and_repeats_do_nothing() {
        let mut a = workers(&["A", "B"], true);
        let d = desired(&["A", "B"], Some("A"), false);
        assert_eq!(plan(&d, false, &mut a), vec![("A".to_string(), true)]);
        assert!(plan(&d, false, &mut a).is_empty());
        // focus moves: exactly one hide and one show, no more
        let d = desired(&["A", "B"], Some("B"), false);
        assert_eq!(plan(&d, false, &mut a), vec![("A".to_string(), false), ("B".to_string(), true)]);
    }

    #[test]
    fn fullscreen_on_the_destination_hides_only_that_worker_and_exit_restores_it() {
        let mut a = workers(&["A", "B"], true);
        plan(&desired(&["A", "B"], Some("A"), false), false, &mut a);
        assert_eq!(plan(&desired(&["A", "B"], Some("A"), true), false, &mut a), vec![("A".to_string(), false)]);
        // repeated fullscreen events do not toggle anything back on
        assert!(plan(&desired(&["A", "B"], Some("A"), true), false, &mut a).is_empty());
        assert_eq!(plan(&desired(&["A", "B"], Some("A"), false), false, &mut a), vec![("A".to_string(), true)]);
    }

    #[test]
    fn manual_hide_survives_a_reload_and_new_workers_start_hidden() {
        let d = desired(&["A", "B"], Some("A"), false);
        let mut a = workers(&["A", "B"], true);
        plan(&d, false, &mut a);
        // user hides, then the supervisor reloads: fresh workers are hidden and stay hidden while manual is set
        assert_eq!(plan(&d, true, &mut a), vec![("A".to_string(), false)]);
        let mut fresh: BTreeMap<String, Applied> = ["A", "B"].iter().map(|n| (n.to_string(), FRESH)).collect();
        assert!(plan(&d, true, &mut fresh).is_empty()); // not ready: nothing is signalled at all
        for a in fresh.values_mut() { a.ready = true; }
        assert!(plan(&d, true, &mut fresh).is_empty());
        // toggling manual back shows only the active output
        assert_eq!(plan(&d, false, &mut fresh), vec![("A".to_string(), true)]);
    }

    #[test]
    fn workers_that_are_not_ready_are_never_signalled() {
        let mut a = workers(&["A"], false);
        assert!(plan(&desired(&["A"], Some("A"), false), false, &mut a).is_empty());
        a.get_mut("A").unwrap().ready = true;
        assert_eq!(plan(&desired(&["A"], Some("A"), false), false, &mut a), vec![("A".to_string(), true)]);
    }

    #[test]
    fn readiness_needs_both_user_signal_handlers() {
        assert!(sigcgt_has_usr("Name:\twaybar\nSigCgt:\tfffffffd80010a02\n"));
        assert!(!sigcgt_has_usr("SigCgt:\t0000000000000200\n")); // USR1 only
        assert!(!sigcgt_has_usr("SigCgt:\t0000000000000000\n"));
        let layers = json!({"A": {"levels": {"2": [{"namespace": "waybar", "pid": 7}]}}});
        assert!(layer_present(&layers, "A", 7));
        assert!(!layer_present(&layers, "B", 7));
        assert!(!layer_present(&layers, "A", 8));
    }

    #[test]
    fn sync_message_roundtrips_with_null_active() {
        let s = SyncState { outputs: vec!["eDP-1".into()], active: None, hidden: true, clock_12h: true, font_family: Some("Noto Sans".into()) };
        assert_eq!(SyncState::from_json(&s.to_json()), Some(s.clone()));
        let bad = s.to_json().replace("Noto Sans", "bad\\nfont");
        assert_eq!(SyncState::from_json(&bad), None);
        assert_eq!(SyncState::from_json(&s.to_json().replace("\"clock_12h\":true,", "")), None);
        assert_eq!(SyncState::from_json("{\"op\":\"other\"}"), None);
    }

    #[test]
    fn fallback_follows_focus_and_hides_only_for_a_fullscreen_destination() {
        use serde_json::json;
        let mons = json!([
            {"id": 0, "name": "A", "width": 100, "height": 100, "scale": 1.0, "transform": 0, "focused": false, "activeWorkspace": {"id": 1}, "specialWorkspace": {"id": 0}},
            {"id": 1, "name": "B", "width": 100, "height": 100, "scale": 1.0, "transform": 0, "focused": true, "activeWorkspace": {"id": 2}, "specialWorkspace": {"id": 0}},
        ]);
        let clients = json!([{"monitor": 0, "workspace": {"id": 1}, "mapped": true, "hidden": false, "fullscreen": 2}]);
        let snap = desktop::parse_snapshot(&mons, &clients);
        let d = desired_from_snapshot(&snap, &NotchSettings::default(), None);
        assert_eq!(d.active.as_deref(), Some("B"));
        assert!(!d.hidden); // fullscreen is on A, the destination is B
    }
}
