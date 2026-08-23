//! Wayland screen capture via `libwayshot` (zwlr_screencopy).
//!
//! `WayshotConnection` is synchronous and opens its own Wayland socket,
//! so each capture runs on `tokio::task::spawn_blocking`. Building a
//! fresh connection per capture is cheap relative to the actual pixel
//! transfer and avoids holding a `WayshotConnection` (`!Send` /
//! `!Sync`) inside the daemon's shared state.

use crate::region_select::Region;
use image::{DynamicImage, RgbaImage};
use libwayshot::{
    region::{LogicalRegion, Position, Region as WayshotRegion, Size},
    WayshotConnection,
};
use osnip_core::IpcError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("connect to wayland: {0}")]
    Connect(String),

    #[error("screenshot failed: {0}")]
    Screenshot(String),

    #[error("blocking task join: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl From<CaptureError> for IpcError {
    fn from(e: CaptureError) -> Self {
        IpcError::CaptureFailed {
            message: e.to_string(),
        }
    }
}

/// Capture the given region as an RGBA image.
///
/// The image returned is keyed by physical pixel dimensions. On HiDPI
/// outputs that may be larger than the logical region — fine for MVP;
/// the GUI layer will render as-is and let the user resize.
pub async fn capture_region(region: Region) -> Result<RgbaImage, CaptureError> {
    let dynamic = tokio::task::spawn_blocking(move || -> Result<DynamicImage, CaptureError> {
        let conn = WayshotConnection::new().map_err(|e| CaptureError::Connect(e.to_string()))?;

        let logical = LogicalRegion {
            inner: WayshotRegion {
                position: Position {
                    x: region.x,
                    y: region.y,
                },
                size: Size {
                    width: region.width,
                    height: region.height,
                },
            },
        };

        conn.screenshot(logical, false)
            .map_err(|e| CaptureError::Screenshot(e.to_string()))
    })
    .await??;

    Ok(dynamic.to_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_error_maps_to_ipc_error() {
        let err = IpcError::from(CaptureError::Connect("no socket".into()));
        match err {
            IpcError::CaptureFailed { message } => {
                assert!(message.contains("no socket"), "got: {message}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // End-to-end capture is intentionally not unit-tested here — it
    // requires a live Wayland compositor with wlr-screencopy support.
    // The manual verification path in the plan covers it.
}
