//! Session-based reminders owned by Omarchy; no additional persistence.
use chrono::{DateTime, Local, TimeZone};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Reminder {
    pub unit: String,
    pub title: String,
    pub at: DateTime<Local>,
}

pub fn validate_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() || title.chars().count() > 200 || title.contains('\0') {
        Err("Enter a title of 1–200 characters".into())
    } else { Ok(()) }
}

fn sanitized(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().filter(|c| !c.is_control() || *c == '\n')
        .take(400).collect::<String>().trim().to_string()
}

fn run(args: &[&str]) -> Result<Vec<u8>, String> { run_program("omarchy", args) }

fn run_program(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut child = Command::new(program).args(args).stdin(Stdio::null())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn()
        .map_err(|e| format!("Cannot run {program}: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err("Cannot read Omarchy output".into());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_done = false;
        let mut err_done = false;
        loop {
            let mut buffer = [0u8; 8192];
            for (stream, dest, done) in [(&mut stdout as &mut dyn Read, &mut out, &mut out_done), (&mut stderr as &mut dyn Read, &mut err, &mut err_done)] {
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => { *done = true; break; }
                        Ok(n) => {
                            if dest.len() + n > MAX_BYTES { return Err("Omarchy output exceeded 1 MiB".into()); }
                            dest.extend_from_slice(&buffer[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(format!("Cannot read Omarchy output: {e}")),
                    }
                }
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                if out_done && err_done {
                    if status.success() { return Ok(out); }
                    let message = sanitized(&err);
                    let message = if message.is_empty() { sanitized(&out) } else { message };
                    return Err(if message.is_empty() { format!("Omarchy failed: {status}") } else { message });
                }
            }
            if Instant::now() >= deadline { return Err("Omarchy reminder command timed out".into()); }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() { unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); } }
    let _ = child.wait();
    result
}

fn parse(bytes: &[u8], now: DateTime<Local>) -> Result<Vec<Reminder>, String> {
    if bytes.len() > MAX_BYTES { return Err("Omarchy output exceeded 1 MiB".into()); }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "Invalid Omarchy reminder response")?;
    let rows = value.get("reminders").and_then(|v| v.as_array()).ok_or("Invalid Omarchy reminder list")?;
    let mut reminders = Vec::new();
    let mut units = std::collections::HashSet::new();
    for row in rows {
        let unit = row.get("unit").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).ok_or("Missing reminder identity")?;
        let title = row.get("label").and_then(|v| v.as_str()).ok_or("Missing reminder label")?;
        let at = row.get("at").and_then(|v| v.as_i64()).and_then(|v| Local.timestamp_opt(v, 0).single()).ok_or("Invalid reminder date")?;
        if at > now && units.insert(unit.to_string()) { reminders.push(Reminder { unit: unit.into(), title: title.into(), at }); }
    }
    reminders.sort_by(|a, b| a.at.cmp(&b.at).then(a.title.cmp(&b.title)));
    Ok(reminders)
}

pub fn list() -> Result<Vec<Reminder>, String> { parse(&run(&["reminder", "show", "--json"])?, Local::now()) }

fn create_with(minutes: u32, title: &str, now: DateTime<Local>, mut runner: impl FnMut(&[&str]) -> Result<Vec<u8>, String>) -> Result<Vec<Reminder>, String> {
    validate_title(title)?;
    if minutes == 0 { return Err("Delay must be a positive whole number of minutes".into()); }
    runner(&["reminder", &minutes.to_string(), title])?;
    parse(&runner(&["reminder", "show", "--json"])?, now)
}

pub fn create(minutes: u32, title: &str) -> Result<Vec<Reminder>, String> { create_with(minutes, title, Local::now(), run) }

/// Omarchy names every reminder's transient timer `omarchy-reminder-<minutes>m-<epoch>`; nothing else may reach systemctl.
pub fn valid_unit(unit: &str) -> bool {
    let Some(rest) = unit.strip_prefix("omarchy-reminder-") else { return false };
    let Some((minutes, stamp)) = rest.split_once("m-") else { return false };
    !minutes.is_empty() && !stamp.is_empty() && minutes.bytes().all(|b| b.is_ascii_digit()) && stamp.bytes().all(|b| b.is_ascii_digit())
}

/// Stop one reminder's timer and forget its message, the same two steps `omarchy reminder clear` takes for all of them.
fn delete_with(unit: &str, mut runner: impl FnMut(&str, &[&str]) -> Result<Vec<u8>, String>, runtime_dir: Option<&std::path::Path>) -> Result<(), String> {
    if !valid_unit(unit) { return Err("Not an Omarchy reminder".into()); }
    runner("systemctl", &["--user", "stop", &format!("{unit}.timer")])?;
    if let Some(dir) = runtime_dir { let _ = std::fs::remove_file(dir.join("omarchy-reminders").join(format!("{unit}.message"))); }
    let _ = runner("omarchy-shell", &["-q", "omarchy.indicators", "refresh"]);
    Ok(())
}

