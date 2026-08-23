//! Clipboard image ingestion via the external `wl-paste` binary.
//!
//! Mirrors the `slurp` pattern used in [`crate::region_select`]: shell
//! out, parse output, surface user-facing errors as in-band
//! [`IpcError`] variants.
//!
//! The clipboard might advertise multiple image MIME types
//! simultaneously (a PNG screenshot, plus a JPEG fallback, plus
//! `image/bmp` from a different source). We prefer **PNG** because it
//! is lossless and the most common screenshot format on Wayland; we
//! fall back to whatever `image/*` type the clipboard offers, then
//! decode through the `image` crate which sniffs the format from the
//! byte stream.

use image::RgbaImage;
use osnip_core::IpcError;
use std::process::Stdio;
use std::sync::Arc;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Preferred MIME types in order. The first one the clipboard
/// advertises is the one we read.
const PREFERRED_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/bmp",
    "image/webp",
    "image/tiff",
];

#[derive(Debug, Error)]
pub enum ClipboardError {
    /// `wl-paste` not on `PATH` or otherwise failed to launch.
    #[error("failed to launch wl-paste: {0}")]
    Launch(#[source] std::io::Error),

    /// `wl-paste --list-types` exited non-zero — typically "Nothing is
    /// copied" / empty clipboard.
    #[error("clipboard is empty or unreadable")]
    Empty,

    /// Clipboard had content but no `image/*` MIME type was offered.
    #[error("clipboard does not contain an image")]
    NoImage,

    /// We picked a MIME type but `wl-paste -t <type>` failed to read it.
    #[error("wl-paste failed to read {mime}: {message}")]
    PasteFailed { mime: String, message: String },

    /// The bytes came back but the `image` crate could not decode them.
    #[error("could not decode clipboard image (mime={mime}): {source}")]
    Decode {
        mime: String,
        #[source]
        source: image::ImageError,
    },

    /// PNG encoding (write path) failed before we could hand bytes to
    /// `wl-copy`.
    #[error("could not encode image to PNG: {0}")]
    Encode(#[source] image::ImageError),

    /// `wl-copy` accepted bytes but exited non-zero, or its stdin pipe
    /// closed early.
    #[error("wl-copy failed: {0}")]
    CopyFailed(String),
}

impl From<ClipboardError> for IpcError {
    fn from(e: ClipboardError) -> Self {
        match e {
            ClipboardError::Empty | ClipboardError::NoImage => IpcError::ClipboardNoImage,
            other => IpcError::CaptureFailed {
                message: other.to_string(),
            },
        }
    }
}

/// Read the current clipboard as an `RgbaImage`. Errors map cleanly to
/// the existing `IpcError::ClipboardNoImage` / `CaptureFailed`
/// variants.
pub async fn read_clipboard_image() -> Result<RgbaImage, ClipboardError> {
    read_clipboard_image_with_program("wl-paste").await
}

/// Test seam — lets a stand-in binary substitute for `wl-paste`.
pub async fn read_clipboard_image_with_program(program: &str) -> Result<RgbaImage, ClipboardError> {
    let listing = Command::new(program)
        .arg("--list-types")
        .output()
        .await
        .map_err(ClipboardError::Launch)?;

    if !listing.status.success() {
        // wl-paste exits non-zero on empty clipboard.
        let stderr = String::from_utf8_lossy(&listing.stderr);
        tracing::debug!(
            code = listing.status.code(),
            stderr = %stderr.trim(),
            "wl-paste --list-types failed",
        );
        return Err(ClipboardError::Empty);
    }

    let types: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let chosen = pick_image_mime(&types).ok_or(ClipboardError::NoImage)?;

    let bytes = Command::new(program)
        .args(["--type", chosen, "--no-newline"])
        .output()
        .await
        .map_err(ClipboardError::Launch)?;

    if !bytes.status.success() {
        let stderr = String::from_utf8_lossy(&bytes.stderr).trim().to_string();
        return Err(ClipboardError::PasteFailed {
            mime: chosen.to_string(),
            message: if stderr.is_empty() {
                format!("exit code {:?}", bytes.status.code())
            } else {
                stderr
            },
        });
    }

    let dyn_img = image::load_from_memory(&bytes.stdout).map_err(|e| ClipboardError::Decode {
        mime: chosen.to_string(),
        source: e,
    })?;
    Ok(dyn_img.to_rgba8())
}

/// Encode `image` as PNG and hand it to the Wayland clipboard via
/// `wl-copy`. `wl-copy` self-detaches as the persistent selection
/// holder until cleared or replaced — the daemon does **not** stay
/// on the hook for the selection.
pub async fn write_clipboard_image(image: Arc<RgbaImage>) -> Result<(), ClipboardError> {
    write_clipboard_image_with_program(image, "wl-copy").await
}

async fn write_clipboard_image_with_program(
    image: Arc<RgbaImage>,
    program: &str,
) -> Result<(), ClipboardError> {
    // Encode on the blocking pool — PNG compression is CPU-bound.
    let png_bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, image::ImageError> {
        let mut buf: Vec<u8> = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)?;
        Ok(buf)
    })
    .await
    .map_err(|e| ClipboardError::CopyFailed(format!("encode join: {e}")))?
    .map_err(ClipboardError::Encode)?;

