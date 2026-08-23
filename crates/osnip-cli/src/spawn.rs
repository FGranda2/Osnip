//! Locating and launching the `osnip-daemon` binary.
//!
//! Two entry points:
//! - [`spawn_daemon_detached`] — fork-and-forget, used when the CLI
//!   notices a missing socket. The child detaches from the controlling
//!   terminal so killing the CLI does not kill the daemon.
//! - [`exec_daemon_foreground`] — replaces the current process with the
//!   daemon binary; used by `osnip daemon` for systemd and debug.
//!
//! Lookup order for the daemon binary:
//! 1. Sibling of the current executable (`<dir-of-cli>/osnip-daemon`).
//!    Catches the cargo-target layout and Arch-style `/usr/bin` installs.
//! 2. `$PATH` resolution by name.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DAEMON_BIN: &str = "osnip-daemon";

/// Resolve the daemon binary path. Prefer a sibling of the running CLI
/// so `cargo run` and packaged installs both work without `PATH` fiddling.
fn resolve_daemon_path() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(DAEMON_BIN);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    // Fall through to PATH; `Command` will resolve it at spawn time.
    Ok(PathBuf::from(DAEMON_BIN))
}

/// Fork the daemon and detach so it survives the CLI exiting.
pub fn spawn_daemon_detached() -> Result<()> {
    let bin = resolve_daemon_path()?;
    let mut cmd = Command::new(&bin);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Detach from the controlling terminal: new session, no tty.
    // SAFETY: `setsid` is async-signal-safe and may be called between
    // `fork` and `exec`. No allocator, locks, or non-async-signal-safe
    // syscalls run in the closure.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc_setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;
    tracing::debug!(pid = child.id(), bin = %bin.display(), "daemon spawned");
    // Intentionally drop the handle: we don't reap; the daemon is its
    // own session leader after `setsid`. On parent exit the kernel
    // reparents to PID 1, which reaps it normally.
    drop(child);
    Ok(())
}

/// Replace the current process with the daemon binary. Never returns on
/// success.
pub fn exec_daemon_foreground() -> Result<()> {
    let bin = resolve_daemon_path()?;
    use std::os::unix::process::CommandExt;
    let err = Command::new(&bin).exec();
    // `exec` only returns on failure.
    Err(err).with_context(|| format!("exec {}", bin.display()))
}

// Tiny direct binding so we don't pull in the full `libc` crate just for
// one call. `setsid(2)` is part of POSIX and stable across every Unix
// we target.
extern "C" {
    fn setsid() -> i32;
}

#[inline]
fn libc_setsid() -> i32 {
    // SAFETY: `setsid` takes no arguments and returns an `int`. It is
    // safe to call from any process state; failure is reported via
    // return value, not a signal.
    unsafe { setsid() }
}
