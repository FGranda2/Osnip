//! Unix-socket accept loop.
//!
//! Lifecycle:
//! 1. Resolve the socket path.
//! 2. Probe for a live daemon (a successful connect → another instance
//!    already owns the socket; we exit rather than fight).
//! 3. Otherwise unlink any stale socket file and `bind`.
//! 4. Accept connections in a loop, spawning a per-connection task
//!    that reads one request, dispatches, writes one response, and
//!    closes the stream.
//!
//! # Two transports, one socket
//!
//! The first byte of a connection selects its framing for the whole of
//! that connection's life:
//!
//! | First byte | Transport | Client |
//! |------------|-----------|--------|
//! | `0x00`     | length-prefixed JSON, one request then close | the `osnip` CLI |
//! | `0x7b` (`{`) | newline-delimited JSON, persistent | the Omarchy bar plugin |
//!
//! This is a decision, not a guess. [`MAX_FRAME_BYTES`] is 1 MiB and is
//! enforced before any allocation, so the high byte of a valid v1
//! length prefix is *always* `0x00` — it can never collide with the `{`
//! that opens a JSON object. Both framings therefore coexist on one
//! socket path, and the CLI needs no changes at all.
//!
//! The NDJSON transport exists because Quickshell's `Socket` reads and
//! writes text through line parsers; it has no way to emit a four-byte
//! big-endian prefix. It is also persistent, which is what lets the bar
//! widget subscribe to pushes instead of polling `list` on a timer.
//!
//! Per-request connections on the v1 path keep the failure model
//! obvious: a hung handler can never wedge a future request, because
//! the next request is a fresh connection.

use crate::handlers::{self, HandlerConfig};
use crate::registry::PinRegistry;
use anyhow::{Context, Result};
use osnip_core::{
    read_frame, write_frame, FrameError, IpcError, IpcRequest, IpcResponse, ShellEventKind,
    ShellMessage, MAX_FRAME_BYTES,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Byte that opens a JSON object, and therefore an NDJSON connection.
const NDJSON_LEAD: u8 = b'{';

/// High byte of every valid length-prefixed frame. See the module docs
/// for why this is guaranteed rather than merely likely.
const LENGTH_PREFIXED_LEAD: u8 = 0x00;

/// Serve forever on `socket_path`. Returns only on a fatal error
/// (failure to bind, listener I/O error). Caller owns the registry
/// because the iced thread needs to share it.
pub async fn serve(
    socket_path: PathBuf,
    config: HandlerConfig,
    registry: Arc<PinRegistry>,
) -> Result<()> {
    let config = Arc::new(config);
    let listener = bind_socket(&socket_path).await?;
    tracing::info!(socket = %socket_path.display(), "daemon listening");

    loop {
        let (stream, _addr) = listener.accept().await.context("accept on daemon socket")?;
        let registry = Arc::clone(&registry);
        let config = Arc::clone(&config);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, registry, config).await {
                tracing::warn!(error = %e, "connection handler error");
            }
        });
    }
}

/// Probe for a live daemon, then bind. If something is already
/// listening, refuse to start; if the file is stale (no listener),
/// unlink and retry.
async fn bind_socket(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => {
                anyhow::bail!(
                    "another osnip-daemon is already listening on {}",
                    path.display()
                );
            }
            Err(e) if is_socket_dead(&e) => {
                tracing::warn!(
                    socket = %path.display(),
                    "removing stale socket file",
                );
                std::fs::remove_file(path)
                    .with_context(|| format!("remove stale socket {}", path.display()))?;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("probe existing socket {}", path.display()));
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket parent dir {}", parent.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("bind socket {}", path.display()))?;
    Ok(listener)
}

