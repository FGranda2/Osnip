//! `osnip` CLI: thin client that talks to `osnip-daemon` over a
//! Unix socket. One subcommand per `IpcRequest` variant, plus `daemon` to
//! foreground the daemon binary (used by systemd / debugging).
//!
//! On a missing socket the CLI auto-spawns the daemon binary, waits up to
//! [`SPAWN_RETRY_TIMEOUT`], and retries the connection exactly once. A
//! single retry is enough for the "first invocation of the day" case;
//! more attempts would just paper over a real bug in the daemon.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use osnip_core::{default_socket_path, read_frame, write_frame, IpcRequest, IpcResponse, PinId};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;

mod render;
mod spawn;

/// How long to wait for the daemon to publish its socket after auto-spawn.
const SPAWN_RETRY_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Polling interval while waiting for the socket to appear.
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Parser, Debug)]
#[command(
    name = "osnip",
    version,
    about = "Snipaste-style screen pinning for wlroots-style Wayland compositors (Niri, Hyprland/Omarchy)"
)]
struct Cli {
    /// Override the daemon socket path. Falls back to
    /// `$OSNIP_SOCKET`, then `$XDG_RUNTIME_DIR/osnip.sock`.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Disable auto-spawning the daemon if the socket is absent.
    #[arg(long, global = true)]
    no_spawn: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run an interactive region capture and pin the result.
    Capture,
    /// Pin the current clipboard image.
    Clipboard,
    /// List active pins.
    List,
    /// Close a single pin by id.
    Close {
        /// Pin id, as printed by `osnip list`.
        id: u64,
    },
    /// Close every pin.
    CloseAll,
    /// Run the daemon in the foreground (execs `osnip-daemon`).
    Daemon,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // The `daemon` subcommand replaces the current process with the daemon
    // binary, so it never returns into the async runtime.
    if matches!(cli.command, Command::Daemon) {
        return spawn::exec_daemon_foreground();
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let socket = resolve_socket(cli.socket)?;

    let request = build_request(&cli.command)?;
    let response = round_trip(&socket, &request, !cli.no_spawn).await?;
    render::print_response(&request, &response);

    // Mirror the response into the process exit code: errors → non-zero
    // so shell pipelines can branch on `if osnip close $id; then ...`.
    match response {
        IpcResponse::Error(_) => std::process::exit(1),
        _ => Ok(()),
    }
}

/// Resolve the socket to talk to, in precedence order: `--socket`,
/// then `$OSNIP_SOCKET`, then the default runtime path.
///
/// The daemon has honored `OSNIP_SOCKET` since it was introduced
/// and the README documents it as a plain environment override, but the
/// CLI only ever read `--socket` — so `OSNIP_SOCKET=… osnip
/// list` silently talked to the default socket, and auto-spawned a
/// second daemon there when nothing was listening.
fn resolve_socket(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("OSNIP_SOCKET") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    default_socket_path().context("XDG_RUNTIME_DIR is unset; pass --socket or set OSNIP_SOCKET")
}

fn build_request(cmd: &Command) -> Result<IpcRequest> {
    Ok(match cmd {
        Command::Capture => IpcRequest::Capture,
        Command::Clipboard => IpcRequest::Clipboard,
        Command::List => IpcRequest::List,
        Command::Close { id } => IpcRequest::Close {
            id: PinId::new(*id),
        },
        Command::CloseAll => IpcRequest::CloseAll,
        Command::Daemon => {
            // Handled before entering the runtime; reaching here is a bug.
            anyhow::bail!("internal: `daemon` subcommand should not reach build_request")
        }
    })
}

/// Connect, send one framed request, await one framed response.
///
/// If the socket is absent and `auto_spawn` is true, fork the daemon
/// binary and retry once after waiting for the socket to appear.
async fn round_trip(socket: &Path, request: &IpcRequest, auto_spawn: bool) -> Result<IpcResponse> {
    let mut stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) if is_socket_missing(&e) && auto_spawn => {
            tracing::info!(
                socket = %socket.display(),
                "daemon socket missing — auto-spawning",
            );
            spawn::spawn_daemon_detached()
                .with_context(|| format!("spawn daemon for socket {}", socket.display()))?;
            wait_for_socket(socket, SPAWN_RETRY_TIMEOUT)
                .await
                .with_context(|| {
                    format!(
                        "daemon did not publish socket {} within {:?}",
                        socket.display(),
                        SPAWN_RETRY_TIMEOUT,
                    )
                })?;
            UnixStream::connect(socket)
                .await
                .with_context(|| format!("connect to {} after spawn", socket.display()))?
        }
        Err(e) => {
            return Err(e).with_context(|| format!("connect to {}", socket.display()));
        }
    };

    write_frame(&mut stream, request)
        .await
        .context("send request frame")?;
    let response: IpcResponse = read_frame(&mut stream)
        .await
        .context("read response frame")?;
    Ok(response)
}

