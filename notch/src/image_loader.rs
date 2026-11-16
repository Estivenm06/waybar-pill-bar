//! Bounded, worker-side decoding of untrusted local image files.
//!
//! `load_file` never runs on the GTK thread: it opens without following links or blocking, refuses anything that
//! is not a regular file (a FIFO never reaches a decoder), caps the encoded size, rejects huge dimensions as soon as
//! the loader reports them and returns only validated RGB/RGBA pixels. `texture` then wraps those pixels on GTK
//! without reopening a path or copying them.

use gtk4::gdk_pixbuf::prelude::*;
use gtk4::gdk_pixbuf::{Colorspace, InterpType, PixbufLoader};
use gtk4::{gdk, glib};
use std::cell::Cell;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Largest accepted source image, in pixels (a 4096x4096 picture).
pub const MAX_SOURCE_PIXELS: i64 = 16_777_216;
/// Largest decoded pixel buffer that is handed out.
const MAX_PIXEL_BYTES: usize = 64 * 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Validated pixels; `bytes` is shared, immutable storage with `rowstride` bytes per row.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub width: i32,
    pub height: i32,
    pub rowstride: usize,
    pub alpha: bool,
    pub bytes: glib::Bytes,
}

/// Absolute paths and local `file:` URIs (empty or `localhost` authority) only.
pub fn local_path(value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.contains('\0') { return None; }
    if value.starts_with('/') { return Some(PathBuf::from(value)); }
    let (scheme, _) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") { return None; }
    let (path, host) = glib::filename_from_uri(value).ok()?;
    if host.is_some_and(|h| !h.is_empty() && !h.eq_ignore_ascii_case("localhost")) { return None; }
    use std::os::unix::ffi::OsStrExt;
    (path.is_absolute() && !path.as_os_str().as_bytes().contains(&0)).then_some(path)
}

fn target_size(w: i32, h: i32, max_edge: i32) -> (i32, i32) {
    let m = w.max(h);
    if m <= max_edge { return (w, h); }
    let scale = |v: i32| ((v as i64 * max_edge as i64 / m as i64).max(1)) as i32;
    (scale(w), scale(h))
}

/// Decodes `path` to at most `max_edge` pixels on its longest side. Errors are fixed strings (no paths, no content).
pub fn load_file(path: &Path, max_edge: i32) -> Result<DecodedImage, String> {
    if max_edge <= 0 { return Err("invalid target size".into()); }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "cannot open image".to_string())?;
    let md = file.metadata().map_err(|_| "cannot inspect image".to_string())?;
    if !md.is_file() { return Err("not a regular file".into()); }
    if md.len() == 0 { return Err("empty image".into()); }
    if md.len() > crate::art_cache::MAX_ART_BYTES { return Err("image file too large".into()); }

    let loader = PixbufLoader::new();
    let rejected = Rc::new(Cell::new(false));
    let dims = Rc::new(Cell::new((0i32, 0i32)));
    {
        let (rejected, dims) = (rejected.clone(), dims.clone());
        loader.connect_size_prepared(move |l, w, h| {
            let pixels = (w as i64).checked_mul(h as i64);
            if w <= 0 || h <= 0 || pixels.is_none_or(|p| p > MAX_SOURCE_PIXELS) {
                rejected.set(true);
                l.set_size(1, 1);
                return;
            }
            dims.set((w, h));
            let (tw, th) = target_size(w, h, max_edge);
            if (tw, th) != (w, h) { l.set_size(tw, th); }
        });
    }
    let fail = |loader: &PixbufLoader, msg: &str| -> Result<DecodedImage, String> {
        let _ = loader.close();
        Err(msg.to_string())
    };

    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return fail(&loader, "cannot read image"),
        };
        total += n as u64;
        if total > crate::art_cache::MAX_ART_BYTES { return fail(&loader, "image file too large"); }
        let wrote = loader.write(&buf[..n]);
        if rejected.get() { return fail(&loader, "image dimensions too large"); }
        if wrote.is_err() { return fail(&loader, "malformed image"); }
    }
    if loader.close().is_err() { return Err("malformed image".into()); }
    if rejected.get() { return Err("image dimensions too large".into()); }
    let mut pb = loader.pixbuf().ok_or_else(|| "no image decoded".to_string())?;
    if pb.width() > max_edge || pb.height() > max_edge {
        let (tw, th) = target_size(pb.width(), pb.height(), max_edge);
        pb = pb.scale_simple(tw, th, InterpType::Bilinear).ok_or_else(|| "cannot scale image".to_string())?;
    }

    let (w, h, alpha, channels) = (pb.width(), pb.height(), pb.has_alpha(), pb.n_channels());
    if pb.colorspace() != Colorspace::Rgb || pb.bits_per_sample() != 8 || channels != if alpha { 4 } else { 3 } {
        return Err("unsupported pixel format".into());
    }
    if w <= 0 || h <= 0 || w > max_edge || h > max_edge { return Err("invalid image size".into()); }
    let rowstride = usize::try_from(pb.rowstride()).map_err(|_| "invalid rowstride".to_string())?;
    let (uw, uh, ch) = (w as usize, h as usize, channels as usize);
    let total_bytes = rowstride.checked_mul(uh).filter(|&t| t <= MAX_PIXEL_BYTES).ok_or_else(|| "image too large".to_string())?;
    let min_bytes = (uh - 1) * rowstride + uw * ch;
    if rowstride < uw * ch || total_bytes < min_bytes { return Err("invalid rowstride".into()); }
    let bytes = pb.read_pixel_bytes();
    if bytes.len() < min_bytes { return Err("short pixel buffer".into()); }
    Ok(DecodedImage { width: w, height: h, rowstride, alpha, bytes })
}

