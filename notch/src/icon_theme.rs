//! Minimal freedesktop icon lookup for notification icons given by theme name.

use std::path::PathBuf;

fn roots() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = std::env::var_os("HOME") {
        v.push(PathBuf::from(&h).join(".local/share/icons"));
        v.push(PathBuf::from(&h).join(".icons"));
    }
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(dirs.split(':').filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("icons")));
    v.push(PathBuf::from("/usr/share/pixmaps"));
    v
}

/// Themes searched in order: the current GTK/Omarchy theme if known, then Adwaita and hicolor.
fn themes() -> Vec<String> {
    let mut t = Vec::new();
    if let Ok(out) = std::process::Command::new("gsettings").args(["get", "org.gnome.desktop.interface", "icon-theme"]).output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
        if !s.is_empty() { t.push(s); }
    }
    t.extend(["Adwaita".to_string(), "hicolor".to_string()]);
    t
}

/// Resolve a themed icon name to a raster/SVG-free file (png), preferring the size closest to `size`.
pub fn find(name: &str, size: u32) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains("..") { return None; }
    let sizes = [size, 48, 64, 32, 96, 128, 24, 256, 22, 16];
    for root in roots() {
        let direct = root.join(format!("{name}.png"));
        if direct.is_file() { return Some(direct); }
        for theme in themes() {
            for s in sizes {
                for sub in ["apps", "status", "devices", "mimetypes", "actions", "categories", "places", "legacy"] {
                    for dir in [format!("{s}x{s}/{sub}"), format!("{sub}/{s}")] {
                        let p = root.join(&theme).join(dir).join(format!("{name}.png"));
                        if p.is_file() { return Some(p); }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_paths() {
        assert!(super::find("../etc/passwd", 32).is_none());
        assert!(super::find("a/b", 32).is_none());
        assert!(super::find("", 32).is_none());
    }
}