fn runtime_dir() -> Option<std::path::PathBuf> { std::env::var_os("XDG_RUNTIME_DIR").map(Into::into) }

pub fn delete(unit: &str) -> Result<Vec<Reminder>, String> {
    delete_with(unit, run_program, runtime_dir().as_deref())?;
    list()
}

/// Update = set the replacement first, then drop the old one, so a failed create never loses the original.
pub fn update(unit: &str, minutes: u32, title: &str) -> Result<Vec<Reminder>, String> {
    if !valid_unit(unit) { return Err("Not an Omarchy reminder".into()); }
    create(minutes, title)?;
    delete_with(unit, run_program, runtime_dir().as_deref()).map_err(|e| format!("New reminder set, but the old one could not be removed: {e}"))?;
    list()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn creation_preserves_unicode_and_shell_punctuation_as_one_argument() {
        let now = Local.timestamp_opt(1_700_000_000, 0).unwrap();
        let title = "Café; $(touch nope) 'mañana'";
        let mut calls = Vec::new();
        let rows = create_with(15, title, now, |args| {
            calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            Ok(if args[1] == "show" { r#"{"reminders":[{"unit":"timer-a","label":"Café","at":1700000900}]}"#.as_bytes().to_vec() } else { Vec::new() })
        });
        assert!(rows.is_ok());
        assert_eq!(calls, vec![vec!["reminder", "15", title], vec!["reminder", "show", "--json"]]);
    }
    #[test]
    fn expired_and_duplicate_units_are_removed_on_successful_list() {
        let now = Local.timestamp_opt(1_700_000_000, 0).unwrap();
        let rows = parse(br#"{"reminders":[{"unit":"old","label":"Old","at":1699999999},{"unit":"next","label":"Next","at":1700000300},{"unit":"next","label":"Next","at":1700000300}]}"#, now).unwrap();
        assert_eq!(rows.iter().map(|r| r.unit.as_str()).collect::<Vec<_>>(), ["next"]);
        assert_eq!(rows[0].at.date_naive(), now.date_naive());
    }
    #[test]
    fn only_omarchy_reminder_units_reach_systemctl() {
        assert!(valid_unit("omarchy-reminder-15m-1700000000"));
        for bad in ["", "omarchy-reminder-", "omarchy-reminder-m-1", "omarchy-reminder-15m-", "omarchy-reminder-15m-1.timer", "sshd", "omarchy-reminder-15m-1 --now", "omarchy-reminder-1x5m-1"] {
            assert!(!valid_unit(bad), "{bad}");
        }
        let mut calls = 0;
        assert!(delete_with("sshd", |_, _| { calls += 1; Ok(vec![]) }, None).is_err());
        assert_eq!(calls, 0);
    }
    #[test]
    fn delete_stops_the_timer_and_removes_only_its_message() {
        let dir = std::env::temp_dir().join(format!("rem-del-{}", std::process::id()));
        let msgs = dir.join("omarchy-reminders");
        std::fs::create_dir_all(&msgs).unwrap();
        std::fs::write(msgs.join("omarchy-reminder-5m-1.message"), "a").unwrap();
        std::fs::write(msgs.join("omarchy-reminder-5m-2.message"), "b").unwrap();
        let mut calls: Vec<Vec<String>> = Vec::new();
        delete_with("omarchy-reminder-5m-1", |p, a| { calls.push(std::iter::once(p).chain(a.iter().copied()).map(String::from).collect()); Ok(vec![]) }, Some(&dir)).unwrap();
        assert_eq!(calls[0], ["systemctl", "--user", "stop", "omarchy-reminder-5m-1.timer"]);
        assert!(!msgs.join("omarchy-reminder-5m-1.message").exists());
        assert!(msgs.join("omarchy-reminder-5m-2.message").exists());
        let failed = delete_with("omarchy-reminder-5m-2", |_, _| Err("denied".into()), Some(&dir));
        assert_eq!(failed.unwrap_err(), "denied");
        assert!(msgs.join("omarchy-reminder-5m-2.message").exists(), "a failed stop keeps the message");
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn failure_never_lists_or_claims_creation() {
        let mut count = 0;
        let title = "Keep this draft";
        let result = create_with(1, title, Local::now(), |_| { count += 1; Err("Permission denied".into()) });
        assert_eq!(result.unwrap_err(), "Permission denied");
        assert_eq!(count, 1);
        assert_eq!(title, "Keep this draft");
    }
}
