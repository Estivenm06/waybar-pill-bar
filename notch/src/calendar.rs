//! Calendar tab: month grid, day agenda (Google Calendar events + Omarchy
//! reminders) and the event/reminder editor. `CalData` holds the data and the
//! editor state; workers (Google worker from `gcal`, a reminder worker here)
//! post their updates into it.

use crate::gcal;
use crate::layout::{Rect, Scroll, Size, pt, rect};
use crate::page::Page;
use crate::reminders;
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{ButtonKind, FieldResult, Hits, TextField, button, scrollbar};
use crate::wayland::{Button, Cursor, Ctx, DrawCtx, Event, Poster};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveTime, TimeZone};
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

// ---------------------------------------------------------------- native reminder worker

#[derive(Debug)]
pub enum NativeCommand {
    Refresh,
    Activity(bool),
    Create { minutes: u32, title: String, generation: u64, replace: Option<String> },
    Delete { unit: String },
}

pub enum NativeUpdate {
    Listed(Result<Vec<reminders::Reminder>, String>),
    Created { generation: u64, result: Result<Vec<reminders::Reminder>, String> },
    Deleted(Result<Vec<reminders::Reminder>, String>),
}

fn native_worker(poster: Poster) -> mpsc::Sender<NativeCommand> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut visible = false;
        loop {
            let command = if visible {
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(c) => c,
                    Err(mpsc::RecvTimeoutError::Timeout) => NativeCommand::Refresh,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() { Ok(c) => c, Err(_) => break }
            };
            let update = match command {
                NativeCommand::Activity(active) => {
                    visible = active;
                    if !active { continue; }
                    NativeUpdate::Listed(reminders::list())
                }
                NativeCommand::Refresh => NativeUpdate::Listed(reminders::list()),
                NativeCommand::Create { minutes, title, generation, replace } => NativeUpdate::Created {
                    generation,
                    result: match replace { Some(unit) => reminders::update(&unit, minutes, &title), None => reminders::create(minutes, &title) },
                },
                NativeCommand::Delete { unit } => NativeUpdate::Deleted(reminders::delete(&unit)),
            };
            poster.post(move |st, _| st.cal.on_native(update));
        }
    });
    tx
}

/// Start both workers. Call once.
pub fn start(poster: &Poster) {
    let p = poster.clone();
    let google = gcal::start_worker(
        move |u| { p.post(move |st, _| st.cal.on_google(u)); true },
        |url| {
            if let Ok(mut c) = std::process::Command::new("xdg-open").arg(url).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
                std::thread::spawn(move || { let _ = c.wait(); });
            }
        },
    );
    let native = native_worker(poster.clone());
    poster.post(move |st, _| { st.cal.google = Some(google); st.cal.native = Some(native); });
}

// ---------------------------------------------------------------- data + editor state

struct PendingGoogle { draft: gcal::EventDraft, generation: u64, uncertain: bool, retry_allowed: bool }

pub struct Editor {
    pub open: bool,
    /// 0 = Omarchy reminder, 1 = Google Calendar.
    pub dest: usize,
    pub native_title: TextField,
    pub minutes: TextField,
    pub google_title: TextField,
    pub date: TextField,
    pub time: TextField,
    pub duration: TextField,
    pub all_day: bool,
    pub alert: u32,
    pub status: String,
    /// Unit of the reminder being edited; saving replaces it.
    pub editing: Option<String>,
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            open: false, dest: 0, native_title: TextField::new("Reminder title"), minutes: TextField::new("Minutes"),
            google_title: TextField::new("Event title"), date: TextField::new("YYYY-MM-DD"), time: TextField::new("HH:MM"),
            duration: TextField::new("Minutes"), all_day: false, alert: 2, status: String::new(), editing: None,
        }
    }
}

pub struct CalData {
    pub events: Vec<gcal::Event>,
    pub reminders: Vec<reminders::Reminder>,
    pub range: Option<gcal::CalendarRange>,
    pub generation: u64,
    pub last_success: Option<DateTime<Local>>,
    pub google_error: Option<String>,
    pub native_error: Option<String>,
    pub loading: bool,
    pub primary_selected: bool,
    pub write_access: bool,
    pub authorizing: bool,
    pub visible: bool,
    pub selected: NaiveDate,
    pub editor: Editor,
    editor_generation: u64,
    next_operation: u64,
    pending_google: Option<PendingGoogle>,
    native_pending: Option<(u64, String, HashSet<String>)>,
    draft_id: Option<String>,
    google: Option<mpsc::Sender<gcal::Command>>,
    native: Option<mpsc::Sender<NativeCommand>>,
    pub note: Option<String>,
    /// Unit whose delete is running (its row is dimmed and ignores clicks).
    pub deleting: Option<String>,
}

impl Default for CalData {
    fn default() -> Self {
        CalData {
            events: vec![], reminders: vec![], range: None, generation: 0, last_success: None, google_error: None, native_error: None,
            loading: false, primary_selected: true, write_access: false, authorizing: false, visible: false,
            selected: Local::now().date_naive(), editor: Editor::default(), editor_generation: 0, next_operation: 0,
            pending_google: None, native_pending: None, draft_id: None, google: None, native: None, note: None, deleting: None,
        }
    }
}

pub fn accept_refresh(generation_now: u64, range_now: Option<gcal::CalendarRange>, range: gcal::CalendarRange, generation: u64) -> bool {
    generation_now == generation && range_now == Some(range)
}

pub fn upsert_event(events: &mut Vec<gcal::Event>, event: gcal::Event) {
    if let Some(e) = events.iter_mut().find(|e| e.calendar_id == event.calendar_id && e.id == event.id) { *e = event; } else { events.push(event); }
}

