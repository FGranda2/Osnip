//! Shared types and wire framing for the osnip daemon ↔ CLI protocol.
//!
//! The wire format is **length-prefixed JSON**: a 4-byte big-endian `u32`
//! length, followed by exactly that many bytes of UTF-8 JSON. Frames are
//! capped to [`MAX_FRAME_BYTES`] to bound memory on a malformed peer.

#![deny(missing_docs)]

pub mod framing;
mod ipc;
mod pin_id;

pub use framing::{read_frame, write_frame, FrameError, MAX_FRAME_BYTES};
pub use ipc::{
    IpcError, IpcRequest, IpcResponse, PinActionKind, PinSummary, ShellEventKind, ShellMessage,
    PROTOCOL_VERSION,
};
pub use pin_id::PinId;

/// Default Unix-socket path, resolved from `$XDG_RUNTIME_DIR`.
///
/// Returns `None` if `XDG_RUNTIME_DIR` is unset or empty — callers must
/// surface this as a configuration error, never fall back to `/tmp`
/// (which is world-writable and unsuitable for a control socket).
#[must_use]
pub fn default_socket_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    let mut p = std::path::PathBuf::from(dir);
    p.push("osnip.sock");
    Some(p)
}

/// Directory for the cached pin thumbnails belonging to the daemon that
/// listens on `socket`.
///
/// Runtime dir rather than a cache dir on purpose: thumbnails are only
/// meaningful while the daemon that owns those pins is alive, and
/// `$XDG_RUNTIME_DIR` is cleared on logout. Returns `None` under the
/// same conditions as [`default_socket_path`].
///
/// The directory is keyed by the socket's file stem rather than being
/// one shared path. A daemon wipes its thumbnail directory on startup
/// (pin ids restart at 1, so last session's files would be served as
/// this session's pins) — and `OSNIP_SOCKET` explicitly allows a
/// second daemon on a second socket, which with a shared directory
/// would delete the first daemon's live thumbnails out from under it.
///
/// ```
/// # use std::path::Path;
/// # use osnip_core::thumbnail_dir_for;
/// # std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
/// assert_eq!(
///     thumbnail_dir_for(Path::new("/run/user/1000/osnip.sock")).unwrap(),
///     Path::new("/run/user/1000/osnip/osnip-thumbs"),
/// );
/// ```
#[must_use]
pub fn thumbnail_dir_for(socket: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    // A socket path always has a file name in practice; fall back to a
    // fixed stem rather than refusing to make thumbnails over it.
    let stem = socket
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("osnip");
    let mut p = std::path::PathBuf::from(dir);
    p.push("osnip");
    p.push(format!("{stem}-thumbs"));
    Some(p)
}
