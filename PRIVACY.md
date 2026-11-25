# Privacy policy: Waybar Pill Bar ("Notch Omarchy")

Waybar Pill Bar is an open-source desktop plugin that runs only on your own computer. It has no server and no analytics.

## Google Calendar access (optional)
- Google shows a "hasn't verified this app" warning for OAuth clients whose consent screen is not verified; `calendar.readonly` is a sensitive scope. The app has no client of its own: you create the OAuth client in your own Google Cloud project, so you control (and can verify) it. See the *Google Calendar* section of the README.
- If you choose to connect Google Calendar, the app asks for one scope: `https://www.googleapis.com/auth/calendar.readonly` (read-only).
- It reads your calendar events only to show the month view, the day agenda and the Meet join button in the notch.
- The OAuth tokens and your own OAuth client credentials are stored only on your computer, in files readable only by your user account (`~/.local/share/waybar-notch/google-token.json` and `~/.config/waybar-notch/google.json`).
- Calendar data is not stored on disk beyond those tokens, and is not sent to the developer or to any third party. The app talks only to Google's own API endpoints.
- You can revoke access at any time at https://myaccount.google.com/permissions, and delete the two files above to remove the local copies.

## Other data
The app reads local system state (media, network, battery, clipboard history, AI tool usage logs) to display it. Most of it stays on your computer. Clipboard history is kept in a private file in `~/.local/share/waybar-notch/`.

Enabling the Omarchy service builds and provisions it without requesting OAuth or other credentials. Disabling or removing
the plugin restores managed desktop files and removes its install record and backup tree. It retains preferences,
Google/WhatsApp credentials, clipboard history, build cache, private provisioning logs and durable recovery helpers.
Provisioning logs live under `${XDG_STATE_HOME:-$HOME/.local/state}/waybar-notch/` with user-only permissions.

Shell restart or logout restores session-managed desktop files, but retains WhatsApp's `Panel.qml`, `BarWidget.qml`
and `WaybarPillAnchor.qml` while `waybar.pill-bar` remains in the `plugins` list in
`${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/shell.json`. Their recovery record and original backups remain too.
Disabling/removing Pill Bar, deleting its checkout, missing/malformed shell configuration or explicit recovery
requests full restoration, subject to file-conflict and filesystem trust checks. Quickshell caches plugin QML;
the newly installed adapter may need one shell restart and is not hot-reloaded. These adapter files contain UI
integration code, not WhatsApp credentials or message history.

Three features contact the network. The first two concern what you play, and nothing else about your media leaves the computer; the third concerns only the plugin version:
- **Lyrics:** the Media tab asks LRCLIB (`lrclib.net`) for lyrics, sending the current track's artist, title and duration as query parameters. There is no API key and no account. Responses are size-limited.
- **Cover art:** when a media player supplies an artwork URL (`http` or `https`), the app, and the optional `media-art` Waybar helper if you enable it, fetch that URL from whatever server the player named. That server sees your IP address and the request. Only public internet addresses are contacted; private, loopback and link-local addresses are refused. Local artwork files are read from disk only.
- **Update check:** once a day the app downloads the public `manifest.json` from `raw.githubusercontent.com` (GitHub sees your IP address and the request) and compares its version with your installed one. Nothing about you or your usage is sent. Disable it with `"notch": {"behavior": {"check_updates": false}}` in `~/.config/waybar-notch/config.json`.

## Use of Google user data
Use of information received from Google APIs adheres to the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), including the Limited Use requirements.

## Contact
Open an issue at https://github.com/Estivenm06/waybar-pill-bar/issues.
