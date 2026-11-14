//! AI usage tab, live: Claude Code tokens (session logs), Gemini / Antigravity prompts (history),
//! OMP tokens (session logs) and Copilot CLI tokens (sqlite db). `Tracker` tails the logs incrementally, so every poll reads only new data.
use chrono::{DateTime, Local, NaiveDate, Utc};
use gtk::{cairo, prelude::*};
use gtk4 as gtk;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

/// A provider counts as "live" when its newest record is younger than this.
const LIVE_SECS: i64 = 90;

#[derive(Default, Clone, PartialEq)]
pub struct Usage {
    pub c5h: u64,
    pub ctoday: u64,
    pub c7d: u64,
    pub cdays: [u64; 7],      // oldest .. today
    pub cmodels: Vec<(String, u64)>,
    pub cmsgs: u64,           // assistant messages today
    pub gtoday: u64,
    pub g7d: u64,
    pub gtotal: u64,
    pub gdays: [u64; 7],
    pub otoday: u64,
    pub o7d: u64,
    pub odays: [u64; 7],
    pub omodels: Vec<(String, u64)>,
    pub cbreak: [u64; 4],     // 7d: input, output, cache write, cache read
    pub obreak: [u64; 4],     // 7d: input, output, cache write, cache read
    pub ocost7d: f64,
    pub ptoday: u64,          // Copilot CLI, same meaning as the o* fields (no cost)
    pub p7d: u64,
    pub pdays: [u64; 7],
    pub pmodels: Vec<(String, u64)>,
    pub pbreak: [u64; 4],
    pub has_claude: bool,
    pub has_gemini: bool,
    pub has_omp: bool,
    pub has_copilot: bool,
    pub c_live: bool,
    pub g_live: bool,
    pub o_live: bool,
    pub p_live: bool,
}

pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn day_slot(d: NaiveDate, today: NaiveDate) -> Option<usize> {
    let ago = (today - d).num_days();
    if (0..7).contains(&ago) { Some(6 - ago as usize) } else { None }
}

fn jsonl_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && depth > 0 {
            jsonl_files(&p, depth - 1, out);
        } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
            let recent = p.metadata().and_then(|m| m.modified()).map(|t| {
                t.elapsed().map(|age| age.as_secs() < 7 * 86400).unwrap_or(false)
            }).unwrap_or(true);
            if recent { out.push(p); }
        }
    }
}

/// Where each provider keeps its logs on this machine.
#[derive(Default, Clone)]
pub struct Sources {
    pub claude: Vec<PathBuf>,
    pub gemini: Option<PathBuf>,
    pub omp: Option<PathBuf>,
    pub copilot: Option<PathBuf>,
}

fn has_sqlite3() -> bool {
    std::env::var_os("PATH").map_or(false, |p| std::env::split_paths(&p).any(|d| d.join("sqlite3").is_file()))
}

impl Sources {
    /// Cheap `exists()` probes, safe to call on every poll: a newly installed tool shows up without a restart.
    /// Only providers the user picked in install.sh (config.json `ai`) are included; no choice = all detected.
    pub fn detect() -> Sources {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let sel = crate::config::ai_selection();
        let on = |id: &str| sel.as_ref().map_or(true, |s| s.iter().any(|x| x == id));
        let dirs: Vec<PathBuf> = match std::env::var("CLAUDE_CONFIG_DIR") {
            Ok(v) if !v.trim().is_empty() => v.split(',').map(|d| Path::new(d.trim()).join("projects")).collect(),
            _ => vec![home.join(".config/claude/projects"), home.join(".claude/projects")],
        };
        let gemini = home.join(".gemini/antigravity-cli/history.jsonl");
        let omp = home.join(".omp/agent/sessions");
        let copilot = home.join(".copilot/session-store.db");
        Sources {
            claude: if on("claude") { dirs.into_iter().filter(|d| d.is_dir()).collect() } else { Vec::new() },
            gemini: (on("gemini") && gemini.is_file()).then_some(gemini),
            omp: (on("omp") && omp.is_dir()).then_some(omp),
            copilot: (on("copilot") && copilot.is_file() && has_sqlite3()).then_some(copilot),
        }
    }
}

