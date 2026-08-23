//! Request → response dispatch.
//!
//! Every handler is total: it returns an [`IpcResponse`] rather than
//! propagating an error, because errors are in-band on this protocol
//! ([`IpcResponse::Error`]). That keeps the connection loop trivial —
//! it never has to decide whether a daemon-internal failure should
//! close the socket or be reported to the client.
//!
//! `Capture` is the first handler that does real work: it shells out
//! to `slurp`, captures pixels via `libwayshot`, and registers a pin.
//! See [`crate::region_select`] and [`crate::capture`].

use crate::app::AppEvent;
use crate::capture;
use crate::clipboard;
use crate::region_select;
use crate::registry::PinRegistry;
use osnip_core::{IpcError, IpcRequest, IpcResponse, PinId};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::UnboundedSender;

/// Daemon-wide context consulted by handlers. Cheap to clone (`Arc`s
/// inside, plus an `Option` for the GUI bridge).
#[derive(Clone, Default)]
pub struct HandlerConfig {
    /// If set, every successful capture is also written to
    /// `<save_dir>/pin-<id>.png`. Headless verification path used in
    /// phase 5a; still useful for debugging now that the GUI exists.
    pub save_dir: Option<std::path::PathBuf>,
    /// Bridge to the iced application. `None` means "no GUI"
    /// (test contexts and the headless 5a build).
    pub app_tx: Option<UnboundedSender<AppEvent>>,
}

impl std::fmt::Debug for HandlerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandlerConfig")
            .field("save_dir", &self.save_dir)
            .field("app_tx", &self.app_tx.is_some())
            .finish()
    }
}

impl HandlerConfig {
    fn notify_gui(&self, event: AppEvent) {
        if let Some(tx) = &self.app_tx {
            if let Err(e) = tx.send(event) {
                tracing::warn!(error = %e, "iced bridge closed; gui notification dropped");
            }
        }
    }
}

/// Dispatch a single request against the shared registry.
pub async fn dispatch(
    registry: &Arc<PinRegistry>,
    config: &HandlerConfig,
    request: IpcRequest,
) -> IpcResponse {
    match request {
        IpcRequest::Capture => handle_capture(registry, config).await,
        IpcRequest::Clipboard => handle_clipboard(registry, config).await,
        IpcRequest::List => IpcResponse::Pins {
            pins: registry.list(),
        },
        IpcRequest::Close { id } => {
            let existed = registry.close(id);
            tracing::info!(pin_id = %id, existed, "close");
            if existed {
                config.notify_gui(AppEvent::ClosePin(id));
            }
            IpcResponse::Ok
        }
        IpcRequest::CloseAll => {
            let n = registry.close_all();
            tracing::info!(removed = n, "close_all");
            if n > 0 {
                config.notify_gui(AppEvent::CloseAllPins);
            }
            IpcResponse::Ok
        }
        IpcRequest::PinAction { id, action } => {
            // Idempotent on a miss, matching `Close`: a bar widget
            // acting on a pin the user just closed should not have to
            // treat that as an error.
            if registry.image(id).is_none() {
                tracing::debug!(pin_id = %id, ?action, "pin_action on unknown pin");
                return IpcResponse::Ok;
            }
            tracing::info!(pin_id = %id, ?action, "pin_action");
            config.notify_gui(AppEvent::PinAction { pin_id: id, action });
            IpcResponse::Ok
        }
        // `Subscribe` is transport-level: the NDJSON session intercepts
        // it before dispatch. Reaching here means a client asked for a
        // stream over the one-shot length-prefixed framing, which has
        // nowhere to push to.
        IpcRequest::Subscribe => IpcResponse::Error(IpcError::BadRequest {
            message: "subscribe requires the newline-delimited JSON transport".into(),
        }),
    }
}

async fn handle_capture(registry: &Arc<PinRegistry>, config: &HandlerConfig) -> IpcResponse {
    let region = match region_select::select_region().await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %e, "region selection ended without a capture");
            return IpcResponse::Error(IpcError::from(e));
        }
    };
    tracing::info!(?region, "region selected");

    let image = match capture::capture_region(region).await {
        Ok(img) => img,
        Err(e) => {
            tracing::warn!(error = %e, "capture failed");
            return IpcResponse::Error(IpcError::from(e));
        }
    };
    tracing::info!(width = image.width(), height = image.height(), "captured");
    // The slurp region is in compositor logical units; libwayshot returns
    // physical pixels. Pass the logical size through so the pin window
    // opens at the same on-screen size as the dragged region on HiDPI.
    let logical_size = Some((region.width, region.height));
    register_pin(registry, config, image, logical_size, "captured")
}

async fn handle_clipboard(registry: &Arc<PinRegistry>, config: &HandlerConfig) -> IpcResponse {
    let image = match clipboard::read_clipboard_image().await {
        Ok(img) => img,
        Err(e) => {
            tracing::info!(error = %e, "clipboard read failed");
            return IpcResponse::Error(IpcError::from(e));
        }
    };
    tracing::info!(
        width = image.width(),
        height = image.height(),
        "clipboard image read",
    );
    register_pin(registry, config, image, None, "clipboard")
}

/// Common tail for both capture paths: insert into the registry,
/// optionally dump a debug PNG, notify the GUI, return `Pinned`.
fn register_pin(
    registry: &Arc<PinRegistry>,
    config: &HandlerConfig,
    image: image::RgbaImage,
    logical_size: Option<(u32, u32)>,
    source: &'static str,
) -> IpcResponse {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let image = Arc::new(image);
    let id = registry.insert(Arc::clone(&image), logical_size, now_ms);

    if let Some(dir) = &config.save_dir {
        if let Err(e) = save_pin_png(dir, id, &image) {
            tracing::warn!(error = %e, pin_id = %id, "could not write debug PNG");
        }
    }

    config.notify_gui(AppEvent::OpenPin(id));
    tracing::info!(pin_id = %id, source, "pinned");
    IpcResponse::Pinned { id }
}

fn save_pin_png(dir: &std::path::Path, id: PinId, image: &image::RgbaImage) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create_dir_all: {e}"))?;
    let path = dir.join(format!("pin-{id}.png"));
    image
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| format!("save {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbaImage;

    fn fixture_image() -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255])))
    }

    #[tokio::test]
    async fn list_reflects_registry() {
        let r = Arc::new(PinRegistry::new());
        r.insert(fixture_image(), None, 42);
        match dispatch(&r, &HandlerConfig::default(), IpcRequest::List).await {
            IpcResponse::Pins { pins } => {
                assert_eq!(pins.len(), 1);
                assert_eq!(pins[0].width, 2);
                assert_eq!(pins[0].height, 2);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_unknown_id_is_ok() {
        let r = Arc::new(PinRegistry::new());
        let resp = dispatch(
            &r,
            &HandlerConfig::default(),
            IpcRequest::Close { id: PinId::new(99) },
        )
        .await;
        assert!(matches!(resp, IpcResponse::Ok));
    }

    #[tokio::test]
    async fn close_all_clears_and_returns_ok() {
        let r = Arc::new(PinRegistry::new());
        r.insert(fixture_image(), None, 0);
        r.insert(fixture_image(), None, 0);
        let resp = dispatch(&r, &HandlerConfig::default(), IpcRequest::CloseAll).await;
        assert!(matches!(resp, IpcResponse::Ok));
        assert!(r.list().is_empty());
    }

    // `Capture` and `Clipboard` are exercised end-to-end manually —
    // they require `slurp`/`wl-paste` and a live Wayland compositor.
    // The respective parsers and error → IpcError mappings are
    // unit-tested in their own modules.
}
