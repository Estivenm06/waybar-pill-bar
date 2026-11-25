//! User choices (`~/.config/waybar-notch/config.json`): the `ai`/`apps` arrays written by the installer
//! plus the typed `notch` preferences object edited in the Settings page.
//!
//! Saves are read-merge-write under an advisory lock (`.config.lock`, shared with waybar-setup.py), so
//! concurrent writers never drop each other's fields and unknown keys are always preserved.
use nix::fcntl::{Flock, FlockArg};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const TAB_IDS: [&str; 6] = ["home", "media", "calendar", "clipboard", "ai", "controls"];
pub const UTILITY_IDS: [&str; 11] =
    ["screenshot", "record", "nightlight", "dnd", "notification-sounds", "wifi", "bluetooth", "lock", "clear-clipboard", "audio", "display"];

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("waybar-notch/config.json")
}

// ---------------------------------------------------------------- typed preferences

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorMode {
    Follow,
    Fixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockFormat {
    Locale,
    H12,
    H24,
}

impl ClockFormat {
    /// Whether this format shows AM/PM; `Locale` follows the system's `date +%p` (decided once per process).
    pub fn uses_12h(self) -> bool {
        match self {
            ClockFormat::H12 => true,
            ClockFormat::H24 => false,
            ClockFormat::Locale => {
                static V: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
                    std::process::Command::new("date").arg("+%p").stderr(std::process::Stdio::null()).output()
                        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty()).unwrap_or(false)
                });
                *V
            }
        }
    }
    pub fn chrono_pattern(self) -> &'static str { if self.uses_12h() { "%I:%M %p" } else { "%H:%M" } }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorSettings {
    pub mode: MonitorMode,
    pub output: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppearanceSettings {
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub accent: Option<String>,
    pub font_family: Option<String>,
    pub font_size: u32,
    pub corner_radius: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutSettings {
    pub collapsed_width: u32,
    pub collapsed_height: u32,
    pub expanded_width: u32,
    pub top_margin: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BehaviorSettings {
    pub open_on_hover: bool,
    pub collapse_delay_ms: u32,
    pub home_reset_secs: u32,
    pub animations: bool,
    pub hide_on_fullscreen: bool,
    pub clock_format: ClockFormat,
    /// Play a short sound for new notifications (cards and popups are unaffected).
    pub notification_sounds: bool,
}

/// A launcher kept in the shortcut row while its app is closed. `class` is the exact normalized (lowercase) window
/// class identity of the app, never a regex or shell text; `desktop_id` names the installed desktop entry to launch.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedApp {
    pub desktop_id: String,
    pub class: String,
}

impl PinnedApp {
    /// Validated pin; `None` for empty values, control characters or path traversal.
    pub fn new(desktop_id: &str, class: &str) -> Option<PinnedApp> {
        let ok = |s: &str, max: usize| !s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control);
        let id_ok = ok(desktop_id, 255) && !desktop_id.contains('/') && !desktop_id.contains("..") && !desktop_id.starts_with('.');
        let class = class.trim().to_lowercase();
        (id_ok && ok(&class, 255)).then(|| PinnedApp { desktop_id: desktop_id.to_string(), class })
    }
    pub fn to_json(&self) -> Value { json!({"desktop_id": self.desktop_id, "class": self.class}) }
}

