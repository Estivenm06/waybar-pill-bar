#!/usr/bin/env python3
"""Connect the notch to Google Calendar.

You sign in to Google and click Allow; this script creates your own Google Cloud project, enables the
Calendar API, sets up the OAuth consent screen, creates a Desktop OAuth client, writes
~/.config/waybar-notch/google.json and finishes the notch login (waybar-notch --google-login).

Google refuses sign-in inside automation-flagged browsers, so the real browser is started normally with a
remote-debugging port (dedicated profile) and Playwright attaches to it only after you are signed in.
The Cloud Console UI changes often: every automated step verifies its result and falls back to a
manual instruction that names the exact page and value.

Needs: python-playwright (system package, no `playwright install`), a Chromium-based browser.
Exit codes: 0 connected, 1 failed, 3 missing prerequisite.
"""
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.request

HOME = os.path.expanduser("~")
CONF = os.path.join(HOME, ".config/waybar-notch")
DATA = os.path.join(HOME, ".local/share/waybar-notch")
GOOGLE_JSON = os.path.join(CONF, "google.json")
PROJECT_FILE = os.path.join(CONF, "project-id")
PROFILE = os.path.join(DATA, "browser-profile")
TOKEN = os.path.join(DATA, "google-token.json")
NOTCH = os.path.join(HOME, ".local/bin/waybar-notch")
CONSOLE = "https://console.cloud.google.com"
PID_RE = re.compile(r"^[a-z][a-z0-9-]{4,28}[a-z0-9]$")
CLIENT_ID_RE = re.compile(r"\d+-[a-z0-9]+\.apps\.googleusercontent\.com")
CLIENT_SECRET_RE = re.compile(r"GOCSPX-[A-Za-z0-9_-]+")
EMAIL_RE = re.compile(r"[\w.+-]+@[\w-]+(?:\.[\w-]+)+")

try:
    from playwright.sync_api import sync_playwright
except ImportError:
    print("python-playwright is missing: sudo pacman -S --needed python-playwright")
    sys.exit(3)


def die(msg, code=1):
    print(f"google-setup: {msg}", file=sys.stderr)
    sys.exit(code)


def prompt(text):
    try:
        return input(text).strip()
    except EOFError:
        die("needs an interactive terminal for the manual fallback")


def find_browser():
    names = [os.environ.get("WAYBAR_NOTCH_BROWSER"), "chromium", "brave", "google-chrome-stable"]
    for n in names:
        if n and shutil.which(n):
            return shutil.which(n)
    print("No Chromium-based browser found: sudo pacman -S --needed chromium "
          "(or set WAYBAR_NOTCH_BROWSER to brave / google-chrome-stable)")
    sys.exit(3)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def tab_urls(port):
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list", timeout=2) as r:
            return [t.get("url", "") for t in json.load(r)]
    except Exception:
        return []


def wait_for_login(port, proc, timeout=600):
    """The console redirects to accounts.google.com until you sign in."""
    print("Sign in to Google in the browser window that just opened...")
    end = time.time() + timeout
    while time.time() < end:
        if proc.poll() is not None:
            die("the browser exited before you signed in")
        if any(u.startswith(CONSOLE + "/") for u in tab_urls(port)):
            return
        time.sleep(2)
    die("timed out waiting for the Google sign-in (10 minutes)")


def step(title, fn, manual, fallback=None):
    """Run one automated step; on any failure print the manual instruction and continue."""
    print(f"== {title}")
    try:
        return fn()
    except Exception as e:  # locators break whenever the console UI changes
        first = str(e).strip().splitlines()[0] if str(e).strip() else type(e).__name__
        print(f"[manual] {manual}\n         (automation failed: {first})")
        prompt("Press Enter when done... ")
        return fallback() if fallback else None


# ------------------------------------------------------------------ steps
def project_id_from_url(url):
    m = re.search(r"[?&]project=([a-z][a-z0-9-]{4,28}[a-z0-9])", url)
    return m.group(1) if m else None


def do_project(page):
    if os.path.exists(PROJECT_FILE):
        pid = open(PROJECT_FILE).read().strip()
        if PID_RE.match(pid):
            print(f"Reusing project {pid}")
            return pid
    page.goto(f"{CONSOLE}/projectcreate", wait_until="domcontentloaded")
    name = page.get_by_label(re.compile(r"project name", re.I)).first
    name.wait_for()
    name.fill("waybar-notch")
    page.get_by_role("button", name=re.compile(r"^Create$")).click()
    end = time.time() + 120
    pid = None
    while time.time() < end and not pid:
        pid = project_id_from_url(page.url)
        time.sleep(1)
    if not pid:
        raise RuntimeError("no project= in the URL after creating the project")
    return pid


