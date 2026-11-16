//! Trusted private files and directories (clipboard history, Google credentials/tokens).
//!
//! Every path is walked component by component with directory descriptors opened `O_NOFOLLOW`, so a symlink
//! anywhere in the chain is refused and a later path substitution cannot redirect an action. Ancestors must be
//! owned by root or by us and must not be writable by other accounts (a root-owned sticky directory such as
//! `/tmp` is allowed). The dedicated directory itself must be ours and not group/world-writable; it is only
//! tightened to 0700 after every check passed. Shared ancestors are never chmodded. Errors name the path
//! only, never file content.
use serde_json::Value;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum size of an OAuth JSON file.
pub const MAX_OAUTH_JSON: u64 = 64 * 1024;

fn euid() -> u32 { unsafe { libc::geteuid() } }

fn errno_err() -> std::io::Error { std::io::Error::last_os_error() }

fn fstat(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 { return Err(errno_err()); }
    Ok(st)
}

fn fmt(st: &libc::stat) -> u32 { st.st_mode as u32 & libc::S_IFMT as u32 }

fn open_at(dir: &OwnedFd, name: &CString, flags: libc::c_int) -> std::io::Result<OwnedFd> {
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC, 0) };
    if fd < 0 { Err(errno_err()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;

/// Shared ancestor: a real directory owned by root or us that other accounts cannot modify.
fn check_ancestor(st: &libc::stat, path: &Path) -> Result<(), String> {
    let mode = st.st_mode as u32;
    if fmt(st) != libc::S_IFDIR as u32 || (st.st_uid != 0 && st.st_uid != euid()) {
        return Err(format!("{} is not a trusted directory", path.display()));
    }
    let sticky_root = st.st_uid == 0 && mode & libc::S_ISVTX as u32 != 0;
    if mode & 0o022 != 0 && !sticky_root {
        return Err(format!("{} is writable by other accounts", path.display()));
    }
    Ok(())
}

/// The dedicated private directory: ours and not writable by group/world.
fn check_dedicated(st: &libc::stat, path: &Path) -> Result<(), String> {
    if fmt(st) != libc::S_IFDIR as u32 || st.st_uid != euid() {
        return Err(format!("{} is not a directory owned by you", path.display()));
    }
    if st.st_mode as u32 & 0o022 != 0 {
        return Err(format!("{} is writable by other accounts", path.display()));
    }
    Ok(())
}

/// Walks `dir` from `/`. Missing components are created only with `create` (0755 intermediates, 0700 leaf);
/// without it a missing component yields `Ok(None)`. The final directory is tightened to 0700 when `tighten`.
fn walk(dir: &Path, create: bool, tighten: bool) -> Result<Option<OwnedFd>, String> {
    if !dir.is_absolute() { return Err(format!("{} is not an absolute path", dir.display())); }
    let mut names = Vec::new();
    for c in dir.components() {
        match c {
            Component::Normal(n) => names.push(CString::new(n.as_bytes()).map_err(|_| format!("invalid path {}", dir.display()))?),
            Component::RootDir | Component::CurDir => {}
            _ => return Err(format!("{} is not a normalized path", dir.display())),
        }
    }
    let root = CString::new("/").unwrap();
    let mut cur = {
        let fd = unsafe { libc::open(root.as_ptr(), DIR_FLAGS | libc::O_CLOEXEC) };
        if fd < 0 { return Err(format!("cannot open /: {}", errno_err())); }
        unsafe { OwnedFd::from_raw_fd(fd) }
    };
    let mut so_far = std::path::PathBuf::from("/");
    let n = names.len();
    if n == 0 {
        check_ancestor(&fstat(&cur).map_err(|e| e.to_string())?, &so_far)?;
        return Ok(Some(cur));
    }
    for (i, name) in names.iter().enumerate() {
        let last = i + 1 == n;
        so_far.push(std::ffi::OsStr::from_bytes(name.as_bytes()));
        let fd = match open_at(&cur, name, DIR_FLAGS) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create { return Ok(None); }
                let mode = if last { 0o700 } else { 0o755 };
                if unsafe { libc::mkdirat(cur.as_raw_fd(), name.as_ptr(), mode) } != 0 {
                    let e = errno_err();
                    if e.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(format!("cannot create {}: {e}", so_far.display()));
                    }
                }
                open_at(&cur, name, DIR_FLAGS).map_err(|e| format!("cannot open {}: {e}", so_far.display()))?
            }
            Err(e) => return Err(format!("cannot open {}: {e}", so_far.display())),
        };
        let st = fstat(&fd).map_err(|e| format!("cannot inspect {}: {e}", so_far.display()))?;
        if last { check_dedicated(&st, &so_far)?; } else { check_ancestor(&st, &so_far)?; }
        if last && tighten && st.st_mode as u32 & 0o777 != 0o700 && unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
            return Err(format!("cannot restrict {}: {}", so_far.display(), errno_err()));
        }
        cur = fd;
    }
    Ok(Some(cur))
}