fn pins_from(v: Option<&Value>) -> Vec<PinnedApp> {
    let mut out: Vec<PinnedApp> = Vec::new();
    for e in v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let Some(p) = PinnedApp::new(e.get("desktop_id").and_then(Value::as_str).unwrap_or(""), e.get("class").and_then(Value::as_str).unwrap_or("")) else { continue };
        if !out.contains(&p) { out.push(p); }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct NotchSettings {
    pub monitor: MonitorSettings,
    pub appearance: AppearanceSettings,
    pub layout: LayoutSettings,
    pub behavior: BehaviorSettings,
    /// Normalized: `home` first, `controls` last, the optional pages between in the chosen order.
    pub tabs: Vec<String>,
    pub utilities: Vec<String>,
    pub pinned_apps: Vec<PinnedApp>,
    /// Saved arrangement of the notch app tiles (shortcut keys); unlisted apps follow in their natural order.
    pub app_order: Vec<String>,
}

impl Default for NotchSettings {
    fn default() -> Self {
        NotchSettings {
            monitor: MonitorSettings { mode: MonitorMode::Follow, output: None },
            appearance: AppearanceSettings {
                background: None,
                foreground: None,
                accent: None,
                font_family: None,
                font_size: 12,
                corner_radius: 20,
            },
            layout: LayoutSettings { collapsed_width: 360, collapsed_height: 40, expanded_width: 560, top_margin: 4 },
            behavior: BehaviorSettings {
                open_on_hover: true,
                collapse_delay_ms: 350,
                home_reset_secs: 10,
                animations: true,
                hide_on_fullscreen: true,
                clock_format: ClockFormat::Locale,
                notification_sounds: false,
            },
            tabs: TAB_IDS.iter().map(|s| s.to_string()).collect(),
            utilities: UTILITY_IDS.iter().map(|s| s.to_string()).collect(),
            pinned_apps: Vec::new(),
            app_order: Vec::new(),
        }
    }
}

fn int_at(v: &Value, group: &str, key: &str, default: u32, lo: u32, hi: u32) -> u32 {
    match v.get(group).and_then(|g| g.get(key)).and_then(Value::as_f64) {
        Some(n) if n.is_finite() => n.round().clamp(lo as f64, hi as f64) as u32,
        _ => default,
    }
}

fn bool_at(v: &Value, group: &str, key: &str, default: bool) -> bool {
    v.get(group).and_then(|g| g.get(key)).and_then(Value::as_bool).unwrap_or(default)
}

pub fn valid_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn valid_font_family(s: &str) -> bool {
    let n = s.chars().count();
    (1..=128).contains(&n) && !s.chars().any(char::is_control)
}

fn color_at(v: &Value, key: &str) -> Option<String> {
    let s = v.get("appearance")?.get(key)?.as_str()?;
    valid_hex_color(s).then(|| s.to_string())
}

fn id_list(v: Option<&Value>, known: &[&str]) -> Option<Vec<String>> {
    let arr = v?.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for s in arr.iter().filter_map(Value::as_str) {
        if known.contains(&s) && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    }
    Some(out)
}

/// `home` first, `controls` last, optional pages between.
pub fn normalize_tabs(ids: &[String]) -> Vec<String> {
    let mut out = vec!["home".to_string()];
    for s in ids {
        if s != "home" && s != "controls" && TAB_IDS.contains(&s.as_str()) && !out.contains(s) {
            out.push(s.clone());
        }
    }
    out.push("controls".to_string());
    out
}

impl NotchSettings {
    /// Reads the `notch` object of a whole config document. Missing/invalid fields fall back to defaults, finite
    /// numbers outside the accepted range clamp.
    pub fn from_json(doc: &Value) -> Self {
        let d = Self::default();
        let n = doc.get("notch").filter(|v| v.is_object()).unwrap_or(&Value::Null);
        let mode = match n.get("monitor").and_then(|m| m.get("mode")).and_then(Value::as_str) {
            Some("fixed") => MonitorMode::Fixed,
            _ => MonitorMode::Follow,
        };
        let output = n.get("monitor").and_then(|m| m.get("output")).and_then(Value::as_str)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control)).map(str::to_string);
        let font_family = n.get("appearance").and_then(|a| a.get("font_family")).and_then(Value::as_str)
            .filter(|s| valid_font_family(s)).map(str::to_string);
        let clock_format = match n.get("behavior").and_then(|b| b.get("clock_format")).and_then(Value::as_str) {
            Some("12h") => ClockFormat::H12,
            Some("24h") => ClockFormat::H24,
            _ => ClockFormat::Locale,
        };
        NotchSettings {
            monitor: MonitorSettings { mode, output },
            appearance: AppearanceSettings {
                background: color_at(n, "background"),
                foreground: color_at(n, "foreground"),
                accent: color_at(n, "accent"),
                font_family,
                font_size: int_at(n, "appearance", "font_size", d.appearance.font_size, 10, 18),
                corner_radius: int_at(n, "appearance", "corner_radius", d.appearance.corner_radius, 0, 40),
            },
            layout: LayoutSettings {
                collapsed_width: int_at(n, "layout", "collapsed_width", d.layout.collapsed_width, 180, 420),
                collapsed_height: int_at(n, "layout", "collapsed_height", d.layout.collapsed_height, 24, 48),
                expanded_width: int_at(n, "layout", "expanded_width", d.layout.expanded_width, 480, 960),
                top_margin: int_at(n, "layout", "top_margin", d.layout.top_margin, 0, 24),
            },
            behavior: BehaviorSettings {
                open_on_hover: bool_at(n, "behavior", "open_on_hover", d.behavior.open_on_hover),
                collapse_delay_ms: int_at(n, "behavior", "collapse_delay_ms", d.behavior.collapse_delay_ms, 100, 2000),
                home_reset_secs: int_at(n, "behavior", "home_reset_secs", d.behavior.home_reset_secs, 0, 120),
                animations: bool_at(n, "behavior", "animations", d.behavior.animations),
                hide_on_fullscreen: bool_at(n, "behavior", "hide_on_fullscreen", d.behavior.hide_on_fullscreen),
                clock_format,
                notification_sounds: bool_at(n, "behavior", "notification_sounds", d.behavior.notification_sounds),
            },
            tabs: id_list(n.get("tabs"), &TAB_IDS).map(|t| normalize_tabs(&t)).unwrap_or(d.tabs),
            utilities: id_list(n.get("utilities"), &UTILITY_IDS).unwrap_or(d.utilities),
            pinned_apps: pins_from(n.get("pinned_apps")),
            app_order: n.get("app_order").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default(),
        }
    }

    /// The complete `notch` object (every supported key).
    pub fn to_json(&self) -> Value {
        let a = &self.appearance;
        json!({
            "monitor": {
                "mode": if self.monitor.mode == MonitorMode::Fixed { "fixed" } else { "follow" },
                "output": self.monitor.output,
            },
            "appearance": {
                "background": a.background, "foreground": a.foreground, "accent": a.accent,
                "font_family": a.font_family, "font_size": a.font_size,
                "corner_radius": a.corner_radius,
            },
            "layout": {
                "collapsed_width": self.layout.collapsed_width, "collapsed_height": self.layout.collapsed_height,
                "expanded_width": self.layout.expanded_width, "top_margin": self.layout.top_margin,
            },
            "behavior": {
                "open_on_hover": self.behavior.open_on_hover,
                "collapse_delay_ms": self.behavior.collapse_delay_ms,
                "home_reset_secs": self.behavior.home_reset_secs,
                "animations": self.behavior.animations,
                "hide_on_fullscreen": self.behavior.hide_on_fullscreen,
                "clock_format": match self.behavior.clock_format {
                    ClockFormat::Locale => "locale", ClockFormat::H12 => "12h", ClockFormat::H24 => "24h",
                },
                "notification_sounds": self.behavior.notification_sounds,
            },
            "tabs": self.tabs,
            "utilities": self.utilities,
            "pinned_apps": self.pinned_apps.iter().map(PinnedApp::to_json).collect::<Vec<_>>(),
            "app_order": self.app_order,
        })
    }
}