/// Providers whose usage source exists on this machine, whatever the saved selection (same probes as `Sources::detect`).
pub fn detected_providers() -> Vec<(&'static str, &'static str)> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let claude = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(v) if !v.trim().is_empty() => v.split(',').any(|d| Path::new(d.trim()).join("projects").is_dir()),
        _ => home.join(".config/claude/projects").is_dir() || home.join(".claude/projects").is_dir(),
    };
    let mut out = Vec::new();
    if claude { out.push(("claude", "Claude Code")); }
    if home.join(".gemini/antigravity-cli/history.jsonl").is_file() { out.push(("gemini", "Gemini / Antigravity")); }
    if home.join(".omp/agent/sessions").is_dir() { out.push(("omp", "OMP")); }
    if home.join(".copilot/session-store.db").is_file() { out.push(("copilot", "Copilot CLI")); }
    out
}

/// Tools that leave usage data we do not read yet.
pub fn unsupported_found() -> Vec<&'static str> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    [("Codex", ".codex"), ("OpenCode", ".local/share/opencode"), ("Cursor", ".cursor"), ("Cline", ".cline")]
        .iter().filter(|(_, p)| home.join(p).exists()).map(|(n, _)| *n).collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind { Claude, Gemini, Omp, Copilot }

struct Rec {
    ts: DateTime<Local>,
    model: String,
    tokens: u64,
    br: [u64; 4], // input, output, cache write, cache read
    cost: f64,
}

#[derive(Default)]
struct FileState {
    offset: u64,                    // end of the last complete line already consumed (Copilot: last row id read)
    recs: HashMap<String, Rec>,     // Claude / OMP / Copilot: message or row id -> record (later lines replace earlier ones)
    gem: Vec<DateTime<Local>>,      // Gemini: prompt timestamps of the last 7 days
    gcount: u64,                    // Gemini: all-time prompt count
    stamp: Option<(std::time::SystemTime, std::time::SystemTime)>, // Copilot: db and wal mtimes at the last query
}

fn parse_claude(line: &str) -> Option<(String, Rec)> {
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") { return None; }
    let o: Value = serde_json::from_str(line).ok()?;
    if o["type"] != "assistant" { return None; }
    let m = &o["message"];
    let (id, ts) = (m["id"].as_str()?, o["timestamp"].as_str()?);
    let model = m["model"].as_str().unwrap_or("");
    if model.is_empty() || model.starts_with('<') { return None; }
    let ts = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Local);
    let us = &m["usage"];
    let g = |k: &str| us[k].as_u64().unwrap_or(0);
    let br = [g("input_tokens"), g("output_tokens"), g("cache_creation_input_tokens"), g("cache_read_input_tokens")];
    Some((id.to_string(), Rec { ts, model: model.to_string(), tokens: br[0] + br[1] + br[2], br, cost: 0.0 }))
}

fn parse_omp(line: &str) -> Option<(String, Rec)> {
    if !line.contains("\"usage\"") { return None; }
    let o: Value = serde_json::from_str(line).ok()?;
    let (us, model) = match o["type"].as_str()? {
        "message" if o["message"]["role"] == "assistant" => (&o["message"]["usage"], &o["message"]["model"]),
        "model_usage" => (&o["usage"], &o["model"]),
        _ => return None,
    };
    if !us.is_object() { return None; }
    let (id, ts) = (o["id"].as_str()?, o["timestamp"].as_str()?);
    let ts = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Local);
    let g = |k: &str| us[k].as_u64().unwrap_or(0);
    let br = [g("input"), g("output"), g("cacheWrite"), g("cacheRead")];
    Some((id.to_string(), Rec {
        ts,
        model: model.as_str().unwrap_or("").to_string(),
        tokens: br[0] + br[1] + br[2],
        br,
        cost: us["cost"]["total"].as_f64().unwrap_or(0.0),
    }))
}

fn parse_gemini(line: &str) -> Option<DateTime<Local>> {
    let o: Value = serde_json::from_str(line).ok()?;
    Some(DateTime::from_timestamp_millis(o["timestamp"].as_i64()?)?.with_timezone(&Local))
}

