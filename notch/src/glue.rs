//! Adapters between the notch UI and the domain modules. Everything the notch
//! and Home page need from other modules goes through here so the UI code
//! stays independent of module internals.

use crate::media::MediaPresence;
use crate::notch::CollapsedMedia;
use crate::render::Image;
use crate::state::State;

pub use crate::power::actively_charging;

pub struct NowPlaying {
    pub present: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub art: Option<Image>,
}

pub fn now_playing(st: &State) -> NowPlaying {
    let m = &st.media;
    NowPlaying {
        present: m.presence == MediaPresence::Present,
        playing: m.playing,
        title: m.title.clone(),
        artist: m.artist.clone(),
        art: m.art.clone(),
    }
}

pub fn collapsed_media(st: &State) -> Option<CollapsedMedia> {
    let m = &st.media;
    (m.presence == MediaPresence::Present).then(|| CollapsedMedia { art: m.art.clone(), bars: m.bars, playing: m.playing })
}

pub fn set_active(expanded: bool) {
    crate::media::set_active(expanded);
    crate::sysinfo::set_active(expanded);
}

pub fn play_pause() { crate::media::play_pause(); }
