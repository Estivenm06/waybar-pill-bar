//! Bounded cache of downloaded album covers (`notch-art-<hex>` in the runtime dir, which is RAM-backed).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Generated covers kept besides the pinned ones.
pub const KEEP: usize = 8;

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
        if pins.contains(&path) { pinned += 1; continue; }
        others.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), path));
    }
    others.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in others.into_iter().skip(keep.saturating_sub(pinned)) {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("art cache: remove {}: {e}", p.display()),
        }
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
}
