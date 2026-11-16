//! Status of the app pills in the first Waybar pill (running / idle).
//!
//! The supervisor owns one `Tracker` fed by Hyprland events; it keeps a tiny JSON cache per app and nudges the
//! Waybar workers with a real-time signal when something changed. The pills themselves run
//! `waybar-notch --waybar-app-status <id>` once at start and on that signal, which only reads the cache.
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const CATALOG: &str = include_str!("../../files/setup/apps-catalog.json");

#[derive(Clone, Debug)]
struct CatalogApp {
    id: String,
    class: String,
    proc_name: Option<String>,
    exe: Vec<String>,
    web: Option<String>,
}

fn catalog() -> Result<Vec<CatalogApp>, String> {
    let v: Value = serde_json::from_str(CATALOG).map_err(|e| format!("apps catalog: {e}"))?;
    let arr = v.as_array().ok_or("apps catalog is not an array")?;
    arr.iter().map(|a| {
        let s = |k: &str| a[k].as_str().map(str::to_string);
        Ok(CatalogApp {
            id: s("id").ok_or("catalog entry without id")?,
            class: s("class").ok_or("catalog entry without class")?,
            proc_name: s("proc"),
            exe: a["exe"].as_array().map(|e| e.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            web: s("web"),
        })
    }).collect()
}

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// Launch argv, same preference order as the setup helper; `None` when the app is not installed.
fn launch_argv(a: &CatalogApp) -> Option<Vec<String>> {
    if let Some(url) = &a.web {
        return on_path("omarchy-launch-webapp").then(|| vec!["omarchy-launch-webapp".to_string(), url.clone()]);
    }
    a.exe.iter().find(|e| on_path(e)).map(|e| vec![e.clone()])
}

fn status_json(running: bool) -> String {
    format!("{{\"text\":\"\",\"class\":\"{}\"}}\n", if running { "running" } else { "idle" })
}

struct App {
    id: String,
    class: String,
    matcher: Regex,
    proc_name: Option<String>,
    background: bool,
    launch: Vec<String>,
    /// Last status written to the cache (`None` = nothing written yet).
    status: Option<bool>,
}

pub(crate) struct Tracker {
    dir: PathBuf,
    apps: Vec<App>,
    clients: Option<Value>,
    procs: HashSet<String>,
    signal: Option<i32>,
    ensure_script: PathBuf,
    ensuring: BTreeMap<String, Child>,
}

impl Tracker {
    /// `document` is the root user configuration (`apps` array); `bar_config` the rendered Waybar config.
    pub(crate) fn new(dir: PathBuf, document: &Value, bar_config: &Value) -> Result<Self, String> {
        let known = catalog()?;
        let apps_cfg = match document.get("apps") {
            None | Some(Value::Null) => &Vec::new(),
            Some(Value::Array(a)) => a,
            Some(_) => return Err("configuration `apps` is not an array".into()),
        };
        let mut apps = Vec::new();
        for e in apps_cfg {
            let Some(id) = e["id"].as_str() else { continue };
            let Some(c) = known.iter().find(|c| c.id == id) else { continue };
            if apps.iter().any(|a: &App| a.id == id) { continue; }
            let Some(launch) = launch_argv(c) else { continue };
            let matcher = RegexBuilder::new(&c.class).case_insensitive(true).build().map_err(|e| format!("class of {id}: {e}"))?;
            apps.push(App {
                id: id.to_string(), class: c.class.clone(), matcher, proc_name: c.proc_name.clone(),
                background: e["background"].as_bool().unwrap_or(false), launch, status: None,
            });
        }
        let signal = apps.iter()
            .filter_map(|a| bar_config.get(format!("custom/{}", a.id))?.get("signal")?.as_i64())
            .find(|s| (1..=30).contains(s)).map(|s| s as i32);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("{}: {e}", dir.display()))?;
        for c in &known {
            if !apps.iter().any(|a| a.id == c.id) { let _ = std::fs::remove_file(dir.join(format!("{}.json", c.id))); }
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        Ok(Self {
            dir, apps, clients: None, procs: HashSet::new(), signal,
            ensure_script: home.join(".config/waybar/scripts/waybar-bgapp.sh"), ensuring: BTreeMap::new(),
        })
    }

    pub(crate) fn signal_offset(&self) -> Option<i32> { self.signal }

    pub(crate) fn clients(&self) -> Option<&Value> { self.clients.as_ref() }

    fn has_window(&self, a: &App) -> bool {
        self.clients.as_ref().and_then(Value::as_array)
            .is_some_and(|cs| cs.iter().any(|c| a.matcher.is_match(c["class"].as_str().unwrap_or(""))))
    }

    fn running(&self, a: &App) -> bool {
        a.background || self.has_window(a) || a.proc_name.as_ref().is_some_and(|p| self.procs.contains(p))
    }

    /// Write changed statuses; true when any cache file changed.
    fn publish(&mut self) -> Result<bool, String> {
        let mut changed = false;
        for i in 0..self.apps.len() {
            let run = self.running(&self.apps[i]);
            if self.apps[i].status == Some(run) { continue; }
            write_private(&self.dir.join(format!("{}.json", self.apps[i].id)), &status_json(run))?;
            self.apps[i].status = Some(run);
            changed = true;
        }
        Ok(changed)
    }

    /// A fresh `j/clients` answer. Only invalid input is an error (the previous status stays valid).
    pub(crate) fn update_clients(&mut self, clients: Value) -> Result<bool, String> {
        if !clients.is_array() { return Err("hyprland clients is not an array".into()); }
        self.clients = Some(clients);
        let changed = self.publish()?;
        self.maintain();
        Ok(changed)
    }

    /// Periodic work: process-name scan (shared by all apps), background-app maintenance.
    pub(crate) fn refresh(&mut self) -> Result<bool, String> {
        if self.clients.is_none() { return Ok(false); }
        let wanted: HashSet<&str> = self.apps.iter().filter(|a| !a.background && !self.has_window(a)).filter_map(|a| a.proc_name.as_deref()).collect();
        let mut found = HashSet::new();
        if !wanted.is_empty() {
            if let Ok(rd) = std::fs::read_dir("/proc") {
                for e in rd.flatten() {
                    if !e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())) { continue; }
                    if let Ok(c) = std::fs::read_to_string(e.path().join("comm")) {
                        let c = c.trim_end_matches('\n');
                        if wanted.contains(c) { found.insert(c.to_string()); }
                    }
                }
            }
        }
        self.procs = found;
        let changed = self.publish()?;
        self.maintain();
        Ok(changed)
    }

    /// Background apps whose window is gone (they must come back on the hidden workspace).
    fn missing_background(&self) -> Vec<usize> {
        if self.clients.is_none() { return Vec::new(); }
        (0..self.apps.len()).filter(|&i| self.apps[i].background && !self.has_window(&self.apps[i])).collect()
    }

    /// At most one live `waybar-bgapp.sh ensure` per app.
    fn maintain(&mut self) {
        self.reap();
        for i in self.missing_background() {
            let a = &self.apps[i];
            if self.ensuring.contains_key(&a.id) { continue; }
            let mut cmd = Command::new(&self.ensure_script);
            cmd.arg("ensure").arg(&a.id).arg(&a.class).args(&a.launch)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
            // SAFETY: only pthread_sigmask runs between fork and exec (the supervisor blocks its signals).
            unsafe {
                cmd.pre_exec(|| nix::sys::signal::SigSet::empty().thread_set_mask().map_err(std::io::Error::from));
            }
            match cmd.spawn() {
                Ok(child) => { self.ensuring.insert(a.id.clone(), child); }
                Err(e) => eprintln!("waybar-supervisor: cannot start ensure for {}: {e}", a.id),
            }
        }
    }

    pub(crate) fn reap(&mut self) {
        self.ensuring.retain(|_, c| matches!(c.try_wait(), Ok(None)));
    }

    pub(crate) fn shutdown(&mut self) {
        for (_, mut c) in std::mem::take(&mut self.ensuring) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn write_private(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    res.map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("cannot write {}: {e}", path.display()) })
}