fn is_socket_missing(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match UnixStream::connect(path).await {
            Ok(_) => return Ok(()),
            Err(e) if is_socket_missing(&e) => {
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for {}", path.display());
                }
                tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
            }
            Err(e) => return Err(e).context("probe socket while waiting"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use osnip_core::{IpcError, PinSummary};
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    /// Spin up a fake daemon that accepts one connection, reads one
    /// request, replies with `response`, then exits. Returns the socket
    /// path inside `dir`.
    async fn fake_daemon(
        dir: &Path,
        response: IpcResponse,
    ) -> (PathBuf, tokio::task::JoinHandle<IpcRequest>) {
        let path = dir.join("osnip.sock");
        let listener = UnixListener::bind(&path).expect("bind fake socket");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let req: IpcRequest = read_frame(&mut stream).await.expect("read req");
            write_frame(&mut stream, &response)
                .await
                .expect("write resp");
            req
        });
        (path, handle)
    }

    #[tokio::test]
    async fn list_round_trips_empty_pins() {
        let dir = tempdir().expect("tempdir");
        let (path, server) = fake_daemon(dir.path(), IpcResponse::Pins { pins: vec![] }).await;

        let resp = round_trip(&path, &IpcRequest::List, false)
            .await
            .expect("round trip");
        let req_seen = server.await.expect("server task");

        assert_eq!(req_seen, IpcRequest::List);
        assert!(matches!(resp, IpcResponse::Pins { pins } if pins.is_empty()));
    }

    #[tokio::test]
    async fn close_with_id_round_trips() {
        let dir = tempdir().expect("tempdir");
        let (path, server) = fake_daemon(dir.path(), IpcResponse::Ok).await;

        let resp = round_trip(&path, &IpcRequest::Close { id: PinId::new(42) }, false)
            .await
            .expect("round trip");
        let req_seen = server.await.expect("server task");

        assert_eq!(req_seen, IpcRequest::Close { id: PinId::new(42) });
        assert!(matches!(resp, IpcResponse::Ok));
    }

    #[tokio::test]
    async fn missing_socket_without_spawn_returns_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.sock");
        let err = round_trip(&path, &IpcRequest::List, false)
            .await
            .expect_err("should fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("connect to"), "got: {msg}");
    }

    #[tokio::test]
    async fn error_response_is_propagated() {
        let dir = tempdir().expect("tempdir");
        let (path, server) = fake_daemon(
            dir.path(),
            IpcResponse::Error(IpcError::NotImplemented {
                feature: "capture".into(),
            }),
        )
        .await;

        let resp = round_trip(&path, &IpcRequest::Capture, false)
            .await
            .expect("round trip");
        let _ = server.await;
        assert!(matches!(
            resp,
            IpcResponse::Error(IpcError::NotImplemented { ref feature }) if feature == "capture"
        ));
    }

    #[test]
    fn socket_resolution_prefers_flag_then_env_then_default() {
        // No other test touches OSNIP_SOCKET, so mutating it here
        // cannot race the rest of the suite.
        let previous = std::env::var_os("OSNIP_SOCKET");
        std::env::set_var("OSNIP_SOCKET", "/tmp/from-env.sock");

        assert_eq!(
            resolve_socket(Some(PathBuf::from("/tmp/from-flag.sock"))).expect("flag"),
            PathBuf::from("/tmp/from-flag.sock"),
            "an explicit --socket must beat the environment",
        );
        assert_eq!(
            resolve_socket(None).expect("env"),
            PathBuf::from("/tmp/from-env.sock"),
            "OSNIP_SOCKET must be honored, as the README promises",
        );

        // An empty value is not a path; fall through to the default.
        std::env::set_var("OSNIP_SOCKET", "");
        let fallback = resolve_socket(None);
        match default_socket_path() {
            Some(expected) => assert_eq!(fallback.expect("default"), expected),
            None => assert!(fallback.is_err()),
        }

        match previous {
            Some(v) => std::env::set_var("OSNIP_SOCKET", v),
            None => std::env::remove_var("OSNIP_SOCKET"),
        }
    }

    #[test]
    fn pin_summary_unused_in_cli_smoke() {
        // Cheap proof the type is reachable from the CLI crate so the
        // wire contract stays in sync.
        let s = PinSummary {
            id: PinId::new(1),
            width: 1,
            height: 1,
            created_at_unix_ms: 1,
            thumbnail: None,
            revision: 0,
        };
        assert_eq!(s.id.get(), 1);
    }
}
