//! The Settings page (reachable from the Controls page): appearance, layout, behavior, monitors, tabs, AI usage,
//! apps and utilities. Widgets edit a draft; nothing persists until Apply, and Apply never restarts the notch.
use crate::config::{self, ClockFormat, MonitorMode, NotchSettings};
use crate::Msg;
use gtk::{gio, glib, prelude::*};
use gtk4 as gtk;
use serde_json::{json, Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use async_channel::Sender;
use std::time::Duration;

const CATALOG: &str = include_str!("../../files/setup/apps-catalog.json");

pub const TAB_LABELS: [(&str, &str); 4] = [("media", "Media"), ("calendar", "Calendar"), ("clipboard", "Clipboard"), ("ai", "AI usage")];
pub const UTILITY_LABELS: [(&str, &str); 10] = [
    ("screenshot", "Screenshot"), ("record", "Record"), ("nightlight", "Reading Mode"), ("dnd", "Do Not Disturb"),
    ("wifi", "Wi-Fi"), ("bluetooth", "Bluetooth"), ("lock", "Lock"), ("clear-clipboard", "Clear clipboard"),
    ("audio", "Audio"), ("display", "Displays"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct AppSel {
    pub id: String,
    pub background: bool,
}

/// Saved `ai`/`apps` plus what this machine offers, loaded off the GTK thread.
pub struct DocInfo {
    pub err: Option<String>,
    pub ai: Option<Vec<String>>,
    pub apps: Vec<AppSel>,
    pub detected: Vec<(String, String)>,
    pub unsupported: Vec<String>,
    pub installed: HashSet<String>,
}

pub enum Saved {
    Ok(Box<NotchSettings>),
    /// Preferences are on disk but the app rendering failed.
    AppsFailed(Box<NotchSettings>, String),
    Err(String),
}

pub enum SettingsEvent {
    Disk(Box<NotchSettings>, Option<String>),
    Doc(Box<DocInfo>),
    Saved(Saved),
}

struct CatalogApp {
    id: String,
    name: String,
    exes: Vec<String>,
    web: bool,
}

fn catalog() -> Vec<CatalogApp> {
    let v: Value = serde_json::from_str(CATALOG).unwrap_or(Value::Null);
    v.as_array().map(|a| a.iter().filter_map(|e| Some(CatalogApp {
        id: e["id"].as_str()?.to_string(),
        name: e["name"].as_str()?.to_string(),
        exes: e["exe"].as_array().map(|x| x.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        web: e.get("web").is_some(),
    })).collect()).unwrap_or_default()
}

fn setup_script() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/waybar-notch/setup/waybar-setup.py")
}

fn load_doc_info() -> DocInfo {
    let (doc, err) = match config::read_document() {
        Ok(d) => (d, None),
        Err(e) => (Value::Null, Some(e)),
    };
    let ai = doc.get("ai").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect());
    let apps = doc.get("apps").and_then(Value::as_array).map(|a| a.iter().filter_map(|e| Some(AppSel {
        id: e["id"].as_str()?.to_string(),
        background: e["background"].as_bool().unwrap_or(false),
    })).collect()).unwrap_or_default();
    let installed = catalog().into_iter()
        .filter(|a| if a.web { crate::on_path("omarchy-launch-webapp") } else { a.exes.iter().any(|e| crate::on_path(e)) })
        .map(|a| a.id).collect();
    DocInfo {
        err, ai, apps,
        detected: crate::usage::detected_providers().into_iter().map(|(i, l)| (i.to_string(), l.to_string())).collect(),
        unsupported: crate::usage::unsupported_found().into_iter().map(str::to_string).collect(),
        installed,
    }
}

/// Reload preferences from disk whenever the config directory changes (atomic renames included).
pub fn watch_config(tx: Sender<Msg>) {
    let Some(dir) = config::path().parent().map(|p| p.to_path_buf()) else { return };
    let load = move |tx: &Sender<Msg>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let (s, e) = config::load_settings();
            let _ = tx.try_send(Msg::Settings(SettingsEvent::Disk(Box::new(s), e)));
        });
    };
    let debounce: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let start = {
        let (tx, dir, debounce) = (tx.clone(), dir.clone(), debounce.clone());
        move || -> bool {
            let Ok(mon) = gio::File::for_path(&dir).monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else { return false };
            let (tx, debounce) = (tx.clone(), debounce.clone());
            mon.connect_changed(move |_, _, _, _| {
                if let Some(id) = debounce.borrow_mut().take() { id.remove(); }
                let (tx, d2) = (tx.clone(), debounce.clone());
                *debounce.borrow_mut() = Some(glib::timeout_add_local_once(Duration::from_millis(150), move || {
                    d2.borrow_mut().take();
                    load(&tx);
                }));
            });
            std::mem::forget(mon);
            true
        }
    };
    if dir.is_dir() && start() { return; }
    // no config directory yet: notice its first creation (stat off-thread, once a second)
    let tx2 = tx;
    glib::timeout_add_local(Duration::from_secs(1), move || {
        if dir.is_dir() && start() {
            load(&tx2);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

pub struct SettingsPage {
    pub root: gtk::Box,
    tx: Sender<Msg>,
    draft: RefCell<NotchSettings>,
    active: RefCell<NotchSettings>,
    doc: RefCell<Option<DocInfo>>,
    ai_draft: RefCell<Option<Vec<String>>>,
    apps_draft: RefCell<Vec<AppSel>>,
    apps_base: RefCell<Vec<AppSel>>,
    apps_pending: Cell<bool>,
    outputs: RefCell<Vec<String>>,
    refreshers: RefCell<Vec<Box<dyn Fn()>>>,
    invalid: RefCell<HashSet<&'static str>>,
    loading: Cell<bool>,
    busy: Cell<bool>,
    status: gtk::Label,
    apply_btn: gtk::Button,
    ai_box: gtk::Box,
    apps_box: gtk::Box,
    app_order: RefCell<Vec<String>>,
}

fn lbl(t: &str, cls: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(t));
    l.set_xalign(0.0);
    l.add_css_class(cls);
    l
}

fn name_label(label: &str) -> gtk::Label {
    let l = lbl(label, "settings-label");
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_hexpand(true);
    l.set_width_chars(1);
    l.set_max_width_chars(24);
    l
}

fn hint(t: &str) -> gtk::Label {
    let l = lbl(t, "settings-hint");
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_width_chars(1);
    l.set_hexpand(true);
    l
}

/// Name on the left (wrapping), compact control on the right.
fn row(label: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let r = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    r.add_css_class("settings-row");
    w.set_halign(gtk::Align::End);
    w.set_valign(gtk::Align::Center);
    r.append(&name_label(label));
    r.append(w);
    r
}

/// Name above a full-width control (dropdowns, custom colors, sliders).
fn wide_row(label: &str, w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let r = gtk::Box::new(gtk::Orientation::Vertical, 6);
    r.add_css_class("settings-row");
    w.set_halign(gtk::Align::Fill);
    w.set_hexpand(true);
    r.append(&name_label(label));
    r.append(w);
    r
}

fn card(parent: &gtk::Box, title: &str) -> gtk::Box {
    let c = gtk::Box::new(gtk::Orientation::Vertical, 6);
    c.add_css_class("settings-card");
    c.append(&lbl(title, "settings-card-title"));
    parent.append(&c);
    c
}

/// Wrap a check button's own label at word/char boundaries without touching its indicator.
fn wrap_check(cb: &gtk::CheckButton) {
    let mut c = cb.first_child();
    while let Some(w) = c {
        if let Some(l) = w.downcast_ref::<gtk::Label>() {
            l.set_wrap(true);
            l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            l.set_width_chars(1);
            l.set_max_width_chars(28);
            l.set_xalign(0.0);
        }
        c = w.next_sibling();
    }
    cb.set_hexpand(true);
}

/// Pango attributes selecting `family` (and an absolute pixel size) without markup or CSS providers.
fn family_attrs(family: &str, px: Option<u32>) -> gtk::pango::AttrList {
    let a = gtk::pango::AttrList::new();
    a.insert(gtk::pango::AttrString::new_family(family));
    if let Some(px) = px { a.insert(gtk::pango::AttrSize::new_size_absolute(px as i32 * gtk::pango::SCALE)); }
    a
}

/// Selected text ellipsizes (never widens the page); the popup list wraps so every choice stays readable.
/// `preview_system`: font-family picker mode; each row is drawn in its own family ("System default" uses this family).
fn constrain_dropdown(d: &gtk::DropDown, preview_system: Option<&str>) {
    let sys = preview_system.map(str::to_string);
    let preview = move |l: &gtk::Label, name: &str| {
        let Some(sys) = &sys else { return };
        let family = if name == "System default" { sys.as_str() } else { name };
        l.set_attributes(Some(&family_attrs(family, None)));
    };
    let preview2 = preview.clone();
    let text = |item: &gtk::ListItem| item.item().and_downcast::<gtk::StringObject>().map(|o| o.string().to_string());
    let sel = gtk::SignalListItemFactory::new();
    sel.connect_setup(|_, o| {
        let Some(item) = o.downcast_ref::<gtk::ListItem>() else { return };
        let l = gtk::Label::new(None);
        l.set_width_chars(1);
        l.set_max_width_chars(1);
        l.set_hexpand(true);
        l.set_xalign(0.0);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        item.set_child(Some(&l));
    });
    sel.connect_bind(move |_, o| {
        let Some(item) = o.downcast_ref::<gtk::ListItem>() else { return };
        let (Some(l), Some(t)) = (item.child().and_downcast::<gtk::Label>(), text(item)) else { return };
        l.set_text(&t);
        l.set_tooltip_text(Some(&t));
        preview(&l, &t);
    });
    let list = gtk::SignalListItemFactory::new();
    list.connect_setup(|_, o| {
        let Some(item) = o.downcast_ref::<gtk::ListItem>() else { return };
        let l = gtk::Label::new(None);
        l.set_xalign(0.0);
        l.set_wrap(true);
        l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        l.set_width_chars(1);
        l.set_max_width_chars(32);
        item.set_child(Some(&l));
    });
    list.connect_bind(move |_, o| {
        let Some(item) = o.downcast_ref::<gtk::ListItem>() else { return };
        let (Some(l), Some(t)) = (item.child().and_downcast::<gtk::Label>(), text(item)) else { return };
        l.set_text(&t);
        preview2(&l, &t);
    });
    d.set_factory(Some(&sel));
    d.set_list_factory(Some(&list));
}

fn dropdown(items: &[&str]) -> gtk::DropDown {
    let d = gtk::DropDown::from_strings(items);
    constrain_dropdown(&d, None);
    d
}

impl SettingsPage {
    /// `on_close` runs for Back/Cancel/Escape/after leaving (the caller switches to the Controls page).
    pub fn new(tx: Sender<Msg>, initial: NotchSettings, on_close: Rc<dyn Fn()>) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.add_css_class("settings-page");
        root.set_margin_start(8);
        root.set_margin_end(8);
        root.set_margin_top(10);
        root.set_margin_bottom(10);
        let page = Rc::new(SettingsPage {
            root: root.clone(), tx,
            draft: RefCell::new(initial.clone()), active: RefCell::new(initial),
            doc: RefCell::new(None), ai_draft: RefCell::new(None), apps_draft: RefCell::new(vec![]), apps_base: RefCell::new(vec![]),
            apps_pending: Cell::new(false), outputs: RefCell::new(vec![]), refreshers: RefCell::new(vec![]),
            invalid: RefCell::new(HashSet::new()), loading: Cell::new(false), busy: Cell::new(false),
            status: lbl("", "settings-hint"), apply_btn: gtk::Button::with_label("Apply"),
            ai_box: gtk::Box::new(gtk::Orientation::Vertical, 4), apps_box: gtk::Box::new(gtk::Orientation::Vertical, 4),
            app_order: RefCell::new(catalog().into_iter().map(|a| a.id).collect()),
        });

        // header
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let back = gtk::Button::with_label("\u{f060}");
        back.add_css_class("back-btn");
        let title = lbl("Settings", "cap");
        title.set_hexpand(true);
        head.append(&back);
        head.append(&title);
        root.append(&head);

        // scrolled body
        let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
        body.set_margin_end(6);
        let sw = gtk::ScrolledWindow::new();
        sw.set_vexpand(true);
        sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        sw.set_min_content_height(0);
        sw.set_child(Some(&body));
        root.append(&sw);

        page.build_appearance(&body);
        page.build_layout(&body);
        page.build_behavior(&body);
        page.build_monitors(&body);
        page.build_tabs(&body);
        let ai = card(&body, "AI usage");
        ai.append(&page.ai_box);
        let apps = card(&body, "Apps");
        apps.append(&page.apps_box);
        apps.append(&hint("Deselecting or changing an app never closes it. A background app is started hidden and revived if it is missing; the new hidden-workspace rule only applies to windows opened afterwards."));
        page.build_utilities(&body);

        // footer
        page.status.set_wrap(true);
        page.status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        page.status.set_width_chars(1);
        page.status.set_lines(3);
        page.status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        page.status.set_visible(false);
        root.append(&page.status);
        let foot = gtk::Box::new(gtk::Orientation::Vertical, 6);
        foot.add_css_class("settings-footer");
        let reset = gtk::Button::with_label("Reset to defaults");
        reset.set_tooltip_text(Some("Resets notch appearance, layout, behavior, monitor, tabs and utilities in the draft; AI usage and apps are not changed."));
        let cancel = gtk::Button::with_label("Cancel");
        for b in [&reset, &cancel, &page.apply_btn] { b.add_css_class("small-btn"); }
        page.apply_btn.add_css_class("settings-primary");
        reset.set_halign(gtk::Align::Start);
        let acts = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        acts.set_halign(gtk::Align::End);
        acts.append(&cancel);
        acts.append(&page.apply_btn);
        foot.append(&reset);
        foot.append(&acts);
        root.append(&foot);

        {
            let (p, c) = (page.clone(), on_close.clone());
            let cancel_fn = move || { p.discard(); c(); };
            let f = Rc::new(cancel_fn);
            for b in [&back, &cancel] { let f = f.clone(); b.connect_clicked(move |_| f()); }
            let key = gtk::EventControllerKey::new();
            let f2 = f.clone();
            key.connect_key_pressed(move |_, k, _, _| {
                if k == gtk::gdk::Key::Escape { f2(); glib::Propagation::Stop } else { glib::Propagation::Proceed }
            });
            root.add_controller(key);
        }
        {
            let p = page.clone();
            reset.connect_clicked(move |_| {
                {
                    let mut d = p.draft.borrow_mut();
                    let def = NotchSettings::default();
                    d.monitor = def.monitor; d.appearance = def.appearance; d.layout = def.layout;
                    d.behavior = def.behavior; d.tabs = def.tabs; d.utilities = def.utilities;
                }
                p.invalid.borrow_mut().clear();
                p.refresh_all();
                p.set_status("Defaults loaded into the draft; Apply to keep them.");
            });
            let p = page.clone();
            page.apply_btn.connect_clicked(move |_| p.apply());
        }
        page
    }

    fn set_status(&self, t: &str) {
        self.status.set_text(t);
        self.status.set_tooltip_text(if t.is_empty() { None } else { Some(t) });
        self.status.set_visible(!t.is_empty());
        let _ = self.tx.try_send(Msg::Relayout);
    }

    fn refresh_all(&self) {
        self.loading.set(true);
        for r in self.refreshers.borrow().iter() { r(); }
        self.loading.set(false);
        self.update_apply();
    }

    fn update_apply(&self) {
        self.apply_btn.set_sensitive(!self.busy.get() && self.invalid.borrow().is_empty()
            && self.doc.borrow().as_ref().map_or(true, |d| d.err.is_none()));
    }

    /// Edit the draft through `f` (ignored while widgets are being refreshed from it).
    fn edit(&self, f: impl FnOnce(&mut NotchSettings)) {
        if self.loading.get() { return; }
        f(&mut self.draft.borrow_mut());
    }

    // ---- sections
    fn spin(self: &Rc<Self>, parent: &gtk::Box, label: &str, lo: u32, hi: u32, get: fn(&NotchSettings) -> u32, set: fn(&mut NotchSettings, u32)) -> gtk::SpinButton {
        let s = gtk::SpinButton::with_range(lo as f64, hi as f64, 1.0);
        s.set_numeric(true);
        s.set_width_chars(5);
        s.set_max_width_chars(5);
        parent.append(&row(label, &s));
        let p = self.clone();
        s.connect_value_changed(move |s| { let v = s.value() as u32; p.edit(|d| set(d, v)); });
        let (p, s2) = (self.clone(), s.clone());
        self.refreshers.borrow_mut().push(Box::new(move || s2.set_value(get(&p.draft.borrow()) as f64)));
        s
    }

    fn switch(self: &Rc<Self>, parent: &gtk::Box, label: &str, get: fn(&NotchSettings) -> bool, set: fn(&mut NotchSettings, bool)) {
        let s = gtk::Switch::new();
        s.set_valign(gtk::Align::Center);
        parent.append(&row(label, &s));
        let p = self.clone();
        s.connect_active_notify(move |s| { let v = s.is_active(); p.edit(|d| set(d, v)); });
        let (p, s2) = (self.clone(), s.clone());
        self.refreshers.borrow_mut().push(Box::new(move || s2.set_active(get(&p.draft.borrow()))));
    }

    fn color(self: &Rc<Self>, parent: &gtk::Box, label: &'static str, acc: fn(&mut NotchSettings) -> &mut Option<String>) {
        let sw = gtk::Switch::new();
        sw.set_valign(gtk::Align::Center);
        let entry = gtk::Entry::new();
        entry.set_max_length(7);
        entry.set_width_chars(8);
        entry.set_placeholder_text(Some("#RRGGBB"));
        entry.set_hexpand(true);
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        b.append(&lbl("Custom", "settings-label"));
        b.append(&sw);
        b.append(&entry);
        parent.append(&wide_row(label, &b));
        let apply = {
            let (p, sw, entry) = (self.clone(), sw.clone(), entry.clone());
            move || {
                if p.loading.get() { return; }
                let text = entry.text().to_string();
                entry.set_sensitive(sw.is_active());
                if !sw.is_active() {
                    p.invalid.borrow_mut().remove(label);
                    entry.remove_css_class("bad");
                    acc(&mut p.draft.borrow_mut()).take();
                } else if config::valid_hex_color(&text) {
                    p.invalid.borrow_mut().remove(label);
                    entry.remove_css_class("bad");
                    *acc(&mut p.draft.borrow_mut()) = Some(text);
                } else {
                    p.invalid.borrow_mut().insert(label);
                    entry.add_css_class("bad");
                }
                p.update_apply();
            }
        };
        let a1 = apply.clone();
        sw.connect_active_notify(move |_| a1());
        entry.connect_changed(move |_| apply());
        let (p, sw2, e2) = (self.clone(), sw, entry);
        self.refreshers.borrow_mut().push(Box::new(move || {
            let cur = acc(&mut p.draft.borrow_mut()).clone();
            sw2.set_active(cur.is_some());
            e2.set_text(cur.as_deref().unwrap_or(""));
            e2.set_sensitive(cur.is_some());
            e2.remove_css_class("bad");
            p.invalid.borrow_mut().remove(label);
        }));
    }

    fn build_appearance(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Appearance");
        self.color(body, "Background", |d| &mut d.appearance.background);
        self.color(body, "Text", |d| &mut d.appearance.foreground);
        self.color(body, "Accent", |d| &mut d.appearance.accent);
        let op = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.55, 1.0, 0.01);
        op.set_draw_value(true);
        body.append(&wide_row("Notch background opacity", &op));
        body.append(&hint("Applies to collapsed and expanded panels. Blur is provided by the compositor."));
        let p = self.clone();
        op.connect_value_changed(move |s| { let v = (s.value() * 100.0).round() / 100.0; p.edit(|d| d.appearance.opacity = v); });
        let (p, o2) = (self.clone(), op);
        self.refreshers.borrow_mut().push(Box::new(move || o2.set_value(p.draft.borrow().appearance.opacity)));

        // font family: only installed families can be chosen
        let mut fams: Vec<String> = self.root.pango_context().list_families().iter().map(|f| f.name().to_string()).collect();
        fams.sort_by_key(|f| f.to_lowercase());
        let model = gtk::StringList::new(&["System default"]);
        for f in &fams { model.append(f); }
        let fam = gtk::DropDown::new(Some(model.clone()), gtk::Expression::NONE);
        fam.set_enable_search(true);
        let sys_font = crate::theme::Theme::load().font;
        constrain_dropdown(&fam, Some(&sys_font));
        body.append(&wide_row("Font family (notch and pills)", &fam));
        let sample = gtk::Label::new(Some("Aa Bb — The quick brown fox 0123456789"));
        sample.add_css_class("font-sample");
        sample.set_wrap(true);
        sample.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        sample.set_width_chars(1);
        sample.set_xalign(0.0);
        sample.set_hexpand(true);
        body.append(&sample);
        let upd: Rc<dyn Fn()> = {
            let (p, sample) = (self.clone(), sample.clone());
            Rc::new(move || {
                let (fam, px) = { let d = p.draft.borrow(); (d.appearance.font_family.clone(), d.appearance.font_size) };
                sample.set_attributes(Some(&family_attrs(fam.as_deref().unwrap_or(&sys_font), Some(px))));
            })
        };
        let p = self.clone();
        let m2 = model.clone();
        let u2 = upd.clone();
        fam.connect_selected_notify(move |d| {
            let i = d.selected();
            let name = if i == 0 { None } else { m2.string(i).map(|s| s.to_string()) };
            p.edit(|s| s.appearance.font_family = name);
            u2();
        });
        let (p, fam2, u3) = (self.clone(), fam, upd.clone());
        self.refreshers.borrow_mut().push(Box::new(move || {
            let cur = p.draft.borrow().appearance.font_family.clone();
            let mut idx = 0;
            if let Some(c) = cur {
                idx = (0..model.n_items()).find(|i| model.string(*i).is_some_and(|s| s == c)).unwrap_or_else(|| { model.append(&c); model.n_items() - 1 });
            }
            fam2.set_selected(idx);
            u3();
        }));
        let size = self.spin(body, "Notch font size (px)", 10, 18, |d| d.appearance.font_size, |d, v| d.appearance.font_size = v);
        size.connect_value_changed(move |_| upd());
        self.spin(body, "Corner radius", 0, 40, |d| d.appearance.corner_radius, |d, v| d.appearance.corner_radius = v);
    }

    fn build_layout(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Layout");
        self.spin(body, "Collapsed width", 180, 420, |d| d.layout.collapsed_width, |d, v| d.layout.collapsed_width = v);
        self.spin(body, "Collapsed height", 24, 48, |d| d.layout.collapsed_height, |d, v| d.layout.collapsed_height = v);
        self.spin(body, "Maximum expanded width", 480, 960, |d| d.layout.expanded_width, |d, v| d.layout.expanded_width = v);
        self.spin(body, "Top margin", 0, 24, |d| d.layout.top_margin, |d, v| d.layout.top_margin = v);
    }

    fn build_behavior(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Behavior");
        self.switch(body, "Open on hover", |d| d.behavior.open_on_hover, |d, v| d.behavior.open_on_hover = v);
        self.spin(body, "Collapse delay (ms)", 100, 2000, |d| d.behavior.collapse_delay_ms, |d, v| d.behavior.collapse_delay_ms = v);
        self.spin(body, "Return to Home after (s, 0 = never)", 0, 120, |d| d.behavior.home_reset_secs, |d, v| d.behavior.home_reset_secs = v);
        self.switch(body, "Animations", |d| d.behavior.animations, |d, v| d.behavior.animations = v);
        self.switch(body, "Hide on fullscreen", |d| d.behavior.hide_on_fullscreen, |d, v| d.behavior.hide_on_fullscreen = v);
        let clock = dropdown(&["System locale", "12-hour", "24-hour"]);
        body.append(&wide_row("Clock format", &clock));
        let p = self.clone();
        clock.connect_selected_notify(move |d| {
            let f = [ClockFormat::Locale, ClockFormat::H12, ClockFormat::H24][(d.selected() as usize).min(2)];
            p.edit(|s| s.behavior.clock_format = f);
        });
        let (p, c2) = (self.clone(), clock);
        self.refreshers.borrow_mut().push(Box::new(move || {
            c2.set_selected(match p.draft.borrow().behavior.clock_format { ClockFormat::Locale => 0, ClockFormat::H12 => 1, ClockFormat::H24 => 2 });
        }));
    }

    fn build_monitors(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Monitors");
        let mode = dropdown(&["Follow the focused screen", "Always on one screen"]);
        body.append(&wide_row("Notch and pills", &mode));
        let out = gtk::DropDown::new(None::<gtk::StringList>, gtk::Expression::NONE);
        constrain_dropdown(&out, None);
        body.append(&wide_row("Screen", &out));
        let p = self.clone();
        mode.connect_selected_notify(move |d| {
            let m = if d.selected() == 1 { MonitorMode::Fixed } else { MonitorMode::Follow };
            p.edit(|s| s.monitor.mode = m);
        });
        let p = self.clone();
        out.connect_selected_notify(move |d| {
            let i = d.selected() as usize;
            let names = p.output_choices();
            if let Some(n) = names.get(i).cloned() { p.edit(|s| s.monitor.output = Some(n)); }
        });
        let (p, m2, o2) = (self.clone(), mode, out);
        self.refreshers.borrow_mut().push(Box::new(move || {
            let d = p.draft.borrow().clone();
            m2.set_selected(if d.monitor.mode == MonitorMode::Fixed { 1 } else { 0 });
            let names = p.output_choices();
            let labels: Vec<String> = names.iter().map(|n| if p.outputs.borrow().contains(n) { n.clone() } else { format!("{n} (disconnected)") }).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            o2.set_model(Some(&gtk::StringList::new(&refs)));
            o2.set_selected(d.monitor.output.as_ref().and_then(|c| names.iter().position(|n| n == c)).unwrap_or(0) as u32);
            o2.set_sensitive(d.monitor.mode == MonitorMode::Fixed);
        }));
    }

    /// Connected outputs plus the saved one if it is currently missing.
    fn output_choices(&self) -> Vec<String> {
        let mut v = self.outputs.borrow().clone();
        if let Some(o) = &self.draft.borrow().monitor.output { if !v.contains(o) { v.push(o.clone()); } }
        v
    }

    pub fn set_outputs(&self, names: Vec<String>) {
        if *self.outputs.borrow() == names { return; }
        *self.outputs.borrow_mut() = names;
        if self.root.is_mapped() { self.refresh_all(); }
    }

    /// Ordered checklist: enabled ids (in order) first, then the rest; Up/Down reorder.
    fn checklist(self: &Rc<Self>, parent: &gtk::Box, all: &'static [(&'static str, &'static str)], get: fn(&NotchSettings) -> Vec<String>, set: fn(&mut NotchSettings, Vec<String>)) {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 3);
        parent.append(&list);
        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(all.iter().map(|(i, _)| *i).collect()));
        let render: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let f: Rc<dyn Fn()> = {
            let (p, list, order, render) = (self.clone(), list.clone(), order.clone(), render.clone());
            Rc::new(move || {
                let enabled = get(&p.draft.borrow());
                {
                    let mut o = order.borrow_mut();
                    let mut next: Vec<&'static str> = enabled.iter().filter_map(|e| all.iter().find(|(i, _)| i == e).map(|(i, _)| *i)).collect();
                    for id in o.iter() { if !next.contains(id) { next.push(id); } }
                    *o = next;
                }
                while let Some(c) = list.first_child() { list.remove(&c); }
                let ids: Vec<&'static str> = order.borrow().clone();
                for (n, id) in ids.iter().enumerate() {
                    let label = all.iter().find(|(i, _)| i == id).map(|(_, l)| *l).unwrap_or(id);
                    let r = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                    r.add_css_class("settings-list-row");
                    let cb = gtk::CheckButton::with_label(label);
                    wrap_check(&cb);
                    cb.set_active(enabled.iter().any(|e| e == id));
                    let (up, down) = (gtk::Button::with_label("\u{f077}"), gtk::Button::with_label("\u{f078}"));
                    for b in [&up, &down] { b.add_css_class("small-btn"); b.add_css_class("settings-reorder"); b.set_size_request(28, 28); b.set_valign(gtk::Align::Center); }
                    up.set_sensitive(n > 0);
                    down.set_sensitive(n + 1 < ids.len());
                    r.append(&cb); r.append(&up); r.append(&down);
                    list.append(&r);
                    let commit = {
                        let (p, order) = (p.clone(), order.clone());
                        move |checked: &HashSet<&'static str>| {
                            let v: Vec<String> = order.borrow().iter().filter(|i| checked.contains(*i)).map(|s| s.to_string()).collect();
                            p.edit(|d| set(d, v));
                        }
                    };
                    let checked_now = {
                        let p = p.clone();
                        move || -> HashSet<&'static str> { get(&p.draft.borrow()).iter().filter_map(|e| all.iter().find(|(i, _)| i == e).map(|(i, _)| *i)).collect() }
                    };
                    {
                        let (commit, checked_now, id) = (commit.clone(), checked_now.clone(), *id);
                        cb.connect_toggled(move |c| {
                            let mut s = checked_now();
                            if c.is_active() { s.insert(id); } else { s.remove(id); }
                            commit(&s);
                        });
                    }
                    for (btn, delta) in [(&up, -1i32), (&down, 1)] {
                        let (order, commit, checked_now, render, id) = (order.clone(), commit.clone(), checked_now.clone(), render.clone(), *id);
                        btn.connect_clicked(move |_| {
                            {
                                let mut o = order.borrow_mut();
                                if let Some(pos) = o.iter().position(|x| *x == id) {
                                    let to = (pos as i32 + delta).clamp(0, o.len() as i32 - 1) as usize;
                                    o.swap(pos, to);
                                }
                            }
                            commit(&checked_now());
                            if let Some(r) = render.borrow().as_ref() { r(); }
                        });
                    }
                }
            })
        };
        *render.borrow_mut() = Some(f.clone());
        let p = self.clone();
        self.refreshers.borrow_mut().push(Box::new(move || { let _ = &p; f(); }));
    }

    fn build_tabs(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Tabs");
        body.append(&hint("Home and Controls are always shown."));
        self.checklist(body, &TAB_LABELS, |d| d.tabs.iter().filter(|t| *t != "home" && *t != "controls").cloned().collect(),
            |d, v| d.tabs = config::normalize_tabs(&v));
    }

    fn build_utilities(self: &Rc<Self>, body: &gtk::Box) {
        let body = &card(body, "Utilities");
        body.append(&hint("Settings is always shown."));
        self.checklist(body, &UTILITY_LABELS, |d| d.utilities.clone(), |d, v| d.utilities = v);
    }

    // ---- AI / apps (rebuilt when document info arrives or an entry changes)
    fn render_ai(self: &Rc<Self>) {
        while let Some(c) = self.ai_box.first_child() { self.ai_box.remove(&c); }
        let doc = self.doc.borrow();
        let Some(doc) = doc.as_ref() else { self.ai_box.append(&hint("Loading…")); return };
        if doc.detected.is_empty() { self.ai_box.append(&hint("No supported AI tool found on this machine.")); }
        for (id, label) in &doc.detected {
            let on = self.ai_draft.borrow().as_ref().map_or(true, |s| s.iter().any(|x| x == id));
            let cb = gtk::CheckButton::with_label(label);
            wrap_check(&cb);
            cb.set_active(on);
            let (p, id, detected) = (self.clone(), id.clone(), doc.detected.clone());
            cb.connect_toggled(move |c| {
                if p.loading.get() { return; }
                let mut cur = p.ai_draft.borrow().clone().unwrap_or_else(|| detected.iter().map(|(i, _)| i.clone()).collect());
                cur.retain(|x| *x != id);
                if c.is_active() { cur.push(id.clone()); }
                *p.ai_draft.borrow_mut() = Some(cur);
            });
            self.ai_box.append(&cb);
        }
        if !doc.unsupported.is_empty() {
            self.ai_box.append(&hint(&format!("Found but not supported yet: {}", doc.unsupported.join(", "))));
        }
    }

    fn render_apps(self: &Rc<Self>) {
        while let Some(c) = self.apps_box.first_child() { self.apps_box.remove(&c); }
        let doc = self.doc.borrow();
        let Some(doc) = doc.as_ref() else { self.apps_box.append(&hint("Loading…")); return };
        let names: HashMap<String, String> = catalog().into_iter().map(|a| (a.id, a.name)).collect();
        // shown apps first, in their saved order, then the rest of the catalog
        let mut order: Vec<String> = self.apps_draft.borrow().iter().map(|a| a.id.clone()).collect();
        for id in self.app_order.borrow().iter() { if !order.contains(id) { order.push(id.clone()); } }
        for (n, id) in order.iter().enumerate() {
            let Some(name) = names.get(id) else { continue };
            let sel = self.apps_draft.borrow().iter().find(|a| a.id == *id).cloned();
            let r = gtk::Box::new(gtk::Orientation::Vertical, 4);
            r.add_css_class("settings-list-row");
            let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let installed = doc.installed.contains(id);
            let show = gtk::CheckButton::with_label(&format!("{name}{}", if installed { "" } else { " (not installed)" }));
            wrap_check(&show);
            show.set_active(sel.is_some());
            let bg = gtk::CheckButton::with_label("Background");
            wrap_check(&bg);
            bg.set_margin_start(24);
            bg.set_active(sel.as_ref().is_some_and(|a| a.background));
            bg.set_sensitive(sel.is_some());
            let (up, down) = (gtk::Button::with_label("\u{f077}"), gtk::Button::with_label("\u{f078}"));
            for b in [&up, &down] { b.add_css_class("small-btn"); b.add_css_class("settings-reorder"); b.set_size_request(28, 28); b.set_valign(gtk::Align::Center); }
            up.set_sensitive(n > 0 && sel.is_some());
            down.set_sensitive(n + 1 < order.len() && sel.is_some());
            for w in [show.upcast_ref::<gtk::Widget>(), up.upcast_ref(), down.upcast_ref()] { top.append(w); }
            r.append(&top);
            r.append(&bg);
            self.apps_box.append(&r);
            {
                let (p1, id1) = (self.clone(), id.clone());
                show.connect_toggled(move |c| {
                    if p1.loading.get() { return; }
                    {
                        let mut a = p1.apps_draft.borrow_mut();
                        a.retain(|x| x.id != id1);
                        if c.is_active() { a.push(AppSel { id: id1.clone(), background: false }); }
                    }
                    p1.render_apps();
                });
                let (p, id) = (self.clone(), id.clone());
                bg.connect_toggled(move |c| {
                    if p.loading.get() { return; }
                    if let Some(a) = p.apps_draft.borrow_mut().iter_mut().find(|x| x.id == id) { a.background = c.is_active(); }
                });
            }
            for (btn, delta) in [(&up, -1i32), (&down, 1)] {
                let (p, id) = (self.clone(), id.clone());
                btn.connect_clicked(move |_| {
                    {
                        let mut a = p.apps_draft.borrow_mut();
                        if let Some(pos) = a.iter().position(|x| x.id == id) {
                            let to = (pos as i32 + delta).clamp(0, a.len() as i32 - 1) as usize;
                            a.swap(pos, to);
                        }
                    }
                    p.render_apps();
                });
            }
        }
    }

    // ---- lifecycle
    /// The page became visible: start from the active preferences and look at the file in the background.
    pub fn on_show(self: &Rc<Self>) {
        *self.draft.borrow_mut() = self.active.borrow().clone();
        self.invalid.borrow_mut().clear();
        self.set_status("");
        *self.doc.borrow_mut() = None;
        self.refresh_all();
        self.render_ai();
        self.render_apps();
        let tx = self.tx.clone();
        std::thread::spawn(move || { let _ = tx.try_send(Msg::Settings(SettingsEvent::Doc(Box::new(load_doc_info())))); });
    }

    pub fn discard(&self) {
        *self.draft.borrow_mut() = self.active.borrow().clone();
        self.set_status("");
    }

    pub fn on_doc(self: &Rc<Self>, d: DocInfo) {
        if let Some(e) = &d.err { self.set_status(e); }
        let dirty = self.doc.borrow().is_some();
        if !dirty {
            *self.ai_draft.borrow_mut() = d.ai.clone();
            *self.apps_draft.borrow_mut() = d.apps.clone();
        }
        *self.apps_base.borrow_mut() = d.apps.clone();
        *self.doc.borrow_mut() = Some(d);
        self.loading.set(true);
        self.render_ai();
        self.render_apps();
        self.loading.set(false);
        self.update_apply();
    }

    /// Preferences changed on disk (or were saved): the draft is kept, with a notice if it would differ.
    pub fn on_external(self: &Rc<Self>, s: NotchSettings, err: Option<String>) {
        let changed = *self.active.borrow() != s;
        *self.active.borrow_mut() = s;
        if let Some(e) = err { self.set_status(&e); }
        else if changed && self.root.is_mapped() { self.set_status("The config file changed on disk; your draft is kept, Apply writes only what you changed."); }
    }

    fn apply(self: &Rc<Self>) {
        if self.busy.get() { return; }
        let draft = self.draft.borrow().clone();
        let mut patch: Map<String, Value> = Map::new();
        let np = config::diff_patch(&self.active.borrow().to_json(), &draft.to_json());
        if !np.is_empty() { patch.insert("notch".into(), Value::Object(np)); }
        let ai_changed = self.doc.borrow().as_ref().is_some_and(|d| d.ai != *self.ai_draft.borrow()) && self.ai_draft.borrow().is_some();
        if ai_changed { patch.insert("ai".into(), json!(self.ai_draft.borrow().clone().unwrap_or_default())); }
        let apps = self.apps_draft.borrow().clone();
        let apps_changed = apps != *self.apps_base.borrow();
        if apps_changed {
            patch.insert("apps".into(), Value::Array(apps.iter().map(|a| json!({"id": a.id, "background": a.background})).collect()));
        }
        let run_apps = apps_changed || self.apps_pending.get();
        self.busy.set(true);
        self.update_apply();
        self.set_status(if run_apps { "Saving and applying…" } else { "Saving…" });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let saved = if patch.is_empty() { Ok(()) } else { config::save_patch(&patch) };
            let out = match saved {
                Err(e) => Saved::Err(e),
                Ok(()) if run_apps => match run_apply() {
                    Ok(()) => Saved::Ok(Box::new(draft)),
                    Err(e) => Saved::AppsFailed(Box::new(draft), e),
                },
                Ok(()) => Saved::Ok(Box::new(draft)),
            };
            let _ = tx.try_send(Msg::Settings(SettingsEvent::Saved(out)));
        });
    }

    pub fn on_saved(self: &Rc<Self>, s: &Saved) {
        self.busy.set(false);
        match s {
            Saved::Ok(n) => {
                *self.active.borrow_mut() = (**n).clone();
                self.apps_pending.set(false);
                *self.apps_base.borrow_mut() = self.apps_draft.borrow().clone();
                if let Some(d) = self.doc.borrow_mut().as_mut() { d.ai = self.ai_draft.borrow().clone(); }
                self.set_status("Saved and applied");
            }
            Saved::AppsFailed(n, e) => {
                *self.active.borrow_mut() = (**n).clone();
                self.apps_pending.set(true);
                *self.apps_base.borrow_mut() = self.apps_draft.borrow().clone();
                if let Some(d) = self.doc.borrow_mut().as_mut() { d.ai = self.ai_draft.borrow().clone(); }
                self.set_status(&format!("Preferences saved; app changes not applied: {e}"));
            }
            Saved::Err(e) => self.set_status(&format!("Not saved: {e}")),
        }
        self.update_apply();
    }
}