/// Fields of `new` that differ from `old`, nested, as a patch for [`save_patch`].
pub fn diff_patch(old: &Value, new: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let (Some(o), Some(n)) = (old.as_object(), new.as_object()) {
        for (k, nv) in n {
            match o.get(k) {
                Some(ov) if ov == nv => {}
                Some(ov) if ov.is_object() && nv.is_object() => {
                    out.insert(k.clone(), Value::Object(diff_patch(ov, nv)));
                }
                _ => {
                    out.insert(k.clone(), nv.clone());
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- AI selection

fn ai_from(doc: &Value) -> Option<Vec<String>> {
    let arr = doc.get("ai")?.as_array()?;
    Some(arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
}

/// AI providers the user chose to show. `None` (no file, unparsable, or no `ai` array) = every detected provider.
pub fn ai_selection() -> Option<Vec<String>> {
    ai_from(&read_document().ok()?)
}

// ---------------------------------------------------------------- persistence

fn read_document_at(p: &Path) -> Result<Value, String> {
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Object(Map::new())),
        Err(e) => return Err(format!("cannot read {}: {e}", p.display())),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err(format!("{} is not a JSON object; fix or remove it", p.display())),
        Err(e) => Err(format!("{} is not valid JSON ({e}); fix or remove it", p.display())),
    }
}

/// The whole document; a missing file is an empty object, unreadable or malformed content is an error naming the path.
pub fn read_document() -> Result<Value, String> {
    read_document_at(&path())
}

/// Preferences from the document, plus the read error when the file exists but is unusable (defaults are used then).
pub fn load_settings() -> (NotchSettings, Option<String>) {
    match read_document() {
        Ok(doc) => (NotchSettings::from_json(&doc), None),
        Err(e) => (NotchSettings::default(), Some(e)),
    }
}

pub fn merge_into(doc: &mut Value, patch: &Map<String, Value>) {
    if !doc.is_object() {
        *doc = Value::Object(Map::new());
    }
    let obj = doc.as_object_mut().expect("object ensured above");
    for (k, v) in patch {
        match (obj.get_mut(k), v) {
            (Some(cur), Value::Object(p)) if cur.is_object() => merge_into(cur, p),
            _ => {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Under the config lock: re-read the latest document, let `make` derive a patch from it, merge and write atomically.
fn update_at(p: &Path, make: impl FnOnce(&Value) -> Result<Map<String, Value>, String>) -> Result<(), String> {
    let dir = p.parent().ok_or_else(|| format!("invalid config path {}", p.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock_path = dir.join(".config.lock");
    let lock_file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, e)| format!("cannot lock {}: {e}", lock_path.display()))?;

    let mut doc = read_document_at(p)?;
    let patch = make(&doc)?;
    merge_into(&mut doc, &patch);
    let mut text = serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    text.push('\n');

    let tmp = dir.join(format!(".config.json.tmp.{}.{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let res = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.flush()?;
        f.sync_all()?;
        std::fs::rename(&tmp, p)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", p.display()));
    }
    Ok(())
}

fn save_patch_at(p: &Path, patch: &Map<String, Value>) -> Result<(), String> {
    update_at(p, |_| Ok(patch.clone()))
}

/// Locks, re-reads the latest document, recursively merges `patch` (arrays replace) and atomically writes it.
pub fn save_patch(patch: &Map<String, Value>) -> Result<(), String> {
    save_patch_at(&path(), patch)
}

fn update_pins_at(p: &Path, edit: impl FnOnce(&mut Vec<PinnedApp>)) -> Result<Vec<PinnedApp>, String> {
    let mut result = Vec::new();
    update_at(p, |doc| {
        let mut pins = pins_from(doc.get("notch").and_then(|n| n.get("pinned_apps")));
        edit(&mut pins);
        let mut seen: Vec<PinnedApp> = Vec::new();
        for pin in pins { if !seen.contains(&pin) { seen.push(pin); } }
        result = seen;
        let arr: Vec<Value> = result.iter().map(PinnedApp::to_json).collect();
        let mut notch = Map::new();
        notch.insert("pinned_apps".into(), Value::Array(arr));
        let mut patch = Map::new();
        patch.insert("notch".into(), Value::Object(notch));
        Ok(patch)
    })?;
    Ok(result)
}

/// Edit the latest saved pins under the config lock (never a stale in-memory copy); returns what was written.
pub fn update_pins(edit: impl FnOnce(&mut Vec<PinnedApp>)) -> Result<Vec<PinnedApp>, String> {
    update_pins_at(&path(), edit)
}

fn toggle_notification_sounds_at(p: &Path) -> Result<bool, String> {
    let mut now = false;
    update_at(p, |doc| {
        now = !NotchSettings::from_json(doc).behavior.notification_sounds;
        let mut behavior = Map::new();
        behavior.insert("notification_sounds".into(), Value::Bool(now));
        let mut notch = Map::new();
        notch.insert("behavior".into(), Value::Object(behavior));
        let mut patch = Map::new();
        patch.insert("notch".into(), Value::Object(notch));
        Ok(patch)
    })?;
    Ok(now)
}

/// Invert only `notch.behavior.notification_sounds` in the latest saved document (under the config lock) and
/// return the value that was written. Every other field is preserved; a malformed document is refused.
pub fn toggle_notification_sounds() -> Result<bool, String> {
    toggle_notification_sounds_at(&path())
}

fn save_app_order_at(p: &Path, order: &[String]) -> Result<(), String> {
    let mut notch = Map::new();
    notch.insert("app_order".into(), Value::Array(order.iter().cloned().map(Value::String).collect()));
    let mut patch = Map::new();
    patch.insert("notch".into(), Value::Object(notch));
    save_patch_at(p, &patch)
}

/// Persist the user's arrangement of the notch app tiles (`notch.app_order`, shortcut keys; arrays replace).
pub fn save_app_order(order: &[String]) -> Result<(), String> {
    save_app_order_at(&path(), order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_order_is_replaced_whole_and_keeps_pins_and_unknown_fields() {
        let d = tmpdir("order");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"keep": 1, "notch": {"future": true, "pinned_apps": [{"desktop_id": "a.desktop", "class": "alpha"}], "app_order": ["x", "y", "z"]}}"#).unwrap();
        save_app_order_at(&p, &["cfg:steam".to_string(), "app:alpha".to_string()]).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["notch"]["app_order"], json!(["cfg:steam", "app:alpha"]));
        assert_eq!((doc["keep"].clone(), doc["notch"]["future"].clone()), (json!(1), json!(true)));
        let s = NotchSettings::from_json(&doc);
        assert_eq!(s.app_order, ["cfg:steam", "app:alpha"]);
        assert_eq!(s.pinned_apps.len(), 1);
    }

    #[test]
    fn toggling_sounds_flips_only_that_flag_and_keeps_everything_else() {
        let d = tmpdir("sounds");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"other": {"keep": [1, 2]}, "notch": {"pinned_apps": [], "behavior": {"notification_sounds": false, "animations": false, "future": "x"}}}"#).unwrap();
        assert_eq!(toggle_notification_sounds_at(&p), Ok(true));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["notch"]["behavior"], json!({"notification_sounds": true, "animations": false, "future": "x"}));
        assert_eq!(doc["other"], json!({"keep": [1, 2]}));
        assert!(NotchSettings::from_json(&doc).behavior.notification_sounds);
        assert_eq!(toggle_notification_sounds_at(&p), Ok(false));
        // a missing document starts from the default (muted) and turns sounds on
        assert_eq!(toggle_notification_sounds_at(&d.join("new.json")), Ok(true));
    }

    #[test]
    fn toggling_sounds_refuses_a_malformed_document_and_leaves_it_untouched() {
        let d = tmpdir("sounds-bad");
        let p = d.join("config.json");
        std::fs::write(&p, "{ not json").unwrap();
        assert!(toggle_notification_sounds_at(&p).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wn-config-{tag}-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_and_wrong_types_use_defaults_and_ranges_clamp() {
        let s = NotchSettings::from_json(&json!({"notch": {
            "appearance": {"opacity": 0.78, "font_size": "big", "corner_radius": 999, "background": "red", "accent": "#aBc123",
                           "font_family": "bad\nfont"},
            "layout": {"expanded_width": 5000, "collapsed_width": 100.4, "top_margin": -3},
            "behavior": {"collapse_delay_ms": 50, "home_reset_secs": 0, "clock_format": "weird", "animations": "no"},
            "monitor": {"mode": "fixed", "output": ""},
        }}));
        assert!(s.to_json()["appearance"].get("opacity").is_none());
        assert_eq!(s.appearance.font_size, 12);
        assert_eq!(s.appearance.corner_radius, 40);
        assert_eq!(s.appearance.background, None);
        assert_eq!(s.appearance.accent.as_deref(), Some("#aBc123"));
        assert_eq!(s.appearance.font_family, None);
        assert_eq!(s.layout.expanded_width, 960);
        assert_eq!(s.layout.collapsed_width, 180);
        assert_eq!(s.layout.top_margin, 0);
        assert_eq!(s.behavior.collapse_delay_ms, 100);
        assert_eq!(s.behavior.home_reset_secs, 0);
        assert_eq!(s.behavior.clock_format, ClockFormat::Locale);
        assert!(s.behavior.animations);
        assert_eq!(s.monitor, MonitorSettings { mode: MonitorMode::Fixed, output: None });
        assert_eq!(NotchSettings::from_json(&json!({})), NotchSettings::default());
        assert_eq!(NotchSettings::from_json(&json!({"notch": s.to_json()})), s);
    }

    #[test]
    fn pins_validate_dedupe_and_survive_concurrent_patches() {
        let s = NotchSettings::from_json(&json!({"notch": {"pinned_apps": [
            {"desktop_id": "a.desktop", "class": "Alpha"}, {"desktop_id": "a.desktop", "class": "alpha"},
            {"desktop_id": "../x.desktop", "class": "x"}, {"desktop_id": "", "class": "x"}, {"desktop_id": "b\n.desktop", "class": "b"},
            {"desktop_id": "c.desktop"}, 5]}}));
        assert_eq!(s.pinned_apps, [PinnedApp { desktop_id: "a.desktop".into(), class: "alpha".into() }]);
        assert!(NotchSettings::from_json(&json!({"notch": {"pinned_apps": "x"}})).pinned_apps.is_empty());
        assert_eq!(NotchSettings::from_json(&json!({"notch": s.to_json()})), s);

        let d = tmpdir("pins");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"keep": 1, "notch": {"unknown": true, "pinned_apps": [{"desktop_id": "a.desktop", "class": "alpha"}]}}"#).unwrap();
        let after = update_pins_at(&p, |v| v.push(PinnedApp::new("b.desktop", "beta").unwrap())).unwrap();
        assert_eq!(after.len(), 2);
        // an unrelated settings patch written afterwards keeps the pins; a pin edit keeps unrelated fields
        save_patch_at(&p, json!({"notch": {"layout": {"collapsed_width": 300}}}).as_object().unwrap()).unwrap();
        update_pins_at(&p, |v| v.retain(|x| x.class != "alpha")).unwrap();
        let doc = read_document_at(&p).unwrap();
        assert_eq!((doc["keep"].as_i64(), doc["notch"]["unknown"].as_bool(), doc["notch"]["layout"]["collapsed_width"].as_i64()), (Some(1), Some(true), Some(300)));
        let st = NotchSettings::from_json(&doc);
        assert_eq!(st.pinned_apps, [PinnedApp { desktop_id: "b.desktop".into(), class: "beta".into() }]);
        assert_eq!(st.layout.collapsed_width, 300);
        assert!(!st.behavior.notification_sounds);
    }

    #[test]
    fn tabs_keep_home_first_controls_last_and_allow_hiding() {
        let t = |v: Value| NotchSettings::from_json(&json!({"notch": {"tabs": v}})).tabs;
        assert_eq!(t(json!(["controls", "ai", "bogus", "home", "ai", "media"])), ["home", "ai", "media", "controls"]);
        assert_eq!(t(json!([])), ["home", "controls"]);
        assert_eq!(t(json!("nope")), NotchSettings::default().tabs);
    }

    #[test]
    fn utilities_dedupe_and_empty_list_is_meaningful() {
        let u = |v: Value| NotchSettings::from_json(&json!({"notch": {"utilities": v}})).utilities;
        assert_eq!(u(json!(["lock", "settings", "lock", "dnd"])), ["lock", "dnd"]);
        assert!(u(json!([])).is_empty());
        assert_eq!(u(json!(5)), NotchSettings::default().utilities);
    }

    #[test]
    fn legacy_opacity_is_ignored_and_preserved_on_save() {
        let d = tmpdir("opacity");
        let p = d.join("config.json");
        std::fs::write(&p, r#"{"sentinel":1,"notch":{"appearance":{"opacity":0.78,"font_size":14,"future":"x"}}}"#).unwrap();
        let doc = read_document_at(&p).unwrap();
        let s = NotchSettings::from_json(&doc);
        assert_eq!(s.appearance.font_size, 14);
        assert!(s.to_json()["appearance"].get("opacity").is_none());
        let patch = json!({"notch": {"appearance": {"font_size": 15}}});
        save_patch_at(&p, patch.as_object().unwrap()).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["sentinel"], 1);
        assert_eq!(v["notch"]["appearance"], json!({"opacity": 0.78, "font_size": 15, "future": "x"}));
    }

    #[test]
    fn explicit_empty_ai_is_none_and_missing_is_all() {
        assert_eq!(ai_from(&json!({"ai": []})), Some(vec![]));
        assert_eq!(ai_from(&json!({"ai": ["claude", 4, "x"]})), Some(vec!["claude".into(), "x".into()]));
        assert_eq!(ai_from(&json!({"ai": "claude"})), None);
        assert_eq!(ai_from(&json!({})), None);
    }

    #[test]
    fn save_merges_recursively_preserves_unknown_and_replaces_arrays() {
        let d = tmpdir("merge");
        let p = d.join("cfg/config.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, r#"{"ai":["claude"],"sentinel":{"keep":1},"notch":{"layout":{"expanded_width":700,"future":true},"tabs":["home","ai","controls"]}}"#).unwrap();
        let patch = json!({"notch": {"layout": {"top_margin": 4}, "tabs": ["home","media","controls"]}});
        save_patch_at(&p, patch.as_object().unwrap()).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["sentinel"]["keep"], 1);
        assert_eq!(v["ai"], json!(["claude"]));
        assert_eq!(v["notch"]["layout"], json!({"expanded_width": 700, "future": true, "top_margin": 4}));
        assert_eq!(v["notch"]["tabs"], json!(["home", "media", "controls"]));
        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap()).unwrap()
            .filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(leftovers.iter().all(|n| n == "config.json" || n == ".config.lock"), "{leftovers:?}");
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn missing_file_saves_and_malformed_file_is_never_overwritten() {
        let d = tmpdir("bad");
        let p = d.join("config.json");
        let patch = json!({"ai": []});
        save_patch_at(&p, patch.as_object().unwrap()).unwrap();
        assert_eq!(read_document_at(&p).unwrap(), json!({"ai": []}));

        std::fs::write(&p, "{ not json").unwrap();
        let err = save_patch_at(&p, patch.as_object().unwrap()).unwrap_err();
        assert!(err.contains(p.to_str().unwrap()), "{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
        std::fs::write(&p, "[1]").unwrap();
        assert!(save_patch_at(&p, patch.as_object().unwrap()).is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "[1]");
    }

    #[test]
    fn concurrent_writers_do_not_lose_each_others_fields() {
        let d = tmpdir("race");
        let p = d.join("config.json");
        let handles: Vec<_> = (0..16).map(|i| {
            let p = p.clone();
            std::thread::spawn(move || {
                let patch = json!({"notch": {"k": {format!("f{i}"): i}}});
                save_patch_at(&p, patch.as_object().unwrap()).unwrap();
            })
        }).collect();
        for h in handles {
            h.join().unwrap();
        }
        let v = read_document_at(&p).unwrap();
        for i in 0..16 {
            assert_eq!(v["notch"]["k"][format!("f{i}")], i);
        }
    }

    #[test]
    fn diff_patch_contains_only_changed_nested_fields() {
        let a = NotchSettings::default();
        let mut b = a.clone();
        b.layout.expanded_width = 720;
        b.appearance.background = Some("#112233".into());
        b.tabs = vec!["home".into(), "controls".into()];
        let p = diff_patch(&a.to_json(), &b.to_json());
        assert_eq!(Value::Object(p), json!({
            "layout": {"expanded_width": 720},
            "appearance": {"background": "#112233"},
            "tabs": ["home", "controls"],
        }));
    }
}