fn is_socket_dead(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

async fn handle_connection(
    stream: UnixStream,
    registry: Arc<PinRegistry>,
    config: Arc<HandlerConfig>,
) -> Result<(), FrameError> {
    let (mut rd, wr) = stream.into_split();

    let mut lead = [0u8; 1];
    match rd.read_exact(&mut lead).await {
        Ok(_) => {}
        // A connect-then-close with no bytes is how `bind_socket`
        // probes for a live daemon. That is a health check, not a fault.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e.into()),
    }

    match lead[0] {
        LENGTH_PREFIXED_LEAD => length_prefixed_session(lead[0], rd, wr, registry, config).await,
        NDJSON_LEAD => ndjson_session(lead[0], rd, wr, registry, config).await,
        other => {
            tracing::warn!(
                lead_byte = other,
                "connection used an unknown transport; closing"
            );
            Ok(())
        }
    }
}

/// Re-attach the byte we consumed for transport detection.
///
/// `read_frame` and the line reader both want a stream that starts at
/// the beginning of the message, so the lead byte is chained back in
/// front of the socket rather than reconstructed at each call site.
fn restore_lead(lead: u8, rd: OwnedReadHalf) -> impl tokio::io::AsyncRead + Unpin {
    AsyncReadExt::chain(std::io::Cursor::new([lead]), rd)
}

/// v1: read exactly one length-prefixed request, answer it, hang up.
async fn length_prefixed_session(
    lead: u8,
    rd: OwnedReadHalf,
    mut wr: OwnedWriteHalf,
    registry: Arc<PinRegistry>,
    config: Arc<HandlerConfig>,
) -> Result<(), FrameError> {
    let mut rd = restore_lead(lead, rd);
    let request: IpcRequest = read_frame(&mut rd).await?;
    tracing::debug!(?request, "request");
    let response: IpcResponse = handlers::dispatch(&registry, &config, request).await;
    write_frame(&mut wr, &response).await?;
    Ok(())
}

/// v2: newline-delimited JSON, one object per line, connection held
/// open until the peer hangs up.
///
/// Requests are dispatched in order on this task. A `Capture` blocks
/// the loop for as long as the user takes to drag a region, which is
/// deliberate — but it does not stall the event stream, because pushes
/// are written by a separate task fed through `out_tx`.
async fn ndjson_session(
    lead: u8,
    rd: OwnedReadHalf,
    wr: OwnedWriteHalf,
    registry: Arc<PinRegistry>,
    config: Arc<HandlerConfig>,
) -> Result<(), FrameError> {
    let mut reader = BufReader::new(restore_lead(lead, rd));

    // One task owns the write half. Both the request loop and the event
    // forwarder are producers, and a socket cannot be written from two
    // places without interleaving partial lines.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(write_lines(wr, out_rx));

    send_json(
        &out_tx,
        &ShellMessage::Hello {
            protocol: osnip_core::PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: capabilities(&registry),
        },
    );

    let mut events: Option<JoinHandle<()>> = None;
    let mut line = Vec::new();

    loop {
        line.clear();
        match read_line_capped(&mut reader, &mut line, MAX_FRAME_BYTES as usize).await {
            Ok(true) => {}
            Ok(false) => break, // clean EOF: peer hung up
            Err(e) => {
                tracing::warn!(error = %e, "ndjson read failed; closing connection");
                break;
            }
        }

        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }

        let request: IpcRequest = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                // A malformed line is the client's bug, not a reason to
                // drop a connection the bar depends on. Report and read on.
                send_json(
                    &out_tx,
                    &IpcResponse::Error(IpcError::BadRequest {
                        message: e.to_string(),
                    }),
                );
                continue;
            }
        };

        if matches!(request, IpcRequest::Subscribe) {
            if events.is_none() {
                events = Some(spawn_event_forwarder(Arc::clone(&registry), out_tx.clone()));
                tracing::debug!("client subscribed to pin events");
            }
            send_json(&out_tx, &IpcResponse::Ok);
            continue;
        }

        tracing::debug!(?request, "ndjson request");
        let response = handlers::dispatch(&registry, &config, request).await;
        send_json(&out_tx, &response);
    }

    // The forwarder holds a clone of `out_tx`, so dropping ours is not
    // enough to close the writer's channel — it has to be stopped first.
    if let Some(handle) = events {
        handle.abort();
    }
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Which optional features this build actually has, as bare strings so
/// that adding one never breaks an older client's decode.
fn capabilities(registry: &PinRegistry) -> Vec<String> {
    let mut caps = vec!["subscribe".to_string(), "pin_action".to_string()];
    if registry.thumbnails_enabled() {
        caps.push("thumbnails".to_string());
    }
    caps
}

