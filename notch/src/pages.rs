//! Page registry for the notch.

use crate::page::Page;

pub fn build() -> Vec<(&'static str, Box<dyn Page>)> {
    let mut v: Vec<(&'static str, Box<dyn Page>)> = vec![("home", Box::new(crate::home::Home::new()))];
    for (id, kind) in [("wifi", "wifi"), ("bt", "bluetooth"), ("audio", "volume"), ("battery", "battery"), ("display", "display")] {
        if let Some(p) = crate::popups::BodyPage::new(kind) { v.push((id, Box::new(p))); }
    }
    v.push(("controls", Box::new(crate::controls::Controls::new())));
    v.push(("apps", Box::new(crate::apps_page::AppsPage::new())));
    v.push(("clipboard", Box::new(crate::controls::Clipboard::new())));
    v.push(("weather", Box::new(crate::info_pages::WeatherPage::new())));
    v.push(("ai", Box::new(crate::info_pages::AiPage::new())));
    v.push(("media", Box::new(crate::media_page::MediaPage::new())));
    v.push(("calendar", Box::new(crate::calendar::CalendarPage::new())));
    v.push(("settings", Box::new(crate::settings_page::SettingsPage::new())));
    v.push(("whatsapp", Box::new(crate::whatsapp::WhatsAppPage)));
    v
}
