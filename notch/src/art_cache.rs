//! Bounded cache of downloaded album covers (`notch-art-<hex>` in the runtime dir, which is RAM-backed).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Generated covers kept besides the pinned ones.
pub const KEEP: usize = 8;

/// Largest artwork file that is downloaded or kept (bounds runtime-directory usage).
pub const MAX_ART_BYTES: u64 = 8 * 1024 * 1024;

pub fn path_for(dir: &Path, url: &str) -> PathBuf {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    dir.join(format!("notch-art-{:x}", h.finish()))
}

fn is_generated(name: &str) -> bool {
    name.strip_prefix("notch-art-").is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Keep at most `keep` generated covers: every pinned path first, then the newest others.
/// Only our own regular files with generated names are touched (symlinks and strangers are left alone).
pub fn prune(dir: &Path, keep: usize, pins: &[PathBuf]) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let Ok(dmd) = std::fs::metadata(dir) else { return };
    let me = dmd.uid(); // the runtime dir is ours; only files owned by the same user are touched
    let mut others: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut pinned = 0usize;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_generated(name) { continue; }
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_file() { continue; }
        let path = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.uid() != me { continue; }
        if md.len() > MAX_ART_BYTES {
            remove(&path); // an oversized generated file is never worth keeping, pinned or not
            continue;
        }
        if pins.contains(&path) { pinned += 1; continue; }
        others.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), path));
    }
    others.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in others.into_iter().skip(keep.saturating_sub(pinned)) { remove(&p); }
}

fn remove(p: &Path) {
    match std::fs::remove_file(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("art cache: remove {}: {e}", p.display()),
    }
}

/// A cached cover is reusable only when it is our own regular file (never a symlink) of a sane size.
pub fn is_usable(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file() && m.uid() == unsafe { libc::geteuid() } && (1..=MAX_ART_BYTES).contains(&m.len()))
        .unwrap_or(false)
}

/// Copies at most `MAX_ART_BYTES`; a chunk that would cross the limit is not written and is an error.
pub(super) fn copy_limited(reader: &mut impl std::io::Read, writer: &mut impl std::io::Write) -> std::io::Result<u64> {
    let mut buf = [0u8; 16 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let room = (MAX_ART_BYTES - total) as usize;
        if n > room {
            // nothing of the crossing chunk is written
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "artwork exceeds the size limit"));
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("art-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(p: &Path, age_secs: u64) {
        std::fs::write(p, b"x").unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn keeps_pins_and_newest_and_ignores_strangers() {
        let d = scratch("prune");
        for i in 0..12u64 { touch(&d.join(format!("notch-art-{i:x}")), 100 + i * 10); } // 0 newest .. b oldest
        touch(&d.join("unrelated.png"), 1000);
        touch(&d.join("notch-art-zz"), 1000);
        std::os::unix::fs::symlink("/etc/hostname", d.join("notch-art-ff")).unwrap();
        let oldest = d.join("notch-art-b");
        prune(&d, KEEP, &[oldest.clone()]);
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        left.sort();
        assert!(oldest.exists(), "pinned cover survives");
        let generated = left.iter().filter(|n| is_generated(n) && **n != "notch-art-ff").count();
        assert_eq!(generated, KEEP);
        for n in ["notch-art-0", "notch-art-6"] { assert!(left.contains(&n.to_string()), "{n} newest kept"); }
        assert!(!left.contains(&"notch-art-7".to_string()));
        assert!(left.contains(&"unrelated.png".to_string()) && left.contains(&"notch-art-zz".to_string()) && left.contains(&"notch-art-ff".to_string()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Reader that yields `n` bytes in odd-sized chunks.
    struct Fill(u64);
    impl std::io::Read for Fill {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(7919).min(self.0 as usize);
            buf[..n].fill(7);
            self.0 -= n as u64;
            Ok(n)
        }
    }

    #[test]
    fn copy_limited_accepts_exactly_the_limit_and_never_writes_past_it() {
        let mut out = Vec::new();
        assert_eq!(copy_limited(&mut Fill(MAX_ART_BYTES), &mut out).unwrap(), MAX_ART_BYTES);
        assert_eq!(out.len() as u64, MAX_ART_BYTES);
        let mut out = Vec::new();
        let err = copy_limited(&mut Fill(MAX_ART_BYTES + 1), &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(out.len() as u64 <= MAX_ART_BYTES);
    }

    #[test]
    fn prune_drops_oversized_generated_files_even_when_pinned_but_keeps_strangers() {
        let d = scratch("oversize");
        let big = d.join("notch-art-a");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_ART_BYTES + 1).unwrap();
        let ok = d.join("notch-art-b");
        std::fs::write(&ok, b"x").unwrap();
        let stranger = d.join("other-big");
        std::fs::File::create(&stranger).unwrap().set_len(MAX_ART_BYTES + 1).unwrap();
        let link = d.join("notch-art-c");
        std::os::unix::fs::symlink(&stranger, &link).unwrap();
        prune(&d, KEEP, &[big.clone(), ok.clone()]);
        assert!(!big.exists());
        assert!(ok.exists() && stranger.exists());
        assert!(std::fs::symlink_metadata(&link).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn usable_cache_entries_are_bounded_regular_files() {
        let d = scratch("usable");
        let p = d.join("notch-art-1");
        assert!(!is_usable(&p));
        std::fs::write(&p, b"").unwrap();
        assert!(!is_usable(&p));
        std::fs::write(&p, b"x").unwrap();
        assert!(is_usable(&p));
        let l = d.join("notch-art-2");
        std::os::unix::fs::symlink(&p, &l).unwrap();
        assert!(!is_usable(&l));
        std::fs::File::create(d.join("notch-art-3")).unwrap().set_len(MAX_ART_BYTES + 1).unwrap();
        assert!(!is_usable(&d.join("notch-art-3")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