    let mut child = Command::new(program)
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ClipboardError::Launch)?;

    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClipboardError::CopyFailed("wl-copy stdin unavailable".into()))?;
        stdin
            .write_all(&png_bytes)
            .await
            .map_err(|e| ClipboardError::CopyFailed(format!("write stdin: {e}")))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| ClipboardError::CopyFailed(format!("close stdin: {e}")))?;
    }

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| ClipboardError::CopyFailed(format!("wait: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ClipboardError::CopyFailed(if stderr.is_empty() {
            format!("exit code {:?}", out.status.code())
        } else {
            stderr
        }));
    }
    Ok(())
}

/// Pick the most desirable image MIME type from a list of advertised
/// types. Returns the borrowed slice from `types` so the caller can
/// pass it straight to `wl-paste --type`.
fn pick_image_mime(types: &[String]) -> Option<&str> {
    for preferred in PREFERRED_MIMES {
        if let Some(t) = types.iter().find(|t| *t == *preferred) {
            return Some(t.as_str());
        }
    }
    types
        .iter()
        .find(|t| t.starts_with("image/"))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_png_when_available() {
        let types: Vec<String> = ["image/jpeg", "image/bmp", "image/png", "text/plain"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_image_mime(&types), Some("image/png"));
    }

    #[test]
    fn falls_back_to_jpeg_when_no_png() {
        let types: Vec<String> = ["image/bmp", "image/jpeg", "text/plain"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_image_mime(&types), Some("image/jpeg"));
    }

    #[test]
    fn accepts_unknown_image_mime_as_last_resort() {
        let types: Vec<String> = vec!["image/x-novel".into(), "text/plain".into()];
        assert_eq!(pick_image_mime(&types), Some("image/x-novel"));
    }

    #[test]
    fn returns_none_when_no_image_type_present() {
        let types: Vec<String> = vec!["text/plain".into(), "text/html".into()];
        assert_eq!(pick_image_mime(&types), None);
    }

    #[test]
    fn returns_none_for_empty_listing() {
        assert_eq!(pick_image_mime(&[]), None);
    }

    #[test]
    fn empty_maps_to_clipboard_no_image() {
        assert!(matches!(
            IpcError::from(ClipboardError::Empty),
            IpcError::ClipboardNoImage
        ));
    }

    #[test]
    fn no_image_maps_to_clipboard_no_image() {
        assert!(matches!(
            IpcError::from(ClipboardError::NoImage),
            IpcError::ClipboardNoImage
        ));
    }

    #[test]
    fn paste_failed_maps_to_capture_failed() {
        let err = IpcError::from(ClipboardError::PasteFailed {
            mime: "image/png".into(),
            message: "boom".into(),
        });
        assert!(matches!(err, IpcError::CaptureFailed { .. }));
    }

    #[tokio::test]
    async fn missing_program_is_launch_error() {
        let err = read_clipboard_image_with_program("definitely-not-a-real-binary-xyz")
            .await
            .expect_err("should fail");
        assert!(matches!(err, ClipboardError::Launch(_)));
    }

    #[tokio::test]
    async fn write_path_missing_program_is_launch_error() {
        let img = Arc::new(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255])));
        let err = write_clipboard_image_with_program(img, "definitely-not-a-real-binary-xyz")
            .await
            .expect_err("should fail");
        assert!(matches!(err, ClipboardError::Launch(_)));
    }

    #[tokio::test]
    async fn write_path_nonzero_exit_is_copy_failed() {
        // `false` exits 1 immediately. With `Stdio::piped()` we still
        // have to handle the stdin gracefully — `false` closes its
        // input fd as it exits, and our helper must surface a
        // `CopyFailed` rather than a generic stdin error. We accept
        // either CopyFailed variant since `false` racing against our
        // write is implementation-defined.
        let img = Arc::new(RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255])));
        let err = write_clipboard_image_with_program(img, "false")
            .await
            .expect_err("should fail");
        assert!(matches!(err, ClipboardError::CopyFailed(_)));
    }

    #[tokio::test]
    async fn nonzero_exit_is_empty_clipboard() {
        // `false` exits 1 and writes nothing — same shape as wl-paste
        // on an empty clipboard.
        let err = read_clipboard_image_with_program("false")
            .await
            .expect_err("should fail");
        assert!(matches!(err, ClipboardError::Empty));
    }
}