pub fn positive_minutes(value: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) { return Err("Enter a positive whole number of minutes".into()); }
    value.parse::<u32>().ok().filter(|v| *v > 0).ok_or_else(|| "Enter a positive whole number of minutes".into())
}

pub fn build_draft(title: &str, date: &str, time: &str, duration: &str, all_day: bool, alert: u32) -> Result<gcal::EventDraft, String> {
    reminders::validate_title(title)?;
    let date_value = NaiveDate::parse_from_str(date, "%Y-%m-%d").map_err(|_| "Enter a valid date as YYYY-MM-DD")?;
    if date_value.format("%Y-%m-%d").to_string() != date { return Err("Enter a valid date as YYYY-MM-DD".into()); }
    let time_value = if all_day { NaiveTime::MIN } else {
        let v = NaiveTime::parse_from_str(time, "%H:%M").map_err(|_| "Enter a valid time as HH:MM")?;
        if v.format("%H:%M").to_string() != time { return Err("Enter a valid time as HH:MM".into()); }
        v
    };
    let start = Local.from_local_datetime(&date_value.and_time(time_value)).single().ok_or("Local time is ambiguous or does not exist")?;
    let end = if all_day {
        let next = date_value.succ_opt().ok_or("Date is out of range")?;
        Local.from_local_datetime(&next.and_time(NaiveTime::MIN)).single().ok_or("Local time is ambiguous or does not exist")?
    } else {
        start.checked_add_signed(ChronoDuration::minutes(positive_minutes(duration)? as i64)).ok_or("Duration is too large")?
    };
    let alert_minutes = match alert { 0 => Some(0), 1 => Some(5), 2 => Some(15), 3 => Some(30), 4 => Some(60), 5 => None, _ => return Err("Select a valid alert".into()) };
    Ok(gcal::EventDraft { id: String::new(), title: title.to_owned(), start, end, all_day, alert_minutes })
}

pub const ALERTS: [&str; 6] = ["At start", "5 min before", "15 min before", "30 min before", "1 hour before", "No alert"];

impl CalData {
    pub fn request_google(&mut self) {
        let range = match gcal::CalendarRange::for_month(self.selected) {
            Ok(r) => r,
            Err(e) => { self.google_error = Some(e.to_string()); return; }
        };
        self.generation = self.generation.wrapping_add(1);
        self.range = Some(range);
        self.loading = true;
        let cmd = gcal::Command::Refresh { range, generation: self.generation, visible: self.visible };
        if self.google.as_ref().is_none_or(|g| g.send(cmd).is_err()) {
            self.loading = false;
            self.google_error = Some("Google Calendar worker stopped".into());
        }
    }

    fn native_send(&self, c: NativeCommand) -> bool { self.native.as_ref().is_some_and(|n| n.send(c).is_ok()) }

    pub fn set_visible(&mut self, visible: bool) {
        let changed = self.visible != visible;
        self.visible = visible;
        if changed { self.native_send(NativeCommand::Activity(visible)); }
        if visible { self.native_send(NativeCommand::Refresh); self.request_google(); }
    }

    pub fn select_date(&mut self, date: NaiveDate) {
        self.selected = date;
        let range = gcal::CalendarRange::for_month(date).ok();
        if range != self.range { self.request_google(); }
    }

    pub fn open_editor(&mut self) {
        if self.editor.open { return; }
        self.next_operation = self.next_operation.wrapping_add(1);
        self.editor_generation = self.next_operation;
        self.draft_id = None;
        let e = &mut self.editor;
        e.open = true;
        e.native_title.set_text("");
        e.google_title.set_text("");
        e.minutes.set_text("15");
        e.date.set_text(&self.selected.to_string());
        e.time.set_text("09:00");
        e.duration.set_text("30");
        e.all_day = false;
        e.alert = 2;
        e.dest = 0;
        e.status.clear();
        for f in [&mut e.native_title, &mut e.minutes, &mut e.google_title, &mut e.date, &mut e.time, &mut e.duration] { f.focused = false; }
        e.native_title.focused = true;
    }

    /// Open the editor on an existing Omarchy reminder: saving sets the changed reminder and removes the old one.
    pub fn edit_reminder(&mut self, unit: &str) {
        if self.editor.open || self.busy() || self.deleting.is_some() { return; }
        let Some(r) = self.reminders.iter().find(|r| r.unit == unit).cloned() else { return };
        self.open_editor();
        let left = (r.at - Local::now()).num_seconds().max(0);
        let e = &mut self.editor;
        e.editing = Some(r.unit);
        e.dest = 0;
        e.native_title.set_text(&r.title);
        e.minutes.set_text(&((left + 59) / 60).max(1).to_string());
    }

    pub fn delete_reminder(&mut self, unit: &str) {
        if self.editor.open || self.busy() || self.deleting.is_some() { return; }
        if !self.reminders.iter().any(|r| r.unit == unit) { return; }
        self.deleting = Some(unit.to_string());
        self.native_error = None;
        if !self.native_send(NativeCommand::Delete { unit: unit.to_string() }) {
            self.deleting = None;
            self.native_error = Some("Omarchy reminder worker stopped".into());
        }
    }

    pub fn cancel_editor(&mut self) {
        self.editor.open = false;
        self.editor.editing = None;
        self.draft_id = None;
        if self.pending_google.as_ref().is_some_and(|p| p.retry_allowed) { self.pending_google = None; }
    }

    pub fn busy(&self) -> bool { self.native_pending.is_some() || self.pending_google.as_ref().is_some_and(|p| !p.retry_allowed) }

    pub fn authorize(&mut self, write: bool) {
        if self.authorizing { return; }
        self.authorizing = true;
        if self.google.as_ref().is_none_or(|g| g.send(gcal::Command::Authorize { write }).is_err()) {
            self.authorizing = false;
            self.editor.status = "Google Calendar worker stopped".into();
        }
    }

