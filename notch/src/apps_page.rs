//! Applications page: every open or pinned application (configured apps included), per-window focus rows,
//! pin/unpin and the installed-application chooser used when a window's desktop entry cannot be inferred.
use crate::apps::{Shortcut, norm_class};
use crate::config::{self, PinnedApp};
use crate::desktop_entries;
use crate::layout::{Rect, Scroll, Size, pt, rect};
use crate::page::Page;
use crate::render::{Canvas, HAlign, TextStyle, TextSys, VAlign, r_all, with_a};
use crate::state::{NavTo, State};
use crate::ui::{ButtonKind, Hits, Palette, button, scrollbar};
use crate::wayland::{Button, Ctx, Cursor, DrawCtx, Event};

const ROW_H: f32 = 40.0;
const WIN_H: f32 = 32.0;
const HEAD_H: f32 = 30.0;
const ID_BASE: u32 = 100;

/// Paint a shortcut's icon in `r`: the configured glyph, the decoded desktop icon, or a letter tile.
pub fn paint_icon(cv: &mut Canvas, pal: &Palette, st: &State, s: &Shortcut, r: Rect) {
    if let Some(b) = s.configured_id.as_deref().and_then(|id| st.apps.button(id)) {
        let running = st.apps.running(s);
        let color = if running { crate::pills::brand(&b.id, pal) } else { pal.dim };
        cv.text(&b.glyph, &TextStyle::icon(r.h * 0.72, color), r.center(), HAlign::Center, VAlign::Middle);
        return;
    }
    match st.apps.icons.get(&s.icon_key) {
        Some(Some(img)) => cv.image(img, r, r_all(r.h * 0.18)),
        _ => {
            cv.fill_rrect(r, r_all(r.h * 0.28), with_a(pal.accent, 0.30));
            let letter: String = s.name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
            cv.text(&letter, &TextStyle::new(r.h * 0.55, pal.fg_hi).weight(700), r.center(), HAlign::Center, VAlign::Middle);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Back,
    Row(String),
    Pin(String),
    Unpin(String),
    Window(String, String),
    Choose(String),
    CancelChoose,
}

struct Choosing {
    class: String,
}

pub struct AppsPage {
    hits: Hits,
    scroll: Scroll,
    view_h: f32,
    acts: Vec<(u32, Act)>,
    pressed: Option<Act>,
    expanded: Option<String>,
    choosing: Option<Choosing>,
    busy: bool,
    status: Option<String>,
}

impl AppsPage {
    pub fn new() -> AppsPage {
        AppsPage { hits: Hits::default(), scroll: Scroll::default(), view_h: 0.0, acts: Vec::new(), pressed: None, expanded: None, choosing: None, busy: false, status: None }
    }

    fn meta(st: &State, s: &Shortcut) -> String {
        if s.pin.is_some() && st.apps.catalog_ready && !st.apps.catalog.iter().any(|e| Some(&e.id) == s.desktop_id.as_ref()) && s.windows == 0 {
            return "Application unavailable".into();
        }
        match (s.windows, s.pin.is_some(), s.configured_id.is_some()) {
            (0, true, _) => "Pinned · not running".into(),
            (0, false, true) => if st.apps.running(s) { "Running".into() } else { "Not running".into() },
            (1, p, _) => format!("1 window{}", if p { " · pinned" } else { "" }),
            (n, p, _) => format!("{n} windows{}", if p { " · pinned" } else { "" }),
        }
    }

    fn content_h(&self, st: &State) -> f32 {
        if self.choosing.is_some() { return st.apps.catalog.len().max(1) as f32 * ROW_H; }
        let mut h = 0.0;
        for s in &st.apps.shortcuts {
            h += ROW_H;
            if self.expanded.as_deref() == Some(s.key.as_str()) {
                h += st.apps.open.iter().find(|o| o.key == s.key).map_or(0, |o| o.windows.len()) as f32 * WIN_H;
            }
        }
        h.max(ROW_H)
    }

    fn save_pin(&mut self, ctx: &mut Ctx, pin: Option<PinnedApp>, remove_class: Option<String>) {
        if self.busy { return; }
        self.busy = true;
        self.status = Some("Saving…".into());
        let poster = ctx.poster.clone();
        std::thread::spawn(move || {
            let res = config::update_pins(|pins| {
                if let Some(c) = &remove_class { pins.retain(|p| &p.class != c); }
                if let Some(p) = pin { if !pins.contains(&p) { pins.push(p); } }
            });
            poster.post(move |st, _| match res {
                Ok(pins) => {
                    st.settings.pinned_apps = pins;
                    st.apps.set_pins(&st.settings.pinned_apps);
                    st.apps.pin_result = Some(Ok(()));
                }
                Err(e) => st.apps.pin_result = Some(Err(e)),
            });
        });
    }

    fn act(&mut self, a: Act, ctx: &mut Ctx) {
        match a {
            Act::Back => ctx.state.nav = Some(NavTo::Page("controls")),
            Act::Row(key) => {
                let Some(s) = ctx.state.apps.shortcut(&key).cloned() else { return };
                if s.windows > 0 {
                    self.expanded = if self.expanded.as_deref() == Some(key.as_str()) { None } else { Some(key) };
                } else {
                    ctx.state.apps.activate_shortcut(&key, false, ctx.state.notch.output.as_deref());
                }
            }
            Act::Pin(key) => {
                let Some(s) = ctx.state.apps.shortcut(&key).cloned() else { return };
                match s.desktop_id.as_deref().and_then(|id| PinnedApp::new(id, &s.class)) {
                    Some(p) => self.save_pin(ctx, Some(p), None),
                    None => { self.choosing = Some(Choosing { class: s.class.clone() }); self.scroll.offset = 0.0; self.status = None; }
                }
            }
            Act::Unpin(key) => {
                let Some(class) = ctx.state.apps.shortcut(&key).and_then(|s| s.pin.as_ref().map(|p| p.class.clone())) else { return };
                self.save_pin(ctx, None, Some(class));
            }
            Act::Window(key, addr) => ctx.state.apps.activate_window(&key, &addr),
            Act::Choose(id) => {
                let Some(c) = self.choosing.take() else { return };
                match PinnedApp::new(&id, &c.class) {
                    Some(p) => self.save_pin(ctx, Some(p), None),
                    None => self.status = Some("Not saved: invalid application".into()),
                }
            }
            Act::CancelChoose => { self.choosing = None; self.status = None; }
        }
    }

    fn reg(&mut self, r: Rect, view: Rect, a: Act) -> bool {
        let id = ID_BASE + self.acts.len() as u32;
        self.acts.push((id, a));
        self.hits.add_in(r, view, id)
    }

    fn act_at(&self, id: u32) -> Option<Act> { self.acts.iter().find(|(i, _)| *i == id).map(|(_, a)| a.clone()) }
}

impl Page for AppsPage {
    fn natural(&mut self, avail: Size, st: &State, _: &mut TextSys) -> Size {
        Size { w: avail.w, h: (HEAD_H + 4.0 + self.content_h(st)).clamp(120.0, 360.0f32.min(avail.h.max(120.0))) }
    }

    fn shown(&mut self, ctx: &mut Ctx) {
        self.choosing = None;
        self.status = None;
        self.scroll.offset = 0.0;
        self.expanded = ctx.state.apps.page_focus.take();
        // installed applications are read off the UI thread whenever the page opens
        let poster = ctx.poster.clone();
        std::thread::spawn(move || {
            let idx = desktop_entries::refresh();
            let entries = idx.entries().to_vec();
            poster.post(move |st, _| { st.apps.catalog = entries; st.apps.catalog_ready = true; st.apps.rebuild(); });
        });
    }

    fn tick(&mut self, _: f32, ctx: &mut Ctx) -> bool {
        if let Some(res) = ctx.state.apps.pin_result.take() {
            self.busy = false;
            self.status = match res { Ok(()) => None, Err(e) => Some(format!("Not saved: {e}")) };
        }
        false
    }

    fn keeps_open(&self) -> bool { self.choosing.is_some() || self.busy }

    fn cursor(&self) -> Cursor { self.hits.cursor() }

    fn draw(&mut self, cv: &mut Canvas, dc: &mut DrawCtx, area: Rect) {
        let st = dc.state;
        let pal = &st.pal;
        self.hits.clear();
        self.acts.clear();
        let back = rect(area.x, area.y, 60.0, 26.0);
        let hb = self.reg(back, area, if self.choosing.is_some() { Act::CancelChoose } else { Act::Back });
        button(cv, pal, back, if self.choosing.is_some() { "Cancel" } else { "\u{f053} Back" }, ButtonKind::Plain, hb, false);
        let title = if self.choosing.is_some() { "Choose application to launch" } else { "Applications" };
        cv.text(title, &TextStyle::new(pal.fs(15.0), pal.fg_hi).bold(), pt(area.x + 72.0, area.y + 13.0), HAlign::Left, VAlign::Middle);
        if let Some(s) = &self.status {
            cv.text_in(s, &pal.text_dim(10.5), rect(area.x + 220.0, area.y, area.w - 220.0, 26.0), HAlign::Right);
        }

        let list = rect(area.x, area.y + HEAD_H + 4.0, area.w, (area.h - HEAD_H - 4.0).max(40.0));
        self.view_h = list.h;
        let content_h = self.content_h(st);
        self.scroll.content_h = content_h;
        self.scroll.clamp(list.h);
        let mut y = list.y - self.scroll.offset;
        cv.with_clip(list, [0.0; 4], |cv| {
            if self.choosing.is_some() {
                if st.apps.catalog.is_empty() {
                    cv.text_in(if st.apps.catalog_ready { "No installed applications found" } else { "Loading…" }, &pal.text_dim(12.0), rect(list.x + 4.0, y, list.w, ROW_H), HAlign::Left);
                }
                for e in &st.apps.catalog {
                    let r = rect(list.x, y, list.w - 8.0, ROW_H);
                    if r.bottom() > list.y && r.y < list.bottom() {
                        let hov = self.reg(r, list, Act::Choose(e.id.clone()));
                        if hov { cv.fill_rrect(r, r_all(10.0), pal.hover); }
                        cv.text_in(&e.name, &TextStyle::new(pal.fs(13.0), pal.fg_hi).weight(600), rect(r.x + 8.0, r.y + 4.0, r.w - 16.0, 18.0), HAlign::Left);
                        cv.text_in(&e.id, &pal.text_dim(10.5), rect(r.x + 8.0, r.y + 22.0, r.w - 16.0, 14.0), HAlign::Left);
                    }
                    y += ROW_H;
                }
                return;
            }
            if st.apps.shortcuts.is_empty() {
                cv.text_in("No open or pinned applications", &pal.text_dim(12.0), rect(list.x + 4.0, y + 8.0, list.w, 24.0), HAlign::Left);
                return;
            }
            for s in &st.apps.shortcuts {
                let r = rect(list.x, y, list.w - 8.0, ROW_H);
                let open = self.expanded.as_deref() == Some(s.key.as_str());
                if r.bottom() > list.y && r.y < list.bottom() {
                    let hov = self.reg(r, list, Act::Row(s.key.clone()));
                    cv.fill_rrect(r, r_all(10.0), with_a(pal.fg, if hov { 0.10 } else if s.active { 0.07 } else { 0.03 }));
                    if s.active { cv.fill_rrect(rect(r.x + 3.0, r.y + 10.0, 3.0, r.h - 20.0), r_all(1.5), pal.accent_hi); }
                    paint_icon(cv, pal, st, s, rect(r.x + 12.0, r.y + 6.0, 28.0, 28.0));
                    let right = if s.configured_id.is_some() { 0.0 } else { 64.0 };
                    cv.text_in(&s.name, &TextStyle::new(pal.fs(13.0), pal.fg_hi).weight(600), rect(r.x + 48.0, r.y + 4.0, r.w - 56.0 - right, 18.0), HAlign::Left);
                    let unavailable = Self::meta(st, s) == "Application unavailable";
                    let meta_style = if unavailable { pal.text_dim(10.5).color(pal.red) } else { pal.text_dim(10.5) };
                    cv.text_in(&Self::meta(st, s), &meta_style, rect(r.x + 48.0, r.y + 22.0, r.w - 56.0 - right, 14.0), HAlign::Left);
                    if s.configured_id.is_none() {
                        let b = rect(r.right() - 60.0, r.y + 8.0, 54.0, 24.0);
                        let (label, act) = if s.pin.is_some() { ("Unpin", Act::Unpin(s.key.clone())) } else { ("Pin", Act::Pin(s.key.clone())) };
                        let hb = self.reg(b, list, act);
                        button(cv, pal, b, label, ButtonKind::Plain, hb, false);
                    }
                }
                y += ROW_H;
                if open {
                    if let Some(g) = st.apps.open.iter().find(|o| o.key == s.key) {
                        for w in &g.windows {
                            let wr = rect(list.x + 24.0, y, list.w - 32.0, WIN_H);
                            if wr.bottom() > list.y && wr.y < list.bottom() {
                                let hov = self.reg(wr, list, Act::Window(s.key.clone(), w.address.clone()));
                                if hov { cv.fill_rrect(wr, r_all(8.0), pal.hover); }
                                let ws = if w.workspace_name.starts_with("special:") { "Hidden".to_string() } else { format!("WS {}", w.workspace) };
                                cv.text_in(&ws, &pal.text_dim(10.5), rect(wr.right() - 60.0, wr.y, 56.0, wr.h), HAlign::Right);
                                let title = if w.title.is_empty() { norm_class(&w.initial_class, &w.class) } else { w.title.clone() };
                                cv.text_in(&title, &pal.text(11.5), rect(wr.x + 8.0, wr.y, wr.w - 76.0, wr.h), HAlign::Left);
                            }
                            y += WIN_H;
                        }
                    }
                }
            }
        });
        scrollbar(cv, pal, list, self.scroll.offset, self.scroll.content_h);
    }

    fn event(&mut self, ev: &Event, _area: Rect, ctx: &mut Ctx) {
        if let Event::Scroll { dy, .. } = ev { self.scroll.by(*dy, self.view_h); return; }
        if let Event::Press(_, p) = ev { self.pressed = self.hits.at(*p).and_then(|id| self.act_at(id)); }
        let Some(c) = self.hits.event(ev) else { return };
        if c.button != Button::Left { return; }
        // a release must land on the same action it was pressed on, even if the list shifted underneath
        let Some(a) = self.act_at(c.id) else { return };
        if self.pressed.take().as_ref() != Some(&a) { return; }
        self.act(a, ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::{Apps, OpenApp, OpenWindow};
    use crate::page::testutil::Harness;

    fn window(addr: &str, class: &str, focus: i64) -> OpenWindow {
        OpenWindow { address: addr.into(), class: class.into(), initial_class: class.into(), title: format!("{class} window"), workspace: 1, workspace_name: "1".into(), focus_order: focus }
    }

    fn state() -> State {
        let mut st = State::new();
        let mut apps = Apps::default();
        apps.set_open(vec![
            OpenApp { key: "app:brave".into(), name: "Brave".into(), configured_id: None, desktop_id: Some("brave.desktop".into()), windows: vec![window("0x1", "brave", 0), window("0x2", "brave", 1)], active: true },
            OpenApp { key: "app:tool".into(), name: "Tool".into(), configured_id: None, desktop_id: None, windows: vec![window("0x3", "tool", 2)], active: false },
        ]);
        st.apps = apps;
        st
    }

    #[test]
    fn rows_expand_and_pin_flow_chooses_a_desktop_entry_for_unresolved_apps() {
        let mut h = Harness::new();
        let mut st = state();
        let mut page = AppsPage::new();
        let area = h.render(&mut page, &mut st, "/tmp/wn-apps-page-a.png");
        let click = |h: &mut Harness, page: &mut AppsPage, st: &mut State, id_act: Act| {
            let id = page.acts.iter().find(|(_, a)| *a == id_act).map(|(i, _)| *i).expect("action drawn");
            let r = page.hits.items_rect(id);
            let p = r.center();
            h.send(page, st, area, Event::PointerMove(p));
            h.send(page, st, area, Event::Press(Button::Left, p));
            h.send(page, st, area, Event::Release(Button::Left, p));
        };
        click(&mut h, &mut page, &mut st, Act::Row("app:brave".into()));
        assert_eq!(page.expanded.as_deref(), Some("app:brave"));
        h.render(&mut page, &mut st, "/tmp/wn-apps-page-b.png");
        assert!(page.acts.iter().any(|(_, a)| *a == Act::Window("app:brave".into(), "0x2".into())), "explicit window rows");
        click(&mut h, &mut page, &mut st, Act::Pin("app:tool".into()));
        assert!(page.choosing.is_some(), "no desktop entry: the chooser opens instead of guessing a command");
        st.apps.catalog = vec![desktop_entries::DesktopEntry { id: "tool.desktop".into(), name: "Tool App".into(), icon: String::new(), wm_class: String::new() }];
        st.apps.catalog_ready = true;
        h.render(&mut page, &mut st, "/tmp/wn-apps-page-c.png");
        assert!(page.acts.iter().any(|(_, a)| *a == Act::Choose("tool.desktop".into())));
        click(&mut h, &mut page, &mut st, Act::CancelChoose);
        assert!(page.choosing.is_none());
    }

    #[test]
    fn empty_list_says_so() {
        let mut h = Harness::new();
        let mut st = State::new();
        let mut page = AppsPage::new();
        h.render(&mut page, &mut st, "/tmp/wn-apps-page-empty.png");
        assert!(page.acts.iter().all(|(_, a)| *a == Act::Back));
    }
}