/// Re-render the Waybar config/autostart from the saved `apps` with the installed helper (never cargo/install.sh).
fn run_apply() -> Result<(), String> {
    let script = setup_script();
    if !script.is_file() {
        return Err(format!("{} is missing; run ./install.sh once", script.display()));
    }
    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script).arg("apply");
    if let Some(sig) = crate::desktop::hypr_sig() { cmd.env("HYPRLAND_INSTANCE_SIGNATURE", sig); }
    let out = cmd.output().map_err(|e| format!("cannot run python3: {e}"))?;
    if out.status.success() { return Ok(()); }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(if err.is_empty() { format!("helper exited with {}", out.status) } else { err.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descendants(w: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        let mut c = w.first_child();
        while let Some(x) = c { out.push(x.clone()); descendants(&x, out); c = x.next_sibling(); }
    }

    fn bounds(w: &gtk::Widget, root: &gtk::Widget) -> gtk::graphene::Rect { w.compute_bounds(root).expect("bounds") }

    /// Needs a display: `cargo test -- --ignored`. Real constructor + production CSS at compact widths and large fonts.
    #[test]
    #[ignore]
    fn settings_cards_fit_compact_widths() {
        gtk::init().expect("display");
        let long = "An Extremely Long Font Family Name That Cannot Possibly Fit In A Compact Dropdown Condensed Black Italic";
        for font in [12u32, 18] {
            let mut st = NotchSettings::default();
            st.appearance.font_size = font;
            st.appearance.font_family = Some(long.into());
            st.monitor.mode = MonitorMode::Fixed;
            st.monitor.output = Some("DISCONNECTED-OUTPUT-WITH-A-VERY-LONG-CONNECTOR-NAME".into());
            crate::theme::configure(&st.appearance);
            let prov = gtk::CssProvider::new();
            prov.load_from_string(&crate::main_css());
            gtk::style_context_add_provider_for_display(&gtk::gdk::Display::default().unwrap(), &prov, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            for width in [312i32, 472] {
                let (tx, _rx) = async_channel::unbounded();
                let page = SettingsPage::new(tx, st.clone(), Rc::new(|| {}));
                page.set_outputs(vec!["eDP-1".into()]);
                page.on_doc(DocInfo {
                    err: None, ai: None, apps: vec![],
                    detected: vec![("p".into(), "A synthetic provider with a remarkably long display name for wrapping".into())],
                    unsupported: vec![], installed: HashSet::new(),
                });
                page.refresh_all();
                let root: gtk::Widget = page.root.clone().upcast();
                crate::prepare_page_measure(&root, width, 480);
                let nat = root.measure(gtk::Orientation::Vertical, width).1;
                let h = nat.min(480);
                root.allocate(width, h, -1, None);
                let ctx = glib::MainContext::default();
                for _ in 0..50 { if !ctx.iteration(false) { break; } }
                root.allocate(width, h, -1, None);
                let tag = format!("font {font} width {width}");
                let (iw, ih) = (width - 16, h - 20);
                assert_eq!(root.width(), iw, "{tag}");
                let mut all = vec![]; descendants(&root, &mut all);
                for w in &all {
                    if w.has_css_class("settings-row") {
                        let (card, rb) = (w.parent().unwrap(), bounds(w, &root));
                        let cb = bounds(&card, &root);
                        assert!(rb.x() >= cb.x() - 0.5 && rb.x() + rb.width() <= cb.x() + cb.width() + 0.5, "row outside card {tag}");
                        if let (Some(a), Some(b)) = (w.first_child(), w.last_child()) {
                            if a != b && w.downcast_ref::<gtk::Box>().is_some_and(|b| b.orientation() == gtk::Orientation::Horizontal) {
                                let (ab, bb) = (bounds(&a, &root), bounds(&b, &root));
                                assert!(ab.x() + ab.width() <= bb.x() + 0.5, "label/control overlap {tag}");
                                assert!(ab.width() >= 1.0, "label vanished {tag}");
                            }
                        }
                    }
                    if let Some(d) = w.downcast_ref::<gtk::DropDown>() {
                        let b = bounds(d.upcast_ref(), &root);
                        assert!(b.x() >= 0.0 && b.x() + b.width() <= iw as f32 + 0.5, "dropdown wider than page {tag}");
                    }
                    if w.has_css_class("settings-footer") {
                        let b = bounds(w, &root);
                        assert!(b.y() >= 0.0 && b.y() + b.height() <= ih as f32 + 0.5 && b.x() + b.width() <= iw as f32 + 0.5, "footer outside root {tag}");
                    }
                    if w.has_css_class("settings-primary") {
                        let b = bounds(w, &root);
                        assert!(b.x() >= 0.0 && b.x() + b.width() <= iw as f32 + 0.5, "Apply outside {tag}");
                    }
                }
                // scroll to the end: the last Utilities row becomes visible
                let sw = all.iter().find_map(|w| w.downcast_ref::<gtk::ScrolledWindow>().cloned()).expect("scroller");
                let adj = sw.vadjustment();
                adj.set_value(adj.upper() - adj.page_size());
                root.allocate(width, h, -1, None);
                assert!(adj.value() > 0.0, "scroller did not scroll {tag}: {} of {}", adj.value(), adj.upper());
                let last = all.iter().filter(|w| w.has_css_class("settings-list-row")).last().expect("rows").clone();
                let (lb, sb) = (bounds(&last, sw.upcast_ref()), sw.height() as f32);
                assert!(lb.y() + lb.height() <= sb + 1.0 && lb.y() >= -1.0, "last row unreachable {tag}: {lb:?} in {sb}");
            }
        }
    }

    fn sample_family(w: &gtk::Label) -> String {
        let attrs = w.attributes().expect("sample has attributes");
        attrs.attributes().iter().find_map(|a| a.downcast_ref::<gtk::pango::AttrString>().map(|s| s.value().to_string()))
            .expect("family attribute")
    }

    /// Needs a display: `cargo test -- --ignored --test-threads=1`. The preview follows the draft, never the active settings.
    #[test]
    #[ignore]
    fn font_preview_tracks_draft_and_discard() {
        gtk::init().expect("display");
        let mut st = NotchSettings::default();
        st.appearance.font_family = Some("Noto Sans".into());
        crate::theme::configure(&st.appearance);
        let (tx, _rx) = async_channel::unbounded();
        let page = SettingsPage::new(tx, st.clone(), Rc::new(|| {}));
        page.refresh_all();
        let root: gtk::Widget = page.root.clone().upcast();
        let mut all = vec![]; descendants(&root, &mut all);
        let sample = all.iter().find(|w| w.has_css_class("font-sample")).and_then(|w| w.downcast_ref::<gtk::Label>()).cloned().expect("sample");
        let dd = all.iter().filter_map(|w| w.downcast_ref::<gtk::DropDown>()).find(|d| d.enables_search()).cloned().expect("font picker");
        assert_eq!(sample_family(&sample), "Noto Sans");
        let model = dd.model().and_downcast::<gtk::StringList>().unwrap();
        let idx = (0..model.n_items()).find(|i| model.string(*i).is_some_and(|s| s == "JetBrainsMono Nerd Font")).expect("installed");
        dd.set_selected(idx);
        assert_eq!(sample_family(&sample), "JetBrainsMono Nerd Font");
        assert_eq!(page.draft.borrow().appearance.font_family.as_deref(), Some("JetBrainsMono Nerd Font"));
        assert_eq!(page.active.borrow().appearance.font_family.as_deref(), Some("Noto Sans"));
        page.discard();
        page.refresh_all();
        assert_eq!(sample_family(&sample), "Noto Sans");
    }
}
