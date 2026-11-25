mod app_status;
mod apps;
mod apps_page;
mod audio;
mod art_cache;
mod art_http;
mod calendar;
mod config;
mod controls;
mod desktop;
mod dock;
mod desktop_entries;
mod gcal;
mod glue;
mod home;
mod hypr;
mod icon_theme;
mod info_pages;
mod image_loader;
mod layout;
mod lyrics;
mod media;
mod media_page;
mod net;
mod notch;
mod notch_content;
mod notification_relay;
mod notify;
mod osd;
mod page;
mod pages;
mod pills;
mod popups;
mod power;
mod private_files;
mod render;
mod rightpill;
mod reminders;
mod state;
mod settings_page;
mod sysinfo;
#[cfg(test)]
mod testkit;
mod theme;
mod usage;
mod ui;
mod wayland;
mod whatsapp;
mod weather;
mod wifi;

use state::State;
use wayland::{Cmd, Shell};

/// Per-output pills plus the notch on the selected output.
struct AppShell {
    pills: pills::PillShell,
    notch_open: bool,
}

impl Shell for AppShell {
    fn outputs_changed(&mut self, state: &mut State, cmds: &mut Vec<Cmd>) {
        self.pills.outputs_changed(state, cmds);
        let connected = |state: &State, n: &str| state.outputs.iter().any(|o| o.name == n);
        // `App::rescan_outputs` already closed the notch panel if its output vanished: forget it and finish
        // whatever it hosted, so a fresh Home page opens on a surviving output
        let lost = self.notch_open && state.notch.output.as_deref().is_some_and(|n| !connected(state, n));
        if self.notch_open && (lost || state.outputs.is_empty()) {
            self.notch_open = false;
            state.notch = state::NotchInfo::default();
            state.osd.open = false;
            whatsapp::drop_session(state, "output removed", None, &mut Vec::new());
        }
        let target = desktop::select_output(&state.desk.snap, &state.settings, None, None)
            .filter(|n| connected(state, n))
            .or_else(|| state.outputs.iter().map(|o| o.name.clone()).min());
        if !self.notch_open && target.is_some() {
            self.notch_open = true;
            // seed the selection before the notch exists so the pills show on its output from the first frame
            state.notch.output = target.clone();
            cmds.push(Cmd::Open { view: Box::new(notch::Notch::new(pages::build())), output: target });
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut shell_pid: Option<i32> = None;
    match args.first().map(String::as_str) {
        Some("--notification-relay") => {
            shell_pid = args.get(1).filter(|_| args.len() == 2).and_then(|p| p.parse::<i32>().ok()).filter(|p| *p > 0);
            if shell_pid.is_none() {
                eprintln!("usage: waybar-notch --notification-relay <shell-pid>");
                std::process::exit(1);
            }
        }
        Some("--google-login") => std::process::exit(google_login(args.iter().any(|a| a == "--print-url"))),
        None => {}
        Some(_) => {
            eprintln!("usage: waybar-notch --notification-relay <shell-pid> | --google-login [--print-url]");
            std::process::exit(1);
        }
    }
    let state = State::new();
    let shell = AppShell { pills: pills::PillShell::new(), notch_open: false };
    let res = wayland::run(state, Box::new(shell), move |poster, _state| {
        sysinfo::start(poster);
        desktop::start(poster);
        media::start(poster);
        weather::start(poster);
        popups::start_fifo(poster);
        calendar::start(poster);
        settings_page::watch_config(poster);
        if let Some(pid) = shell_pid {
            whatsapp::start(pid, poster);
            if let Err(e) = notify::start(pid, poster) {
                eprintln!("waybar-notch: notification relay: {e}");
                std::process::exit(1);
            }
        }
        Vec::new()
    });
    if let Err(e) = res {
        eprintln!("waybar-notch: {e}");
        std::process::exit(1);
    }
}

/// `--google-login`: run the OAuth consent flow in the user's browser (or print the URL for the setup script).
fn google_login(print_url: bool) -> i32 {
    use std::io::Write;
    let open = move |u: &str| {
        if print_url {
            println!("AUTH_URL {u}");
            let _ = std::io::stdout().flush();
        } else if let Ok(mut c) = std::process::Command::new("xdg-open").arg(u).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn() {
            std::thread::spawn(move || { let _ = c.wait(); });
        }
    };
    match gcal::authorize(gcal::AccessMode::ReadOnly, &open) {
        Ok(()) => 0,
        Err(gcal::CalendarError::Message(e)) if e == gcal::ERR_NO_CONFIG => { eprintln!("{e}"); 2 }
        Err(e) => { eprintln!("{e}"); 1 }
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;
    use wayland::OutputInfo;

    fn state(outputs: &[&str], focused: &str) -> State {
        let mut st = State::new();
        st.outputs = outputs.iter().enumerate().map(|(i, n)| OutputInfo { name: n.to_string(), pos: (1600 * i as i32, 0), size: (1600, 1000) }).collect();
        let mon = |id: usize, name: &str| serde_json::json!({"id": id, "name": name, "width": 1600, "height": 1000, "scale": 1.0, "x": 1600 * id, "y": 0,
            "focused": name == focused, "activeWorkspace": {"id": id + 1}, "specialWorkspace": {"id": 0}});
        let mons: Vec<_> = outputs.iter().enumerate().map(|(i, n)| mon(i, n)).collect();
        st.desk.snap = desktop::parse_snapshot(&serde_json::json!(mons), &serde_json::json!([]));
        st
    }

    fn shell() -> AppShell { AppShell { pills: pills::PillShell::new(), notch_open: false } }

    fn open_outputs(cmds: &[Cmd]) -> Vec<Option<&str>> {
        cmds.iter().filter_map(|c| if let Cmd::Open { output, .. } = c { Some(output.as_deref()) } else { None }).collect()
    }

    /// Give every opened panel an id like the app does.
    fn attach(cmds: Vec<Cmd>, next: &mut u32) -> Vec<Cmd> {
        cmds.into_iter().map(|c| match c {
            Cmd::Open { mut view, output } => { view.attached(*next); *next += 1; Cmd::Open { view, output } }
            c => c,
        }).collect()
    }

    #[test]
    fn first_reconciliation_opens_one_notch_on_the_focused_output_and_seeds_the_selection() {
        let mut st = state(&["X", "Y"], "Y");
        let mut sh = shell();
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        assert_eq!(st.notch.output.as_deref(), Some("Y"));
        assert_eq!(open_outputs(&cmds).iter().filter(|o| **o == Some("Y")).count(), 3, "two Y pills and the notch");
        assert_eq!(open_outputs(&cmds).iter().filter(|o| **o == Some("X")).count(), 2);
        cmds.clear();
        sh.outputs_changed(&mut st, &mut cmds);
        assert!(cmds.is_empty(), "nothing is duplicated");
    }

    #[test]
    fn losing_the_notch_output_recreates_it_once_on_a_survivor_and_ends_its_whatsapp_session() {
        let mut st = state(&["X", "Y"], "Y");
        let mut sh = shell();
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        let mut next = 10;
        attach(cmds, &mut next);
        whatsapp::open_for_test(&mut st, "Y");
        assert!(st.wa.live());
        st.osd.open = true;
        // Y is unplugged (the app already closed its panels)
        st.outputs.retain(|o| o.name == "X");
        st.desk = state(&["X"], "X").desk;
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        assert_eq!(open_outputs(&cmds), [Some("X")], "exactly one replacement notch, no duplicate pills");
        assert_eq!(st.notch.output.as_deref(), Some("X"));
        assert!(!st.wa.live() && !st.osd.open);
        assert_eq!(cmds.iter().filter(|c| matches!(c, Cmd::Close(_))).count(), 2, "the vanished output's two pills");
    }

    #[test]
    fn no_outputs_clears_the_selection_and_the_next_output_gets_a_notch_again() {
        let mut st = state(&["X"], "X");
        let mut sh = shell();
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        st.outputs.clear();
        st.desk = state(&[], "").desk;
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        assert!(open_outputs(&cmds).is_empty());
        assert_eq!(st.notch.output, None);
        assert!(!sh.notch_open);
        st.outputs = state(&["Z"], "Z").outputs;
        st.desk = state(&["Z"], "Z").desk;
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        assert_eq!(open_outputs(&cmds).iter().filter(|o| **o == Some("Z")).count(), 3);
        assert_eq!(st.notch.output.as_deref(), Some("Z"));
    }

    #[test]
    fn a_new_output_does_not_move_a_live_notch() {
        let mut st = state(&["X"], "X");
        let mut sh = shell();
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        st.outputs = state(&["X", "Y"], "X").outputs;
        st.desk = state(&["X", "Y"], "X").desk;
        let mut cmds = Vec::new();
        sh.outputs_changed(&mut st, &mut cmds);
        assert_eq!(open_outputs(&cmds), [Some("Y"), Some("Y")], "only Y's pills; the notch stays on X");
    }
}