    pub fn save(&mut self) {
        if !self.editor.open || self.native_pending.is_some() || self.authorizing { return; }
        if let Some(p) = &self.pending_google {
            if !p.retry_allowed { return; }
            let cmd = gcal::Command::Create { draft: p.draft.clone(), generation: p.generation };
            if self.google.as_ref().is_none_or(|g| g.send(cmd).is_err()) { self.editor.status = "Google Calendar worker stopped".into(); return; }
            self.pending_google.as_mut().unwrap().retry_allowed = false;
            self.editor.status = "Saving…".into();
            return;
        }
        let generation = self.editor_generation;
        if self.editor.dest == 0 {
            let title = self.editor.native_title.text.clone();
            let minutes = reminders::validate_title(&title).and_then(|()| positive_minutes(&self.editor.minutes.text));
            let minutes = match minutes { Ok(m) => m, Err(e) => { self.editor.status = e; return; } };
            self.native_pending = Some((generation, title.clone(), self.reminders.iter().map(|r| r.unit.clone()).collect()));
            if !self.native_send(NativeCommand::Create { minutes, title, generation, replace: self.editor.editing.clone() }) {
                self.native_pending = None;
                self.editor.status = "Omarchy reminder worker stopped".into();
                return;
            }
        } else {
            if !self.write_access { self.editor.status = "Enable Google event creation before saving".into(); return; }
            let e = &self.editor;
            let draft = build_draft(&e.google_title.text, &e.date.text, &e.time.text, &e.duration.text, e.all_day, e.alert);
            let mut draft = match draft { Ok(d) => d, Err(err) => { self.editor.status = err; return; } };
            let id = match &self.draft_id {
                Some(id) => id.clone(),
                None => match gcal::new_event_id() { Ok(id) => id, Err(err) => { self.editor.status = err.to_string(); return; } },
            };
            self.draft_id = Some(id.clone());
            draft.id = id;
            if self.google.as_ref().is_none_or(|g| g.send(gcal::Command::Create { draft: draft.clone(), generation }).is_err()) {
                self.editor.status = "Google Calendar worker stopped".into();
                return;
            }
            self.pending_google = Some(PendingGoogle { draft, generation, uncertain: false, retry_allowed: false });
        }
        self.editor.status = "Saving…".into();
    }

    pub fn on_google(&mut self, update: gcal::Update) {
        match update {
            gcal::Update::Loading { range, generation } => { if accept_refresh(self.generation, self.range, range, generation) { self.loading = true; } }
            gcal::Update::Refreshed { range, generation, result } => {
                if !accept_refresh(self.generation, self.range, range, generation) { return; }
                self.loading = false;
                match result {
                    Ok(s) => {
                        self.events = s.events;
                        self.primary_selected = s.primary_selected;
                        self.write_access = s.write_access;
                        self.last_success = Some(Local::now());
                        self.google_error = None;
                    }
                    Err(e) => {
                        if matches!(e, gcal::CalendarError::NeedWriteAccess | gcal::CalendarError::NeedAuth) { self.write_access = false; }
                        self.google_error = Some(e.to_string());
                    }
                }
            }
            gcal::Update::Authorized { write, result } => {
                self.authorizing = false;
                match result {
                    Ok(()) => {
                        if write { self.write_access = true; }
                        self.editor.status = "Google Calendar connected".into();
                        self.request_google();
                    }
                    Err(e) => { self.editor.status = e.to_string(); self.google_error = Some(e.to_string()); }
                }
            }
            gcal::Update::Created { generation, result } => {
                if !self.pending_google.as_ref().is_some_and(|p| p.generation == generation) { return; }
                match result {
                    Ok(ev) => self.created_google(generation, ev),
                    Err(e) => {
                        if e == gcal::CalendarError::UnknownSave { self.pending_google.as_mut().unwrap().uncertain = true; } else {
                            self.pending_google = None;
                            if matches!(e, gcal::CalendarError::NeedWriteAccess | gcal::CalendarError::NeedAuth) { self.write_access = false; }
                        }
                        self.editor.status = e.to_string();
                    }
                }
            }
            gcal::Update::Reconciled { id, generation, result } => {
                if !self.pending_google.as_ref().is_some_and(|p| p.generation == generation && p.draft.id == id) { return; }
                match result {
                    Ok(Some(ev)) => self.created_google(generation, ev),
                    Ok(None) => {
                        if self.editor.open && self.editor_generation == generation {
                            let p = self.pending_google.as_mut().unwrap();
                            p.uncertain = false;
                            p.retry_allowed = true;
                            self.editor.status = "No event was saved. Save can safely retry the same event ID.".into();
                        } else { self.pending_google = None; }
                    }
                    Err(e) => {
                        self.editor.status = format!("Save outcome unknown; refresh before trying again · {e}");
                        self.google_error = Some(e.to_string());
                    }
                }
            }
        }
    }

    fn created_google(&mut self, generation: u64, event: gcal::Event) {
        let date = event.start.date();
        upsert_event(&mut self.events, event);
        self.pending_google = None;
        self.draft_id = None;
        self.generation = self.generation.wrapping_add(1);
        let back = self.editor.open && self.editor_generation == generation;
        if back { self.cancel_editor(); self.selected = date; }
        self.request_google();
        if !self.primary_selected { self.note = Some("Created in Primary · Primary is not selected for normal calendar browsing".into()); }
    }

