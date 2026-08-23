//! Desktop notification helper.
//!
//! Shells out to `notify-send` (libnotify CLI). Whatever the session
//! runs as its notification daemon — `mako` or `dunst` under Niri, the
//! Omarchy shell under Hyprland — picks it up and renders the toast.
//! We fire-and-forget: a missing or failing
//! `notify-send` must never block the daemon or surface an error to the
//! IPC client. Failures log at `warn` and are swallowed.
//!
//! All notifications use `--app-name=osnip` so users can route
//! them in their notification daemon's rules.

use tokio::process::Command;

const APP_NAME: &str = "osnip";

/// Severity hint for the notification daemon. Maps to libnotify's
/// `--urgency` flag.
#[derive(Debug, Clone, Copy)]
pub enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    fn as_flag(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

/// Emit a desktop notification. Body is optional. Errors are logged
/// but never returned — callers cannot react to a notification failure
/// in any meaningful way.
pub async fn notify(summary: &str, body: Option<&str>, urgency: Urgency) {
    let mut cmd = Command::new("notify-send");
    cmd.arg("--app-name").arg(APP_NAME);
    cmd.arg("--urgency").arg(urgency.as_flag());
    cmd.arg(summary);
    if let Some(b) = body {
        cmd.arg(b);
    }
    match cmd.output().await {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            tracing::warn!(
                code = out.status.code(),
                stderr = %stderr.trim(),
                summary,
                "notify-send returned non-zero",
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, summary, "failed to spawn notify-send");
        }
    }
}
