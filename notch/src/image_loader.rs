//! Bounded, worker-side decoding of untrusted local image files.
//!
//! `load_file` never runs on the UI thread: it opens without following links or blocking, refuses anything that
//! is not a regular file (a FIFO never reaches a decoder), caps the encoded size, rejects huge dimensions from the
//! header before decoding and returns only validated straight-alpha RGBA pixels (`Send`). `DecodedImage::into_image`
//! then builds the premultiplied `render::Image` on the UI thread.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// Largest accepted source image, in pixels (a 4096x4096 picture).
pub const MAX_SOURCE_PIXELS: u64 = 16_777_216;

/// Validated straight-alpha RGBA8 pixels, `width * height * 4` bytes.
#[derive(Clone, Debug)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    /// Premultiplies into a drawable image (UI thread).
    pub fn into_image(self) -> Option<crate::render::Image> {
        crate::render::Image::from_rgba8(self.width, self.height, &self.rgba)
    }
}

/// Absolute paths and local `file:` URIs (empty or `localhost` authority) only.
pub fn local_path(value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.contains('\0') { return None; }
    if value.starts_with('/') { return Some(PathBuf::from(value)); }
    let (scheme, _) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("file") { return None; }
    if !value.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("file://")) { return None; }
    let url = url::Url::parse(value).ok()?;
    if url.host_str().is_some_and(|h| !h.is_empty() && !h.eq_ignore_ascii_case("localhost")) { return None; }
    if url.query().is_some() || url.fragment().is_some() { return None; }
    let path = url.to_file_path().ok()?;
    (path.is_absolute() && !path.as_os_str().as_bytes().contains(&0)).then_some(path)
}

fn target_size(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let m = w.max(h);
    if m <= max_edge { return (w, h); }
    let scale = |v: u32| ((v as u64 * max_edge as u64 / m as u64).max(1)) as u32;
    (scale(w), scale(h))
}

/// Decodes `path` to at most `max_edge` pixels on its longest side. Errors are fixed strings (no paths, no content).
pub fn load_file(path: &Path, max_edge: i32) -> Result<DecodedImage, String> {
    if max_edge <= 0 { return Err("invalid target size".into()); }
    let max_edge = max_edge as u32;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "cannot open image".to_string())?;
    let md = file.metadata().map_err(|_| "cannot inspect image".to_string())?;
    if !md.is_file() { return Err("not a regular file".into()); }
    if md.len() == 0 { return Err("empty image".into()); }
    if md.len() > crate::art_cache::MAX_ART_BYTES { return Err("image file too large".into()); }

    let mut data = Vec::with_capacity(md.len() as usize);
    file.take(crate::art_cache::MAX_ART_BYTES + 1).read_to_end(&mut data).map_err(|_| "cannot read image".to_string())?;
    if data.len() as u64 > crate::art_cache::MAX_ART_BYTES { return Err("image file too large".into()); }

    let reader = || image::ImageReader::new(std::io::Cursor::new(&data)).with_guessed_format().map_err(|_| "malformed image".to_string());
    let (w, h) = reader()?.into_dimensions().map_err(|_| "malformed image".to_string())?;
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_SOURCE_PIXELS { return Err("image dimensions too large".into()); }
    let mut dec = reader()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    dec.limits(limits);
    let img = dec.decode().map_err(|_| "malformed image".to_string())?;
    let (tw, th) = target_size(img.width(), img.height(), max_edge);
    let rgba = if (tw, th) != (img.width(), img.height()) {
        image::imageops::resize(&img.to_rgba8(), tw, th, image::imageops::FilterType::Triangle)
    } else {
        img.to_rgba8()
    };
    Ok(DecodedImage { width: tw, height: th, rgba: rgba.into_raw() })
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
        assert_eq!((img.width, img.height), (1, 1));
        assert_eq!(img.rgba.len(), 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn large_picture_is_scaled_to_the_edge_limit() {
        let d = dir("scale");
        let p = d.join("big.png");
        image::RgbImage::from_pixel(600, 300, image::Rgb([255, 0, 0])).save(&p).unwrap();
        let img = load_file(&p, 384).unwrap();
        assert_eq!((img.width, img.height), (384, 192));
        assert_eq!(img.rgba.len(), 384 * 192 * 4);
        assert_eq!(&img.rgba[..4], &[255, 0, 0, 255]);
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
        // A genuine PNG just over the pixel budget (4100 x 4100 > 16,777,216), encoded by the image crate.
        image::GrayImage::new(4100, 4100).save(&p).unwrap();
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