    pub fn on_native(&mut self, update: NativeUpdate) {
        match update {
            NativeUpdate::Deleted(r) => {
                self.deleting = None;
                match r { Ok(v) => { self.reminders = v; self.native_error = None; } Err(e) => self.native_error = Some(e) }
            }
            NativeUpdate::Listed(r) => match r { Ok(v) => { self.reminders = v; self.native_error = None; } Err(e) => self.native_error = Some(e) },
            NativeUpdate::Created { generation, result } => {
                let Some((pg, title, previous)) = self.native_pending.take() else { return };
                if pg != generation { self.native_pending = Some((pg, title, previous)); return; }
                match result {
                    Ok(list) => {
                        let created = list.iter().find(|r| r.title == title && !previous.contains(&r.unit)).map(|r| r.at.date_naive());
                        self.reminders = list;
                        self.native_error = None;
                        let back = self.editor.open && self.editor_generation == generation;
                        match created {
                            Some(d) => if back { self.cancel_editor(); self.selected = d; },
                            None => self.editor.status = "Omarchy returned no new reminder. Check the active list before trying again.".into(),
                        }
                    }
                    Err(e) => { self.native_error = Some(e.clone()); self.editor.status = e; }
                }
            }
        }
    }

    fn status_line(&self, pattern: &str) -> String {
        let mut parts = Vec::new();
        if self.loading { parts.push("Loading Google Calendar…".to_string()); }
        if let Some(at) = self.last_success { parts.push(format!("Last updated {}", at.format(pattern))); }
        if let Some(e) = &self.google_error { parts.push(format!("Google Calendar: {e}")); }
        if parts.is_empty() { parts.push("Google Calendar has not loaded".into()); }
        parts.join(" · ")
    }
}

// ---------------------------------------------------------------- page

const ID_PREV: u32 = 1;
const ID_NEXT: u32 = 2;
const ID_TODAY: u32 = 3;
const ID_ADD: u32 = 4;
const ID_CONNECT: u32 = 5;
const ID_CANCEL: u32 = 6;
const ID_SAVE: u32 = 7;
const ID_DEST0: u32 = 8;
const ID_DEST1: u32 = 9;
const ID_ALLDAY: u32 = 10;
const ID_ALERT: u32 = 11;
const ID_AUTH: u32 = 12;
const ID_DAY0: u32 = 1000;
const ID_JOIN0: u32 = 2000;
const ID_FIELD0: u32 = 3000;
const ID_REM_EDIT0: u32 = 4000;
const ID_REM_DEL0: u32 = 5000;

#[derive(Default)]
pub struct CalendarPage { hits: Hits, scroll: Scroll, grid: Vec<NaiveDate>, kb: bool, rem_units: Vec<String> }

impl CalendarPage {
    pub fn new() -> Self { Self::default() }
}

pub fn month_grid(date: NaiveDate) -> Vec<NaiveDate> {
    let first = date.with_day(1).unwrap();
    let start = first - ChronoDuration::days(first.weekday().num_days_from_sunday() as i64);
    (0..42).map(|i| start + ChronoDuration::days(i)).collect()
}

fn add_months(d: NaiveDate, delta: i32) -> NaiveDate {
    let months = d.year() * 12 + d.month0() as i32 + delta;
    let (y, m) = (months.div_euclid(12), months.rem_euclid(12) as u32 + 1);
    NaiveDate::from_ymd_opt(y, m, d.day().min(28)).or_else(|| NaiveDate::from_ymd_opt(y, m, 1)).unwrap_or(d)
}

impl Page for CalendarPage {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        if st.cal.editor.open { return Size { w: avail.w, h: 330.0 }; }
        Size { w: avail.w, h: 262.0 }
    }

    fn shown(&mut self, ctx: &mut Ctx) {
        if let Some(d) = ctx.state.cal_goto.take() { ctx.state.cal.selected = d; }
        ctx.state.cal.set_visible(true);
    }
    fn hidden(&mut self, ctx: &mut Ctx) {
        ctx.state.cal.set_visible(false);
        ctx.state.cal.cancel_editor();
    }
    fn wants_keyboard(&self) -> bool { self.kb }
    fn keeps_open(&self) -> bool { self.kb }
    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn tick(&mut self, _: f32, ctx: &mut Ctx) -> bool {
        self.kb = ctx.state.cal.editor.open;
        if let Some(d) = ctx.state.cal_goto.take() { ctx.state.cal.select_date(d); }
        false
    }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        self.hits.clear();
        if dc.state.cal.editor.open { self.draw_editor(cv, dc, area); } else { self.draw_month(cv, dc, area); }
    }

    fn event(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx) {
        if ctx.state.cal.editor.open { self.event_editor(ev, area, ctx); } else { self.event_month(ev, area, ctx); }
    }
}

