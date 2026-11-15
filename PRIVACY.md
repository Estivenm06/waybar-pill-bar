# Privacy policy: Waybar Pill Bar ("Notch Omarchy")

Waybar Pill Bar is an open-source desktop plugin that runs only on your own computer. It has no server and no analytics.

## Google Calendar access (optional)
- If you choose to connect Google Calendar, the app asks for one scope: `https://www.googleapis.com/auth/calendar.readonly` (read-only).
- It reads your calendar events only to show the month view, the day agenda and the Meet join button in the notch.
- The OAuth tokens and your own OAuth client credentials are stored only on your computer, in files readable only by your user account (`~/.local/share/waybar-notch/google-token.json` and `~/.config/waybar-notch/google.json`).
- Calendar data is not stored on disk beyond those tokens, and is not sent to the developer or to any third party. The app talks only to Google's own API endpoints.
- You can revoke access at any time at https://myaccount.google.com/permissions, and delete the two files above to remove the local copies.

## Other data
The app also reads local system state (media, network, battery, clipboard history, AI tool usage logs) to display it. All of it stays on your computer. Clipboard history is kept in a private file in `~/.local/share/waybar-notch/`.

## Use of Google user data
Use of information received from Google APIs adheres to the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), including the Limited Use requirements.

## Contact
Open an issue at https://github.com/Estivenm06/waybar-pill-bar/issues.