// ------------------------------------------------------------------ compositor watcher

fn query_clients() -> Result<Value, String> {
    let text = crate::desktop::hypr_req("j/clients").ok_or("hyprland request j/clients failed")?;
    serde_json::from_str(&text).map_err(|e| format!("hyprland j/clients: {e}"))
}

fn relevant(line: &str) -> bool {
    line.split(">>").next().is_some_and(|n| matches!(n, "openwindow" | "closewindow" | "windowtitle" | "windowtitlev2"))
}

/// Report `j/clients` snapshots: once after subscribing, then once per burst of window events. Reconnects after 2s
/// when the compositor goes away; stops when the receiver is dropped.
pub(crate) fn watch_clients(tx: async_channel::Sender<Result<Value, String>>) {
    std::thread::spawn(move || {
        while !tx.is_closed() {
            if let Some((_, stream)) = crate::desktop::hypr_open(".socket2.sock") {
                let (etx, erx) = std::sync::mpsc::channel::<()>();
                std::thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        if relevant(&line) && etx.send(()).is_err() { break; }
                    }
                });
                if tx.send_blocking(query_clients()).is_err() { return; }
                while erx.recv().is_ok() {
                    while erx.recv_timeout(Duration::from_millis(50)).is_ok() {} // coalesce the burst
                    if tx.send_blocking(query_clients()).is_err() { return; }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

// ------------------------------------------------------------------ Waybar-side reader

/// `waybar-notch --waybar-app-status <id>`: one JSON line for the pill.
pub(crate) fn read_status(id: &str) -> Result<String, String> {
    let known = catalog()?;
    if !known.iter().any(|c| c.id == id) { return Err(format!("unknown app id {id:?}")); }
    let dir = crate::bar_controller::runtime_dir().ok_or("XDG_RUNTIME_DIR is not set")?.join("app-status");
    match std::fs::read_to_string(dir.join(format!("{id}.json"))) {
        Ok(t) if t.contains("\"running\"") => Ok(status_json(true)),
        Ok(t) if t.contains("\"idle\"") => Ok(status_json(false)),
        Ok(_) | Err(_) => {
            // no cache yet (supervisor not running): background apps keep their "always active" meaning
            let bg = std::fs::read_to_string(crate::config::path()).ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v["apps"].as_array().cloned())
                .is_some_and(|apps| apps.iter().any(|a| a["id"] == id && a["background"].as_bool() == Some(true)));
            Ok(status_json(bg))
        }
    }
}

pub(crate) fn app_status_dir() -> Option<PathBuf> {
    crate::bar_controller::runtime_dir().map(|d| d.join("app-status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tracker(apps: Value, name: &str) -> Tracker {
        let dir = std::env::temp_dir().join(format!("app-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // make every catalog app "installed"
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for e in ["discord", "vesktop", "steam", "spotify", "omarchy-launch-webapp"] {
            let p = bin.join(e);
            std::fs::write(&p, "").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Prepend (never replace) PATH: image decoding in other tests spawns bwrap from it.
        let old = std::env::var_os("PATH").unwrap_or_default();
        if !std::env::split_paths(&old).any(|p| p == bin) {
            let joined = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&old))).unwrap();
            // SAFETY: tests run single-threaded (--test-threads=1) and mutate PATH only here.
            unsafe { std::env::set_var("PATH", joined); }
        }
        let bar = json!({"custom/steam": {"signal": 7}, "custom/spotify": {"signal": 7}});
        let mut t = Tracker::new(dir.join("cache"), &json!({ "apps": apps }), &bar).unwrap();
        t.ensure_script = dir.join("missing-script");
        t
    }

    #[test]
    fn window_class_matches_case_insensitively_and_changes_once() {
        let mut t = tracker(json!([{"id": "spotify", "background": false}]), "class");
        assert_eq!(t.signal_offset(), Some(7));
        assert!(t.update_clients(json!([{"class": "Spotify"}])).unwrap());
        assert!(!t.update_clients(json!([{"class": "SPOTIFY"}, {"class": "kitty"}])).unwrap());
        assert!(std::fs::read_to_string(t.dir.join("spotify.json")).unwrap().contains("running"));
        assert!(t.update_clients(json!([])).unwrap());
        assert!(std::fs::read_to_string(t.dir.join("spotify.json")).unwrap().contains("idle"));
    }

    #[test]
    fn process_only_app_appears_and_disappears() {
        let mut t = tracker(json!([{"id": "steam", "background": false}]), "proc");
        t.update_clients(json!([])).unwrap();
        t.procs.insert("steam".into());
        assert!(t.publish().unwrap());
        assert_eq!(t.apps[0].status, Some(true));
        t.procs.clear();
        assert!(t.publish().unwrap());
        assert_eq!(t.apps[0].status, Some(false));
    }

    #[test]
    fn background_app_stays_running_and_asks_for_one_ensure_only_while_missing() {
        let mut t = tracker(json!([{"id": "steam", "background": true}]), "bg");
        assert!(t.missing_background().is_empty(), "no clients known yet: do not guess");
        t.update_clients(json!([])).unwrap();
        assert_eq!(t.apps[0].status, Some(true));
        assert_eq!(t.missing_background(), vec![0]);
        t.update_clients(json!([{"class": "steam"}])).unwrap();
        assert!(t.missing_background().is_empty());
    }

    #[test]
    fn invalid_client_answer_keeps_last_known_state() {
        let mut t = tracker(json!([{"id": "spotify", "background": false}]), "lost");
        t.update_clients(json!([{"class": "spotify"}])).unwrap();
        assert!(t.update_clients(json!({"error": 1})).is_err());
        assert_eq!(t.apps[0].status, Some(true));
        assert!(!t.refresh().unwrap());
    }

    #[test]
    fn unknown_ids_are_rejected_by_the_reader() {
        assert!(read_status("../etc/passwd").is_err());
        assert!(read_status("nope").is_err());
    }
}
