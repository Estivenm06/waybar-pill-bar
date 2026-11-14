//! Omarchy theme + font: colors come from `~/.local/state/omarchy/current/theme/colors.toml`,
//! the font from fontconfig (`fc-match monospace`, same source as `omarchy-font-current`).
//! Notch and popups paint with the theme background/foreground; accent, alert and font follow it too.
use crate::config::{AppearanceSettings, NotchSettings};
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;

pub type Rgb = (u8, u8, u8);

const DEF_FONT: &str = "JetBrainsMono Nerd Font";

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub bg: Rgb,
    pub fg: Rgb,
    pub accent: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub font: String,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            bg: (0x0e, 0x12, 0x11),
            fg: (0xf2, 0xf6, 0xf4),
            accent: (0x8f, 0xbc, 0xd8),
            red: (0xe5, 0x48, 0x4d),
            green: (0x98, 0xc3, 0x79),
            yellow: (0xe5, 0xc0, 0x7b),
            font: DEF_FONT.to_string(),
        }
    }
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() { return None; }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let m = |x: u8, y: u8| (x as f64 * (1.0 - t) + y as f64 * t).round().clamp(0.0, 255.0) as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

pub fn hex(c: Rgb) -> String { format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2) }
fn rgba(c: Rgb, a: f64) -> String { format!("rgba({}, {}, {}, {})", c.0, c.1, c.2, a) }

const WHITE: Rgb = (255, 255, 255);
const BLACK: Rgb = (0, 0, 0);
/// App brand colours shown by the Waybar app launchers.
const BRANDS: [(&str, Rgb); 4] = [
    ("discord", (0x58, 0x65, 0xf2)),
    ("steam", (0x66, 0xc0, 0xf4)),
    ("spotify", (0x1e, 0xd7, 0x60)),
    ("whatsapp", (0x25, 0xd3, 0x66)),
];

impl Theme {
    /// Parse flat `key = "#rrggbb"` lines; a missing key or bad value keeps the default for that key.
    pub fn from_colors(text: &str) -> Theme {
        let mut t = Theme::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            let Some(c) = parse_hex(v) else { continue };
            match k.trim() {
                "background" => t.bg = c,
                "foreground" => t.fg = c,
                "accent" => t.accent = c,
                "color1" => t.red = c,
                "color2" => t.green = c,
                "color3" => t.yellow = c,
                _ => {}
            }
        }
        t
    }

    pub fn load() -> Theme {
        let mut t = colors_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| Theme::from_colors(&s))
            .unwrap_or_default();
        if let Some(f) = system_font() { t.font = f; }
        t
    }

    pub fn font_stack(&self) -> String {
        format!("\"{}\", \"Symbols Nerd Font\", \"{}\", monospace", self.font, DEF_FONT)
    }

    /// Pole that "brighter" means on this theme: white on dark backgrounds, black on light ones.
    fn contrast(&self) -> Rgb { if self.is_light() { BLACK } else { WHITE } }
    pub fn accent_hi(&self) -> Rgb { mix(self.accent, self.contrast(), 0.45) }
    pub fn accent_lo(&self) -> Rgb { mix(self.accent, BLACK, 0.25) }
    pub fn accent_top(&self) -> Rgb { mix(self.accent, self.contrast(), 0.65) }
    pub fn alert_hi(&self) -> Rgb { mix(self.red, self.contrast(), 0.45) }
    pub fn is_light(&self) -> bool {
        let (r, g, b) = (self.bg.0 as u32, self.bg.1 as u32, self.bg.2 as u32);
        (299 * r + 587 * g + 114 * b) / 1000 >= 128
    }
    /// Stronger-contrast text: foreground pushed toward white (dark theme) or black (light theme).
    pub fn fg_hi(&self) -> Rgb { mix(self.fg, self.contrast(), 0.35) }

    /// Replace the `{{...}}` placeholders. Plain string replace so it also works inside `alpha(...)`.
    pub fn paint(&self, css: &str) -> String {
        css.replace("{{bg}}", &hex(self.bg))
            .replace("{{fg-hi}}", &hex(self.fg_hi()))
            .replace("{{fg}}", &hex(self.fg))
            .replace("{{accent-hi}}", &hex(self.accent_hi()))
            .replace("{{accent-lo}}", &hex(self.accent_lo()))
            .replace("{{accent-top}}", &hex(self.accent_top()))
            .replace("{{accent}}", &hex(self.accent))
            .replace("{{alert-hi}}", &hex(self.alert_hi()))
            .replace("{{alert}}", &hex(self.red))
            .replace("{{glass-bg}}", &rgba(self.bg, 0.78))
            .replace("{{glass-edge}}", &rgba(self.fg, 0.16))
            .replace("{{glass-sheen}}", &rgba(self.contrast(), 0.06))
            .replace("{{glass-shadow}}", &rgba(BLACK, if self.is_light() { 0.10 } else { 0.22 }))
            .replace("{{font}}", &self.font_stack())
    }

    /// GTK CSS for Waybar (`~/.config/waybar/pill-theme.css`, imported by style.css).
    pub fn waybar_css(&self) -> String {
        let mut s = String::new();
        let mut def = |name: &str, val: String| s.push_str(&format!("@define-color {name} {val};\n"));
        def("pill-bg", rgba(self.bg, 0.78));
        def("pill-bg-hover", rgba(mix(self.bg, self.fg, 0.08), 0.86));
        def("pill-border", rgba(self.fg, 0.16));
        def("pill-sheen", rgba(self.contrast(), 0.06));
        def("text-color", hex(self.fg));
        def("text-dim", rgba(self.fg, 0.66));
        def("active-bg", rgba(self.fg, 0.18));
        def("tooltip-bg", rgba(mix(self.bg, self.fg, 0.04), 0.88));
        def("warn", hex(self.yellow));
        def("crit", hex(self.red));
        def("good", hex(self.green));
        def("accent", hex(self.accent));
        def("accent-hi", hex(self.accent_hi()));
        def("pill-shadow", hex(self.bg));
        for (name, c) in BRANDS {
            def(&format!("brand-{name}"), hex(if self.is_light() { mix(c, BLACK, 0.25) } else { c }));
            def(&format!("brand-{name}-hi"), hex(mix(c, self.contrast(), 0.5)));
        }
        s.push_str(&format!("* {{ font-family: {}; }}\n", self.font_stack()));
        s
    }
}