/// Serialize and queue one line. A closed channel means the peer is
/// already gone, which the read loop will notice on its own.
fn send_json<T: serde::Serialize>(out: &mpsc::UnboundedSender<String>, value: &T) {
    match serde_json::to_string(value) {
        Ok(json) => {
            let _ = out.send(json);
        }
        Err(e) => tracing::error!(error = %e, "could not serialize outbound message"),
    }
}

/// Drain queued lines onto the socket until the channel closes or the
/// peer stops reading.
async fn write_lines(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        if wr.write_all(line.as_bytes()).await.is_err()
            || wr.write_all(b"\n").await.is_err()
            || wr.flush().await.is_err()
        {
            break;
        }
    }
}

/// Forward registry changes to a subscribed client.
///
/// Subscribes *before* taking the opening snapshot, so a change landing
/// between the two is queued rather than lost.
fn spawn_event_forwarder(
    registry: Arc<PinRegistry>,
    out: mpsc::UnboundedSender<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = registry.subscribe();
        send_pins(&out, registry.list());
        loop {
            match rx.recv().await {
                Ok(pins) => send_pins(&out, pins),
                // Every message is a full snapshot, so a lagging
                // subscriber is repaired by sending the current state
                // rather than by replaying what it missed.
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "event subscriber lagged; resending snapshot");
                    send_pins(&out, registry.list());
                }
                Err(RecvError::Closed) => return,
            }
        }
    })
}

fn send_pins(out: &mpsc::UnboundedSender<String>, pins: Vec<osnip_core::PinSummary>) {
    send_json(
        out,
        &ShellMessage::Event {
            event: ShellEventKind::PinsChanged,
            pins,
        },
    );
}

