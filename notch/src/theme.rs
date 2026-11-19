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

const WHITE: Rgb = (255, 255, 255);
const BLACK: Rgb = (0, 0, 0);
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

/// System theme with the notch-only overrides applied.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors_toml() {
        let t = Theme::from_colors("accent = \"#85abbc\"\nbackground = \"#232829\"\nforeground = \"#dcd6d6\"\n");
        assert_eq!(t.accent, (0x85, 0xab, 0xbc));
        assert_eq!(t.bg, (0x23, 0x28, 0x29));
        assert_eq!(t.fg, (0xdc, 0xd6, 0xd6));
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
    fn fg_hi_pushes_toward_contrast_pole() {
        let d = Theme::from_colors("background = \"#232829\"\nforeground = \"#dcd6d6\"\n");
        assert!(!d.is_light());
        assert_eq!(hex(d.fg_hi()), "#e8e4e4");
        let l = Theme::from_colors("background = \"#eff1f5\"\nforeground = \"#4c4f69\"\n");
        assert!(l.is_light());
        assert_eq!(hex(l.fg_hi()), "#313344");
    }
}
