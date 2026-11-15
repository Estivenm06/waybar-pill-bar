#!/usr/bin/env python3
"""Connect the notch to Google Calendar (guided, manual).

You work in your normal browser: this script opens the right Google Cloud Console pages and tells you what
to click, asks for the project ID and the OAuth client ID/secret, writes ~/.config/waybar-notch/google.json
(mode 0600) and then runs the notch login (waybar-notch --google-login), which opens the consent page.

No browser is automated or started with a debugging port; nothing signed in is ever exposed locally.
Needs an interactive terminal (the client secret is read without echo). `xdg-open` is optional: the URLs
are always printed.
Exit codes: 0 connected, 1 failed.
"""
import getpass
import json
import os
import re
import subprocess
import sys
import tempfile
import warnings

HOME = os.path.expanduser("~")
CONF = os.path.join(HOME, ".config/waybar-notch")
GOOGLE_JSON = os.path.join(CONF, "google.json")
PROJECT_FILE = os.path.join(CONF, "project-id")
NOTCH = os.path.join(HOME, ".local/bin/waybar-notch")
CONSOLE = "https://console.cloud.google.com"
PID_RE = re.compile(r"^[a-z][a-z0-9-]{4,28}[a-z0-9]$")
CLIENT_ID_RE = re.compile(r"\d+-[a-z0-9]+\.apps\.googleusercontent\.com")


def die(msg, code=1):
    print(f"google-setup: {msg}", file=sys.stderr)
    sys.exit(code)


def prompt(text):
    try:
        return input(text).strip()
    except EOFError:
        die("input ended before setup finished; nothing was connected")


def prompt_secret(text):
    with warnings.catch_warnings():
        warnings.simplefilter("error", getpass.GetPassWarning)
        try:
            return getpass.getpass(text).strip()
        except EOFError:
            die("input ended before setup finished; nothing was connected")
        except getpass.GetPassWarning:
            die("cannot read the secret without echo; run this in an interactive terminal")


def open_page(url):
    """Print the URL and try to open it in the normal browser; failure only means 'open it by hand'."""
    print(f"   {url}")
    try:
        r = subprocess.run(["xdg-open", url], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=20)
        ok = r.returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        ok = False
    if not ok:
        print("   (could not open a browser automatically; open the URL above by hand)")


def write_private(path, text):
    """Atomically replace `path` with a file that was mode 0600 from creation."""
    d = os.path.dirname(path)
    tmp = None
    try:
        os.makedirs(d, exist_ok=True)
        fd, tmp = tempfile.mkstemp(prefix=".tmp-", dir=d)
        with os.fdopen(fd, "w") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
        tmp = None
    except OSError as e:
        raise RuntimeError(f"cannot write {path}: {e.strerror or type(e).__name__}") from None
    finally:
        if tmp:
            try:
                os.unlink(tmp)
            except OSError:
                pass


def write_google_json(cid, sec):
    write_private(GOOGLE_JSON, json.dumps({"client_id": cid, "client_secret": sec}))


def saved_project():
    try:
        with open(PROJECT_FILE) as f:
            pid = f.read().strip()
    except OSError:
        return None
    return pid if PID_RE.match(pid) else None


def project_fallback():
    while True:
        pid = prompt("Paste the Project ID (shown in the project picker, e.g. waybar-notch-123456): ")
        if PID_RE.match(pid):
            return pid
        print("That is not a valid project ID.")


def client_fallback():
    while True:
        cid = prompt("Paste the Client ID (ends with .apps.googleusercontent.com): ")
        if not CLIENT_ID_RE.fullmatch(cid):
            print("The client ID does not look right, try again.")
            continue
        sec = prompt_secret("Paste the Client secret (input hidden): ")
        if sec:
            return cid, sec
        print("The client secret is empty, try again.")


def do_project():
    pid = saved_project()
    if pid:
        print(f"Reusing project {pid}")
        return pid
    print("== 1. Create a Google Cloud project")
    print("   Sign in, name the project 'waybar-notch', click Create, then copy its Project ID.")
    open_page(f"{CONSOLE}/projectcreate")
    pid = project_fallback()
    try:
        write_private(PROJECT_FILE, pid + "\n")
    except RuntimeError as e:
        print(f"google-setup: {e} (continuing)", file=sys.stderr)
    return pid


def main():
    if not sys.stdin.isatty():
        die("needs an interactive terminal; no configuration was changed")
    pid = do_project()
    print("== 2. Enable the Google Calendar API")
    print("   Click Enable.")
    open_page(f"{CONSOLE}/apis/library/calendar-json.googleapis.com?project={pid}")
    prompt("Press Enter when done... ")

    print("== 3. Configure the OAuth consent screen")
    print("   App name 'Waybar Notch', audience External, your email as support/contact, then save.")
    open_page(f"{CONSOLE}/auth/overview?project={pid}")
    prompt("Press Enter when done... ")

    print("== 4. Add yourself as a test user")
    print("   Audience > Test users > Add users: the Google account whose calendar you will use.")
    print("   The app stays in Testing: its refresh tokens expire after 7 days. When the notch shows")
    print("   'Connect Google Calendar' again, press it or run ~/.local/bin/waybar-notch --google-login")
    open_page(f"{CONSOLE}/auth/audience?project={pid}")
    prompt("Press Enter when done... ")

    print("== 5. Create a Desktop app OAuth client")
    print("   Application type 'Desktop app', then copy the Client ID and Client secret.")
    open_page(f"{CONSOLE}/auth/clients/create?project={pid}")
    cid, sec = client_fallback()
    try:
        write_google_json(cid, sec)
    except RuntimeError as e:
        die(str(e))
    print(f"Saved {GOOGLE_JSON} (mode 0600)")

    print("== 6. Sign in to the notch")
    if not os.access(NOTCH, os.X_OK):
        die(f"{NOTCH} is missing; run the installer first")
    print("   A consent page opens in your browser: choose your account and click Allow.")
    try:
        rc = subprocess.run([NOTCH, "--google-login"]).returncode
    except OSError as e:
        die(f"cannot run {NOTCH}: {e.strerror or type(e).__name__}")
    if rc != 0:
        die(f"login failed (exit {rc}); re-run after fixing the problem, or run {NOTCH} --google-login")
    print("Google Calendar connected.")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print(file=sys.stderr)
        die("interrupted; nothing was connected", 130)
