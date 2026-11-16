# waybar-pill-bar

A pill-style Waybar plus a Rust "notch" for **Arch Linux + Omarchy 4.x (Hyprland, Wayland)**. One `./install.sh` gives you translucent rounded pills, an expanding notch (media, calendar, clipboard, AI usage, quick controls), click popups, notification cards and a volume/brightness overlay. Colors and font follow your active Omarchy theme and restyle live when you change the theme or font. No personal data is included: the installer substitutes your own `$HOME`.

Target: Omarchy 4.x with the **Lua Hyprland config** (`~/.config/hypr/autostart.lua`, `bindings.lua`, the `o.bind` / `o.window` / `o.exec_on_start` API). Omarchy 3.x (`.conf` configs) is not supported; the installer stops with `This installer targets Omarchy 4.x (Lua Hyprland config).` if it does not find the Lua files.

## Installation (Omarchy plugin)

Pill Bar is an Omarchy **service plugin** (`waybar.pill-bar`) and is installed only that way; there is no standalone mode. It is a bridge, not a rewrite: the plugin's `Service.qml` holds a lease on a small backend (`files/scripts/waybar-pill-plugin`) that starts the Rust/GTK notch and Waybar while the plugin is enabled and, when it is disabled, removed, or the shell exits, stops them and restores the native bar, notifications and OSD. It is not a Quickshell-rendered bar.