/// Consume the complete lines appended since the last call. A partial trailing line is left for the next poll;
/// a shrunk file (truncated / rotated) is dropped and re-read from the start.
fn ingest(path: &Path, kind: Kind, st: &mut FileState) {
    let Ok(mut f) = std::fs::File::open(path) else { return };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return };
    if len < st.offset { *st = FileState::default(); }
    if len == st.offset { return; }
    if f.seek(SeekFrom::Start(st.offset)).is_err() { return; }
    let mut buf = Vec::with_capacity((len - st.offset) as usize);
    if f.take(len - st.offset).read_to_end(&mut buf).is_err() { return; }
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else { return };
    for raw in buf[..end].split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(raw) else { continue };
        match kind {
            Kind::Claude => if let Some((id, r)) = parse_claude(line) { st.recs.insert(id, r); },
            Kind::Omp => if let Some((id, r)) = parse_omp(line) { st.recs.insert(id, r); },
            Kind::Gemini => if let Some(ts) = parse_gemini(line) { st.gcount += 1; st.gem.push(ts); },
            Kind::Copilot => {} // read by ingest_copilot
        }
    }
    st.offset += end as u64 + 1;
}

/// Copilot CLI keeps usage in a SQLite db; the `sqlite3` CLI (no extra crate) reads the rows added since the last poll.
/// `st.offset` = last row id read; `st.stamp` = db / wal mtimes at the last query (unchanged = nothing to read).
fn ingest_copilot(path: &Path, now: DateTime<Local>, st: &mut FileState) {
    let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let stamp = (mtime(path), mtime(Path::new(&wal)));
    if st.stamp == Some(stamp) { return; }
    let cutoff = (now - chrono::Duration::days(7)).with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let sql = format!(
        "SELECT id,created_at,model,input_tokens,output_tokens,cache_write_tokens,cache_read_tokens FROM assistant_usage_events WHERE id > {} AND created_at >= '{}' ORDER BY id;",
        st.offset, cutoff);
    let Ok(out) = Command::new("sqlite3").args(["-readonly", "-separator", "|"]).arg(path).arg(sql).output() else { return };
    if !out.status.success() { return; }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let f: Vec<&str> = line.split('|').collect();
        if f.len() != 7 { continue; }
        let Ok(id) = f[0].parse::<u64>() else { continue };
        st.offset = st.offset.max(id);
        let Ok(ts) = DateTime::parse_from_rfc3339(f[1]) else { continue };
        let g = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        let (input, output, cw, cr) = (g(3), g(4), g(5), g(6));
        let net_in = input.saturating_sub(cr + cw); // input_tokens includes the cache read + write tokens
        st.recs.insert(id.to_string(), Rec {
            ts: ts.with_timezone(&Local),
            model: f[2].to_string(),
            tokens: net_in + output + cw,
            br: [net_in, output, cw, cr],
            cost: 0.0,
        });
    }
    st.stamp = Some(stamp);
}

/// Incremental reader of every provider's logs; keeps the last 7 days of records in memory.
#[derive(Default)]
pub struct Tracker {
    files: HashMap<PathBuf, (Kind, FileState)>,
}

fn newest(slot: &mut Option<DateTime<Local>>, ts: DateTime<Local>) {
    if slot.map_or(true, |t| ts > t) { *slot = Some(ts); }
}

