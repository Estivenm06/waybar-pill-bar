//! Lyrics for the media tab via LRCLIB (lrclib.net, free, no API key).
//! Synced (LRC) lyrics when available, plain text otherwise. Fetched off the UI thread.
use serde_json::Value;
use std::time::Duration;

const API: &str = "https://lrclib.net/api";
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// Bounds on parsed lyrics: timestamped entries and aggregate UTF-8 lyric text.
const MAX_ENTRIES: usize = 4096;
const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Lyrics {
    Synced(Vec<(f64, String)>),
    Plain(Vec<String>),
    #[default]
    None,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(8))).build().into()
}

fn get_json(path: &str, query: &[(&str, &str)]) -> Option<Value> {
    let mut req = agent().get(format!("{API}/{path}"));
    for (k, v) in query { req = req.query(*k, *v); }
    let mut resp = req.call().ok()?;
    let text = resp.body_mut().with_config().limit(MAX_RESPONSE_BYTES).read_to_string().ok()?;
    serde_json::from_str(&text).ok()
}

/// Parse LRC text: `[mm:ss.xx] line`, one or more timestamps per line; metadata tags are skipped.
/// Any limit violation (4096 entries, 1 MiB of aggregate lyric text) returns an empty result so the caller
/// falls back to plain lyrics. The repeated text is validated before anything is cloned.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out: Vec<(f64, String)> = Vec::new();
    let mut bytes = 0usize;
    for raw in text.lines() {
        let mut rest = raw.trim_start();
        let mut stamps: Vec<f64> = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            let tag = &stripped[..end];
            let Some((m, s)) = tag.split_once(':') else { break };
            let (Ok(m), Ok(s)) = (m.parse::<f64>(), s.parse::<f64>()) else { break };
            let t = m * 60.0 + s;
            if t.is_finite() && t >= 0.0 {
                if out.len() + stamps.len() >= MAX_ENTRIES { return Vec::new(); }
                stamps.push(t);
            }
            rest = stripped[end + 1..].trim_start();
        }
        let line = rest.trim();
        let Some(add) = stamps.len().checked_mul(line.len()) else { return Vec::new() };
        let Some(total) = bytes.checked_add(add) else { return Vec::new() };
        if total > MAX_TEXT_BYTES { return Vec::new(); }
        bytes = total;
        for t in stamps { out.push((t, line.to_string())); }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// Index of the line that is current at `pos` seconds (last line whose timestamp has passed).
pub fn current(lines: &[(f64, String)], pos: f64) -> Option<usize> {
    let n = lines.partition_point(|(t, _)| *t <= pos);
    n.checked_sub(1)
}

/// Plain fallback under the same bounds as synced lyrics; over-limit text yields no lyrics.
fn plain_lines(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for l in text.lines() {
        let l = l.trim();
        bytes = bytes.checked_add(l.len())?;
        if out.len() >= MAX_ENTRIES || bytes > MAX_TEXT_BYTES { return None; }
        out.push(l.to_string());
    }
    Some(out)
}

fn from_record(v: &Value) -> Option<Lyrics> {
    if let Some(s) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(s);
        if !lines.is_empty() { return Some(Lyrics::Synced(lines)); }
    }
    let plain = v["plainLyrics"].as_str().filter(|s| !s.trim().is_empty())?;
    plain_lines(plain).map(Lyrics::Plain)
}

