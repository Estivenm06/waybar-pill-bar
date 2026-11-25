//! "Update available" check. A few minutes after start and then once a day, the installed `manifest.json` version
//! is compared with the one on the repository's `main` branch. A newer version raises one notification per version
//! (remembered in the state directory) that names the update command. Only the public manifest is requested: no
//! identifier, no usage data. Opt out with `"notch": {"behavior": {"check_updates": false}}` in the config file.
use crate::wayland::Poster;
use std::path::PathBuf;
use std::time::Duration;

const MANIFEST_URL: &str = "https://raw.githubusercontent.com/Estivenm06/waybar-pill-bar/main/manifest.json";
const MAX_BYTES: u64 = 64 * 1024;
const FIRST_DELAY: Duration = Duration::from_secs(300);
const INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// `major.minor.patch` with an optional `-suffix`; anything else is not a version.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().split(['-', '+']).next()?;
    let mut it = core.split('.');
    let mut n = || it.next()?.parse::<u64>().ok();
    let v = (n()?, n()?, n()?);
    it.next().is_none().then_some(v)
}

pub fn is_newer(local: &str, remote: &str) -> bool {
    matches!((parse_version(local), parse_version(remote)), (Some(l), Some(r)) if r > l)
}

fn manifest_version(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text).ok()?.get("version")?.as_str().map(str::to_string)
}

fn enabled() -> bool {
    crate::config::read_document().ok()
        .and_then(|d| d.get("notch")?.get("behavior")?.get("check_updates")?.as_bool())
        .unwrap_or(true)
}

fn checkout_manifest() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("omarchy/plugins/waybar.pill-bar/manifest.json"))
}

fn notified_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("waybar-notch/update-notified"))
}

fn fetch_remote() -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build().into();
    let mut resp = agent.get(MANIFEST_URL).call().ok()?;
    let text = resp.body_mut().with_config().limit(MAX_BYTES).read_to_string().ok()?;
    manifest_version(&text)
}

/// One check. Returns the newer remote version when it should be announced (not already announced).
fn check_once() -> Option<(String, String)> {
    let local = manifest_version(&std::fs::read_to_string(checkout_manifest()?).ok()?)?;
    let remote = fetch_remote()?;
    if !is_newer(&local, &remote) { return None; }
    let marker = notified_path()?;
    if std::fs::read_to_string(&marker).is_ok_and(|s| s.trim() == remote) { return None; }
    if let Some(dir) = marker.parent() { let _ = std::fs::create_dir_all(dir); }
    std::fs::write(&marker, format!("{remote}\n")).ok()?;
    Some((local, remote))
}

pub fn start(_poster: &Poster) {
    std::thread::Builder::new().name("update-check".into()).spawn(|| {
        std::thread::sleep(FIRST_DELAY);
        loop {
            if enabled() {
                if let Some((local, remote)) = check_once() {
                    crate::apps::run_detached("notify-send", &["--app-name=Waybar Pill Bar", "-u", "low",
                        "Pill Bar update available",
                        &format!("Version {remote} is available (installed {local}). Run ~/.config/omarchy/plugins/waybar.pill-bar/update.sh")]);
                }
            }
            std::thread::sleep(INTERVAL);
        }
    }).ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.2.0", "0.10.0"));
        assert!(is_newer("0.2.0", "1.0.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("0.2.0-beta", "0.2.1"));
    }

    #[test]
    fn malformed_versions_never_announce() {
        for bad in ["", "x", "1.2", "1.2.3.4", "1.x.3", "-1.0.0"] {
            assert!(!is_newer("0.2.0", bad), "{bad}");
            assert!(!is_newer(bad, "9.9.9"), "{bad}");
        }
        assert_eq!(manifest_version(r#"{"version":"0.3.1"}"#).as_deref(), Some("0.3.1"));
        assert_eq!(manifest_version(r#"{"version":3}"#), None);
        assert_eq!(manifest_version("not json"), None);
    }
}
