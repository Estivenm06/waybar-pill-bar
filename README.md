# waybar-pill-bar

A pill-style Waybar plus a Rust "notch" for **Arch Linux + Omarchy 4.x (Hyprland, Wayland)**. One `./install.sh` gives you translucent rounded pills, an expanding notch (media, calendar, clipboard, AI usage, quick controls), click popups, a notification daemon and a volume/brightness overlay. Colors and font follow your active Omarchy theme and restyle live when you change the theme or font. No personal data is included: the installer substitutes your own `$HOME`.

Target: Omarchy 4.x with the **Lua Hyprland config** (`~/.config/hypr/autostart.lua`, `bindings.lua`, the `o.bind` / `o.window` / `o.exec_on_start` API). Omarchy 3.x (`.conf` configs) is not supported; the installer stops with `This installer targets Omarchy 4.x (Lua Hyprland config).` if it does not find the Lua files.

## Is `./install.sh` enough?

Almost. It does everything from your side except installing packages:

1. Install the pacman packages listed under [Requirements](#requirements--what-you-must-do) (the script only checks and stops if something is missing).
2. Run `./install.sh`. It builds the Rust notch (`cargo`), installs Waybar files, the notch binary, systemd units and theme hooks, and patches your Hyprland Lua config (with backups).

Needs Omarchy 4.x with the Lua Hyprland config; it will not work on other setups as-is.

### As an Omarchy plugin?

This baseline is standalone (Waybar config + Rust/GTK4 binary run by systemd units). Omarchy plugin integration is added in the next commit as a service-plugin bridge; no UI rewrite is involved.

Local promo assets are excluded from the repository and not needed to install.

## What you get

| Feature | What it is |
|---|---|
| **Waybar pills** | Launcher, workspaces, app launchers (Discord, Steam, Spotify, WhatsApp; only the ones installed), media pill, status pill (Bluetooth, Wi-Fi, display, volume, battery) and clock. Theme-colored, translucent, animated. |
| **Notch** | Rust, GTK4 + layer-shell. Collapsed: cover art, title, live equalizer. Hover: springs open into **Home / Media / Calendar / Clipboard / AI / Controls** tabs. |
| **Device details** | On Home, the battery, Wi-Fi and Bluetooth chips open detail pages inside the notch (Back returns Home). Battery shows charge, live power draw, estimated time, capacity, health and cycles from sysfs, plus power profiles. Wi-Fi and Bluetooth show a status card with connected/saved/paired lists. |
| **Popups** | Volume, Wi-Fi, Bluetooth, battery, display; open under the clicked pill on the monitor under the cursor. |
| **Notifications** | The notch is the `org.freedesktop.Notifications` daemon: black cards below the right pill, click = default action, right click = dismiss, hidden in fullscreen. Do Not Disturb is shared with Omarchy (`~/.local/state/omarchy/notifications.json`); critical notifications bypass it. |
| **OSD** | Notch-style overlay for volume and brightness keys (and Wi-Fi/Bluetooth toggles). |
| **Per-app volume** | Master slider plus a slider and mute button per playing application, live via `pactl subscribe`. |
| **Display controls** | Per-monitor on/off (never the last one), per-monitor brightness (only where supported), scale 1 / 1.25 / 1.6 / 2, text size. |
| **Google Calendar** | Month calendar with event days marked, day agenda, Join button for Meet links. Read-only (`calendar.readonly`). The installer offers a guided login. |
| **Lyrics** | Synced lyrics in the Media tab from LRCLIB (no API key). |
| **Clipboard** | Last 50 text copies in `~/.local/share/waybar-notch/clipboard.json`; click an entry to copy it again. |
| **AI tab** | Token/prompt usage for Claude Code, Gemini/Antigravity, OMP and Copilot CLI (needs `sqlite3`), from their local logs. The installer detects which tools you use and lets you choose the tiles. |
| **Controls tab** | Reading Mode (night light), Do Not Disturb, screenshot, screen recording tiles. |

## Technologies

| Area | Used |
|---|---|
| OS / desktop | Arch Linux, Omarchy 4.x, Hyprland (Lua config; IPC socket and `hyprctl`) |
| Bar | Waybar, run as the packaged `waybar.service` (systemd user service) |
| Notch, popups, OSD | Rust 2024 (`cargo`); crates `gtk4 0.11`, `gtk4-layer-shell 0.8`, `zbus 6` (D-Bus notification server), `chrono`, `serde_json`, `ureq` (HTTP), `futures-lite` |
| Rendering | GTK4 + layer-shell + Cairo (equalizer drawing; FFT computed in-process from `parec` audio) |
| Audio | PipeWire/PulseAudio via `pactl`, `pamixer`, `parec` |
| Media | MPRIS via `playerctl`; LRCLIB (lyrics) |
| Connectivity | BlueZ (`bluetoothctl`), NetworkManager (`nmcli`) |
| Power | UPower, sysfs `power_supply`, `power-profiles-daemon` |
| Clipboard | `wl-clipboard` (`wl-paste --watch`, `wl-copy`) |
| Equalizer on the bar | `cava` (optional, Waybar eq module) |
| Calendar | Google Calendar API, OAuth2 loopback flow |
| Google setup | Python 3 + Playwright (setup only; attaches to your own browser) |
| Services | systemd user units (`waybar.service`, `waybar-notch.service`) |
| Fonts | Nerd Fonts; font taken from fontconfig (`fc-match monospace`) |
| Misc | `jq`, Omarchy hooks (`theme-set.d`, `font-set.d`), `omarchy-*` commands (brightness, monitor scaling, text size, night light, screenshot, screenrecord, plugins) |

## Requirements / what you must do

1. **Install the packages** (the installer only checks, it never installs):
   ```bash
   sudo pacman -S --needed waybar playerctl jq libpulse pamixer gtk4 gtk4-layer-shell pkgconf bluez-utils networkmanager upower power-profiles-daemon rust fontconfig wl-clipboard cava
   ```
   For the guided Google login also: `sudo pacman -S --needed python-playwright` and a Chromium-based browser (`chromium`, `brave` or `google-chrome-stable`; or set `WAYBAR_NOTCH_BROWSER`).
2. **Clone and install**:
   ```bash
   git clone https://github.com/Estivenm06/waybar-pill-bar.git waybar-pill-bar && cd waybar-pill-bar
   ./install.sh                 # asks about Google Calendar
   ./install.sh --google        # run the Google login without asking
   ./install.sh --no-google     # skip it
   ./install.sh --no-restart    # files only, no service restart, no Google step
   ./install.sh --reconfigure   # ask again which AI tiles and apps to show
   ```
3. **Sign in to Google when the browser opens** (only if you chose Google Calendar). Expect: your Google sign-in, then pages of the Cloud Console that the script fills in for you, then Google's consent screen for your own app. That last screen says **"Google hasn't verified this app"**: click *Advanced*, then *Go to Waybar Notch (unsafe)*, then *Continue* and *Allow*. It is your own app in your own project. Apps left in Testing get refresh tokens that expire after 7 days (see [Google Calendar](#google-calendar)).
4. Logging out and in is not required.

## Adapts to your setup

| What | Detected from | Behavior |
|---|---|---|
| Theme colors | `~/.local/state/omarchy/current/theme/colors.toml` (`background foreground accent color1 color2 color3`) | Waybar pills follow background/foreground fully (generated `~/.config/waybar/pill-theme.css`). Notch and popups stay dark (the notch must blend with the bezel); their accent, alert color and the equalizer follow the theme. Missing or bad values fall back per key to a built-in palette. |
| Font | `fc-match monospace` (same source as `omarchy-font-current`) | Used everywhere, with `Symbols Nerd Font` and `JetBrainsMono Nerd Font` behind it so icons survive a non-Nerd font. |
| Live restyle | Omarchy hooks `theme-set.d/50-waybar-pill-bar` and `font-set.d/50-waybar-pill-bar` | On theme/font change: regenerate `pill-theme.css`, `SIGUSR2` to Waybar, `waybar-popupctl reload-theme` to the notch. |
| 12h / 24h clock | `date +%p` (empty = 24h) | Waybar clock format substituted at install; the notch decides at start. |
| Battery / AC | `/sys/class/power_supply/*` (type `Battery`, not `scope=Device`; `Mains`/`USB` online) | No `BAT0` assumption; a desktop without battery shows `AC`. |
| App launchers | `command -v discord/vesktop`, `steam`, `spotify`, `omarchy-launch-webapp` | Only installed apps are offered; you pick which show and which run in the background (`files/setup/apps-catalog.json`). |
| Monitors | Hyprland IPC | Popups pin to the monitor under the cursor and scale with its width. |
| Brightness | `omarchy-brightness-display` | Sliders appear only when it reports a value. |
| Notifications / OSD | Omarchy plugins `omarchy.notifications`, `omarchy.osd` | The installer disables them if enabled (the notch replaces both), records them in `~/.local/state/waybar-notch/disabled-plugins` and prints the owner of the D-Bus name; `uninstall.sh` re-enables them. If another daemon still owns `org.freedesktop.Notifications`, stop it and run `systemctl --user restart waybar-notch`. If `omarchy plugin disable` does not stick, add the ids to `disabledPlugins` in `~/.config/omarchy/shell.json`. |

## Install

```bash
./install.sh [--no-restart] [--google | --no-google] [--reconfigure]
```

In order: checks dependencies; backs up what it replaces into `~/.config/waybar/backup-<timestamp>/`; installs Waybar files (substituting `__HOME__` and the clock format); asks (in a terminal, first run or `--reconfigure`) which detected AI tools get a usage tile and which installed apps appear in the first pill and which of them run hidden in the background (a background app's icon always looks active and it is restarted if closed); builds the notch and installs it to `~/.local/bin/waybar-notch`; generates `pill-theme.css`; installs the theme/font hooks; appends two managed blocks (`waybar-pill-bar BEGIN/END`) to `autostart.lua` and `bindings.lua` (re-running refreshes them; it comments out Omarchy's plain `o.launch_on_start("waybar")`); disables the Omarchy notification/OSD plugins; enables `waybar.service` and `waybar-notch.service` and applies the change (Waybar is only *reloaded*, never restarted, and apps start in their own systemd scope, so running apps such as Spotify keep playing); offers the Google Calendar login.

## Uninstall

```bash
./uninstall.sh           # restore the newest backup, remove services, hooks, pill-theme.css, re-enable plugins
./uninstall.sh --purge   # also delete the Google OAuth client, token and sign-in browser profile
```

Without `--purge` it prints the paths it keeps: `~/.config/waybar-notch/google.json`, `~/.local/share/waybar-notch/google-token.json`, `~/.local/share/waybar-notch/browser-profile`. Scripts added to `~/.config/waybar/scripts` stay; delete them by hand.

## Customising

- **Pill colors/layout**: `~/.config/waybar/style.css` (colors come from `@define-color` names in the generated `pill-theme.css`; do not edit that file, it is regenerated).
- **Notch and popup CSS**: `notch/src/style.css` and `POPUP_CSS` in `notch/src/popups.rs`; `{{accent}} {{accent-hi}} {{accent-lo}} {{accent-top}} {{alert}} {{alert-hi}} {{font}}` are theme placeholders. Rebuild with `./install.sh`.
- **Auto-close time**: `AUTO_CLOSE` at the top of `notch/src/popups.rs`.
- **Keyboard layout pill**: `hyprland/language` in `config.jsonc` uses `keyboard-name: at-translated-set-2-keyboard`; find yours with `hyprctl devices`.
- **Launcher classes**: `class` regexes in `files/setup/apps-catalog.json`; find a class with `hyprctl clients | grep class`. Adding an app = one catalog entry + its id in the app selectors of `files/style.css` and `BRANDS` in `notch/src/theme.rs`.
- **Which AI tiles and apps show**: `./install.sh --reconfigure` asks again; the answers live in `~/.config/waybar-notch/config.json` (`ai`: provider ids, `apps`: pill order and `background` flag). AI tiles apply live; apps need `./install.sh`.
- **Preferred player**: `PLAYERS` in `media-follow.py` and the `playerctl -p` in the media click commands.
- **Animations**: delete the `/* ---- Animations ---- */` block from `style.css` and the two `layer_rule` lines in `autostart.lua` to turn them off.

## Troubleshooting

- Bar or notch missing: `systemctl --user status waybar waybar-notch`, `journalctl --user -u waybar -u waybar-notch`; restart with `systemctl --user restart waybar waybar-notch` (do not `killall waybar`: systemd treats it as a clean stop).
- Popup does nothing: `waybar-popupctl <volume|wifi|bluetooth|battery|display|calendar|reload-theme>` writes to the FIFO `$XDG_RUNTIME_DIR/waybar-popupd.fifo` (the file name is historical; `waybar-notch` owns it) and starts the service if needed. Check `pgrep -af waybar-notch`.
- Colors did not change after a theme switch: run `~/.local/bin/waybar-notch --gen-waybar-theme` and `systemctl --user kill --kill-whom=main -s SIGUSR2 waybar.service`; check that `~/.config/omarchy/hooks/theme-set.d/50-waybar-pill-bar` exists.
- App pills show stale running/idle state: the supervisor caches status in `$XDG_RUNTIME_DIR/waybar-notch/app-status/<id>.json` (fed by Hyprland window events) and signals Waybar with `SIGRTMIN+N` (the `signal` of the generated `custom/<app>` modules). The pills themselves run `waybar-notch --waybar-app-status <id>` once; there is no polling. Wi-Fi uses Waybar's native `network` module (interface `wl*`; edit `interface` if your adapter is not named `wl…`).
- Notifications do not appear: `busctl --user status org.freedesktop.Notifications | grep Comm=` must print `Comm=waybar-notch`.
- Popup opens but outside clicks do nothing: the popup window must use keyboard mode `ON_DEMAND`; `EXCLUSIVE` makes Hyprland send all pointer input to the popup.
- `hyprctl dispatch focuswindow ...` fails: on the Lua config use `hyprctl dispatch 'hl.dsp.focus({ window = "address:0x..." })'`.
- Waybar config errors: `python3 -c "import json;json.load(open('$HOME/.config/waybar/config.jsonc'))"`.

## Google Calendar

The setup script `~/.config/waybar/scripts/google-setup.py` (run by the installer, re-run it any time):

1. Starts your Chromium-based browser normally with a dedicated profile (`~/.local/share/waybar-notch/browser-profile`) and a local remote-debugging port. You sign in to Google there; Google blocks sign-in inside automation-flagged browsers, so Playwright attaches only afterwards.
2. Creates the Cloud project `waybar-notch` (id kept in `~/.config/waybar-notch/project-id`), enables the Calendar API, configures the consent screen (External, you as test user), tries to publish the app and creates a Desktop OAuth client.
3. Writes `~/.config/waybar-notch/google.json` (mode 0600) and runs `waybar-notch --google-login --print-url`; the consent page opens in the same browser and you click through it.

Every step verifies its result; when a step cannot be automated the script prints `[manual] ...` naming the exact console page and value and waits for Enter.

**7-day sign-ins:** for personal Desktop apps without a homepage/privacy URL, *Publish app* is currently disabled, and apps left in Testing get refresh tokens that expire after 7 days. When the notch shows "Connect Google Calendar" again, press it (one click) or run `~/.local/bin/waybar-notch --google-login`.

**Manual route:** create a "Desktop app" OAuth client in Google Cloud Console (Calendar API enabled), save `{"client_id": "...", "client_secret": "..."}` to `~/.config/waybar-notch/google.json`, then press "Connect Google Calendar" in the notch's Calendar tab. The refresh token is stored in `~/.local/share/waybar-notch/google-token.json`.

CLI: `waybar-notch --google-login [--print-url]` exits 0 (token saved), 2 (`google.json` missing) or 1 (other error); `--print-url` prints `AUTH_URL <url>` instead of opening a browser. `waybar-notch --gen-waybar-theme` writes `~/.config/waybar/pill-theme.css`.

## Layout

```
waybar-pill-bar/
├── install.sh                 installer (backs up first, never deletes)
├── uninstall.sh               restores the newest backup, undoes the install (--purge)
├── notch/                     Rust crate -> ~/.local/bin/waybar-notch
│   └── src/
│       ├── main.rs            notch UI, tabs, CLI (--gen-waybar-theme, --google-login), FIFO messages
│       ├── popups.rs          click popups, FIFO server, Hyprland IPC
│       ├── theme.rs           Omarchy theme/font -> CSS placeholders and pill-theme.css
│       ├── power.rs           battery / AC detection from sysfs
│       ├── notify.rs          org.freedesktop.Notifications server
│       ├── osd.rs             volume/brightness overlay
│       ├── gcal.rs            Google Calendar OAuth + events
│       ├── lyrics.rs          LRCLIB lyrics
│       ├── usage.rs           AI usage tab (Claude, Gemini, OMP, Copilot CLI)
│       ├── config.rs          reads ~/.config/waybar-notch/config.json (chosen AI tiles)
│       └── style.css          notch CSS (theme placeholders)
└── files/
    ├── config.jsonc           Waybar config -> ~/.config/waybar/config.jsonc
    ├── style.css              Waybar style  -> ~/.config/waybar/style.css (imports pill-theme.css)
    ├── setup/                 read by install.sh only: apps-catalog.json, waybar-setup.py (choices, config + autostart rendering)
    ├── scripts/               -> ~/.config/waybar/scripts/
    │   ├── waybar-popupctl         FIFO client; starts the service if needed
    │   ├── google-setup.py         guided Google Calendar login (Playwright)
    │   ├── waybar-app.sh           class-based launcher click
    │   ├── waybar-bgapp.sh         background app ensure/toggle (WhatsApp by default)
    │   ├── waybar-volume-osd.py    volume keys with OSD
    │   ├── waybar-brightness-key   brightness keys with OSD
    │   ├── media-follow.py / media-art.py / media-eq.py   media pill, cover art, cava bars
    ├── hooks/pill-theme-hook  installed as theme-set.d and font-set.d hook
    ├── systemd/waybar-notch.service
    └── hypr/                  managed blocks for your Hyprland Lua config
        ├── autostart.snippet.lua
        └── bindings.snippet.lua
```

## Verified vs not verified

Exercised on the author's machine (Omarchy 4.x, Hyprland, one laptop with battery, theme `amekoji`):

- `cargo build --release` and `cargo test` (theme parsing and fallback, color mix, battery selection, FFT, calendar helpers).
- `waybar-notch --gen-waybar-theme` output; `--google-login --print-url` exit code 2 without `google.json`, and a single `AUTH_URL` line (no browser launched) with a dummy client.
- `./install.sh --no-google` end to end: launcher filtering (all four apps installed here), hooks installed, `pill-theme.css` generated, both services active, `Comm=waybar-notch` owns the notification bus.
- Live theme change with `omarchy-theme-set` to Catppuccin Latte (light pills, blue accent, popup slider gradient in the new accent) and Gruvbox, then back to Amekoji with the original colors restored.
- `./uninstall.sh` then `./install.sh` round trip, including re-enabling and re-disabling `omarchy.osd` (`omarchy.notifications` was already disabled on this machine).

Not verified:

- Omarchy 3.x.
- WhatsApp with a browser other than Brave (the window class regex `web\.whatsapp\.com` is a substring match; confirm with `hyprctl clients | grep -i whatsapp`).
- `google-setup.py` against the live Cloud Console: it was only syntax-checked. All console URLs and locators are unverified and the console UI changes often; the `[manual]` fallbacks are the safety net. The Publish-app flow and a fresh-account run are unverified too.
- 24h locale, a machine without a battery or without some of the apps, a non-Nerd Omarchy font, and the `font-set.d` hook firing (the `theme-set.d` one was exercised).
- A full logout/login cycle.

## Multiple monitors and Settings

- `waybar.service` runs `waybar-notch --waybar-supervisor` (a managed drop-in, `~/.config/systemd/user/waybar.service.d/50-waybar-notch.conf`, `KillMode=process`). It keeps one hidden-by-default Waybar per output and shows only the pills of the screen that hosts the notch; focus changes only send show/hide signals. A fullscreen window (not maximized) on that screen hides the notch and its pills. An open notch/popup stays on its screen until it closes, then follows the focus.
- `Super+Shift+Space` and the theme hook signal only the supervisor (`systemctl --user kill --kill-whom=main ...`).
- Settings: Controls tab → Settings (or `waybar-popupctl settings`). Preferences live in the `notch` object of `~/.config/waybar-notch/config.json` (the installer's `ai`/`apps` stay as they are); Apply never restarts the notch. App changes run the installed helper `~/.local/share/waybar-notch/setup/waybar-setup.py apply` (backs up, validates, rolls back on Hyprland errors, reloads the supervisor).
- Tests: `cargo test --manifest-path notch/Cargo.toml`, `python3 -m unittest files/setup/test_waybar_setup.py`.
