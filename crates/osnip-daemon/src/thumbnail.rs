//! Cached PNG thumbnails for out-of-process viewers.
//!
//! The Omarchy bar plugin runs inside `omarchy-shell`, a separate
//! process with no access to the daemon's pixel buffers. Handing it a
//! file path is the cheapest way to show a real preview: Quickshell's
//! `Image` loads a PNG off disk directly, so the pixels never cross the
//! IPC socket.
//!
//! Files live under `$XDG_RUNTIME_DIR/osnip/thumbs/<id>.png` (see
//! [`osnip_core::default_thumbnail_dir`]) and are the daemon's to
//! manage: written on pin creation, rewritten after every transform,
//! unlinked on close, and wiped wholesale at startup.
//!
//! Encoding is synchronous. At [`MAX_EDGE`] the downscale-plus-encode
//! costs on the order of a millisecond, which is not worth the
//! complexity of an async pipeline — and doing it inline means the
//! thumbnail is on disk *before* the `pins_changed` event that
//! advertises it, so a subscriber never races to an absent file.

use anyhow::{Context, Result};
use image::RgbaImage;
use osnip_core::PinId;
use std::path::{Path, PathBuf};

/// Longest edge of a generated thumbnail, in pixels.
///
/// Large enough to stay legible in a panel grid on a HiDPI display,
/// small enough that encoding is imperceptible.
pub const MAX_EDGE: u32 = 256;

/// Path a pin's thumbnail occupies, whether or not it exists yet.
#[must_use]
pub fn path(dir: &Path, id: PinId) -> PathBuf {
    dir.join(format!("{id}.png"))
}

/// Downscale `image` and write it as a PNG for `id`.
///
/// The write goes to a temporary sibling and is then renamed, so a
/// reader watching the path either sees the previous thumbnail or the
/// new one — never a half-encoded file. Rename is atomic because both
/// paths are in the same directory, hence the same filesystem.
pub fn write(dir: &Path, id: PinId, image: &RgbaImage) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create thumbnail dir {}", dir.display()))?;

    let (w, h) = scaled_size(image.width(), image.height());
    let small = image::imageops::thumbnail(image, w, h);

    let final_path = path(dir, id);
    let tmp_path = dir.join(format!(".{id}.png.tmp"));
    small
        .save_with_format(&tmp_path, image::ImageFormat::Png)
        .with_context(|| format!("encode thumbnail {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), final_path.display()))?;
    Ok(final_path)
}

/// Delete a pin's thumbnail. A missing file is success, not an error —
/// the caller is asserting the thumbnail is gone, not that it removed it.
pub fn remove(dir: &Path, id: PinId) {
    let p = path(dir, id);
    match std::fs::remove_file(&p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %p.display(), error = %e, "could not remove thumbnail"),
    }
}

/// Remove every thumbnail in `dir`.
///
/// Called at daemon startup: pin ids restart from 1 each run, so stale
/// files from a previous session would otherwise be served as if they
/// belonged to this one's pins.
pub fn clear(dir: &Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "could not read thumbnail dir");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "png") || is_temp_file(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn is_temp_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n.ends_with(".tmp"))
}

/// Fit `(width, height)` inside a [`MAX_EDGE`] square, preserving
/// aspect ratio. Images already within the box are left alone rather
/// than upscaled. Never returns a zero dimension: a 1x4000 capture
/// still has to produce a decodable PNG.
fn scaled_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_EDGE || longest == 0 {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(MAX_EDGE) / f64::from(longest);
    let w = (f64::from(width) * scale).round() as u32;
    let h = (f64::from(height) * scale).round() as u32;
    (w.max(1), h.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]))
    }

    #[test]
    fn scaled_size_fits_the_box_and_keeps_aspect() {
        assert_eq!(scaled_size(1024, 512), (256, 128));
        assert_eq!(scaled_size(512, 1024), (128, 256));
        assert_eq!(scaled_size(256, 256), (256, 256));
    }

    #[test]
    fn scaled_size_does_not_upscale() {
        assert_eq!(scaled_size(40, 30), (40, 30));
    }

    #[test]
    fn scaled_size_never_collapses_an_extreme_aspect_to_zero() {
        let (w, h) = scaled_size(4000, 1);
        assert_eq!(w, 256);
        assert!(h >= 1, "height rounded to {h}");
    }

    #[test]
    fn write_produces_a_decodable_png_within_the_box() {
        let dir = tempdir().expect("tempdir");
        let p = write(dir.path(), PinId::new(7), &fixture(1000, 500)).expect("write");
        assert_eq!(p, dir.path().join("7.png"));
        let decoded = image::open(&p).expect("decode").to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (256, 128));
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), PinId::new(1), &fixture(300, 300)).expect("write");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .flatten()
            .filter(|e| is_temp_file(&e.path()))
            .collect();
        assert!(leftovers.is_empty(), "temp file survived the rename");
    }

    #[test]
    fn write_overwrites_an_existing_thumbnail() {
        let dir = tempdir().expect("tempdir");
        let id = PinId::new(2);
        write(dir.path(), id, &fixture(1000, 500)).expect("first");
        let p = write(dir.path(), id, &fixture(500, 1000)).expect("second");
        let decoded = image::open(&p).expect("decode").to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (128, 256));
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempdir().expect("tempdir");
        let id = PinId::new(3);
        write(dir.path(), id, &fixture(10, 10)).expect("write");
        remove(dir.path(), id);
        assert!(!path(dir.path(), id).exists());
        remove(dir.path(), id);
    }

    #[test]
    fn clear_empties_the_directory() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), PinId::new(1), &fixture(10, 10)).expect("write");
        write(dir.path(), PinId::new(2), &fixture(10, 10)).expect("write");
        clear(dir.path());
        assert!(!path(dir.path(), PinId::new(1)).exists());
        assert!(!path(dir.path(), PinId::new(2)).exists());
    }

    #[test]
    fn clear_on_a_missing_directory_is_not_an_error() {
        let dir = tempdir().expect("tempdir");
        clear(&dir.path().join("does-not-exist"));
    }
}