/// Read one `\n`-terminated line into `buf`, refusing to buffer more
/// than `cap` bytes.
///
/// `AsyncBufReadExt::read_until` would allocate an entire oversized
/// line before we could object; going through `fill_buf` lets the cap
/// be checked once per buffered chunk, so a peer that never sends a
/// newline is cut off after `cap` bytes rather than after however much
/// memory it felt like spending.
///
/// Returns `Ok(false)` at clean EOF, `Ok(true)` when a line was read.
async fn read_line_capped<R>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<bool>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let (found, used) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                // EOF. A trailing fragment with no newline is still a
                // line worth parsing; nothing at all means we are done.
                return Ok(!buf.is_empty());
            }
            match available.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    buf.extend_from_slice(&available[..i]);
                    (true, i + 1)
                }
                None => {
                    buf.extend_from_slice(available);
                    (false, available.len())
                }
            }
        };
        reader.consume(used);
        if found {
            return Ok(true);
        }
        if buf.len() > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("line exceeded {cap} bytes without a newline"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Binds, runs the accept loop in a background task, sends a
    /// `List` request, and verifies the empty-pins response.
    #[tokio::test]
    async fn list_round_trips_against_real_listener() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("osnip.sock");
        let path_clone = path.clone();
        let server = tokio::spawn(async move {
            // serve() loops forever; the test will abort it.
            let _ = serve(
                path_clone,
                HandlerConfig::default(),
                Arc::new(PinRegistry::new()),
            )
            .await;
        });

        // Wait for the socket to appear (bind is sync after `serve`
        // starts, but the spawn may not have run yet).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if UnixStream::connect(&path).await.is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("daemon did not bind {} in time", path.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut s = UnixStream::connect(&path).await.expect("connect");
        write_frame(&mut s, &IpcRequest::List).await.expect("write");
        let resp: IpcResponse = read_frame(&mut s).await.expect("read");
        assert!(matches!(resp, IpcResponse::Pins { pins } if pins.is_empty()));

        server.abort();
    }

    #[tokio::test]
    async fn close_unknown_id_round_trips_ok() {
        // Sanity: even paths that do real work go through the same
        // accept loop without wedging it on first invocation.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("osnip.sock");
        let path_clone = path.clone();
        let server = tokio::spawn(async move {
            let _ = serve(
                path_clone,
                HandlerConfig::default(),
                Arc::new(PinRegistry::new()),
            )
            .await;
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if UnixStream::connect(&path).await.is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("daemon did not bind in time");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let mut s = UnixStream::connect(&path).await.expect("connect");
        write_frame(
            &mut s,
            &IpcRequest::Close {
                id: osnip_core::PinId::new(424242),
            },
        )
        .await
        .expect("write");
        let resp: IpcResponse = read_frame(&mut s).await.expect("read");
        assert!(matches!(resp, IpcResponse::Ok));

        server.abort();
    }

    /// Start a daemon on a temp socket and wait for it to bind.
    /// Returns the socket path, the tempdir (kept alive by the caller),
    /// the shared registry, and the server task.
    async fn start_daemon(
        registry: Arc<PinRegistry>,
    ) -> (PathBuf, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("osnip.sock");
        let path_clone = path.clone();
        let server = tokio::spawn(async move {
            let _ = serve(path_clone, HandlerConfig::default(), registry).await;
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if UnixStream::connect(&path).await.is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("daemon did not bind {} in time", path.display());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        (path, dir, server)
    }

    fn fixture_image() -> Arc<image::RgbaImage> {
        Arc::new(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([1, 2, 3, 255]),
        ))
    }

    /// Read one NDJSON line and decode it as a generic JSON value.
    async fn next_json<R>(reader: &mut R) -> serde_json::Value
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut line = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .expect("timed out waiting for a line")
        .expect("read line");
        assert!(read > 0, "connection closed while awaiting a line");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad json {line:?}: {e}"))
    }

    #[tokio::test]
    async fn ndjson_connection_opens_with_hello() {
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        // The hello is unsolicited, but the daemon cannot know this is
        // NDJSON until we send a `{`, so a request has to come first.
        wr.write_all(b"{\"kind\":\"list\"}\n").await.expect("write");

        let hello = next_json(&mut reader).await;
        assert_eq!(hello["kind"], "hello");
        assert_eq!(hello["protocol"], osnip_core::PROTOCOL_VERSION);
        let caps = hello["capabilities"].as_array().expect("capabilities");
        assert!(caps.iter().any(|c| c == "subscribe"));
        assert!(caps.iter().any(|c| c == "pin_action"));
        // This registry has no thumbnail dir, so the capability must
        // not be advertised.
        assert!(!caps.iter().any(|c| c == "thumbnails"));

        let listed = next_json(&mut reader).await;
        assert_eq!(listed["kind"], "pins");

        server.abort();
    }

    #[tokio::test]
    async fn subscribe_pushes_a_snapshot_then_every_change() {
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        wr.write_all(b"{\"kind\":\"subscribe\"}\n")
            .await
            .expect("write");

        assert_eq!(next_json(&mut reader).await["kind"], "hello");
        assert_eq!(next_json(&mut reader).await["kind"], "ok");

        // Opening snapshot: empty, but sent unprompted so the client
        // never has to issue a `list` of its own.
        let opening = next_json(&mut reader).await;
        assert_eq!(opening["kind"], "event");
        assert_eq!(opening["event"], "pins_changed");
        assert_eq!(opening["pins"].as_array().expect("pins").len(), 0);

        // A change made entirely outside this connection must reach it.
        let id = registry.insert(fixture_image(), None, 7);
        let added = next_json(&mut reader).await;
        assert_eq!(added["kind"], "event");
        let pins = added["pins"].as_array().expect("pins");
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0]["id"], id.get());
        assert_eq!(pins[0]["width"], 4);

        registry.close(id);
        let removed = next_json(&mut reader).await;
        assert_eq!(removed["pins"].as_array().expect("pins").len(), 0);

        server.abort();
    }

    #[tokio::test]
    async fn ndjson_connection_serves_many_requests() {
        // The whole point of the second transport: the bar holds one
        // connection open for the life of the shell.
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);
        wr.write_all(b"{\"kind\":\"list\"}\n").await.expect("write");
        assert_eq!(next_json(&mut reader).await["kind"], "hello");
        assert_eq!(next_json(&mut reader).await["kind"], "pins");

        for _ in 0..3 {
            wr.write_all(b"{\"kind\":\"list\"}\n").await.expect("write");
            assert_eq!(next_json(&mut reader).await["kind"], "pins");
        }

        server.abort();
    }

    #[tokio::test]
    async fn malformed_line_is_reported_without_dropping_the_connection() {
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        wr.write_all(b"{not json at all\n").await.expect("write");
        assert_eq!(next_json(&mut reader).await["kind"], "hello");
        let err = next_json(&mut reader).await;
        assert_eq!(err["kind"], "error");
        assert_eq!(err["data"]["kind"], "bad_request");

        // Still usable afterwards — a typo from the panel must not cost
        // the user their live pin feed.
        wr.write_all(b"{\"kind\":\"list\"}\n").await.expect("write");
        assert_eq!(next_json(&mut reader).await["kind"], "pins");

        server.abort();
    }

    #[tokio::test]
    async fn pin_action_on_unknown_pin_is_ok() {
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        wr.write_all(b"{\"kind\":\"pin_action\",\"id\":999,\"action\":\"copy\"}\n")
            .await
            .expect("write");
        assert_eq!(next_json(&mut reader).await["kind"], "hello");
        assert_eq!(next_json(&mut reader).await["kind"], "ok");

        server.abort();
    }

    #[tokio::test]
    async fn subscribe_over_length_prefixed_framing_is_refused() {
        // There is nowhere to push to on a one-shot connection; saying
        // so beats accepting a subscription that silently never fires.
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let mut s = UnixStream::connect(&path).await.expect("connect");
        write_frame(&mut s, &IpcRequest::Subscribe)
            .await
            .expect("write");
        let resp: IpcResponse = read_frame(&mut s).await.expect("read");
        assert!(
            matches!(resp, IpcResponse::Error(IpcError::BadRequest { .. })),
            "unexpected: {resp:?}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn both_transports_share_one_socket() {
        // The CLI and the bar plugin are live at the same time on a
        // real desktop; the lead-byte discriminator has to hold up.
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect ndjson");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);
        wr.write_all(b"{\"kind\":\"subscribe\"}\n")
            .await
            .expect("write");
        assert_eq!(next_json(&mut reader).await["kind"], "hello");
        assert_eq!(next_json(&mut reader).await["kind"], "ok");
        assert_eq!(next_json(&mut reader).await["kind"], "event");

        // v1 client on the same socket, concurrently.
        let mut cli = UnixStream::connect(&path).await.expect("connect v1");
        write_frame(&mut cli, &IpcRequest::List)
            .await
            .expect("write");
        let resp: IpcResponse = read_frame(&mut cli).await.expect("read");
        assert!(matches!(resp, IpcResponse::Pins { pins } if pins.is_empty()));

        // And the subscriber still gets its push.
        registry.insert(fixture_image(), None, 0);
        let event = next_json(&mut reader).await;
        assert_eq!(event["pins"].as_array().expect("pins").len(), 1);

        server.abort();
    }

    #[tokio::test]
    async fn oversized_line_without_a_newline_is_cut_off() {
        let registry = Arc::new(PinRegistry::new());
        let (path, _dir, server) = start_daemon(Arc::clone(&registry)).await;

        let stream = UnixStream::connect(&path).await.expect("connect");
        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);

        // Open an object and then never close it or send a newline.
        wr.write_all(b"{").await.expect("write lead");
        assert_eq!(next_json(&mut reader).await["kind"], "hello");

        let junk = vec![b'x'; 64 * 1024];
        let mut written = 0usize;
        while written <= MAX_FRAME_BYTES as usize {
            if wr.write_all(&junk).await.is_err() {
                break; // daemon hung up on us, which is the point
            }
            written += junk.len();
        }

        // The session must end rather than buffer without limit. Whether
        // that surfaces as a clean EOF or an ECONNRESET depends on how
        // much of our junk was still in flight when the daemon hung up;
        // both mean the connection is gone, which is what we assert.
        let mut line = String::new();
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .expect("timed out; connection was not closed");
        match ended {
            Ok(0) => {}
            Ok(_) => panic!("expected the connection to end, got {line:?}"),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected error: {e}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn stale_socket_file_is_replaced() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("osnip.sock");
        // Create a stale file: bind, then drop the listener (Tokio's
        // `UnixListener` does not unlink on drop, so the file persists).
        {
            let _stale = UnixListener::bind(&path).expect("bind stale");
        }
        assert!(path.exists());

        let path_clone = path.clone();
        let server = tokio::spawn(async move {
            let _ = serve(
                path_clone,
                HandlerConfig::default(),
                Arc::new(PinRegistry::new()),
            )
            .await;
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if UnixStream::connect(&path).await.is_ok() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("daemon did not replace stale socket");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        server.abort();
    }
}