def project_fallback():
    while True:
        pid = prompt("Paste the Project ID (console project picker, e.g. waybar-notch-123456): ")
        if PID_RE.match(pid):
            return pid
        print("That is not a valid project ID.")


def do_api(page, pid):
    page.goto(f"{CONSOLE}/apis/library/calendar-json.googleapis.com?project={pid}", wait_until="domcontentloaded")
    enable = page.get_by_role("button", name=re.compile(r"^Enable$", re.I))
    done = page.get_by_text(re.compile(r"API Enabled|^Manage$", re.I)).first
    enable.or_(done).first.wait_for(timeout=60000)
    if enable.count():
        enable.first.click()
    done.wait_for(timeout=90000)


def do_consent(page, pid, email):
    page.goto(f"{CONSOLE}/auth/overview?project={pid}", wait_until="domcontentloaded")
    start = page.get_by_role("button", name=re.compile(r"Get started", re.I))
    other = page.get_by_role("button", name=re.compile(r"Create OAuth client", re.I))
    start.or_(other).first.wait_for(timeout=60000)
    if not start.count():
        return  # consent screen already configured
    start.first.click()
    page.get_by_label(re.compile(r"App name", re.I)).first.fill("Waybar Notch")
    page.get_by_role("combobox", name=re.compile(r"support email", re.I)).first.click()
    page.get_by_role("option").first.click()
    page.get_by_role("button", name=re.compile(r"^Next$")).click()
    page.get_by_role("radio", name=re.compile(r"External", re.I)).check()
    page.get_by_role("button", name=re.compile(r"^Next$")).click()
    page.get_by_label(re.compile(r"Email addresses", re.I)).first.fill(email)
    page.keyboard.press("Enter")
    page.get_by_role("button", name=re.compile(r"^Next$")).click()
    page.get_by_role("checkbox", name=re.compile(r"agree", re.I)).check()
    page.get_by_role("button", name=re.compile(r"^Continue$")).click()
    page.get_by_role("button", name=re.compile(r"^Create$")).click()
    start.first.wait_for(state="hidden", timeout=60000)


def do_audience(page, pid, email):
    page.goto(f"{CONSOLE}/auth/audience?project={pid}", wait_until="domcontentloaded")
    page.get_by_text(re.compile(r"Test users", re.I)).first.wait_for(timeout=60000)
    if not page.get_by_text(email).count():
        page.get_by_role("button", name=re.compile(r"Add users", re.I)).first.click()
        box = page.get_by_role("textbox").last
        box.fill(email)
        page.keyboard.press("Enter")
        page.get_by_role("button", name=re.compile(r"^Save$")).click()
    page.get_by_text(email).first.wait_for(timeout=30000)


def try_publish(page):
    """Best effort: apps left in Testing get refresh tokens that expire after 7 days."""
    try:
        pub = page.get_by_role("button", name=re.compile(r"Publish app", re.I)).first
        if pub.is_enabled(timeout=3000):
            pub.click()
            page.get_by_role("button", name=re.compile(r"^Confirm$", re.I)).click(timeout=5000)
            page.get_by_text(re.compile(r"In production", re.I)).first.wait_for(timeout=15000)
            print("App published (sign-ins do not expire after 7 days).")
            return
    except Exception:
        pass
    print('Google limits unpublished apps to 7-day sign-ins: when the notch shows '
          '"Connect Google Calendar" again, press it (one click).')


def do_client(page, pid):
    page.goto(f"{CONSOLE}/auth/clients/create?project={pid}", wait_until="domcontentloaded")
    page.get_by_role("combobox", name=re.compile(r"Application type", re.I)).first.click()
    page.get_by_role("option", name=re.compile(r"Desktop app", re.I)).click()
    page.get_by_label(re.compile(r"^Name", re.I)).first.fill("waybar-notch")
    page.get_by_role("button", name=re.compile(r"^Create$")).click()
    page.get_by_text(CLIENT_SECRET_RE).first.wait_for(timeout=60000)
    text = page.inner_text("body")
    cid, sec = CLIENT_ID_RE.search(text), CLIENT_SECRET_RE.search(text)
    if not (cid and sec):
        raise RuntimeError("client id/secret not found in the dialog")
    return cid.group(0), sec.group(0)