impl CalendarPage {
    fn draw_month(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        let cal = &st.cal;
        let today = Local::now().date_naive();
        let left = rect(area.x, area.y, 238.0, area.h);
        let right = rect(left.right() + 10.0, area.y, area.w - 248.0, area.h);

        // month card
        cv.fill_rrect(left, r_all(14.0), pal.glass);
        cv.stroke_rrect(left, r_all(14.0), 1.0, with_a(pal.fg, 0.18));
        let c = left.pad(10.0);
        let prev = rect(c.x, c.y, 26.0, 22.0);
        let next = rect(c.right() - 26.0, c.y, 26.0, 22.0);
        for (r, id, g) in [(prev, ID_PREV, "\u{f053}"), (next, ID_NEXT, "\u{f054}")] {
            let hov = self.hits.add(r, id);
            cv.fill_rrect(r, r_all(10.0), with_a(pal.fg, if hov { 0.14 } else { 0.06 }));
            cv.text(g, &TextStyle::icon(10.0, pal.dim), r.center(), HAlign::Center, VAlign::Middle);
        }
        cv.text(&cal.selected.format("%B %Y").to_string(), &TextStyle::new(pal.fs(13.0), pal.fg_hi).bold(), pt(c.center().x, c.y + 11.0), HAlign::Center, VAlign::Middle);
        let cw = c.w / 7.0;
        for (i, d) in ["S", "M", "T", "W", "T", "F", "S"].iter().enumerate() {
            cv.text(d, &TextStyle::new(pal.fs(10.0), with_a(pal.accent, 0.75)).bold(), pt(c.x + cw * (i as f32 + 0.5), c.y + 36.0), HAlign::Center, VAlign::Middle);
        }
        self.grid = month_grid(cal.selected);
        for (i, d) in self.grid.clone().into_iter().enumerate() {
            let (col, row) = (i % 7, i / 7);
            let r = rect(c.x + cw * col as f32 + 1.0, c.y + 46.0 + row as f32 * 28.0, cw - 2.0, 26.0);
            let hov = self.hits.add(r, ID_DAY0 + i as u32);
            let sel = d == cal.selected;
            let other = d.month() != cal.selected.month();
            if sel { cv.fill_rrect(r, r_all(9.0), with_a(pal.accent, 0.92)); } else if hov { cv.fill_rrect(r, r_all(9.0), with_a(pal.fg, 0.10)); }
            if d == today { cv.stroke_rrect(r, r_all(9.0), 1.0, pal.accent_hi); }
            let col_t = if sel { pal.bg } else if other { with_a(pal.fg, 0.28) } else if d == today { pal.fg_hi } else { pal.fg };
            let mut sty = TextStyle::new(pal.fs(11.0), col_t);
            if sel || d == today { sty = sty.bold(); }
            cv.text(&d.day().to_string(), &sty, pt(r.center().x, r.y + 12.0), HAlign::Center, VAlign::Middle);
            let marked = cal.events.iter().any(|e| e.on_day(d)) || cal.reminders.iter().any(|r| r.at.date_naive() == d);
            if marked { cv.fill_circle(pt(r.center().x, r.bottom() - 3.0), 1.5, if sel { pal.bg } else { with_a(pal.accent, 0.85) }); }
        }

        // agenda card
        cv.fill_rrect(right, r_all(14.0), pal.glass);
        cv.stroke_rrect(right, r_all(14.0), 1.0, with_a(pal.fg, 0.18));
        let c = right.pad(10.0);
        cv.text_in(&cal.selected.format("%a, %-d %b").to_string(), &TextStyle::new(pal.fs(12.0), with_a(pal.fg, 0.85)).weight(600), rect(c.x, c.y, c.w - 110.0, 22.0), HAlign::Left);
        let add = rect(c.right() - 26.0, c.y, 26.0, 22.0);
        let ha = self.hits.add(add, ID_ADD);
        cv.fill_rrect(add, r_all(10.0), with_a(pal.accent, if ha { 0.4 } else { 0.25 }));
        cv.text("+", &TextStyle::new(pal.fs(14.0), pal.accent_hi).bold(), add.center(), HAlign::Center, VAlign::Middle);
        let tb = rect(add.x - 62.0, c.y, 56.0, 22.0);
        let ht = self.hits.add(tb, ID_TODAY);
        cv.fill_rrect(tb, r_all(10.0), with_a(pal.red, if ht { 0.34 } else { 0.18 }));
        cv.text("Today", &TextStyle::new(pal.fs(11.0), pal.fg_hi).bold(), tb.center(), HAlign::Center, VAlign::Middle);
        cv.fill_rect(rect(c.x, c.y + 28.0, c.w, 1.0), with_a(pal.fg, 0.10));

        let pattern = st.settings.behavior.clock_format.chrono_pattern();
        let status = cal.note.clone().unwrap_or_else(|| cal.status_line(pattern));
        let list = rect(c.x, c.y + 34.0, c.w, c.h - 34.0 - 26.0);
        let mut events: Vec<&gcal::Event> = cal.events.iter().filter(|e| e.on_day(cal.selected)).collect();
        events.sort_by(|a, b| b.all_day.cmp(&a.all_day).then(a.start.cmp(&b.start)).then(a.title.cmp(&b.title)));
        let mut rems: Vec<&reminders::Reminder> = cal.reminders.iter().filter(|r| r.at.date_naive() == cal.selected).collect();
        rems.sort_by_key(|r| r.at);
        let n = events.len() + rems.len();
        self.scroll.content_h = n as f32 * 38.0;
        self.scroll.clamp(list.h);
        if n == 0 { cv.text_in("Nothing planned", &pal.text_dim(12.0), rect(list.x, list.y + 4.0, list.w, 22.0), HAlign::Left); }
        cv.with_clip(list, [0.0; 4], |cv| {
            let mut i = 0usize;
            for e in &events {
                let r = rect(list.x, list.y + i as f32 * 38.0 - self.scroll.offset, list.w - 6.0, 34.0);
                cv.fill_rrect(r, r_all(12.0), with_a(pal.fg, 0.06));
                cv.fill_rrect(rect(r.x + 6.0, r.y + 5.0, 4.0, r.h - 10.0), r_all(2.0), if e.link.is_some() { pal.accent_hi } else { pal.accent });
                let when = if e.all_day { "All day".to_string() } else { e.start.format(pattern).to_string() };
                let tw = if e.link.is_some() { r.w - 80.0 } else { r.w - 22.0 };
                cv.text_in(&e.title, &TextStyle::new(pal.fs(12.0), pal.fg_hi).weight(600), rect(r.x + 16.0, r.y + 3.0, tw, 16.0), HAlign::Left);
                cv.text_in(&format!("{when} · {}", e.calendar_name), &pal.text_dim(10.0), rect(r.x + 16.0, r.y + 19.0, tw, 13.0), HAlign::Left);
                if e.link.is_some() {
                    let j = rect(r.right() - 52.0, r.y + 6.0, 46.0, 22.0);
                    let hj = self.hits.add_in(j, list, ID_JOIN0 + i as u32);
                    cv.fill_rrect(j, r_all(9.0), with_a(pal.accent, if hj { 0.42 } else { 0.22 }));
                    cv.text("Join", &TextStyle::new(pal.fs(11.0), pal.accent_hi).bold(), j.center(), HAlign::Center, VAlign::Middle);
                }
                i += 1;
            }
            self.rem_units.clear();
            for (j, r_) in rems.iter().enumerate() {
                let r = rect(list.x, list.y + i as f32 * 38.0 - self.scroll.offset, list.w - 6.0, 34.0);
                let gone = cal.deleting.as_deref() == Some(r_.unit.as_str());
                cv.with_alpha(if gone { 0.45 } else { 1.0 }, |cv| {
                    cv.fill_rrect(r, r_all(12.0), with_a(pal.fg, 0.06));
                    cv.fill_rrect(rect(r.x + 6.0, r.y + 5.0, 4.0, r.h - 10.0), r_all(2.0), pal.yellow);
                    let tw = r.w - 22.0 - 56.0;
                    cv.text_in(&r_.title, &TextStyle::new(pal.fs(12.0), pal.fg_hi).weight(600), rect(r.x + 16.0, r.y + 3.0, tw, 16.0), HAlign::Left);
                    cv.text_in(&format!("{} · Omarchy", r_.at.format(pattern)), &pal.text_dim(10.0), rect(r.x + 16.0, r.y + 19.0, tw, 13.0), HAlign::Left);
                    let edit = rect(r.right() - 54.0, r.y + 5.0, 24.0, 24.0);
                    let del = rect(r.right() - 28.0, r.y + 5.0, 24.0, 24.0);
                    let (he, hd) = if gone { (false, false) } else { (self.hits.add_in(edit, list, ID_REM_EDIT0 + j as u32), self.hits.add_in(del, list, ID_REM_DEL0 + j as u32)) };
                    cv.fill_circle(edit.center(), 11.0, with_a(pal.fg, if he { 0.22 } else { 0.08 }));
                    cv.text("\u{f040}", &TextStyle::icon(11.0, pal.fg_hi), edit.center(), HAlign::Center, VAlign::Middle);
                    cv.fill_circle(del.center(), 11.0, with_a(pal.red, if hd { 0.45 } else { 0.16 }));
                    cv.text("\u{f1f8}", &TextStyle::icon(11.0, if hd { pal.fg_hi } else { pal.red }), del.center(), HAlign::Center, VAlign::Middle);
                });
                self.rem_units.push(r_.unit.clone());
                i += 1;
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
        // footer: status + connect
        let foot = rect(c.x, c.bottom() - 22.0, c.w, 22.0);
        if cal.last_success.is_none() || cal.google_error.is_some() {
            let b = rect(foot.right() - 64.0, foot.y, 64.0, 22.0);
            let hb = self.hits.add(b, ID_CONNECT);
            button(cv, pal, b, if cal.authorizing { "Waiting…" } else { "Connect" }, ButtonKind::Accent, hb && !cal.authorizing, false);
            cv.text_in(&status, &pal.text_dim(9.5), rect(foot.x, foot.y, foot.w - 70.0, foot.h), HAlign::Left);
        } else {
            cv.text_in(&status, &pal.text_dim(9.5), foot, HAlign::Left);
        }
        if let Some(e) = &cal.native_error { cv.text_in(&format!("Omarchy: {e}"), &pal.text_dim(9.5), rect(c.x, c.bottom() - 38.0, c.w, 14.0), HAlign::Left); }
    }

    fn event_month(&mut self, ev: &Event, area: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, area.h - 100.0); return; }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        let cal = &mut ctx.state.cal;
        match c.id {
            ID_PREV => { let d = add_months(cal.selected, -1); cal.select_date(d); }
            ID_NEXT => { let d = add_months(cal.selected, 1); cal.select_date(d); }
            ID_TODAY => cal.select_date(Local::now().date_naive()),
            ID_ADD => cal.open_editor(),
            ID_CONNECT => cal.authorize(false),
            id if (ID_REM_EDIT0..ID_REM_EDIT0 + 1000).contains(&id) => {
                if let Some(u) = self.rem_units.get((id - ID_REM_EDIT0) as usize).cloned() { cal.edit_reminder(&u); }
            }
            id if (ID_REM_DEL0..ID_REM_DEL0 + 1000).contains(&id) => {
                if let Some(u) = self.rem_units.get((id - ID_REM_DEL0) as usize).cloned() { cal.delete_reminder(&u); }
            }
            id if (ID_DAY0..ID_DAY0 + 42).contains(&id) => { if let Some(d) = self.grid.get((id - ID_DAY0) as usize).copied() { cal.select_date(d); } }
            id if id >= ID_JOIN0 && id < ID_FIELD0 => {
                let sel = cal.selected;
                let mut events: Vec<&gcal::Event> = cal.events.iter().filter(|e| e.on_day(sel)).collect();
                events.sort_by(|a, b| b.all_day.cmp(&a.all_day).then(a.start.cmp(&b.start)).then(a.title.cmp(&b.title)));
                if let Some(link) = events.get((id - ID_JOIN0) as usize).and_then(|e| e.link.clone()) {
                    let _ = std::process::Command::new("xdg-open").arg(link).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                }
            }
            _ => {}
        }
    }

    fn fields<'a>(e: &'a mut Editor) -> Vec<&'a mut TextField> {
        if e.dest == 0 { vec![&mut e.native_title, &mut e.minutes] } else if e.all_day { vec![&mut e.google_title, &mut e.date] } else { vec![&mut e.google_title, &mut e.date, &mut e.time, &mut e.duration] }
    }

    fn draw_editor(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        let cal = &st.cal;
        let e = &cal.editor;
        let busy = cal.busy();
        let back = rect(area.x, area.y, 86.0, 26.0);
        let hb = self.hits.add(back, ID_CANCEL);
        button(cv, pal, back, "\u{f053} Cancel", ButtonKind::Plain, hb, false);
        cv.text(if e.editing.is_some() { "Edit reminder" } else { "Add an event or reminder" }, &TextStyle::new(pal.fs(13.0), pal.fg_hi).bold(), pt(area.right(), area.y + 13.0), HAlign::Right, VAlign::Middle);
        let seg = rect(area.x, area.y + 36.0, area.w, 28.0);
        for (i, (id, label)) in [(ID_DEST0, "Omarchy reminder"), (ID_DEST1, "Google Calendar · Primary")].into_iter().enumerate() {
            if e.editing.is_some() && i == 1 { continue; }
            let r = rect(seg.x + i as f32 * seg.w / 2.0, seg.y, seg.w / 2.0 - 3.0, seg.h);
            let hov = self.hits.add(r, id);
            let on = e.dest == i;
            cv.fill_rrect(r, r_all(10.0), if on { with_a(pal.accent, 0.28) } else { with_a(pal.fg, if hov { 0.12 } else { 0.06 }) });
            cv.text(label, &TextStyle::new(pal.fs(12.0), if on { pal.accent_hi } else { pal.fg }).weight(600), r.center(), HAlign::Center, VAlign::Middle);
        }
        let mut y = seg.bottom() + 10.0;
        let labels: Vec<&str> = if e.dest == 0 { vec!["Title", "Remind me in (minutes)"] } else if e.all_day { vec!["Title", "Date (YYYY-MM-DD)"] } else { vec!["Title", "Date (YYYY-MM-DD)", "Time (HH:MM)", "Duration (minutes)"] };
        // fields are drawn from the live editor through interior copies in `Self::fields` order
        let n = labels.len();
        let mut idx = 0usize;
        let st_dim = pal.text_dim(10.5);
        let rows: Vec<(&str, &TextField)> = {
            let ee = &cal.editor;
            let list: Vec<&TextField> = if ee.dest == 0 { vec![&ee.native_title, &ee.minutes] } else if ee.all_day { vec![&ee.google_title, &ee.date] } else { vec![&ee.google_title, &ee.date, &ee.time, &ee.duration] };
            labels.iter().copied().zip(list).collect()
        };
        for (label, field) in rows {
            cv.text(label, &st_dim, pt(area.x + 2.0, y + 14.0), HAlign::Left, VAlign::Middle);
            let r = rect(area.x + 150.0, y, area.w - 150.0, 28.0);
            self.hits.add_cursor(r, ID_FIELD0 + idx as u32, Cursor::Text);
            let mut f = field.clone();
            f.draw(cv, pal, r);
            idx += 1;
            y += 34.0;
        }
        let _ = n;
        if e.dest == 0 {
            let due = crate::calendar::positive_minutes(&cal.editor.minutes.text).and_then(|m| Local::now().checked_add_signed(ChronoDuration::minutes(m as i64)).ok_or_else(|| "Delay is too large".to_string()));
            let pattern = st.settings.behavior.clock_format.chrono_pattern();
            let text = match due { Ok(at) => format!("Due {} {}", at.format("%a, %-d %b %Y"), at.format(pattern)), Err(e) => e };
            cv.text_in(&text, &pal.text_dim(11.0), rect(area.x, y, area.w, 18.0), HAlign::Left);
            y += 22.0;
        } else {
            let r = rect(area.x, y, 120.0, 24.0);
            let ha = self.hits.add(r, ID_ALLDAY);
            cv.fill_rrect(rect(r.x, r.y + 4.0, 16.0, 16.0), r_all(4.0), if e.all_day { pal.accent } else { with_a(pal.fg, if ha { 0.24 } else { 0.14 }) });
            if e.all_day { cv.text("\u{f00c}", &TextStyle::icon(10.0, pal.bg), pt(r.x + 8.0, r.y + 12.0), HAlign::Center, VAlign::Middle); }
            cv.text("All day", &pal.text(12.0), pt(r.x + 24.0, r.y + 12.0), HAlign::Left, VAlign::Middle);
            let ar = rect(area.x + 150.0, y, area.w - 150.0, 26.0);
            let hh = self.hits.add(ar, ID_ALERT);
            cv.fill_rrect(ar, r_all(10.0), with_a(pal.fg, if hh { 0.12 } else { 0.07 }));
            cv.text_in(&format!("Alert: {}", ALERTS[e.alert as usize % ALERTS.len()]), &pal.text(12.0), ar.pad_xy(10.0, 0.0), HAlign::Left);
            y += 32.0;
            if !cal.write_access {
                let b = rect(area.x, y, 200.0, 24.0);
                let hb = self.hits.add(b, ID_AUTH);
                button(cv, pal, b, if cal.authorizing { "Waiting for browser…" } else { "Enable Google event creation" }, ButtonKind::Plain, hb && !cal.authorizing, false);
                cv.text_in("Needs permission to edit events on calendars you own.", &pal.text_dim(10.0), rect(b.right() + 8.0, y, area.right() - b.right() - 8.0, 24.0), HAlign::Left);
                y += 30.0;
            }
        }
        let save = rect(area.right() - 90.0, area.bottom() - 28.0, 90.0, 28.0);
        let can_save = !busy && !cal.authorizing && (e.dest == 0 || cal.write_access);
        let hs = self.hits.add(save, ID_SAVE);
        cv.with_alpha(if can_save { 1.0 } else { 0.45 }, |cv| button(cv, pal, save, "Save", ButtonKind::Accent, hs && can_save, false));
        cv.text_in(&e.status, &pal.text(11.0), rect(area.x, area.bottom() - 26.0, area.w - 100.0, 24.0), HAlign::Left);
        let _ = y;
    }

    fn event_editor(&mut self, ev: &Event, _area: Rect, ctx: &mut Ctx) {
        let ts = &mut *ctx.text;
        let field_style = ctx.state.pal.text(12.0);
        let cal = &mut ctx.state.cal;
        if let Event::Key(k) = ev {
            use smithay_client_toolkit::seat::keyboard::Keysym;
            if cal.busy() { return; }
            if k.sym == Keysym::Tab {
                let e = &mut cal.editor;
                let mut fs = Self::fields(e);
                let cur = fs.iter().position(|f| f.focused).unwrap_or(0);
                let next = if k.shift { (cur + fs.len() - 1) % fs.len() } else { (cur + 1) % fs.len() };
                for (i, f) in fs.iter_mut().enumerate() { f.focused = i == next; }
                return;
            }
            let mut res = FieldResult::None;
            for f in Self::fields(&mut cal.editor) { if f.focused { res = f.key(k); } }
            match res {
                FieldResult::Submit => cal.save(),
                FieldResult::Cancel => cal.cancel_editor(),
                _ => {}
            }
            return;
        }
        // pointer: focus field on press (needs hit order before click detection)
        if let Event::Press(Button::Left, p) = ev {
            if let Some(id) = self.hits.at(*p).filter(|i| *i >= ID_FIELD0) {
                let idx = (id - ID_FIELD0) as usize;
                let fx = p.x - (_area.x + 150.0 + 10.0);
                let mut fs = Self::fields(&mut cal.editor);
                for (i, f) in fs.iter_mut().enumerate() { f.focused = i == idx; }
                if let Some(f) = fs.get_mut(idx) {
                    f.click_at(ts, &field_style, fx, false);
                }
            }
        }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        match c.id {
            ID_CANCEL => cal.cancel_editor(),
            ID_SAVE => cal.save(),
            ID_DEST0 if !cal.busy() => cal.editor.dest = 0,
            ID_DEST1 if !cal.busy() && cal.editor.editing.is_none() => cal.editor.dest = 1,
            ID_ALLDAY if !cal.busy() => cal.editor.all_day = !cal.editor.all_day,
            ID_ALERT if !cal.busy() => cal.editor.alert = (cal.editor.alert + 1) % ALERTS.len() as u32,
            ID_AUTH => cal.authorize(true),
            _ => {}
        }
        let _ = NavTo::Collapse;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_input_is_positive_and_whole() {
        for invalid in ["", "0", "-1", "+1", "1.5", " 15", "4294967296"] { assert!(positive_minutes(invalid).is_err()); }
        assert_eq!(positive_minutes("15"), Ok(15));
    }

    #[test]
    fn drafts_validate_dates_times_titles_and_alerts() {
        for (date, time, duration) in [("2026-02-30", "09:00", "30"), ("2026-01-01", "24:01", "30"), ("2026-01-01", "9:00", "30"), ("2026-01-01", "09:00", "0")] {
            assert!(build_draft("Meeting", date, time, duration, false, 2).is_err());
        }
        assert!(build_draft(" ", "2026-01-01", "09:00", "30", false, 2).is_err());
        let draft = build_draft("Café; $(nothing)", "2026-01-15", "09:00", "30", false, 5).unwrap();
        assert_eq!(draft.end.signed_duration_since(draft.start), ChronoDuration::minutes(30));
        assert_eq!(draft.alert_minutes, None);
        let all_day = build_draft("Holiday", "2026-01-31", "ignored", "ignored", true, 2).unwrap();
        assert_eq!(all_day.end.date_naive(), NaiveDate::from_ymd_opt(2026, 2, 1).unwrap());
        assert_eq!(all_day.alert_minutes, Some(15));
    }

    #[test]
    fn stale_range_and_pre_creation_results_are_rejected() {
        let range = gcal::CalendarRange::for_month(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()).unwrap();
        let next = gcal::CalendarRange::for_month(NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()).unwrap();
        assert!(accept_refresh(7, Some(range), range, 7));
        assert!(!accept_refresh(7, Some(range), range, 6));
        assert!(!accept_refresh(7, Some(range), next, 7));
    }

    #[test]
    fn immediate_upserts_keep_calendar_identity_separate() {
        let date = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let event = gcal::Event { id: "same".into(), calendar_id: "primary".into(), calendar_name: "Primary".into(), title: "Test".into(), start: date, end: date + ChronoDuration::minutes(30), all_day: false, link: None, location: String::new() };
        let mut events = Vec::new();
        upsert_event(&mut events, event.clone());
        upsert_event(&mut events, event.clone());
        assert_eq!(events.len(), 1);
        let mut other = event;
        other.calendar_id = "secondary".into();
        upsert_event(&mut events, other);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn grid_is_six_sunday_first_weeks_covering_the_month() {
        let g = month_grid(NaiveDate::from_ymd_opt(2026, 11, 18).unwrap());
        assert_eq!(g.len(), 42);
        assert_eq!(g[0].weekday(), chrono::Weekday::Sun);
        assert!(g.contains(&NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()) && g.contains(&NaiveDate::from_ymd_opt(2026, 11, 30).unwrap()));
        assert_eq!(add_months(NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(), 1), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());
        assert_eq!(add_months(NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), -1), NaiveDate::from_ymd_opt(2025, 12, 15).unwrap());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::page::testutil::Harness;

    #[test]
    fn render_month_and_editor() {
        let mut st = State::new();
        let now = Local::now().naive_local();
        st.cal.events.push(gcal::Event { id: "1".into(), calendar_id: "p".into(), calendar_name: "Primary".into(), title: "Team sync".into(), start: now, end: now + ChronoDuration::minutes(30), all_day: false, link: Some("https://meet.example".into()), location: String::new() });
        st.cal.last_success = Some(Local::now());
        let mut h = Harness::new();
        let mut p = CalendarPage::new();
        h.render(&mut p, &mut st, "/tmp/cal_month.png");
        st.cal.open_editor();
        h.render(&mut p, &mut st, "/tmp/cal_editor.png");
    }
}
