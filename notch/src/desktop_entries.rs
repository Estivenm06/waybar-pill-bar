//! Installed desktop entries: the index used to name open windows, find their icon and decide what a pinned
//! shortcut may launch. Built off the UI thread from `$XDG_DATA_HOME/applications` then each
//! `$XDG_DATA_DIRS/applications`; the first entry of an id wins, so user entries override system ones
//! (including `Hidden=true` masking).
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub struct DesktopEntry {
    /// Standard desktop id (`org.gnome.Foo.desktop`, nested dirs joined with `-`).
    pub id: String,
    pub name: String,
    pub icon: String,
    pub wm_class: String,
}

#[derive(Debug, Default)]
pub struct DesktopIndex {
    /// Launchable, displayable entries sorted by id.
    entries: Vec<DesktopEntry>,
}

impl DesktopIndex {
    pub fn entries(&self) -> &[DesktopEntry] { &self.entries }

    pub fn get(&self, id: &str) -> Option<&DesktopEntry> { self.entries.iter().find(|e| e.id == id) }

    /// `StartupWMClass` (case-insensitive) first, then the desktop id without `.desktop`; ties go to the
    /// lexically first id (entries are sorted).
    pub fn find_class(&self, initial_class: &str, class: &str) -> Option<&DesktopEntry> {
        let wanted: Vec<String> = [initial_class, class].iter().filter(|c| !c.is_empty()).map(|c| c.to_lowercase()).collect();
        if wanted.is_empty() { return None; }
        self.entries.iter().find(|e| !e.wm_class.is_empty() && wanted.contains(&e.wm_class.to_lowercase()))
            .or_else(|| self.entries.iter().find(|e| wanted.contains(&e.id.strip_suffix(".desktop").unwrap_or(&e.id).to_lowercase())))
    }
}

/// Application directories in lookup order.
pub fn roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let data_home = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut v = vec![data_home.join("applications")];
    v.extend(dirs.split(':').filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("applications")));
    v
}

/// A desktop id that can be stored and handed to the launcher.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 255 && !id.chars().any(char::is_control) && !id.contains('/') && !id.contains("..") && !id.starts_with('.')
}

struct Raw {
    kind: String,
    name: String,
    exec: String,
    try_exec: String,
    icon: String,
    wm_class: String,
    no_display: bool,
    hidden: bool,
    dbus: bool,
}

/// Only the `[Desktop Entry]` group; localized keys are ignored.
fn parse(text: &str) -> Raw {
    let mut r = Raw { kind: String::new(), name: String::new(), exec: String::new(), try_exec: String::new(), icon: String::new(), wm_class: String::new(), no_display: false, hidden: false, dbus: false };
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') { inside = line == "[Desktop Entry]"; continue; }
        if !inside || line.starts_with('#') { continue; }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Type" => r.kind = v.into(),
            "Name" => r.name = v.into(),
            "Exec" => r.exec = v.into(),
            "TryExec" => r.try_exec = v.into(),
            "Icon" => r.icon = v.into(),
            "StartupWMClass" => r.wm_class = v.into(),
            "NoDisplay" => r.no_display = v == "true",
            "Hidden" => r.hidden = v == "true",
            "DBusActivatable" => r.dbus = v == "true",
            _ => {}
        }
    }
    r
}

fn try_exec_ok(t: &str) -> bool {
    if t.is_empty() { return true; }
    if t.starts_with('/') {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(t).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    }
    crate::app_status::on_path(t)
}

fn walk(base: &Path, dir: &Path, depth: u32, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<_> = rd.flatten().collect();
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let p = e.path();
        let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
        if is_dir {
            if depth < 6 { walk(base, &p, depth + 1, out); }
        } else if p.extension().is_some_and(|x| x == "desktop") && std::fs::metadata(&p).is_ok_and(|m| m.is_file()) {
            let Ok(rel) = p.strip_prefix(base) else { continue };
            let id = rel.to_string_lossy().replace('/', "-");
            out.push((id, p));
        }
    }
}