def client_fallback():
    while True:
        cid = prompt("Paste the Client ID (ends with .apps.googleusercontent.com): ")
        sec = prompt("Paste the Client secret (starts with GOCSPX-): ")
        if CLIENT_ID_RE.fullmatch(cid) and sec:
            return cid, sec
        print("The client ID does not look right, try again.")


def write_google_json(cid, sec):
    os.makedirs(CONF, exist_ok=True)
    with open(GOOGLE_JSON, "w") as f:
        json.dump({"client_id": cid, "client_secret": sec}, f)
    os.chmod(GOOGLE_JSON, 0o600)


def account_email(page):
    """The signed-in account, read from the console's account button; asked if it cannot be found."""
    try:
        label = page.locator('a[aria-label^="Google Account"]').first.get_attribute("aria-label", timeout=10000)
        m = EMAIL_RE.search(label or "")
        if m:
            return m.group(0)
    except Exception:
        pass
    return prompt("Your Google account email: ")


def do_login(ctx):
    if not os.path.exists(NOTCH):
        raise RuntimeError(f"{NOTCH} not found: run ./install.sh first")
    proc = subprocess.Popen([NOTCH, "--google-login", "--print-url"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    line = proc.stdout.readline().strip()
    if not line.startswith("AUTH_URL "):
        proc.wait()
        raise RuntimeError(proc.stderr.read().strip() or "waybar-notch --google-login failed")
    page = ctx.new_page()
    page.goto(line[len("AUTH_URL "):], wait_until="domcontentloaded")
    print("In the browser: choose your account. If Google says 'hasn't verified this app', click "
          "Advanced > Go to Waybar Notch (unsafe) > Continue, then Allow. (The app is your own.)")
    try:
        rc = proc.wait(timeout=185)
    except subprocess.TimeoutExpired:
        proc.kill()
        raise RuntimeError("sign-in timed out")
    if rc != 0:
        raise RuntimeError(proc.stderr.read().strip() or f"login exited with {rc}")


def main():
    browser = find_browser()
    os.makedirs(PROFILE, exist_ok=True)
    port = free_port()
    proc = subprocess.Popen([browser, f"--user-data-dir={PROFILE}", f"--remote-debugging-port={port}",
                             "--no-first-run", "--no-default-browser-check", f"{CONSOLE}/"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_for_login(port, proc)
        with sync_playwright() as pw:
            br = pw.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
            ctx = br.contexts[0]
            page = next((p for p in ctx.pages if p.url.startswith(CONSOLE)), None) or ctx.new_page()
            page.set_default_timeout(30000)
            email = account_email(page)

            pid = step("1/6 Google Cloud project", lambda: do_project(page),
                       f"Open {CONSOLE}/projectcreate, create a project named 'waybar-notch', then copy its Project ID.",
                       project_fallback)
            os.makedirs(CONF, exist_ok=True)
            with open(PROJECT_FILE, "w") as f:
                f.write(pid + "\n")

            step("2/6 Enable the Google Calendar API", lambda: do_api(page, pid),
                 f"Open {CONSOLE}/apis/library/calendar-json.googleapis.com?project={pid} and click Enable.")
            step("3/6 OAuth consent screen", lambda: do_consent(page, pid, email),
                 f"Open {CONSOLE}/auth/overview?project={pid} > Get started: App name 'Waybar Notch', "
                 f"support email {email}, Audience 'External', contact email {email}, accept the policy, Create.")

            def audience():
                do_audience(page, pid, email)
                try_publish(page)
            step("4/6 Test user", audience,
                 f"Open {CONSOLE}/auth/audience?project={pid} > Test users > Add users: enter {email}, Save.")

            creds = step("5/6 Desktop OAuth client", lambda: do_client(page, pid),
                         f"Open {CONSOLE}/auth/clients/create?project={pid}: Application type 'Desktop app', "
                         "name 'waybar-notch', Create; copy the Client ID and the Client secret shown.",
                         client_fallback)
            write_google_json(*creds)
            print(f"Wrote {GOOGLE_JSON}")

            step("6/6 Notch sign-in", lambda: do_login(ctx),
                 "Run: ~/.local/bin/waybar-notch --google-login   (a browser tab opens; choose your account and click Allow)")
        if not os.path.exists(TOKEN):
            die("the Google login did not finish (no token saved)")
        print("Google Calendar connected")
    except KeyboardInterrupt:
        die("interrupted")
    finally:
        proc.terminate()


if __name__ == "__main__":
    main()