fn sorted_shares(m: HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

impl Tracker {
    pub fn poll(&mut self, src: &Sources, now: DateTime<Local>) -> Usage {
        let today = now.date_naive();

        let mut found: Vec<(PathBuf, Kind)> = Vec::new();
        let mut tmp = Vec::new();
        for d in &src.claude { jsonl_files(d, 3, &mut tmp); }
        found.extend(tmp.drain(..).map(|p| (p, Kind::Claude)));
        if let Some(d) = &src.omp { jsonl_files(d, 2, &mut tmp); }
        found.extend(tmp.drain(..).map(|p| (p, Kind::Omp)));
        if let Some(g) = &src.gemini { found.push((g.clone(), Kind::Gemini)); }
        if let Some(c) = &src.copilot { found.push((c.clone(), Kind::Copilot)); }

        let keep: HashSet<&PathBuf> = found.iter().map(|(p, _)| p).collect();
        self.files.retain(|p, _| keep.contains(p));
        for (p, kind) in &found {
            let (_, st) = self.files.entry(p.clone()).or_insert_with(|| (*kind, FileState::default()));
            if *kind == Kind::Copilot { ingest_copilot(p, now, st); } else { ingest(p, *kind, st); }
            let recent = |d: NaiveDate| (today - d).num_days() <= 6;
            st.recs.retain(|_, r| recent(r.ts.date_naive()));
            st.gem.retain(|t| recent(t.date_naive()));
        }

        let mut u = Usage {
            has_claude: !src.claude.is_empty(),
            has_gemini: src.gemini.is_some(),
            has_omp: src.omp.is_some(),
            has_copilot: src.copilot.is_some(),
            ..Usage::default()
        };
        let (mut c_new, mut g_new, mut o_new, mut p_new) = (None, None, None, None);
        let (mut cmodels, mut omodels, mut pmodels): (HashMap<String, u64>, HashMap<String, u64>, HashMap<String, u64>) = Default::default();
        let mut seen: HashSet<&str> = HashSet::new();
        for (kind, st) in self.files.values() {
            match kind {
                Kind::Claude => for (id, r) in &st.recs {
                    if !seen.insert(id.as_str()) { continue; }
                    newest(&mut c_new, r.ts);
                    let d = r.ts.date_naive();
                    if let Some(slot) = day_slot(d, today) {
                        u.cdays[slot] += r.tokens;
                        u.c7d += r.tokens;
                        for i in 0..4 { u.cbreak[i] += r.br[i]; }
                        *cmodels.entry(r.model.trim_start_matches("claude-").to_string()).or_default() += r.tokens;
                    }
                    if d == today { u.ctoday += r.tokens; u.cmsgs += 1; }
                    if (now - r.ts).num_minutes() < 300 { u.c5h += r.tokens; }
                },
                Kind::Omp => for r in st.recs.values() {
                    newest(&mut o_new, r.ts);
                    let d = r.ts.date_naive();
                    if let Some(slot) = day_slot(d, today) {
                        u.odays[slot] += r.tokens;
                        u.o7d += r.tokens;
                        for i in 0..4 { u.obreak[i] += r.br[i]; }
                        u.ocost7d += r.cost;
                        if !r.model.is_empty() { *omodels.entry(r.model.clone()).or_default() += r.tokens; }
                    }
                    if d == today { u.otoday += r.tokens; }
                },
                Kind::Copilot => for r in st.recs.values() {
                    newest(&mut p_new, r.ts);
                    let d = r.ts.date_naive();
                    if let Some(slot) = day_slot(d, today) {
                        u.pdays[slot] += r.tokens;
                        u.p7d += r.tokens;
                        for i in 0..4 { u.pbreak[i] += r.br[i]; }
                        if !r.model.is_empty() { *pmodels.entry(r.model.clone()).or_default() += r.tokens; }
                    }
                    if d == today { u.ptoday += r.tokens; }
                },
                Kind::Gemini => {
                    u.gtotal += st.gcount;
                    for ts in &st.gem {
                        newest(&mut g_new, *ts);
                        let d = ts.date_naive();
                        if let Some(slot) = day_slot(d, today) { u.gdays[slot] += 1; u.g7d += 1; }
                        if d == today { u.gtoday += 1; }
                    }
                }
            }
        }
        u.cmodels = sorted_shares(cmodels);
        u.omodels = sorted_shares(omodels);
        u.pmodels = sorted_shares(pmodels);
        let live = |t: Option<DateTime<Local>>| t.map_or(false, |t| (now - t).num_seconds() < LIVE_SECS);
        u.c_live = live(c_new);
        u.g_live = live(g_new);
        u.o_live = live(o_new);
        u.p_live = live(p_new);
        u
    }
}

pub struct AiUi {
    c_big: gtk::Label,
    c_info: gtk::Label,
    c_detail: gtk::Label,
    c_days: Rc<RefCell<Vec<f64>>>,
    c_spark: gtk::DrawingArea,
    c_dot: gtk::Label,
    c_row: gtk::Box,
    g_big: gtk::Label,
    g_info: gtk::Label,
    g_days: Rc<RefCell<Vec<f64>>>,
    g_spark: gtk::DrawingArea,
    g_dot: gtk::Label,
    g_row: gtk::Box,
    o_big: gtk::Label,
    o_info: gtk::Label,
    o_detail: gtk::Label,
    o_days: Rc<RefCell<Vec<f64>>>,
    o_spark: gtk::DrawingArea,
    o_dot: gtk::Label,
    o_row: gtk::Box,
    p_big: gtk::Label,
    p_info: gtk::Label,
    p_detail: gtk::Label,
    p_days: Rc<RefCell<Vec<f64>>>,
    p_spark: gtk::DrawingArea,
    p_dot: gtk::Label,
    p_row: gtk::Box,
    empty: gtk::Label,
}

fn spark(days: Rc<RefCell<Vec<f64>>>, rgb: (f64, f64, f64)) -> gtk::DrawingArea {
    let a = gtk::DrawingArea::new();
    a.set_content_height(32);
    a.set_hexpand(true);
    a.set_vexpand(true);
    a.set_valign(gtk::Align::Fill);
    a.set_draw_func(move |_, cr: &cairo::Context, w, h| {
        if w <= 0 || h <= 0 { return; }
        let (w, h) = (w as f64, h as f64);
        let d = days.borrow();
        let n = d.len().max(1) as f64;
        let gap = 4.0_f64.min(w / (n * 2.0));
        let bw = (w - gap * (n - 1.0)) / n;
        if bw <= 0.0 { return; }
        for (i, v) in d.iter().enumerate() {
            let bh = (v * h).max(2.0).min(h);
            let r = 3.0_f64.min(bw / 2.0).min(bh / 2.0);
            let (x, y, bottom) = (i as f64 * (bw + gap), h - bh, h);
            let last = i + 1 == d.len();
            cr.set_source_rgba(rgb.0, rgb.1, rgb.2, if last { 1.0 } else { 0.45 });
            cr.new_sub_path();
            cr.arc(x + r, y + r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
            cr.arc(x + bw - r, y + r, r, 1.5 * std::f64::consts::PI, 2.0 * std::f64::consts::PI);
            cr.line_to(x + bw, bottom);
            cr.line_to(x, bottom);
            cr.close_path();
            let _ = cr.fill();
        }
    });
    a
}

fn wrap_label(l: &gtk::Label) {
    l.set_xalign(0.0);
    l.set_hexpand(true);
    l.set_single_line_mode(false);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_max_width_chars(1);
}

fn tile_row(dot: &gtk::Label, title: &str, big_lbl: &gtk::Label, sub_lbl: &gtk::Label, info_lbl: &gtk::Label, detail_lbl: Option<&gtk::Label>, spark: &gtk::DrawingArea) -> gtk::Box {
    let t = gtk::Box::new(gtk::Orientation::Vertical, 4);
    t.add_css_class("ai-tile");
    t.set_hexpand(true);
    t.set_vexpand(true);
    t.set_valign(gtk::Align::Fill);

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let h = gtk::Label::new(Some(title));
    h.add_css_class("cap");
    h.set_xalign(0.0);
    h.set_hexpand(true);
    h.set_max_width_chars(1);
    h.set_ellipsize(gtk::pango::EllipsizeMode::End);
    head.append(dot);
    head.append(&h);
    big_lbl.add_css_class("ai-big");
    head.append(big_lbl);
    sub_lbl.add_css_class("sub");
    sub_lbl.set_xalign(0.0);
    t.append(&head);
    t.append(sub_lbl);

    info_lbl.add_css_class("ai-val");
    wrap_label(info_lbl);
    t.append(info_lbl);
    if let Some(d) = detail_lbl {
        d.add_css_class("sub");
        d.add_css_class("ai-detail");
        wrap_label(d);
        t.append(d);
    }
    t.append(spark);
    t
}

fn dot(css: &str) -> gtk::Label {
    let d = gtk::Label::new(Some("\u{f111}"));
    d.add_css_class(css);
    d
}

pub fn build() -> (gtk::Box, AiUi) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.add_css_class("ai-page");
    page.set_homogeneous(true);
    page.set_hexpand(true);
    page.set_vexpand(true);
    page.set_valign(gtk::Align::Fill);
    page.set_margin_start(4);
    page.set_margin_end(4);
    page.set_margin_top(4);
    page.set_margin_bottom(4);

    let c_big = gtk::Label::new(Some("–"));
    let c_sub = gtk::Label::new(Some("tokens today"));
    let c_info = gtk::Label::new(None);
    let c_days = Rc::new(RefCell::new(vec![0.0; 7]));
    let c_spark = spark(c_days.clone(), (0.851, 0.467, 0.341));
    let c_detail = gtk::Label::new(None);
    let c_dot = dot("dot-claude");
    let c_row = tile_row(&c_dot, "Claude", &c_big, &c_sub, &c_info, Some(&c_detail), &c_spark);

    let g_big = gtk::Label::new(Some("–"));
    let g_sub = gtk::Label::new(Some("prompts today"));
    let g_info = gtk::Label::new(None);
    let g_days = Rc::new(RefCell::new(vec![0.0; 7]));
    let g_spark = spark(g_days.clone(), (0.541, 0.706, 0.973));
    let g_dot = dot("dot-gemini");
    let g_row = tile_row(&g_dot, "Gemini", &g_big, &g_sub, &g_info, None, &g_spark);

    let o_big = gtk::Label::new(Some("–"));
    let o_sub = gtk::Label::new(Some("tokens today"));
    let o_info = gtk::Label::new(None);
    let o_days = Rc::new(RefCell::new(vec![0.0; 7]));
    let o_spark = spark(o_days.clone(), (0.451, 0.824, 0.541));
    let o_detail = gtk::Label::new(None);
    let o_dot = dot("dot-omp");
    let o_row = tile_row(&o_dot, "OMP", &o_big, &o_sub, &o_info, Some(&o_detail), &o_spark);

    let p_big = gtk::Label::new(Some("–"));
    let p_sub = gtk::Label::new(Some("tokens today"));
    let p_info = gtk::Label::new(None);
    let p_days = Rc::new(RefCell::new(vec![0.0; 7]));
    let p_spark = spark(p_days.clone(), (0.639, 0.443, 0.969));
    let p_detail = gtk::Label::new(None);
    let p_dot = dot("dot-copilot");
    let p_row = tile_row(&p_dot, "Copilot", &p_big, &p_sub, &p_info, Some(&p_detail), &p_spark);

    let empty = gtk::Label::new(Some("No AI tool logs found. Reads ~/.claude/projects, ~/.gemini/antigravity-cli/history.jsonl, ~/.omp/agent/sessions, ~/.copilot/session-store.db."));
    empty.add_css_class("sub");
    empty.set_wrap(true);
    empty.set_max_width_chars(1);
    empty.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    empty.set_xalign(0.0);
    empty.set_hexpand(true);
    empty.set_vexpand(true);
    empty.set_valign(gtk::Align::Center);
    empty.set_visible(false);

    page.append(&c_row);
    page.append(&g_row);
    page.append(&o_row);
    page.append(&p_row);
    page.append(&empty);

    (page, AiUi {
        c_big, c_info, c_detail, c_days, c_spark, c_dot, c_row,
        g_big, g_info, g_days, g_spark, g_dot, g_row,
        o_big, o_info, o_detail, o_days, o_spark, o_dot, o_row,
        p_big, p_info, p_detail, p_days, p_spark, p_dot, p_row,
        empty,
    })
}

fn shares(m: &[(String, u64)]) -> String {
    let total: u64 = m.iter().map(|x| x.1).sum();
    if total == 0 { return String::new(); }
    m.iter().take(3).map(|(n, t)| format!("{n} {}%", t * 100 / total)).collect::<Vec<_>>().join(" · ")
}

fn set_live(dot: &gtk::Label, live: bool) {
    if live { dot.add_css_class("live") } else { dot.remove_css_class("live") }
}

fn set_txt(l: &gtk::Label, t: &str) {
    l.set_text(t);
    l.set_tooltip_text(Some(t));
}

impl AiUi {
    pub fn update(&self, u: &Usage) {
        self.c_row.set_visible(u.has_claude);
        self.g_row.set_visible(u.has_gemini);
        self.o_row.set_visible(u.has_omp);
        self.empty.set_visible(!(u.has_claude || u.has_gemini || u.has_omp || u.has_copilot));
        set_live(&self.c_dot, u.c_live);
        set_live(&self.g_dot, u.g_live);
        set_live(&self.o_dot, u.o_live);

        self.c_big.set_text(&compact(u.ctoday));
        set_txt(&self.c_info, &format!("5h: {}  ·  Today: {} ({} msgs)  ·  7d: {} (avg {}/d)", compact(u.c5h), compact(u.ctoday), u.cmsgs, compact(u.c7d), compact(u.c7d / 7)));
        set_txt(&self.c_detail, &format!("7d  In {} · Out {} · Cache w {} / r {}  |  {}", compact(u.cbreak[0]), compact(u.cbreak[1]), compact(u.cbreak[2]), compact(u.cbreak[3]), shares(&u.cmodels)));
        let peak = u.cdays.iter().copied().max().unwrap_or(1).max(1) as f64;
        *self.c_days.borrow_mut() = u.cdays.iter().map(|d| *d as f64 / peak).collect();
        self.c_spark.queue_draw();

        self.g_big.set_text(&u.gtoday.to_string());
        set_txt(&self.g_info, &format!("Today: {}  ·  7d: {} (avg {}/d)  ·  All: {}", u.gtoday, u.g7d, u.g7d / 7, u.gtotal));
        let gp = u.gdays.iter().copied().max().unwrap_or(1).max(1) as f64;
        *self.g_days.borrow_mut() = u.gdays.iter().map(|d| *d as f64 / gp).collect();
        self.g_spark.queue_draw();

        self.o_big.set_text(&compact(u.otoday));
        set_txt(&self.o_info, &format!("Today: {}  ·  7d: {} (avg {}/d)", compact(u.otoday), compact(u.o7d), compact(u.o7d / 7)));
        set_txt(&self.o_detail, &format!("7d  In {} · Out {} · Cache w {} / r {} · ${:.2}  |  {}", compact(u.obreak[0]), compact(u.obreak[1]), compact(u.obreak[2]), compact(u.obreak[3]), u.ocost7d, shares(&u.omodels)));
        let op = u.odays.iter().copied().max().unwrap_or(1).max(1) as f64;
        *self.o_days.borrow_mut() = u.odays.iter().map(|d| *d as f64 / op).collect();
        self.o_spark.queue_draw();

        self.p_row.set_visible(u.has_copilot);
        set_live(&self.p_dot, u.p_live);
        self.p_big.set_text(&compact(u.ptoday));
        set_txt(&self.p_info, &format!("Today: {}  ·  7d: {} (avg {}/d)", compact(u.ptoday), compact(u.p7d), compact(u.p7d / 7)));
        set_txt(&self.p_detail, &format!("7d  In {} · Out {} · Cache w {} / r {}  |  {}", compact(u.pbreak[0]), compact(u.pbreak[1]), compact(u.pbreak[2]), compact(u.pbreak[3]), shares(&u.pmodels)));
        let pp = u.pdays.iter().copied().max().unwrap_or(1).max(1) as f64;
        *self.p_days.borrow_mut() = u.pdays.iter().map(|d| *d as f64 / pp).collect();
        self.p_spark.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notch-usage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn now() -> DateTime<Local> { Local.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).single().unwrap() }

    fn claude_line(id: &str, ago_min: i64, out: u64) -> String {
        format!(r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"{id}","model":"claude-x","usage":{{"input_tokens":10,"output_tokens":{out},"cache_creation_input_tokens":5,"cache_read_input_tokens":100}}}}}}"#,
            (now() - Duration::minutes(ago_min)).to_rfc3339())
    }

    fn omp_line(kind: &str, id: &str, ago_min: i64, i: u64, o: u64, cw: u64, cr: u64) -> String {
        let ts = (now() - Duration::minutes(ago_min)).to_rfc3339();
        let us = format!(r#"{{"input":{i},"output":{o},"cacheRead":{cr},"cacheWrite":{cw},"totalTokens":{},"cost":{{"total":0.5}}}}"#, i + o + cr + cw);
        if kind == "message" {
            format!(r#"{{"type":"message","id":"{id}","timestamp":"{ts}","message":{{"role":"assistant","model":"m1","usage":{us}}}}}"#)
        } else {
            format!(r#"{{"type":"model_usage","id":"{id}","timestamp":"{ts}","model":"m2","usage":{us}}}"#)
        }
    }

    #[test]
    fn claude_tail_counts_only_complete_new_lines() {
        let dir = tmp("claude");
        let file = dir.join("proj/s.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{}\n{}\n", claude_line("m1", 3, 20), claude_line("m2", 2, 20))).unwrap();
        let src = Sources { claude: vec![dir.clone()], ..Sources::default() };
        let mut t = Tracker::default();
        assert_eq!(t.poll(&src, now()).ctoday, 70);

        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        write!(f, "{}", claude_line("m3", 1, 30)).unwrap();
        assert_eq!(t.poll(&src, now()).ctoday, 70, "partial trailing line must wait for its newline");
        writeln!(f).unwrap();
        let u = t.poll(&src, now());
        assert_eq!(u.ctoday, 70 + 10 + 30 + 5);
        assert_eq!(u.cmsgs, 3);
        assert!(u.c_live && u.has_claude && !u.has_omp);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn omp_counts_both_line_types_once_without_cache_read() {
        let dir = tmp("omp");
        std::fs::create_dir_all(dir.join("s1")).unwrap();
        let a = omp_line("message", "a1", 5, 100, 50, 10, 1000);
        let body = format!("{a}\n{}\n{a}\n{}\n", omp_line("usage", "u1", 4, 1, 2, 3, 500), r#"{"type":"message","id":"x","timestamp":"2026-05-10T00:00:00Z","message":{"role":"user"}}"#);
        std::fs::write(dir.join("s1/a.jsonl"), body).unwrap();
        let src = Sources { omp: Some(dir.clone()), ..Sources::default() };
        let u = Tracker::default().poll(&src, now());
        assert_eq!(u.otoday, 160 + 6);
        assert_eq!(u.obreak, [101, 52, 13, 1500]);
        assert_eq!(u.omodels.len(), 2);
        assert!((u.ocost7d - 1.0).abs() < 1e-9);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn truncated_file_is_reread_from_start() {
        let dir = tmp("trunc");
        let file = dir.join("p/s.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{}\n{}\n", claude_line("m1", 3, 20), claude_line("m2", 2, 20))).unwrap();
        let src = Sources { claude: vec![dir.clone()], ..Sources::default() };
        let mut t = Tracker::default();
        assert_eq!(t.poll(&src, now()).ctoday, 70);
        std::fs::write(&file, format!("{}\n", claude_line("m9", 1, 5))).unwrap();
        assert_eq!(t.poll(&src, now()).ctoday, 20);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_sources_yield_empty_usage() {
        let u = Tracker::default().poll(&Sources::default(), now());
        assert!(u == Usage::default());
        assert!(!u.has_claude && !u.has_gemini && !u.has_omp);
    }

    #[test]
    fn copilot_incremental_rows() {
        if !has_sqlite3() { return; }
        let dir = tmp("copilot");
        let db = dir.join("session-store.db");
        let sql = |q: &str| {
            let o = std::process::Command::new("sqlite3").arg(&db).arg(q).output().unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        };
        let ts = |d: Duration| (now() - d).with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let row = |ago: Duration, i: u64, o: u64, cw: u64, cr: u64| format!(
            "INSERT INTO assistant_usage_events(created_at,model,input_tokens,output_tokens,cache_write_tokens,cache_read_tokens) VALUES('{}','m',{i},{o},{cw},{cr});", ts(ago));
        sql("CREATE TABLE assistant_usage_events(id INTEGER PRIMARY KEY AUTOINCREMENT, model TEXT NOT NULL, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, created_at TEXT);");
        sql(&format!("{}{}{}", row(Duration::minutes(5), 1000, 50, 100, 600), row(Duration::minutes(4), 200, 20, 0, 0), row(Duration::days(10), 999, 999, 0, 0)));

        let src = Sources { copilot: Some(db.clone()), ..Sources::default() };
        let mut t = Tracker::default();
        let u = t.poll(&src, now());
        assert_eq!(u.ptoday, (300 + 50 + 100) + (200 + 20));
        assert_eq!(u.pbreak, [500, 70, 100, 600]);
        assert!(u.has_copilot && !u.p_live, "newest row is 4 min old: not live");

        std::thread::sleep(std::time::Duration::from_millis(30));
        sql(&row(Duration::minutes(1), 10, 5, 0, 0));
        let u = t.poll(&src, now());
        assert_eq!(u.ptoday, 685);
        assert!(u.p_live, "newest row is 1 min old");
        assert_eq!(t.poll(&src, now()).ptoday, 685, "unchanged db must not be double counted");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