/// Makes sure the dedicated directory exists, is ours, is not a symlink and is 0700. Shared ancestors
/// (XDG_DATA_HOME, HOME, ...) are validated but never chmodded.
#[cfg_attr(not(test), allow(dead_code))]
pub fn secure_private_dir(dir: &Path) -> Result<(), String> {
    walk(dir, true, true).map(|_| ())
}

fn leaf_name(path: &Path) -> Result<CString, String> {
    let n = path.file_name().filter(|_| path.components().next_back().is_some_and(|c| matches!(c, Component::Normal(_))))
        .ok_or_else(|| format!("invalid path {}", path.display()))?;
    CString::new(n.as_bytes()).map_err(|_| format!("invalid path {}", path.display()))
}

fn parent_of(path: &Path) -> Result<&Path, String> {
    path.parent().ok_or_else(|| format!("invalid path {}", path.display()))
}

/// Reads a private file of at most `cap` bytes. Missing parent/file is `Ok(None)` and creates nothing.
/// Symlinks, special files, foreign owners and multiply linked files are refused; permissions are tightened
/// to 0600 only after those checks.
pub fn read_private_bytes(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, String> {
    let Some(dirfd) = walk(parent_of(path)?, false, true)? else { return Ok(None) };
    let name = leaf_name(path)?;
    let fd = match open_at(&dirfd, &name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK) {
        Ok(fd) => fd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot open {}: {e}", path.display())),
    };
    let st = fstat(&fd).map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if fmt(&st) != libc::S_IFREG as u32 || st.st_uid != euid() || st.st_nlink != 1 {
        return Err(format!("{} is not a regular file owned by you", path.display()));
    }
    if st.st_size as u64 > cap { return Err(format!("{} is too large", path.display())); }
    if st.st_mode as u32 & 0o777 != 0o600 && unsafe { libc::fchmod(fd.as_raw_fd(), 0o600) } != 0 {
        return Err(format!("cannot restrict {}: {}", path.display(), errno_err()));
    }
    let mut buf = Vec::new();
    std::fs::File::from(fd).take(cap + 1).read_to_end(&mut buf).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if buf.len() as u64 > cap { return Err(format!("{} is too large", path.display())); }
    Ok(Some(buf))
}

/// Reads OAuth-sized JSON (64 KiB cap). Absence is `Ok(None)`; unsafe or malformed files are path-only errors.
pub fn read_private_json(path: &Path) -> Result<Option<Value>, String> {
    let Some(bytes) = read_private_bytes(path, MAX_OAUTH_JSON)? else { return Ok(None) };
    serde_json::from_slice(&bytes).map(Some).map_err(|_| format!("{} is not valid JSON", path.display()))
}

/// Atomically publishes `data` at `path` as 0600 inside the validated private directory. The temporary file
/// is `<prefix>.<pid>.<seq>`, created exclusively relative to the directory descriptor; the old file survives
/// any failure before the final rename. An existing symlink/special/foreign-owned leaf is refused.
pub fn write_private_file(path: &Path, data: &[u8], prefix: &str, seq: &AtomicU64) -> Result<(), String> {
    let dirfd = walk(parent_of(path)?, true, true)?.ok_or_else(|| format!("cannot write {}: directory missing", path.display()))?;
    let name = leaf_name(path)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dirfd.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        if fmt(&st) != libc::S_IFREG as u32 || st.st_uid != euid() {
            return Err(format!("cannot write {}: not a regular file owned by you", path.display()));
        }
    } else {
        let e = errno_err();
        if e.kind() != std::io::ErrorKind::NotFound { return Err(format!("cannot inspect {}: {e}", path.display())); }
    }
    for _ in 0..16 {
        let tmp = CString::new(format!("{prefix}.{}.{}", std::process::id(), seq.fetch_add(1, Ordering::Relaxed))).unwrap();
        let raw = unsafe {
            libc::openat(dirfd.as_raw_fd(), tmp.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600 as libc::c_uint)
        };
        if raw < 0 {
            let e = errno_err();
            if e.kind() == std::io::ErrorKind::AlreadyExists { continue; }
            return Err(format!("cannot write {}: {e}", path.display()));
        }
        let mut f = unsafe { std::fs::File::from_raw_fd(raw) };
        let res = f.write_all(data).and_then(|_| f.flush()).and_then(|_| f.sync_all()).and_then(|_| {
            if unsafe { libc::renameat(dirfd.as_raw_fd(), tmp.as_ptr(), dirfd.as_raw_fd(), name.as_ptr()) } != 0 { Err(errno_err()) } else { Ok(()) }
        });
        if let Err(e) = res {
            unsafe { libc::unlinkat(dirfd.as_raw_fd(), tmp.as_ptr(), 0); }
            return Err(format!("cannot write {}: {e}", path.display()));
        }
        return Ok(());
    }
    Err(format!("cannot write {}: no unused temporary name", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn real_root_owned_paths_are_refused_without_being_touched() {
        if unsafe { libc::geteuid() } == 0 { return; }
        let dir = std::path::Path::new("/etc/ssl");
        let file = std::path::Path::new("/etc/hostname");
        let (Ok(dm), Ok(fm)) = (std::fs::metadata(dir), std::fs::metadata(file)) else { return };
        if dm.uid() != 0 || fm.uid() != 0 { return; }
        let before = (dm.mode(), fm.mode());
        assert!(secure_private_dir(dir).is_err(), "a root-owned dedicated directory must be refused");
        assert!(read_private_bytes(file, 1024).is_err(), "a root-owned file must be refused");
        assert_eq!((std::fs::metadata(dir).unwrap().mode(), std::fs::metadata(file).unwrap().mode()), before);
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pf-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn missing_parent_and_file_are_absent_and_create_nothing() {
        let d = tmpdir("absent");
        assert_eq!(read_private_json(&d.join("nope/x.json")), Ok(None));
        assert!(!d.join("nope").exists());
        std::fs::create_dir(d.join("dir")).unwrap();
        assert_eq!(read_private_json(&d.join("dir/x.json")), Ok(None));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn reads_json_and_tightens_loose_modes() {
        let d = tmpdir("read");
        let dir = d.join("cfg");
        std::fs::create_dir(&dir).unwrap();
        let f = dir.join("g.json");
        std::fs::write(&f, r#"{"a":1}"#).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_private_json(&f).unwrap().unwrap()["a"], 1);
        assert_eq!(std::fs::metadata(&f).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_symlink_fifo_directory_oversize_and_hardlink_without_echo() {
        let d = tmpdir("reject");
        let dir = d.join("cfg");
        std::fs::create_dir(&dir).unwrap();
        let victim = d.join("victim");
        std::fs::write(&victim, "SECRETPAYLOAD").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("link.json")).unwrap();
        let e = read_private_json(&dir.join("link.json")).unwrap_err();
        assert!(!e.contains("SECRETPAYLOAD"), "{e}");
        let fifo = std::ffi::CString::new(dir.join("fifo.json").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_private_json(&dir.join("fifo.json")).is_err());
        std::fs::create_dir(dir.join("sub.json")).unwrap();
        assert!(read_private_json(&dir.join("sub.json")).is_err());
        std::fs::write(dir.join("big.json"), vec![b' '; MAX_OAUTH_JSON as usize + 1]).unwrap();
        assert!(read_private_json(&dir.join("big.json")).is_err());
        std::fs::write(dir.join("bad.json"), "SECRETPAYLOAD not json").unwrap();
        let e = read_private_json(&dir.join("bad.json")).unwrap_err();
        assert!(!e.contains("SECRETPAYLOAD"), "{e}");
        std::fs::write(dir.join("one.json"), "{}").unwrap();
        std::fs::hard_link(dir.join("one.json"), dir.join("two.json")).unwrap();
        assert!(read_private_json(&dir.join("one.json")).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SECRETPAYLOAD");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn symlinked_and_loose_directories_are_refused_unchanged() {
        let d = tmpdir("dirs");
        let real = d.join("real");
        std::fs::create_dir(&real).unwrap();
        let linked = d.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();
        assert!(secure_private_dir(&linked).is_err());
        assert!(write_private_file(&linked.join("t.json"), b"x", ".t", &SEQ).is_err());
        assert!(read_private_json(&linked.join("t.json")).is_err());
        assert!(std::fs::read_dir(&real).unwrap().next().is_none());
        let loose = d.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(secure_private_dir(&loose).is_err());
        assert_eq!(std::fs::metadata(&loose).unwrap().mode() & 0o777, 0o770);
        // a group-writable shared ancestor is refused and not chmodded
        let shared = d.join("shared");
        std::fs::create_dir_all(shared.join("inner")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(secure_private_dir(&shared.join("inner")).is_err());
        assert_eq!(std::fs::metadata(&shared).unwrap().mode() & 0o777, 0o775);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_creates_private_dir_and_file_and_refuses_bad_leaves() {
        let d = tmpdir("write");
        let f = d.join("a/b/t.json");
        write_private_file(&f, b"{}", ".t.tmp", &SEQ).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(d.join("a/b")).unwrap().mode() & 0o777, 0o700);
        let victim = d.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let l = d.join("a/b/l.json");
        std::os::unix::fs::symlink(&victim, &l).unwrap();
        assert!(write_private_file(&l, b"new", ".t.tmp", &SEQ).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        std::fs::create_dir(d.join("a/b/dir.json")).unwrap();
        assert!(write_private_file(&d.join("a/b/dir.json"), b"new", ".t.tmp", &SEQ).is_err());
        let names: Vec<_> = std::fs::read_dir(d.join("a/b")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        assert!(names.iter().all(|n| !n.starts_with(".t.tmp")), "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