/// Index of `roots` (first definition of an id wins, even when hidden).
pub fn build(roots: &[PathBuf]) -> DesktopIndex {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for root in roots {
        let mut files = Vec::new();
        walk(root, root, 0, &mut files);
        for (id, path) in files {
            if !seen.insert(id.clone()) { continue; }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let r = parse(&text);
            let launchable = !r.exec.is_empty() || r.dbus;
            if r.kind != "Application" || r.hidden || r.no_display || !launchable || !try_exec_ok(&r.try_exec) || !valid_id(&id) { continue; }
            let name = if r.name.is_empty() { id.trim_end_matches(".desktop").to_string() } else { r.name };
            entries.push(DesktopEntry { id, name, icon: r.icon, wm_class: r.wm_class });
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    DesktopIndex { entries }
}

static CACHE: Mutex<Option<Arc<DesktopIndex>>> = Mutex::new(None);

/// The cached index, built on first use (blocking: call from a worker).
pub fn cached() -> Arc<DesktopIndex> {
    if let Some(i) = CACHE.lock().ok().and_then(|c| c.clone()) { return i; }
    refresh()
}

/// Rebuild from disk and replace the cache (blocking: call from a worker).
pub fn refresh() -> Arc<DesktopIndex> {
    let idx = Arc::new(build(&roots()));
    if let Ok(mut c) = CACHE.lock() { *c = Some(idx.clone()); }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wn-de-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn user_entries_override_system_and_hidden_masks() {
        let (user, sys) = (dir("u"), dir("s"));
        put(&user, "a.desktop", "[Desktop Entry]\nType=Application\nName=User A\nExec=a\n");
        put(&sys, "a.desktop", "[Desktop Entry]\nType=Application\nName=Sys A\nExec=a\n");
        put(&user, "b.desktop", "[Desktop Entry]\nType=Application\nName=Hidden B\nExec=b\nHidden=true\n");
        put(&sys, "b.desktop", "[Desktop Entry]\nType=Application\nName=Sys B\nExec=b\n");
        put(&sys, "nest/c.desktop", "[Desktop Entry]\nType=Application\nName=C\nExec=c\n[Desktop Action x]\nName=Other\nExec=zzz\n");
        put(&sys, "nodisp.desktop", "[Desktop Entry]\nType=Application\nName=N\nExec=n\nNoDisplay=true\n");
        put(&sys, "noexec.desktop", "[Desktop Entry]\nType=Application\nName=NoExec\n");
        put(&sys, "dbus.desktop", "[Desktop Entry]\nType=Application\nName=Dbus\nDBusActivatable=true\n");
        put(&sys, "gone.desktop", "[Desktop Entry]\nType=Application\nName=Gone\nExec=x\nTryExec=/nonexistent/binary\n");
        put(&sys, "link.desktop", "[Desktop Entry]\nType=Link\nName=L\nURL=x\n");
        let idx = build(&[user.clone(), sys.clone()]);
        let ids: Vec<_> = idx.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a.desktop", "dbus.desktop", "nest-c.desktop"]);
        assert_eq!(idx.get("a.desktop").unwrap().name, "User A");
        assert_eq!(idx.get("nest-c.desktop").unwrap().name, "C");
    }

    #[test]
    fn class_matching_prefers_startup_wm_class_then_id_with_stable_ties() {
        let d = dir("m");
        put(&d, "zz.desktop", "[Desktop Entry]\nType=Application\nName=Z\nExec=z\nStartupWMClass=Brave-Browser\n");
        put(&d, "aa.desktop", "[Desktop Entry]\nType=Application\nName=A\nExec=a\nStartupWMClass=brave-browser\n");
        put(&d, "brave-browser.desktop", "[Desktop Entry]\nType=Application\nName=B\nExec=b\n");
        put(&d, "foot.desktop", "[Desktop Entry]\nType=Application\nName=Foot\nExec=foot\n");
        let idx = build(&[d]);
        assert_eq!(idx.find_class("", "brave-browser").unwrap().id, "aa.desktop");
        assert_eq!(idx.find_class("foot", "x").unwrap().id, "foot.desktop");
        assert!(idx.find_class("", "").is_none());
        assert!(idx.find_class("nothing", "else").is_none());
    }

    #[test]
    fn ids_are_validated() {
        assert!(valid_id("org.foo.Bar.desktop"));
        for bad in ["", "../x.desktop", "a/b.desktop", "a\0.desktop", ".hidden", "a\n"] { assert!(!valid_id(bad), "{bad:?}"); }
    }
}