/// Wraps already validated pixels; cheap and safe on the GTK thread.
pub fn texture(image: DecodedImage) -> gdk::MemoryTexture {
    let format = if image.alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    gdk::MemoryTexture::new(image.width, image.height, format, &image.bytes, image.rowstride)
}

/// A real 1x1 RGBA PNG for tests.
#[cfg(test)]
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
    0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63,
    0xf8, 0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("image-loader-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tiny_png_decodes_with_alpha() {
        let d = dir("tiny");
        let p = d.join("a.png");
        std::fs::write(&p, TINY_PNG).unwrap();
        let img = load_file(&p, 384).unwrap();
        assert_eq!((img.width, img.height, img.alpha), (1, 1, true));
        assert!(img.rowstride >= 4 && img.bytes.len() >= 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn large_picture_is_scaled_to_the_edge_limit() {
        let d = dir("scale");
        let p = d.join("big.png");
        let pb = gtk4::gdk_pixbuf::Pixbuf::new(Colorspace::Rgb, false, 8, 600, 300).unwrap();
        pb.fill(0xff0000ff);
        pb.savev(&p, "png", &[]).unwrap();
        let img = load_file(&p, 384).unwrap();
        assert_eq!((img.width, img.height, img.alpha), (384, 192, false));
        assert!(img.rowstride >= 384 * 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn fifo_without_writer_is_rejected_promptly() {
        let d = dir("fifo");
        let p = d.join("pipe");
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert_eq!(load_file(&p, 384).unwrap_err(), "not a regular file");
        assert!(start.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn symlinks_and_directories_are_rejected() {
        let d = dir("links");
        let real = d.join("real.png");
        std::fs::write(&real, TINY_PNG).unwrap();
        let link = d.join("link.png");
        symlink(&real, &link).unwrap();
        assert!(load_file(&link, 384).is_err());
        assert_eq!(load_file(&d, 384).unwrap_err(), "not a regular file");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_one_byte_over_the_cap_is_rejected_before_decoding() {
        let d = dir("cap");
        let p = d.join("big.png");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(crate::art_cache::MAX_ART_BYTES + 1).unwrap();
        assert_eq!(load_file(&p, 384).unwrap_err(), "image file too large");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn oversized_dimensions_are_rejected() {
        let d = dir("dims");
        let p = d.join("huge.png");
        // A genuine PNG just over the pixel budget (4100 x 4100 > 16,777,216), encoded by gdk-pixbuf.
        let pb = gtk4::gdk_pixbuf::Pixbuf::new(Colorspace::Rgb, false, 8, 4100, 4100).unwrap();
        pb.fill(0);
        std::fs::write(&p, pb.save_to_bufferv("png", &[]).unwrap()).unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() < crate::art_cache::MAX_ART_BYTES);
        assert_eq!(load_file(&p, 384).unwrap_err(), "image dimensions too large");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn malformed_and_empty_files_are_rejected() {
        let d = dir("bad");
        let p = d.join("bad.png");
        std::fs::write(&p, b"definitely not an image").unwrap();
        assert!(load_file(&p, 384).is_err());
        std::fs::write(&p, b"").unwrap();
        assert_eq!(load_file(&p, 384).unwrap_err(), "empty image");
        assert!(load_file(&d.join("missing.png"), 384).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn local_path_accepts_only_local_files() {
        assert_eq!(local_path("/tmp/a b.png"), Some(PathBuf::from("/tmp/a b.png")));
        assert_eq!(local_path("file:///tmp/a%20b.png"), Some(PathBuf::from("/tmp/a b.png")));
        assert_eq!(local_path("file://localhost/tmp/x.png"), Some(PathBuf::from("/tmp/x.png")));
        assert_eq!(local_path("FILE:///tmp/x.png"), Some(PathBuf::from("/tmp/x.png")));
        for bad in ["", "relative.png", "./x.png", "file://example.com/x.png", "http://example.com/x.png", "ftp:///x", "file:relative", "/tmp/a\0b", "file:///tmp/a%00b"] {
            assert_eq!(local_path(bad), None, "{bad:?}");
        }
    }
}
