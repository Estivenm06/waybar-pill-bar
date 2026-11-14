//! Lyrics for the media tab via LRCLIB (lrclib.net, free, no API key).
//! Synced (LRC) lyrics when available, plain text otherwise. Fetched off the UI thread.
use serde_json::Value;
use std::time::Duration;

const API: &str = "https://lrclib.net/api";

#[derive(Clone, Debug, PartialEq)]
pub enum Lyrics {
    Synced(Vec<(f64, String)>),
    Plain(Vec<String>),
    None,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(8))).build().into()
}

fn get_json(path: &str, query: &[(&str, &str)]) -> Option<Value> {
    let mut req = agent().get(format!("{API}/{path}"));
    for (k, v) in query { req = req.query(*k, *v); }
    let mut resp = req.call().ok()?;
    serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()
}

/// Parse LRC text: `[mm:ss.xx] line`, one or more timestamps per line; metadata tags are skipped.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            let Some((m, s)) = tag.split_once(':') else { break };
            let (Ok(m), Ok(s)) = (m.parse::<f64>(), s.parse::<f64>()) else { break };
            stamps.push(m * 60.0 + s);
            rest = stripped[end + 1..].trim_start();
        }
        let line = rest.trim().to_string();
        for t in stamps { out.push((t, line.clone())); }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Index of the line that is current at `pos` seconds (last line whose timestamp has passed).
pub fn current(lines: &[(f64, String)], pos: f64) -> Option<usize> {
    let n = lines.partition_point(|(t, _)| *t <= pos);
    n.checked_sub(1)
}

fn from_record(v: &Value) -> Option<Lyrics> {
    if let Some(s) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(s);
        if !lines.is_empty() { return Some(Lyrics::Synced(lines)); }
    }
    let plain = v["plainLyrics"].as_str().filter(|s| !s.trim().is_empty())?;
    Some(Lyrics::Plain(plain.lines().map(|l| l.trim().to_string()).collect()))
}

/// Exact match first (uses duration for disambiguation), then fuzzy search.
pub fn fetch(artist: &str, title: &str, len_secs: f64) -> Lyrics {
    if title.trim().is_empty() { return Lyrics::None; }
    let dur = format!("{}", len_secs.round() as i64);
    let exact = get_json("get", &[("track_name", title), ("artist_name", artist), ("duration", &dur)]);
    if let Some(l) = exact.as_ref().and_then(from_record) { return l; }
    let Some(list) = get_json("search", &[("track_name", title), ("artist_name", artist)]) else { return Lyrics::None };
    let items = list.as_array().cloned().unwrap_or_default();
    let synced = items.iter().find_map(|v| v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()).map(|_| v));
    if let Some(l) = synced.and_then(from_record) { return l; }
    items.iter().find_map(from_record).unwrap_or(Lyrics::None)
}