fn colors_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("omarchy/current/theme/colors.toml"))
}

fn system_font() -> Option<String> {
    let out = Command::new("fc-match").args(["monospace", "-f", "%{family}\n"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let f = s.lines().next()?.split(',').next()?.trim().replace('"', "");
    if f.is_empty() { None } else { Some(f) }
}

thread_local! {
    static SYS: RefCell<Theme> = RefCell::new(Theme::load());
    static APP: RefCell<AppearanceSettings> = RefCell::new(NotchSettings::default().appearance);
    static CUR: RefCell<Theme> = RefCell::new(Theme::load());
}

/// System theme with the notch-only overrides applied (Waybar's generated CSS keeps using the system theme).
fn effective(sys: &Theme, a: &AppearanceSettings) -> Theme {
    let mut t = sys.clone();
    let hex = |o: &Option<String>| o.as_deref().and_then(parse_hex);
    if let Some(c) = hex(&a.background) { t.bg = c; }
    if let Some(c) = hex(&a.foreground) { t.fg = c; }
    if let Some(c) = hex(&a.accent) { t.accent = c; }
    if let Some(f) = &a.font_family { t.font = f.replace(['\\', '"'], ""); }
    t
}

fn recompute() {
    let t = SYS.with(|s| APP.with(|a| effective(&s.borrow(), &a.borrow())));
    CUR.with(|c| *c.borrow_mut() = t);
}

pub fn current() -> Theme { CUR.with(|c| c.borrow().clone()) }
pub fn accent() -> Rgb { CUR.with(|c| c.borrow().accent) }
pub fn fg() -> Rgb { CUR.with(|c| c.borrow().fg) }
/// Re-read the system theme, keep the notch overrides.
pub fn reload() { let t = Theme::load(); SYS.with(|s| *s.borrow_mut() = t); recompute(); }
/// Apply the notch's own appearance preferences.
pub fn configure(a: &AppearanceSettings) { APP.with(|x| *x.borrow_mut() = a.clone()); recompute(); }

/// Multiply every `font-size: Npx` by `factor` (borders, radii and layout px stay as they are).
pub fn scale_font_sizes(css: &str, factor: f64) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("font-size:") {
        let (head, tail) = rest.split_at(i + "font-size:".len());
        out.push_str(head);
        let t = tail.trim_start();
        let ws = &tail[..tail.len() - t.len()];
        let end = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
        if end > 0 && t[end..].starts_with("px") {
            out.push_str(ws);
            out.push_str(&format!("{:.2}", t[..end].parse::<f64>().unwrap_or(0.0) * factor));
            rest = &t[end..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// The notch/OSD/notification stylesheet for the current preferences.
pub fn notch_css(css: &str) -> String {
    let a = APP.with(|a| a.borrow().clone());
    let t = current();
    let scaled = scale_font_sizes(css, a.font_size as f64 / 12.0);
    t.paint(&scaled)
        .replace("{{notch-bg}}", &rgba(t.bg, a.opacity))
        .replace("{{notch-radius}}", &a.corner_radius.to_string())
}

/// Write `~/.config/waybar/pill-theme.css` atomically: system theme colors, with the saved notch font family (if any)
/// shared by the pills. `None` follows the Omarchy system font.
pub fn write_waybar_theme(font_family: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let dir = PathBuf::from(home).join(".config/waybar");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut t = Theme::load();
    if let Some(f) = font_family.filter(|f| crate::config::valid_font_family(f)) { t.font = f.replace(['\\', '"'], ""); }
    let tmp = dir.join(format!("pill-theme.css.{}.{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let res = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(t.waybar_css().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, dir.join("pill-theme.css"))
    })();
    res.map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("{}: {e}", tmp.display()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors_toml() {
        let t = Theme::from_colors("accent = \"#85abbc\"\nbackground = \"#232829\"\nforeground = \"#dcd6d6\"\n");
        assert_eq!(t.accent, (0x85, 0xab, 0xbc));
        assert_eq!(t.bg, (0x23, 0x28, 0x29));
        assert_eq!(t.fg, (0xdc, 0xd6, 0xd6));
        assert_eq!(t.paint("{{accent}}"), "#85abbc");
    }

    #[test]
    fn glass_tokens_follow_palette() {
        let dark = Theme::from_colors("background = \"#232829\"\nforeground = \"#dcd6d6\"\n");
        let light = Theme::from_colors("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\n");
        let css = "{{glass-bg}}|{{glass-edge}}|{{glass-sheen}}|{{glass-shadow}}";
        let (d, l) = (dark.paint(css), light.paint(css));
        assert!(!d.contains("{{") && !l.contains("{{"));
        assert!(d.contains("255, 255, 255") && l.contains("0, 0, 0, 0.06"), "{d} {l}");
        assert!(dark.waybar_css().contains("pill-sheen"));
    }

    #[test]
    fn mix_midpoint() {
        assert_eq!(mix((0, 0, 0), (255, 255, 255), 0.5), (128, 128, 128));
    }

    #[test]
    fn bad_value_falls_back_per_key() {
        let t = Theme::from_colors("accent = \"red\"\nbackground = \"#232829\"\n");
        assert_eq!(t.accent, Theme::default().accent);
        assert_eq!(t.bg, (0x23, 0x28, 0x29));
    }

    #[test]
    fn paint_fg_tokens_dark() {
        let t = Theme::from_colors("background = \"#232829\"\nforeground = \"#dcd6d6\"\n");
        assert_eq!(t.paint("{{bg}} {{fg}} {{fg-hi}}"), "#232829 #dcd6d6 #e8e4e4");
        assert!(!t.is_light());
    }

    #[test]
    fn paint_fg_hi_light() {
        let t = Theme::from_colors("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\n");
        assert!(t.is_light());
        assert_eq!(t.paint("{{fg-hi}}"), "#313344");
    }

    #[test]
    fn font_sizes_scale_but_other_px_values_do_not() {
        let css = ".a { font-size: 12px; border-radius: 10px; min-height: 20px; }\n.b { font-size:10px; }";
        let out = scale_font_sizes(css, 1.5);
        assert_eq!(out, ".a { font-size: 18.00px; border-radius: 10px; min-height: 20px; }\n.b { font-size:15.00px; }");
        assert_eq!(scale_font_sizes(css, 1.0).matches("12.00px").count(), 1);
    }
}
