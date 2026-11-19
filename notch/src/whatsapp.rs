//! WhatsApp page hosting. The WhatsApp plugin's QML window connects to the
//! notch's private socket (`notch_content`), asks for the page (`open`), gets
//! the settled slot rectangle plus the theme (`layout`) and draws its UI over
//! the slot. The notch owns the glass backplate and the page slot; this module
//! holds the session state machine and tells the notch what to do (`Out`).

use crate::layout::{Rect, Size};
use crate::notch_content::{self as nc, Bridge, Event as NcEvent, Phase, Session, Tick};
use crate::page::Page;
use crate::render::{Canvas, HAlign, TextSys, VAlign};
use crate::state::State;
use crate::wayland::{Ctx, DrawCtx, Event, Poster};
use std::cell::Cell;
use std::collections::VecDeque;
use std::time::Instant;

pub const CONTENT_H: f32 = 420.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CloseTo { Collapse, Home, Tab(&'static str) }

/// What the notch must do after `drive`.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Show(&'static str),
    Expand,
    Collapse,
    ScheduleCollapse(u64),
    CancelCollapse,
    /// Leave the WhatsApp page for `page` (only if it is the visible one).
    Leave(&'static str),
}

struct Ext {
    s: Session,
    pin: Option<String>,
    after: CloseTo,
    last: Option<(String, [i32; 4])>,
    prev: Option<[i32; 4]>,
    stable: u32,
}

pub struct Wa {
    bridge: Option<Bridge>,
    events: VecDeque<NcEvent>,
    ext: Option<Ext>,
    /// Slot rectangle (notch-surface coords) recorded by the page while it draws.
    pub slot: Cell<Option<Rect>>,
    started: Instant,
    /// The content reported its first mapping.
    pub mapped: bool,
}

impl Default for Wa {
    fn default() -> Self { Wa { bridge: None, events: VecDeque::new(), ext: None, slot: Cell::new(None), started: Instant::now(), mapped: false } }
}

impl Wa {
    fn now_ms(&self) -> u64 { self.started.elapsed().as_millis() as u64 }
    fn send(&self, conn: u64, frame: serde_json::Value) { if let Some(b) = &self.bridge { b.send(conn, frame); } }
    pub fn live(&self) -> bool { self.ext.is_some() }
    /// Output the notch must stay on while a follow-mode session is live.
    pub fn pin(&self) -> Option<&str> { self.ext.as_ref().and_then(|e| e.pin.as_deref()) }
    pub fn holds_open(&self) -> bool { self.ext.as_ref().is_some_and(|e| e.s.holds_open()) }
}

/// Bind the content socket. Call once; the shell PID restricts who may connect.
pub fn start(shell_pid: i32, poster: &Poster) {
    let p = poster.clone();
    let sink: nc::Sink = std::sync::Arc::new(move |ev| { p.post(move |st, _| st.wa.events.push_back(ev)); true });
    match nc::start(shell_pid, sink) {
        Ok(b) => { poster.post(move |st, _| st.wa.bridge = Some(b)); }
        Err(e) => eprintln!("waybar-notch: WhatsApp content socket: {e}"),
    }
}

/// End the live session (tell the content why). `leave` is the page to go to if WhatsApp is visible.
pub fn drop_session(st: &mut State, reason: &str, leave: Option<&'static str>, out: &mut Vec<Out>) {
    let Some(e) = st.wa.ext.take() else { return };
    let conn = e.s.conn;
    st.wa.send(conn, serde_json::json!({"op": "hide", "session": conn, "reason": reason}));
    if let Some(b) = &st.wa.bridge { b.disconnect(conn); }
    st.wa.mapped = false;
    if let Some(p) = leave { out.push(Out::Leave(p)); }
}

/// Start the bounded close; false when no session is live.
pub fn close(st: &mut State, to: CloseTo) -> bool {
    let now = st.wa.now_ms();
    let conn = {
        let Some(e) = st.wa.ext.as_mut() else { return false };
        e.after = to;
        if !e.s.begin_close(now) { return true; }
        e.s.conn
    };
    st.wa.send(conn, serde_json::json!({"op": "hide", "session": conn, "reason": "close"}));
    true
}

fn finish(st: &mut State, out: &mut Vec<Out>) {
    let Some(after) = st.wa.ext.as_ref().map(|e| e.after) else { return };
    match after {
        CloseTo::Tab(t) => drop_session(st, "closed", Some(t), out),
        _ => drop_session(st, "closed", Some("home"), out),
    }
    if after == CloseTo::Collapse { out.push(Out::Collapse); }
}

fn fail(st: &mut State, conn: u64, why: &str, out: &mut Vec<Out>) {
    if st.wa.ext.as_ref().is_none_or(|e| e.s.conn != conn) { return; }
    st.wa.send(conn, serde_json::json!({"op": "error", "message": why}));
    drop_session(st, "error", Some("home"), out);
    out.push(Out::Collapse);
}

fn on_event(st: &mut State, ev: NcEvent, out: &mut Vec<Out>) {
    match ev {
        NcEvent::Failed(e) => eprintln!("waybar-notch: WhatsApp content: {e}"),
        NcEvent::Open { conn, output } => open(st, conn, output, out),
        NcEvent::Mapped { conn, serial } => {
            if let Some(e) = st.wa.ext.as_mut().filter(|e| e.s.conn == conn) { if e.s.on_mapped(serial) { st.wa.mapped = true; } }
        }
        NcEvent::Interaction { conn, hovered, busy } => {
            let Some(e) = st.wa.ext.as_mut().filter(|e| e.s.conn == conn) else { return };
            let was_held = e.s.holds_open();
            e.s.on_interaction(hovered, busy);
            if hovered || busy { out.push(Out::CancelCollapse); }
            else if was_held && !st.notch.hover { out.push(Out::ScheduleCollapse(st.settings.behavior.collapse_delay_ms as u64)); }
        }
        NcEvent::Close { conn, home } => {
            if st.wa.ext.as_ref().is_some_and(|e| e.s.conn == conn) { close(st, if home { CloseTo::Home } else { CloseTo::Collapse }); }
        }
        NcEvent::Hidden { conn } => {
            if st.wa.ext.as_ref().is_some_and(|e| e.s.conn == conn && e.s.phase == Phase::Closing) { finish(st, out); }
        }
        NcEvent::Gone { conn } => match st.wa.ext.as_ref().filter(|e| e.s.conn == conn).map(|e| e.s.phase) {
            Some(Phase::Closing) => finish(st, out),
            Some(Phase::Open) => { drop_session(st, "connection lost", Some("home"), out); out.push(Out::Collapse); }
            None => {}
        },
    }
}

fn open(st: &mut State, conn: u64, output: String, out: &mut Vec<Out>) {
    let reject = |st: &State, why: &str| {
        st.wa.send(conn, serde_json::json!({"op": "error", "message": why}));
        if let Some(b) = &st.wa.bridge { b.disconnect(conn); }
    };
    // Validate before touching a live session: a stale request must not tear down a valid one. The page is only
    // ever hosted on the output the notch really occupies.
    if st.notch.output.as_deref() != Some(output.as_str()) { return reject(st, &format!("the notch is not on output {output}")); }
    if st.desk.snap.monitor(&output).is_none() { return reject(st, &format!("output {output} is not connected")); }
    if st.bar_hidden { return reject(st, "the notch is hidden"); }
    if st.sys.capture_pending || st.desk.capture_active { return reject(st, "a screenshot or recording is in progress"); }
    let follow = st.settings.monitor.mode == crate::config::MonitorMode::Follow;
    if st.settings.behavior.hide_on_fullscreen && st.desk.is_fullscreen(&output) { return reject(st, "the notch is hidden by a fullscreen window"); }
    // a replacement session supersedes the old one without bouncing through Home
    drop_session(st, "replaced", None, out);
    let now = st.wa.now_ms();
    st.wa.ext = Some(Ext { s: Session::new(conn, now), pin: follow.then(|| output.clone()), after: CloseTo::Collapse, last: None, prev: None, stable: 0 });
    st.wa.mapped = false;
    out.push(Out::CancelCollapse);
    out.push(Out::Show("whatsapp"));
    out.push(Out::Expand);
    out.push(Out::ScheduleCollapse(3000)); // grace for the pointer to travel from the bar; entering either window cancels it
}

/// Start a session the way the content socket would (other modules' tests need a live one).
#[cfg(test)]
pub fn open_for_test(st: &mut State, output: &str) {
    st.wa.events.push_back(NcEvent::Open { conn: 1, output: output.into() });
    drive(st, false);
}

/// Process queued events, timers and layout publication. `settled`: the card finished opening/resizing.
pub fn drive(st: &mut State, settled: bool) -> Vec<Out> {
    let mut out = Vec::new();
    while let Some(ev) = st.wa.events.pop_front() { on_event(st, ev, &mut out); }
    let now = st.wa.now_ms();
    if let Some((conn, tick)) = st.wa.ext.as_ref().map(|e| (e.s.conn, e.s.tick(now))) {
        match tick {
            Tick::Idle => {}
            Tick::MapTimeout => fail(st, conn, "WhatsApp did not appear in the notch in time", &mut out),
            Tick::CloseTimeout => finish(st, &mut out),
        }
    }
    publish(st, settled, &mut out);
    out
}

/// The slot moved while content is shown: hide it until the next settled layout.
fn publish(st: &mut State, settled: bool, out: &mut Vec<Out>) {
    let Some(conn) = st.wa.ext.as_ref().map(|e| e.s.conn) else { return };
    let slot = st.wa.slot.get().filter(|_| settled && st.notch.expanded);
    let Some(oi) = st.notch.output.clone() else { return };
    let Some(mon) = st.desk.snap.monitor(&oi).cloned() else { return };
    let rect = slot.map(|r| [(st.notch.origin.0 + r.x).round() as i32, (st.notch.origin.1 + r.y).round() as i32, r.w.round() as i32, r.h.round() as i32]);
    let (ow, oh) = (mon.logical_width.round() as i32, mon.logical_height.round() as i32);
    let invalidate = {
        let Some(e) = st.wa.ext.as_mut() else { return };
        match rect {
            None => { e.stable = 0; e.prev = None; false }
            Some(r) => {
                if e.prev == Some(r) { e.stable += 1; } else { e.stable = 0; e.prev = Some(r); }
                e.last.as_ref().is_some_and(|(_, l)| *l != r) && e.stable == 0
            }
        }
    };
    if invalidate {
        if let Some(e) = st.wa.ext.as_mut() { e.last = None; e.s.suspended(); }
        st.wa.mapped = false;
        st.wa.send(conn, serde_json::json!({"op": "suspend", "session": conn}));
    }
    let Some(r) = rect else { return };
    if st.wa.ext.as_ref().is_none_or(|e| e.stable < 2) { return; }
    if r[0] < 0 || r[1] < 0 || r[0] + r[2] > ow || r[1] + r[3] > oh {
        return fail(st, conn, "the notch content falls outside its output", out);
    }
    let serial = {
        let Some(e) = st.wa.ext.as_mut() else { return };
        if e.last.as_ref() == Some(&(oi.clone(), r)) { return; }
        e.last = Some((oi.clone(), r));
        e.s.layout_sent()
    };
    let t = crate::theme::current();
    let a = &st.settings.appearance;
    st.wa.send(conn, serde_json::json!({
        "op": "layout", "version": nc::VERSION, "session": conn, "serial": serial, "output": oi,
        "rect": {"x": r[0], "y": r[1], "width": r[2], "height": r[3]},
        "theme": {
            "background": crate::theme::hex(t.bg), "foreground": crate::theme::hex(t.fg), "foreground_hi": crate::theme::hex(t.fg_hi()),
            "accent": crate::theme::hex(t.accent), "alert": crate::theme::hex(t.red), "font_family": t.font,
            "font_size": a.font_size, "radius": a.corner_radius, "animations": st.settings.behavior.animations,
        },
    }));
}

// ---------------------------------------------------------------- page

#[derive(Default)]
pub struct WhatsAppPage;

impl Page for WhatsAppPage {
    fn natural(&mut self, avail: Size, _: &State, _: &mut TextSys) -> Size { Size { w: avail.w + 32.0, h: CONTENT_H } }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        dc.state.wa.slot.set(Some(area));
        if !dc.state.wa.mapped {
            let pal = &dc.state.pal;
            cv.text("Opening WhatsApp…", &pal.text_dim(12.0), area.center(), HAlign::Center, VAlign::Middle);
        }
    }

    fn event(&mut self, _: &Event, _: Rect, _: &mut Ctx) {}
    fn hidden(&mut self, ctx: &mut Ctx) { ctx.state.wa.slot.set(None); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_output() -> State {
        let mut st = State::new();
        let mon = |id: i64, name: &str, x: i64| serde_json::json!({"id": id, "name": name, "width": 1600, "height": 1000, "scale": 1.0, "x": x, "y": 0, "focused": id == 0, "activeWorkspace": {"id": id + 1}, "specialWorkspace": {"id": 0}});
        st.desk.snap = crate::desktop::parse_snapshot(&serde_json::json!([mon(0, "X", 0), mon(1, "Y", 1600)]), &serde_json::json!([]));
        st.notch.output = Some("X".into());
        st
    }

    #[test]
    fn open_request_creates_session_and_expands() {
        let mut st = state_with_output();
        st.wa.events.push_back(NcEvent::Open { conn: 1, output: "X".into() });
        let out = drive(&mut st, false);
        assert!(st.wa.live());
        assert!(out.contains(&Out::Show("whatsapp")) && out.contains(&Out::Expand));
        assert_eq!(st.wa.pin(), Some("X"), "follow mode pins the output");
    }

    #[test]
    fn unknown_output_is_rejected() {
        let mut st = state_with_output();
        st.wa.events.push_back(NcEvent::Open { conn: 1, output: "NOPE".into() });
        let out = drive(&mut st, false);
        assert!(!st.wa.live() && out.is_empty());
    }

    #[test]
    fn request_for_an_output_the_notch_is_not_on_is_rejected_and_keeps_a_valid_session() {
        let mut st = state_with_output();
        st.wa.events.push_back(NcEvent::Open { conn: 1, output: "X".into() });
        drive(&mut st, false);
        assert!(st.wa.live());
        // Y is connected, but the notch is on X: stale request
        st.wa.events.push_back(NcEvent::Open { conn: 2, output: "Y".into() });
        let out = drive(&mut st, false);
        assert!(st.wa.live() && st.wa.pin() == Some("X"), "the valid session survives");
        assert!(!out.contains(&Out::Leave("home")));
        // nothing published yet: also rejected
        let mut st = state_with_output();
        st.notch.output = None;
        st.wa.events.push_back(NcEvent::Open { conn: 3, output: "X".into() });
        drive(&mut st, false);
        assert!(!st.wa.live());
        // hidden bar
        let mut st = state_with_output();
        st.bar_hidden = true;
        st.wa.events.push_back(NcEvent::Open { conn: 4, output: "X".into() });
        drive(&mut st, false);
        assert!(!st.wa.live());
    }

    #[test]
    fn close_then_hidden_leaves_home_and_collapses() {
        let mut st = state_with_output();
        st.wa.events.push_back(NcEvent::Open { conn: 1, output: "X".into() });
        drive(&mut st, false);
        assert!(close(&mut st, CloseTo::Collapse));
        st.wa.events.push_back(NcEvent::Hidden { conn: 1 });
        let out = drive(&mut st, false);
        assert!(!st.wa.live());
        assert!(out.contains(&Out::Leave("home")) && out.contains(&Out::Collapse));
    }

    #[test]
    fn lost_connection_while_open_drops_and_collapses() {
        let mut st = state_with_output();
        st.wa.events.push_back(NcEvent::Open { conn: 1, output: "X".into() });
        drive(&mut st, false);
        st.wa.events.push_back(NcEvent::Gone { conn: 1 });
        let out = drive(&mut st, false);
        assert!(!st.wa.live() && out.contains(&Out::Collapse));
    }
}
