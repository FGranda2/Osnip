//! `osnip-daemon`: long-running process that owns every pin window
//! and serves CLI requests over a Unix socket.
//!
//! Process layout (phase 5b):
//! - **Main thread**: `iced::daemon` event loop. Wayland windowing
//!   requires the main thread; this is non-negotiable.
//! - **Worker thread**: a `tokio` multi-threaded runtime. Hosts the
//!   IPC accept loop and the slurp/libwayshot capture pipeline.
//! - **Bridge**: `tokio::sync::mpsc::unbounded_channel::<AppEvent>()`.
//!   IPC handlers push events; the iced subscription consumes them
//!   via `iced::stream::channel`.
//!
//! Both threads share the same `Arc<PinRegistry>` — registry is the
//! source of truth for pin metadata and pixels.

use anyhow::{anyhow, Context, Result};
use osnip_core::{default_socket_path, thumbnail_dir_for};
use std::path::PathBuf;
use std::sync::Arc;

mod app;
mod capture;
mod clipboard;
mod config;
mod handlers;
mod image_ops;
mod notify;
mod region_select;
mod registry;
mod save;
mod server;
mod thumbnail;

use crate::app::App;
use crate::config::Config;
use crate::registry::PinRegistry;

fn main() -> Result<()> {
    init_tracing();

    let socket_path: PathBuf = match std::env::var_os("OSNIP_SOCKET") {
        Some(p) => PathBuf::from(p),
        None => default_socket_path()
            .context("XDG_RUNTIME_DIR is unset; set OSNIP_SOCKET to override")?,
    };

    // Thumbnails are what let the Omarchy bar plugin show a real
    // preview of each pin from another process. Without a runtime dir
    // there is nowhere session-scoped to put them, so we run without.
    let registry = Arc::new(match thumbnail_dir_for(&socket_path) {
        Some(dir) => {
            tracing::info!(dir = %dir.display(), "thumbnail cache enabled");
            PinRegistry::with_thumbnail_dir(dir)
        }
        None => {
            tracing::warn!("XDG_RUNTIME_DIR is unset; running without pin thumbnails");
            PinRegistry::new()
        }
    });
    let config = Arc::new(Config::load().context("load user config")?);
    tracing::info!(
        save_dir = %config.save_dir.display(),
        template = %config.filename_template,
        "config loaded",
    );
    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<app::AppEvent>();

    let handler_config = handlers::HandlerConfig {
        save_dir: std::env::var_os("OSNIP_SAVE_DIR").map(PathBuf::from),
        app_tx: Some(app_tx),
    };
    if let Some(dir) = &handler_config.save_dir {
        tracing::info!(dir = %dir.display(), "debug PNG dump enabled");
    }

    // Spawn the tokio runtime on a worker OS thread so the main thread
    // is free for iced. The thread holds the runtime; if it panics or
    // exits, the daemon dies (intentionally — there is no graceful
    // continuation without IPC).
    let registry_for_io = Arc::clone(&registry);
    let socket_path_for_io = socket_path.clone();
    let _io_thread = std::thread::Builder::new()
        .name("osnip-io".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "failed to build tokio runtime");
                    return;
                }
            };
            if let Err(e) = runtime.block_on(server::serve(
                socket_path_for_io,
                handler_config,
                registry_for_io,
            )) {
                tracing::error!(error = %e, "ipc server exited");
            }
        })
        .context("spawn io thread")?;

    // Hand the IPC receiver to the iced subscription via the static
    // slot, then enter the iced event loop. `daemon` blocks the main
    // thread until the application exits.
    app::install_ipc_receiver(app_rx);
    let registry_for_boot = Arc::clone(&registry);
    let config_for_boot = Arc::clone(&config);
    iced::daemon(
        move || App::new(Arc::clone(&registry_for_boot), Arc::clone(&config_for_boot)),
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .style(App::style)
    .run()
    .map_err(|e| anyhow!("iced runtime: {e}"))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