1. Install the pacman packages listed under [Requirements](#requirements--what-you-must-do) (the script only checks and stops if something is missing).
2. Add, set up and enable the plugin:

```
omarchy plugin add https://github.com/Estivenm06/waybar-pill-bar.git
cd ~/.config/omarchy/plugins/waybar.pill-bar
./install.sh --no-google
omarchy plugin enable waybar.pill-bar
# later:
omarchy plugin disable waybar.pill-bar
```

Needs Omarchy 4.x with the Lua Hyprland config and `omarchy plugin list --json` working; setup stops otherwise.

- Setup is explicit: `omarchy plugin add/update` never runs build or privileged hooks, and enabling without `./install.sh` fails with an actionable message. Setup never installs packages, enables the plugin, restarts the shell, hides the desktop or starts a process.
- `./install.sh` installs files only (binary, Waybar config, scripts, hooks, managed Lua blocks). It installs no systemd units. Originals are backed up and recorded in `~/.local/state/waybar-notch/plugin-install.json`; the first backup stays the uninstall baseline.
- While enabled, the backend owns exactly one Waybar/notch pair (the notch launches the per-output Waybar supervisor), hides the native bar and disables `omarchy.notifications`/`omarchy.osd` (and their clones) via `disabledPlugins` in `shell.json`; it restores only what it changed. Popup, theme, Settings and brightness/volume/bar-toggle keys never start anything while the plugin is disabled or not set up (keys fall back to native Omarchy behavior; the theme hook only saves). Disabled helpers never resurrect the product.
- Notifications: Quickshell keeps owning `org.freedesktop.Notifications` (its bus name is process-wide and cannot be released). `Service.qml` loads `NotificationRelay.qml` on the backend's request and forwards each notification to the notch over `$XDG_RUNTIME_DIR/waybar-pill-plugin/notifications.sock` (NDJSON v2 with sequence numbers, acknowledgements and bounded queues, peer-authenticated by PID); the notch renders its usual cards and sends actions/closures back so the sender gets the real D-Bus signals. Activation only reports success after that relay handshake; any failure restores the native desktop. The notch binary itself never claims a notification name and refuses to start without `--notification-relay <shell-pid>`.

- Plugin widgets: the battery's left click always opens the notch's Battery & Power page; with `patcastle.power` enabled its right click opens that plugin's full Power panel. With `io.github.ricky.whatsapp` enabled and WhatsApp selected in the first pill, the icon streams the daemon's unread/link state (`files/scripts/waybar-plugin-widgets whatsapp-status`), left click opens the existing, complete WhatsApp UI inside the central notch (a WhatsApp page on the notch's own glass backplate with its bottom tabs; click the icon again, press Escape or use Back to leave) and right click keeps the web-app toggle. Hosting needs a small adapter (`WaybarPillAnchor.qml`, plus named managed blocks in the WhatsApp plugin's `Panel.qml`; the plugin's own Panel keeps owning the chat client, and a transparent layer window `waybar-notch-content` is placed over the notch's page slot through the private `notch-content.sock`) that `install.sh --plugin` installs, records and `uninstall.sh` restores; after installing, run `omarchy restart shell` once so the shell loads it. The panels are hosted by the parked native bar, so keep those widgets in its layout.
- Qt caches plugin QML for the life of the shell: after changing `Service.qml`/`NotificationRelay.qml` run `omarchy-restart-shell` (disable/enable alone keeps serving the old code).
- Update: disable, `omarchy plugin update waybar.pill-bar`, rerun `./install.sh`, re-enable.
- Full removal: disable, run `./uninstall.sh` (restores the recorded originals; refuses, changing nothing, if an installed file was edited since), then `omarchy plugin remove waybar.pill-bar`. Preferences and Google credentials are kept unless `--purge`. Without an install record `./uninstall.sh` prints `No plugin install record; nothing was changed.` and exits nonzero.
- Older standalone installs (systemd units or unrecorded managed Lua blocks) are detected and refused: `Legacy standalone files detected; remove the old standalone installation with its original uninstaller before running ./install.sh.` Nothing is migrated automatically.
- Crash boundaries: a shell crash closes the lease and cleanup runs automatically. If the backend itself is killed (SIGKILL, power loss) cleanup cannot run: run `files/scripts/waybar-pill-plugin recover` (installed at `~/.config/waybar/scripts/`); the next start also recovers.
- Tests: `python3 -m unittest tests/test_plugin_bridge.py tests/test_omarchy_matrix.py tests/test_notification_relay.py tests/test_plugin_widgets.py tests/test_widget_adapters.py files/setup/test_waybar_setup.py` (one process per file) and `cargo test --manifest-path notch/Cargo.toml`.

Local promo assets are excluded from the repository and not needed to install.

## What you get

| Feature | What it is |
|---|---|
| **Waybar pills** | Launcher, workspaces, app launchers (Discord, Steam, Spotify, WhatsApp; only the ones installed), media pill, status pill (Bluetooth, Wi-Fi, display, volume, battery) and clock. Theme-colored, translucent, animated. |
| **Notch** | Rust, GTK4 + layer-shell. Collapsed: cover art, title, live equalizer. Hover: springs open into **Home / Media / Calendar / Clipboard / AI / Controls** tabs. |
| **Device details** | On Home, the battery, Wi-Fi and Bluetooth chips open detail pages inside the notch (Back returns Home). Battery shows charge, live power draw, estimated time, capacity, health and cycles from sysfs, plus power profiles. Wi-Fi and Bluetooth show a status card with connected/saved/paired lists. |
| **Popups** | Volume, Wi-Fi, Bluetooth, battery, display; open under the clicked pill on the monitor under the cursor. |
| **Notifications** | Black cards below the right pill for notifications the Omarchy shell relays to the notch, click = default action, right click = dismiss, hidden in fullscreen. Do Not Disturb is shared with Omarchy (`~/.local/state/omarchy/notifications.json`); critical notifications bypass it. |
| **OSD** | Notch-style overlay for volume and brightness keys (and Wi-Fi/Bluetooth toggles). |
| **Per-app volume** | Master slider plus a slider and mute button per playing application, live via `pactl subscribe`. |
| **Display controls** | Per-monitor on/off (never the last one), per-monitor brightness (only where supported), scale 1 / 1.25 / 1.6 / 2, text size. |
| **Google Calendar** | Month calendar with event days marked, day agenda, Join button for HTTPS video-call links (Meet, Zoom, Teams, ...; other schemes are never opened). Read-only (`calendar.readonly`). The installer offers a guided login. |
| **Lyrics** | Synced lyrics in the Media tab from LRCLIB (no API key). The current track's artist, title and duration are sent to `lrclib.net`; see `PRIVACY.md`. |
| **Clipboard** | Last 50 text copies in `~/.local/share/waybar-notch/clipboard.json`; click an entry to copy it again. |
| **AI tab** | Token/prompt usage for Claude Code, Gemini/Antigravity, OMP and Copilot CLI (needs `sqlite3`), from their local logs. The installer detects which tools you use and lets you choose the tiles. |
| **Controls tab** | Reading Mode (night light), Do Not Disturb, screenshot, screen recording tiles. |

## Technologies

| Area | Used |
|---|---|
| OS / desktop | Arch Linux, Omarchy 4.x, Hyprland (Lua config; IPC socket and `hyprctl`) |
| Bar | Waybar, one worker per output, run by the plugin backend through the notch supervisor |
| Notch, popups, OSD | Rust 2024 (`cargo`); crates `gtk4 0.11`, `gtk4-layer-shell 0.8`, `zbus 6` (MPRIS player-name validation), `chrono`, `serde_json`, `ureq` (HTTP) |
| Rendering | GTK4 + layer-shell + Cairo (equalizer drawing; FFT computed in-process from `parec` audio) |
| Audio | PipeWire/PulseAudio via `pactl`, `pamixer`, `parec` |
| Media | MPRIS via `playerctl`; LRCLIB (lyrics) |
| Connectivity | BlueZ (`bluetoothctl`), NetworkManager (`nmcli`) |
| Power | UPower, sysfs `power_supply`, `power-profiles-daemon` |
| Clipboard | `wl-clipboard` (`wl-paste --watch`, `wl-copy`) |
| Equalizer on the bar | `cava` (optional, Waybar eq module) |
| Calendar | Google Calendar API, OAuth2 loopback flow |
| Google setup | Python 3 (setup only; manual guide in your own browser) |
| Services | Omarchy service plugin (`Service.qml` + `waybar-pill-plugin` backend); no Pill Bar systemd units |
| Fonts | Nerd Fonts; font taken from fontconfig (`fc-match monospace`) |
| Misc | `jq`, Omarchy hooks (`theme-set.d`, `font-set.d`), `omarchy-*` commands (brightness, monitor scaling, text size, night light, screenshot, screenrecord, plugins) |

## Requirements / what you must do

1. **Install the packages** (the installer only checks, it never installs):
   ```bash
   sudo pacman -S --needed waybar playerctl jq libpulse pamixer gtk4 gtk4-layer-shell pkgconf bluez-utils networkmanager upower power-profiles-daemon rust fontconfig wl-clipboard cava
   ```
   The guided Google login needs only an interactive terminal and your normal browser.
2. **Set up the plugin** (from the plugin checkout, see above):
   ```bash
   cd ~/.config/omarchy/plugins/waybar.pill-bar
   ./install.sh                 # plugin setup; Google Calendar is skipped unless --google
   ./install.sh --google        # run the Google login without asking
   ./install.sh --no-google     # same as the default
   ./install.sh --no-restart    # skip the Hyprland reload and config-error check
   ./install.sh --reconfigure   # ask again which AI tiles and apps to show
   ```
3. **Sign in to Google when the browser opens** (only if you chose Google Calendar). Expect: your Google sign-in, then pages of the Cloud Console that the script fills in for you, then Google's consent screen for your own app. That last screen says **"Google hasn't verified this app"**: click *Advanced*, then *Go to Waybar Notch (unsafe)*, then *Continue* and *Allow*. It is your own app in your own project. Apps left in Testing get refresh tokens that expire after 7 days (see [Google Calendar](#google-calendar)).
4. Logging out and in is not required.

## Adapts to your setup

| What | Detected from | Behavior |
|---|---|---|
| Theme colors | `~/.local/state/omarchy/current/theme/colors.toml` (`background foreground accent color1 color2 color3`) | Waybar pills follow background/foreground fully (generated `~/.config/waybar/pill-theme.css`). Notch and popups stay dark (the notch must blend with the bezel); their accent, alert color and the equalizer follow the theme. Missing or bad values fall back per key to a built-in palette. |
| Font | `fc-match monospace` (same source as `omarchy-font-current`) | Used everywhere, with `Symbols Nerd Font` and `JetBrainsMono Nerd Font` behind it so icons survive a non-Nerd font. |
| Live restyle | Omarchy hooks `theme-set.d/50-waybar-pill-bar` and `font-set.d/50-waybar-pill-bar` | On theme/font change: regenerate `pill-theme.css`, backend `reload` (only while the plugin is active; otherwise the theme is just saved), `waybar-popupctl reload-theme` to the notch. |
| 12h / 24h clock | `date +%p` (empty = 24h) | Waybar clock format substituted at install; the notch decides at start. |
| Battery / AC | `/sys/class/power_supply/*` (type `Battery`, not `scope=Device`; `Mains`/`USB` online) | No `BAT0` assumption; a desktop without battery shows `AC`. |
| App launchers | `command -v discord/vesktop`, `steam`, `spotify`, `omarchy-launch-webapp` | Only installed apps are offered; you pick which show and which run in the background (`files/setup/apps-catalog.json`). |
| Monitors | Hyprland IPC | Popups pin to the monitor under the cursor and scale with its width. |
| Brightness | `omarchy-brightness-display` | Sliders appear only when it reports a value. |
| Notifications / OSD | Omarchy plugins `omarchy.notifications`, `omarchy.osd` (and clones) | The plugin session disables them while enabled (the notch replaces both) and restores exactly what it disabled when the session ends. If `omarchy plugin disable` does not stick, check `disabledPlugins` in `~/.config/omarchy/shell.json`. |

## Install

```bash
./install.sh [--no-restart] [--google | --no-google] [--reconfigure]
```

In order: checks dependencies; requires `manifest.json` and a working `omarchy plugin list --json`; builds the notch; preflights the WhatsApp adapter paths; records the original of every file it will replace (`plugin-state.py begin`); installs the adapter, Waybar files (substituting `__HOME__` and the clock format), scripts, `~/.local/bin/waybar-notch` and the theme/font hooks; asks (in a terminal, first run or `--reconfigure`) which detected AI tools get a usage tile and which installed apps appear in the first pill and which of them run hidden in the background (a background app's icon always looks active and it is restarted while the plugin is active); generates `pill-theme.css`; appends two managed blocks (`waybar-pill-bar BEGIN/END`) to `autostart.lua` and `bindings.lua` (re-running refreshes them); reloads Hyprland unless `--no-restart` and restores the Lua files if Hyprland reports new config errors; records final hashes (`plugin-state.py finish`); and finally runs the Google login only with `--google`. It does not enable the plugin: run `omarchy plugin enable waybar.pill-bar`.

## Uninstall

```bash
./uninstall.sh           # disable the plugin, restore the recorded originals, remove what setup added
./uninstall.sh --purge   # also delete the Google OAuth client, token and sign-in browser profile
```

Only a recorded plugin install is reversed; with no record it changes nothing, not even with `--purge`. Without `--purge` the Google files are kept: `~/.config/waybar-notch/google.json`, `~/.local/share/waybar-notch/google-token.json`; a `browser-profile` left by older versions is kept too (`--purge` deletes it). Scripts added to `~/.config/waybar/scripts` stay; delete them by hand.

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

- Bar or notch missing: `omarchy plugin list` must show `waybar.pill-bar` enabled and `python3 ~/.config/waybar/scripts/waybar-pill-plugin active` must exit 0. If it reports setup missing, run `./install.sh`; after a backend crash run `~/.config/waybar/scripts/waybar-pill-plugin recover`. Disable and re-enable the plugin to restart the pair (do not `killall waybar`).
- Popup does nothing: `waybar-popupctl <volume|wifi|bluetooth|battery|display|calendar|reload-theme>` only writes to the FIFO `$XDG_RUNTIME_DIR/waybar-popupd.fifo` (the file name is historical; `waybar-notch` owns it) while the plugin session is active; it never starts anything. Check `pgrep -af waybar-notch`.
- Colors did not change after a theme switch: run `~/.local/bin/waybar-notch --gen-waybar-theme` and, with the plugin active, `~/.config/waybar/scripts/waybar-pill-plugin reload`; check that `~/.config/omarchy/hooks/theme-set.d/50-waybar-pill-bar` exists. While the plugin is disabled the hook only saves the theme.
- App pills show stale running/idle state: the supervisor caches status in `$XDG_RUNTIME_DIR/waybar-notch/app-status/<id>.json` (fed by Hyprland window events) and signals Waybar with `SIGRTMIN+N` (the `signal` of the generated `custom/<app>` modules). The pills themselves run `waybar-notch --waybar-app-status <id>` once; there is no polling. Wi-Fi uses Waybar's native `network` module (interface `wl*`; edit `interface` if your adapter is not named `wl…`).
- Notifications do not appear: the plugin must be active and `$XDG_RUNTIME_DIR/waybar-pill-plugin/notifications.sock` must exist; Quickshell, not the notch, owns `org.freedesktop.Notifications`.
- Popup opens but outside clicks do nothing: the popup window must use keyboard mode `ON_DEMAND`; `EXCLUSIVE` makes Hyprland send all pointer input to the popup.
- `hyprctl dispatch focuswindow ...` fails: on the Lua config use `hyprctl dispatch 'hl.dsp.focus({ window = "address:0x..." })'`.
- Waybar config errors: `python3 -c "import json;json.load(open('$HOME/.config/waybar/config.jsonc'))"`.

## Google Calendar

The setup script `~/.config/waybar/scripts/google-setup.py` (run by the installer with `--google`, re-run it any time) is a manual guide in your normal browser; no browser is automated and no debugging port is opened:

1. Opens the Cloud Console pages in order (create project `waybar-notch`, enable the Calendar API, consent screen External, add yourself as test user, create a Desktop OAuth client) and prints the exact instructions and URLs; it asks for the Project ID, Client ID and Client secret (the secret is read without echo).
2. Writes `~/.config/waybar-notch/google.json` (mode 0600 from creation) and runs `waybar-notch --google-login`; the consent page opens in your browser and you click through it. Success is reported only when that login exits 0.

The setup needs an interactive terminal.

**7-day sign-ins:** apps left in Testing get refresh tokens that expire after 7 days. To avoid it, publish the app: in Google Auth Platform > *Branding* set the application home page (this repository) and privacy policy link (`PRIVACY.md` in this repository) and add `github.com` as an authorized domain, then *Audience* > *Publish app* > *Confirm*. Google then shows "hasn't verified this app" at sign-in (click *Advanced* > *Go to ... (unsafe)*); that is expected for a personal, unverified app. When the notch shows "Connect Google Calendar" again, press it (one click) or run `~/.local/bin/waybar-notch --google-login`.

**Manual route:** create a "Desktop app" OAuth client in Google Cloud Console (Calendar API enabled), save `{"client_id": "...", "client_secret": "..."}` to `~/.config/waybar-notch/google.json`, then press "Connect Google Calendar" in the notch's Calendar tab. The refresh token is stored in `~/.local/share/waybar-notch/google-token.json`.

CLI: `waybar-notch --google-login [--print-url]` exits 0 (token saved), 2 (`google.json` missing) or 1 (other error); `--print-url` prints `AUTH_URL <url>` instead of opening a browser. `waybar-notch --gen-waybar-theme` writes `~/.config/waybar/pill-theme.css`.

## Layout

```
waybar-pill-bar/
├── install.sh                 plugin setup (records originals first, never deletes)
├── manifest.json, Service.qml  Omarchy service plugin entry
├── tests/                     plugin bridge tests
├── uninstall.sh               restores the recorded originals, undoes the setup (--purge)
├── notch/                     Rust crate -> ~/.local/bin/waybar-notch
│   └── src/
│       ├── main.rs            notch UI, tabs, CLI (--gen-waybar-theme, --google-login), FIFO messages
│       ├── popups.rs          click popups, FIFO server, Hyprland IPC
│       ├── theme.rs           Omarchy theme/font -> CSS placeholders and pill-theme.css
│       ├── power.rs           battery / AC detection from sysfs
│       ├── notify.rs          notification cards, Do Not Disturb
│       ├── notification_relay.rs  private socket to the shell's notification relay
│       ├── notch_content.rs   private socket for plugin content hosted inside the notch
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
    │   ├── waybar-popupctl         FIFO client; only talks to an active plugin session
    │   ├── google-setup.py         guided Google Calendar login (manual)
    │   ├── waybar-app.sh           class-based launcher click
    │   ├── waybar-bgapp.sh         background app ensure/toggle (WhatsApp by default)
    │   ├── waybar-volume-osd.py    volume keys with OSD
    │   ├── waybar-brightness-key   brightness keys with OSD
    │   ├── media-follow.py / media-art.py / media-eq.py   media pill, cover art, cava bars
    ├── hooks/pill-theme-hook  installed as theme-set.d and font-set.d hook
    └── hypr/                  managed blocks for your Hyprland Lua config
        ├── autostart.snippet.lua
        └── bindings.plugin.snippet.lua
```

## Verification

Run for the plugin-only cutover (each Python file in its own process):

- `bash -n install.sh uninstall.sh files/scripts/waybar-popupctl files/scripts/waybar-brightness-key files/hooks/pill-theme-hook`
- `cargo test --manifest-path notch/Cargo.toml` and `cargo build --release --locked --manifest-path notch/Cargo.toml`
- `python3 -m unittest tests/test_plugin_bridge.py`, `tests/test_omarchy_matrix.py` (temporary-HOME install and uninstall, no-record uninstall, legacy refusal, removed-flag usage error), `tests/test_notification_relay.py`, `tests/test_plugin_widgets.py`, `tests/test_widget_adapters.py` (real patched WhatsApp Panel in Quickshell on a private bus) and `files/setup/test_waybar_setup.py`
- Headless subprocess runs of `notch/target/release/waybar-notch` with no arguments, an unknown command, a missing/zero/non-numeric/negative/extra relay PID and extra supervisor arguments: all print the usage line and exit 1 before GTK or D-Bus; `--gen-waybar-theme` still succeeds.

Not verified: Omarchy 3.x; `google-setup.py` against the live Cloud Console and a live Google consent/token exchange; WhatsApp with a browser other than Brave; a machine without a battery; a non-Nerd Omarchy font; a full logout/login cycle. This change did not redeploy or restart any live desktop.

## Multiple monitors and Settings

- The backend runs `waybar-notch --waybar-supervisor` for the one owned Waybar/notch pair. It keeps one hidden-by-default Waybar per output and shows only the pills of the screen that hosts the notch; focus changes only send show/hide signals. A fullscreen window (not maximized) on that screen hides the notch and its pills. An open notch/popup stays on its screen until it closes, then follows the focus.
- `Super+Shift+Space` toggles through `waybar-pill-plugin toggle` (the owned supervisor while the plugin is active, the native bar otherwise); the theme hook uses the backend's `reload`.
- Settings: Controls tab → Settings (or `waybar-popupctl settings`). Preferences live in the `notch` object of `~/.config/waybar-notch/config.json` (the installer's `ai`/`apps` stay as they are); Apply never restarts the notch. App changes run the installed helper `~/.local/share/waybar-notch/setup/waybar-setup.py apply` (backs up, validates, rolls back on Hyprland errors, reloads the supervisor).
- Tests: `cargo test --manifest-path notch/Cargo.toml`, `python3 -m unittest files/setup/test_waybar_setup.py`.
