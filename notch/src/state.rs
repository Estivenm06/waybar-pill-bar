//! Application state shared by every panel. Owned by `wayland::App`; worker
//! threads mutate it only through `Poster::post` closures, views read it via
//! `Ctx::state` / `DrawCtx::state`.
//!
//! Each domain owns its slice: the type lives in the owning module, `State`
//! just holds it. Add fields to a slice in the owning module, not here.

use crate::ui::Palette;
use crate::wayland::OutputInfo;

/// Where a page asks the notch to go next (`State::nav`).
#[derive(Clone, Debug, PartialEq)]
pub enum NavTo {
    /// Switch to a page id ("home", "media", "calendar", "clipboard", "weather", "settings", "ai", "wifi", "bt", "audio", "battery", "controls", "whatsapp").
    Page(&'static str),
    /// Collapse the notch.
    Collapse,
}

/// Compact system status shown by the right pill and the Home chips.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub power: Option<crate::power::PowerSnapshot>,
    pub volume: Volume,
    pub wifi: WifiStatus,
    pub bt: BtStatus,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Volume {
    pub available: bool,
    pub percent: u8,
    pub muted: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WifiStatus {
    pub available: bool,
    pub on: bool,
    /// Active network name, if connected.
    pub ssid: Option<String>,
    /// 0..=100
    pub signal: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BtStatus {
    pub available: bool,
    pub on: bool,
    /// Connected devices, each with its own live battery when it reports one.
    pub connected: Vec<ConnectedBtDevice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectedBtDevice {
    pub mac: String,
    pub name: String,
    pub battery: Option<u8>,
}

/// Where the notch surface sits (kept current by the notch); pages that must
/// position external windows over their area need it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotchInfo {
    pub output: Option<String>,
    /// Top-left of the notch layer surface in the output's logical coordinates.
    pub origin: (f32, f32),
    /// Bottom edge of the collapsed notch (top margin + height): windows must start below it.
    pub bottom: f32,
    /// Whether the card is currently expanded (target state).
    pub expanded: bool,
    /// Pointer is over the notch.
    pub hover: bool,
}

pub struct State {
    pub outputs: Vec<OutputInfo>,
    pub settings: crate::config::NotchSettings,
    /// Rebuilt whenever settings or the Omarchy theme change.
    pub pal: Palette,
    pub status: Status,
    pub net: crate::net::Net,
    pub sys: crate::sysinfo::Sys,
    pub desk: crate::desktop::Desk,
    pub apps: crate::apps::Apps,
    pub media: crate::media::Media,
    pub weather: crate::weather::State,
    pub cal: crate::calendar::CalData,
    pub notes: crate::notify::Notes,
    pub osd: crate::osd::OsdState,
    pub popup: crate::popups::PopupState,
    /// Navigation request set by any page/pill; the notch consumes it.
    pub nav: Option<NavTo>,
    pub notch: NotchInfo,
    /// The user toggled the whole bar off (SIGUSR1 from `waybar-pill-plugin toggle`).
    pub bar_hidden: bool,
    pub wa: crate::whatsapp::Wa,
    /// Result of the last settings save, consumed by the settings page.
    pub pending_settings_result: Option<Result<(crate::config::NotchSettings, Option<String>), String>>,
    pub settings_doc: Option<Box<crate::settings_page::DocInfo>>,
    /// Date the calendar page should open on (set together with `nav = Page("calendar")`).
    pub cal_goto: Option<chrono::NaiveDate>,
}

impl State {
    pub fn new() -> State {
        let (settings, _warn) = crate::config::load_settings();
        crate::theme::configure(&settings.appearance);
        let pal = Palette::current(settings.appearance.corner_radius as f32, settings.appearance.font_size as f32);
        let mut apps = crate::apps::Apps::default();
        apps.set_pins(&settings.pinned_apps);
        apps.set_order(&settings.app_order);
        State {
            outputs: Vec::new(),
            settings,
            pal,
            status: Status::default(),
            net: Default::default(),
            sys: Default::default(),
            desk: Default::default(),
            apps,
            media: Default::default(),
            weather: Default::default(),
            cal: Default::default(),
            notes: Default::default(),
            osd: Default::default(),
            popup: Default::default(),
            nav: None,
            notch: NotchInfo::default(),
            bar_hidden: false,
            wa: Default::default(),
            pending_settings_result: None,
            settings_doc: None,
            cal_goto: None,
        }
    }

    /// Re-derive the palette after a settings/theme change. Settings were replaced, so the pinned launchers
    /// are re-synced too.
    pub fn refresh_palette(&mut self) {
        self.apps.set_pins(&self.settings.pinned_apps);
        self.apps.set_order(&self.settings.app_order);
        crate::theme::configure(&self.settings.appearance);
        let a = &self.settings.appearance;
        self.pal = Palette::current(a.corner_radius as f32, a.font_size as f32);
    }
}
