//! Hyprland IPC: the request socket (`.socket.sock`), the event socket (`.socket2.sock`) and the dispatch helpers.
//!
//! This Hyprland runs a Lua config, so dispatchers are Lua expressions (`hl.dsp.focus({ ... })`), not the old
//! `focuswindow ...` words.
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

const REQ_TIMEOUT: Duration = Duration::from_millis(800);

/// `$XDG_RUNTIME_DIR` (`/tmp` when unset).
pub fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
}

/// Connects to the live instance's socket (`.socket.sock` / `.socket2.sock`). `$HYPRLAND_INSTANCE_SIGNATURE` is tried
/// first, then every instance directory newest-first; a directory only counts if its socket accepts a connection
/// (a crashed compositor leaves its directory behind).
pub fn open(sock: &str) -> Option<(String, UnixStream)> {
    let rt = runtime_dir();
    let try_sig = |sig: &str| UnixStream::connect(format!("{rt}/hypr/{sig}/{sock}")).ok().map(|s| (sig.to_string(), s));
    if let Some(found) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty()).and_then(|s| try_sig(&s)) {
        return Some(found);
    }
    let mut ents: Vec<_> = std::fs::read_dir(format!("{rt}/hypr")).ok()?.flatten().collect();
    ents.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    ents.iter().find_map(|e| try_sig(&e.file_name().to_string_lossy()))
}

/// Signature of the live Hyprland instance, if one answers.
pub fn sig() -> Option<String> {
    open(".socket.sock").map(|(s, _)| s)
}

/// One request/answer round trip (`j/monitors`, `dispatch ...`, ...). Blocking: call it off the UI thread.
pub fn req(cmd: &str) -> Option<String> {
    let (_, mut s) = open(".socket.sock")?;
    s.set_read_timeout(Some(REQ_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(REQ_TIMEOUT)).ok()?;
    s.write_all(cmd.as_bytes()).ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

pub fn json(cmd: &str) -> Result<Value, String> {
    let text = req(cmd).ok_or_else(|| format!("hyprland request {cmd} failed"))?;
    serde_json::from_str(&text).map_err(|e| format!("hyprland {cmd}: {e}"))
}

/// Run a Lua dispatcher expression; Hyprland answers `ok` on success.
pub fn dispatch(expr: &str) -> Result<(), String> {
    let answer = req(&format!("dispatch {expr}")).ok_or_else(|| format!("hyprland dispatch {expr} failed"))?;
    if answer.trim() == "ok" { Ok(()) } else { Err(format!("hyprland dispatch {expr}: {}", answer.trim())) }
}

fn plain(s: &str, extra: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

/// Focus a window by its `j/clients` address (`0x...`).
pub fn focus_window(address: &str) -> Result<(), String> {
    let hex = address.strip_prefix("0x").unwrap_or(address);
    if !plain(hex, "") { return Err(format!("invalid window address {address:?}")); }
    dispatch(&format!("hl.dsp.focus({{ window = \"address:{address}\" }})"))
}

pub fn focus_workspace(id: i64) -> Result<(), String> {
    dispatch(&format!("hl.dsp.focus({{ workspace = {id} }})"))
}

pub fn toggle_special(name: &str) -> Result<(), String> {
    if !plain(name, "_-") { return Err(format!("invalid special workspace name {name:?}")); }
    dispatch(&format!("hl.dsp.workspace.toggle_special(\"{name}\")"))
}

#[derive(Debug, PartialEq)]
pub enum Ev {
    /// The event socket (re)connected: anything may have changed while it was down.
    Connected,
    /// One `name>>data` line accepted by the filter.
    Line(String),
}

/// Follow the event socket from a background thread; reconnects every 2s after the compositor goes away.
/// Only lines accepted by `keep` are sent. The thread ends when the receiver is dropped.
pub fn subscribe(keep: fn(&str) -> bool) -> mpsc::Receiver<Ev> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("hypr-events".into()).spawn(move || {
        loop {
            if let Some((_, stream)) = open(".socket2.sock") {
                if tx.send(Ev::Connected).is_err() { return; }
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if keep(&line) && tx.send(Ev::Line(line)).is_err() { return; }
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }).expect("spawn hypr-events");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_arguments_are_validated_before_they_reach_the_socket() {
        assert!(focus_window("0x55; rm").is_err());
        assert!(focus_window("").is_err());
        assert!(toggle_special("a\"b").is_err());
        assert!(toggle_special("").is_err());
    }
}