/// Exact match first (uses duration for disambiguation), then fuzzy search.
pub fn fetch(artist: &str, title: &str, len_secs: f64) -> Lyrics {
    if title.trim().is_empty() { return Lyrics::None; }
    let dur = format!("{}", len_secs.round() as i64);
    let exact = get_json("get", &[("track_name", title), ("artist_name", artist), ("duration", &dur)]);
    if let Some(l) = exact.as_ref().and_then(from_record) { return l; }
    let Some(list) = get_json("search", &[("track_name", title), ("artist_name", artist)]) else { return Lyrics::None };
    let items: &[Value] = list.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let synced = items.iter().find_map(|v| v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()).map(|_| v));
    if let Some(l) = synced.and_then(from_record) { return l; }
    items.iter().find_map(from_record).unwrap_or(Lyrics::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize) -> String { (0..n).map(|i| format!("[00:{:02}.00]x\n", i % 60)).collect() }

    #[test]
    fn normal_lrc_is_sorted_with_multi_timestamps() {
        let v = parse_lrc("[ar:Someone]\n[00:20.00]second\n[00:10.50][00:30.00] chorus \nplain words\n");
        let got: Vec<_> = v.iter().map(|(t, l)| (*t, l.as_str())).collect();
        assert_eq!(got, vec![(10.5, "chorus"), (20.0, "second"), (30.0, "chorus")]);
        assert_eq!(current(&v, 25.0), Some(1));
        assert_eq!(current(&v, 1.0), None);
    }

    #[test]
    fn nonfinite_and_negative_timestamps_are_ignored() {
        let v = parse_lrc("[inf:00]a\n[NaN:1]b\n[-1:00]c\n[01:1e400]d\n[00:05.00]ok\n");
        assert_eq!(v, vec![(5.0, "ok".to_string())]);
    }

    #[test]
    fn entry_limit_is_exact() {
        assert_eq!(parse_lrc(&lines(MAX_ENTRIES)).len(), MAX_ENTRIES);
        assert!(parse_lrc(&lines(MAX_ENTRIES + 1)).is_empty());
    }

    #[test]
    fn timestamp_heavy_single_line_is_bounded_before_expansion() {
        let stamps = "[00:01.00]".repeat(MAX_ENTRIES);
        assert_eq!(parse_lrc(&format!("{stamps}x")).len(), MAX_ENTRIES);
        let over = "[00:01.00]".repeat(MAX_ENTRIES + 1);
        assert!(parse_lrc(&format!("{over}x")).is_empty());
        // a small input whose expansion would be enormous is rejected without building it
        let huge_line = "y".repeat(MAX_TEXT_BYTES / 2 + 1);
        let two = format!("[00:01.00][00:02.00]{huge_line}");
        assert!(parse_lrc(&two).is_empty());
    }

    #[test]
    fn text_limit_is_exact_on_one_line() {
        let half = "y".repeat(MAX_TEXT_BYTES / 2);
        let ok = parse_lrc(&format!("[00:01.00][00:02.00]{half}"));
        assert_eq!(ok.len(), 2);
        assert_eq!(ok.iter().map(|(_, l)| l.len()).sum::<usize>(), MAX_TEXT_BYTES);
        let over = "y".repeat(MAX_TEXT_BYTES / 2 + 1);
        assert!(parse_lrc(&format!("[00:01.00][00:02.00]{over}")).is_empty());
    }

    #[test]
    fn aggregate_text_across_lines_is_bounded() {
        let quarter = "z".repeat(MAX_TEXT_BYTES / 4);
        let mut text = String::new();
        for i in 0..4 { text.push_str(&format!("[00:0{i}.00]{quarter}\n")); }
        assert_eq!(parse_lrc(&text).len(), 4);
        text.push_str("[00:09.00]q\n");
        assert!(parse_lrc(&text).is_empty());
    }

    #[test]
    fn over_limit_synced_falls_back_to_plain() {
        let rec = serde_json::json!({ "syncedLyrics": lines(MAX_ENTRIES + 1), "plainLyrics": "one\ntwo" });
        assert_eq!(from_record(&rec), Some(Lyrics::Plain(vec!["one".into(), "two".into()])));
        let ok = serde_json::json!({ "syncedLyrics": "[00:01.00]hi", "plainLyrics": "ignored" });
        assert_eq!(from_record(&ok), Some(Lyrics::Synced(vec![(1.0, "hi".into())])));
    }

    #[test]
    fn plain_fallback_limits_apply() {
        let n = "a\n".repeat(MAX_ENTRIES);
        assert!(matches!(from_record(&serde_json::json!({ "plainLyrics": n })), Some(Lyrics::Plain(v)) if v.len() == MAX_ENTRIES));
        let over = "a\n".repeat(MAX_ENTRIES + 1);
        assert_eq!(from_record(&serde_json::json!({ "plainLyrics": over })), None);
        let exact = "b".repeat(MAX_TEXT_BYTES);
        assert!(matches!(from_record(&serde_json::json!({ "plainLyrics": exact })), Some(Lyrics::Plain(_))));
        let big = "b".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(from_record(&serde_json::json!({ "plainLyrics": big })), None);
    }
}
