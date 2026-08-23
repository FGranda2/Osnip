//! Region selection via the external `slurp` binary.
//!
//! `slurp` is the de-facto Wayland region selector and is what the spec
//! locks in for MVP — a custom layer-shell selector is a later
//! milestone. We invoke it as a subprocess, parse its single-line
//! `"X,Y WxH"` stdout, and surface user-cancellation as
//! [`IpcError::CaptureCanceled`] so the CLI can exit cleanly without
//! pretending it was a real failure.

use anyhow::Context;
use osnip_core::IpcError;
use thiserror::Error;
use tokio::process::Command;

/// A rectangular region in compositor logical coordinates, as emitted
/// by `slurp`. `width` and `height` are guaranteed positive — slurp
/// never reports a degenerate region (it requires the user to drag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Errors produced by the region-selection step. Each variant maps to
/// a single in-band [`IpcError`] variant so the IPC layer never has to
/// invent context.
#[derive(Debug, Error)]
pub enum RegionError {
    /// `slurp` was not found on `PATH` or otherwise failed to launch.
    #[error("failed to launch slurp: {0}")]
    Launch(#[source] std::io::Error),

    /// `slurp` exited non-zero — by convention this is the user
    /// pressing Esc. We do not distinguish from "user clicked outside"
    /// because slurp itself does not.
    #[error("user canceled region selection")]
    Canceled,

    /// `slurp`'s stdout was not the expected single-line format.
    #[error("malformed slurp output: {raw:?}")]
    MalformedOutput { raw: String },

    /// I/O while reading slurp's stdout / stderr.
    #[error("i/o while reading slurp: {0}")]
    Io(#[source] std::io::Error),
}

impl From<RegionError> for IpcError {
    fn from(e: RegionError) -> Self {
        match e {
            RegionError::Canceled => IpcError::CaptureCanceled,
            other => IpcError::CaptureFailed {
                message: other.to_string(),
            },
        }
    }
}

/// Spawn `slurp` and await a region selection. Uses the exact binary
/// name `slurp` so users can swap it via `$PATH` if they want
/// `swappy`-style prompts later.
///
/// Default args skip the full-screen dim overlay (`-b 00000000`) — on
/// HiDPI / 4K outputs the translucent fill is the dominant cost in
/// slurp's per-frame redraw and produces visible drag lag. We keep a
/// thin colored border so the selection is still legible. Users who
/// want slurp's default look can override the whole arg list via
/// `OSNIP_SLURP_ARGS` (space-separated).
pub async fn select_region() -> Result<Region, RegionError> {
    let args = slurp_args_from_env();
    select_region_with(
        "slurp",
        args.iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
    )
    .await
}

fn slurp_args_from_env() -> Vec<String> {
    if let Ok(raw) = std::env::var("OSNIP_SLURP_ARGS") {
        return raw.split_whitespace().map(str::to_string).collect();
    }
    // -b 00000000  : transparent background (no full-screen dim, big
    //                win on 4K — this is the lag fix).
    // -c ffffffff  : opaque white selection border so the drag is
    //                still clearly visible without the dim.
    // -w 2         : 2px border weight.
    ["-b", "00000000", "-c", "ffffffff", "-w", "2"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Lower-level entry point that lets tests inject a stand-in binary.
#[cfg(test)]
pub async fn select_region_with_program(program: &str) -> Result<Region, RegionError> {
    select_region_with(program, &[]).await
}

async fn select_region_with(program: &str, args: &[&str]) -> Result<Region, RegionError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(RegionError::Launch)?;

    if !output.status.success() {
        // Mirror slurp's convention: non-zero exit == user canceled.
        // We log stderr at debug so a misconfigured binary is
        // diagnosable without leaking it to the CLI.
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::debug!(
            code = output.status.code(),
            stderr = %stderr.trim(),
            "slurp exited non-zero",
        );
        return Err(RegionError::Canceled);
    }

    let raw = std::str::from_utf8(&output.stdout)
        .map_err(|e| RegionError::MalformedOutput {
            raw: format!("non-utf8 ({e})"),
        })?
        .trim()
        .to_string();
    parse_slurp_output(&raw)
}

/// Parse one line of slurp output: `"X,Y WxH"`.
///
/// Negative origins are valid (multi-monitor setups place outputs in a
/// shared logical coordinate space that can extend left of zero).
/// Width and height must be strictly positive.
pub fn parse_slurp_output(raw: &str) -> Result<Region, RegionError> {
    let malformed = || RegionError::MalformedOutput {
        raw: raw.to_string(),
    };

    let (origin, size) = raw.split_once(' ').ok_or_else(malformed)?;
    let (x_str, y_str) = origin.split_once(',').ok_or_else(malformed)?;
    let (w_str, h_str) = size.split_once('x').ok_or_else(malformed)?;

    let x: i32 = x_str.parse().map_err(|_| malformed())?;
    let y: i32 = y_str.parse().map_err(|_| malformed())?;
    let width: u32 = w_str.parse().map_err(|_| malformed())?;
    let height: u32 = h_str.parse().map_err(|_| malformed())?;

    if width == 0 || height == 0 {
        return Err(malformed());
    }

    Ok(Region {
        x,
        y,
        width,
        height,
    })
}

/// Convenience for callers that want one shot and a structured error.
#[allow(dead_code)] // wired up alongside the iced GUI in 5b
pub async fn select_region_or_ipc_error() -> anyhow::Result<Region> {
    select_region()
        .await
        .map_err(|e| anyhow::anyhow!(IpcError::from(e).to_string()))
        .context("region selection")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_output() {
        let r = parse_slurp_output("100,200 800x600").expect("parse");
        assert_eq!(
            r,
            Region {
                x: 100,
                y: 200,
                width: 800,
                height: 600,
            }
        );
    }

    #[test]
    fn parses_negative_origin() {
        let r = parse_slurp_output("-1920,0 1920x1080").expect("parse");
        assert_eq!(r.x, -1920);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn parses_with_trailing_whitespace_after_trim() {
        let r = parse_slurp_output("0,0 1x1").expect("parse");
        assert_eq!(r.width, 1);
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert!(matches!(
            parse_slurp_output("0,0 0x100"),
            Err(RegionError::MalformedOutput { .. })
        ));
        assert!(matches!(
            parse_slurp_output("0,0 100x0"),
            Err(RegionError::MalformedOutput { .. })
        ));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "garbage",
            "1,2",
            "1,2 3",
            "1,2 3x",
            "ax0 1x1",
            "1.0,2 3x4",
        ] {
            assert!(
                matches!(
                    parse_slurp_output(bad),
                    Err(RegionError::MalformedOutput { .. })
                ),
                "expected malformed for {bad:?}",
            );
        }
    }

    #[test]
    fn maps_canceled_to_capture_canceled() {
        let err = IpcError::from(RegionError::Canceled);
        assert!(matches!(err, IpcError::CaptureCanceled));
    }

    #[test]
    fn maps_other_errors_to_capture_failed() {
        let err = IpcError::from(RegionError::MalformedOutput { raw: "x".into() });
        assert!(matches!(err, IpcError::CaptureFailed { .. }));
    }

    #[tokio::test]
    async fn missing_program_is_launch_error() {
        let err = select_region_with_program("definitely-not-a-real-binary-xyz")
            .await
            .expect_err("should fail");
        assert!(matches!(err, RegionError::Launch(_)));
    }

    #[tokio::test]
    async fn nonzero_exit_is_canceled() {
        // `false` is POSIX-standard and exits 1, mimicking slurp on Esc.
        let err = select_region_with_program("false")
            .await
            .expect_err("should fail");
        assert!(matches!(err, RegionError::Canceled));
    }
}
