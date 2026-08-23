use crate::pin_id::PinId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Wire-protocol version, announced in [`ShellMessage::Hello`] on every
/// NDJSON connection. Bump on any breaking schema change.
///
/// `2` added the NDJSON transport, [`IpcRequest::Subscribe`],
/// [`IpcRequest::PinAction`], and the `thumbnail` / `revision` fields on
/// [`PinSummary`]. All of those are additive: a v1 client speaking the
/// length-prefixed framing is unaffected, which is why the daemon
/// announces the version rather than rejecting on mismatch.
pub const PROTOCOL_VERSION: u16 = 2;

/// A single request from the CLI to the daemon. One request, one response,
/// then the connection closes — no streaming, no multiplexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Run an interactive region capture (`slurp`-driven), then pin the
    /// resulting image. Daemon-side, this is asynchronous; the response
    /// is returned only once the pin window is up (or capture is
    /// canceled / fails).
    Capture,

    /// Pin the current clipboard image, if any.
    Clipboard,

    /// Enumerate all live pins.
    List,

    /// Close the named pin. Idempotent: closing an unknown id is `Ok`,
    /// not an error, so scripts can `close` without racing `list`.
    Close {
        /// The pin to close.
        id: PinId,
    },

    /// Close every live pin.
    CloseAll,

    /// Subscribe this connection to unsolicited [`ShellMessage`] pushes.
    ///
    /// Only meaningful on the NDJSON transport, which keeps the
    /// connection open. Over the length-prefixed framing (one request,
    /// one response, close) there is nowhere to push to, so the daemon
    /// answers with [`IpcError::BadRequest`].
    Subscribe,

    /// Run an action against a pin without focusing its window.
    ///
    /// The same six operations the keyboard exposes on a focused pin.
    /// Idempotent on an unknown id, matching [`IpcRequest::Close`]: a
    /// client that acts on a pin the user just closed gets `Ok`, not an
    /// error it would have to special-case.
    PinAction {
        /// The pin to act on.
        id: PinId,
        /// What to do to it.
        action: PinActionKind,
    },
}

/// An action that can be applied to a pin's pixels.
///
/// Shared between the keyboard handler in the daemon's GUI thread and
/// the IPC surface, so both paths cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinActionKind {
    /// Encode the current pixels and hand them to the clipboard.
    Copy,
    /// Write the current pixels as a PNG to the configured save directory.
    Save,
    /// Rotate 90 degrees clockwise.
    RotateRight,
    /// Rotate 90 degrees counter-clockwise.
    RotateLeft,
    /// Mirror horizontally.
    FlipH,
    /// Mirror vertically.
    FlipV,
}

/// Successful reply payload, or a structured error.
///
/// `Error` is in-band rather than out-of-band because Unix-socket EOFs
/// are ambiguous: a peer that crashed mid-write looks identical to a
/// peer that completed cleanly. An explicit `Error` variant disambiguates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Generic success with no payload (`Close`, `CloseAll`).
    Ok,

    /// Reply to `List`.
    Pins {
        /// Live pins. May be empty.
        pins: Vec<PinSummary>,
    },

    /// Reply to `Capture` / `Clipboard`: the pin that was created.
    Pinned {
        /// Identifier of the newly created pin.
        id: PinId,
    },

    /// Structured failure. See [`IpcError`].
    Error(IpcError),
}

/// Lightweight description of a pin returned by `List`.
///
/// Intentionally minimal — clients render a table; richer queries can be
/// added without breaking the wire format because this is a struct, not
/// a tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinSummary {
    /// Pin identifier.
    pub id: PinId,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Wall-clock creation time, milliseconds since the Unix epoch.
    pub created_at_unix_ms: u64,
    /// Path to a cached PNG thumbnail, when the daemon is writing them.
    ///
    /// `#[serde(default)]` so a v2 client can still decode a summary
    /// produced by a v1 daemon; `skip_serializing_if` keeps the field
    /// out of the wire entirely when thumbnails are disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<std::path::PathBuf>,
    /// Bumped every time the pin's pixels are replaced (rotate, flip).
    ///
    /// A viewer that caches the thumbnail by path needs this to know
    /// the file changed underneath it — the path itself is stable.
    #[serde(default)]
    pub revision: u64,
}

/// A message the daemon pushes to an NDJSON client unprompted.
///
/// Serialized flat, sharing the `kind` discriminator with
/// [`IpcResponse`]. The kinds are disjoint (`hello` / `event` here,
/// `ok` / `pins` / `pinned` / `error` there), so a client can switch on
/// one field across the whole inbound stream without a wrapper layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShellMessage {
    /// First line on every NDJSON connection. Doubles as the client's
    /// capability probe: no round-trip needed to learn what this daemon
    /// supports.
    Hello {
        /// [`PROTOCOL_VERSION`] of the daemon.
        protocol: u16,
        /// Daemon crate version, e.g. `"0.2.0"`.
        version: String,
        /// Optional features this build actually has, as bare strings so
        /// adding one never breaks an older client's decode.
        capabilities: Vec<String>,
    },

    /// State changed. Carries a full snapshot rather than a delta —
    /// pins number in the handful, and a stateless client is a client
    /// that cannot desynchronize.
    Event {
        /// What happened.
        event: ShellEventKind,
        /// Every live pin, after the change.
        pins: Vec<PinSummary>,
    },
}

/// Discriminator for [`ShellMessage::Event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellEventKind {
    /// A pin was added, removed, or had its pixels replaced.
    PinsChanged,
}

/// Daemon-side failures, serialized in [`IpcResponse::Error`].
///
/// Each variant carries enough context for the CLI to render a useful
/// message without inspecting the daemon's logs.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcError {
    /// Functionality is recognized but not yet implemented in this build.
    /// Used during MVP scaffolding so the IPC loop is exercisable before
    /// capture/window code lands.
    #[error("not implemented: {feature}")]
    NotImplemented {
        /// Short label for the missing feature, e.g. `"capture"`.
        feature: String,
    },

    /// Region selection (`slurp`) was canceled by the user.
    #[error("capture canceled")]
    CaptureCanceled,

    /// Screen capture failed.
    #[error("capture failed: {message}")]
    CaptureFailed {
        /// Operator-facing detail.
        message: String,
    },

    /// Clipboard contained no image, or no clipboard provider was found.
    #[error("clipboard does not contain an image")]
    ClipboardNoImage,

    /// Request was malformed or schema-incompatible.
    #[error("bad request: {message}")]
    BadRequest {
        /// Operator-facing detail.
        message: String,
    },

    /// Wire protocol versions don't match.
    #[error("protocol mismatch: client={client}, daemon={daemon}")]
    ProtocolMismatch {
        /// Version reported by the client.
        client: u16,
        /// Version compiled into the daemon.
        daemon: u16,
    },

    /// Catch-all for unexpected daemon-internal failure. The string is
    /// for humans; do not pattern-match on it.
    #[error("internal daemon error: {message}")]
    Internal {
        /// Operator-facing detail.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, &back, "json was: {json}");
    }

    #[test]
    fn requests_roundtrip() {
        roundtrip(&IpcRequest::Capture);
        roundtrip(&IpcRequest::Clipboard);
        roundtrip(&IpcRequest::List);
        roundtrip(&IpcRequest::Close { id: PinId::new(42) });
        roundtrip(&IpcRequest::CloseAll);
        roundtrip(&IpcRequest::Subscribe);
        roundtrip(&IpcRequest::PinAction {
            id: PinId::new(5),
            action: PinActionKind::RotateRight,
        });
    }

    #[test]
    fn pin_action_kinds_use_snake_case_on_the_wire() {
        // The QML plugin writes these strings by hand, so the exact
        // spelling is part of the contract, not an implementation detail.
        let cases = [
            (PinActionKind::Copy, "\"copy\""),
            (PinActionKind::Save, "\"save\""),
            (PinActionKind::RotateRight, "\"rotate_right\""),
            (PinActionKind::RotateLeft, "\"rotate_left\""),
            (PinActionKind::FlipH, "\"flip_h\""),
            (PinActionKind::FlipV, "\"flip_v\""),
        ];
        for (kind, expected) in cases {
            assert_eq!(serde_json::to_string(&kind).expect("serialize"), expected);
        }
    }

    #[test]
    fn shell_messages_roundtrip() {
        roundtrip(&ShellMessage::Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.2.0".into(),
            capabilities: vec!["thumbnails".into()],
        });
        roundtrip(&ShellMessage::Event {
            event: ShellEventKind::PinsChanged,
            pins: vec![],
        });
    }

    #[test]
    fn hello_and_response_kinds_do_not_collide() {
        // ShellMessage and IpcResponse share the `kind` field on one
        // stream; overlapping discriminators would make the client's
        // dispatch ambiguous.
        let shell = ["hello", "event"];
        let responses = ["ok", "pins", "pinned", "error"];
        for s in shell {
            assert!(!responses.contains(&s), "kind `{s}` is claimed twice");
        }
    }

    #[test]
    fn pin_summary_decodes_without_v2_fields() {
        // A v1 daemon never emits `thumbnail` or `revision`; a v2 client
        // must still be able to read its `list` output.
        let v1 = r#"{"id":1,"width":800,"height":600,"created_at_unix_ms":7}"#;
        let got: PinSummary = serde_json::from_str(v1).expect("decode v1 summary");
        assert_eq!(got.thumbnail, None);
        assert_eq!(got.revision, 0);
    }

    #[test]
    fn responses_roundtrip() {
        roundtrip(&IpcResponse::Ok);
        roundtrip(&IpcResponse::Pinned { id: PinId::new(7) });
        roundtrip(&IpcResponse::Pins {
            pins: vec![PinSummary {
                id: PinId::new(1),
                width: 1920,
                height: 1080,
                created_at_unix_ms: 1_700_000_000_000,
                thumbnail: Some("/run/user/1000/osnip/thumbs/1.png".into()),
                revision: 3,
            }],
        });
        roundtrip(&IpcResponse::Error(IpcError::CaptureCanceled));
        roundtrip(&IpcResponse::Error(IpcError::ProtocolMismatch {
            client: 1,
            daemon: 2,
        }));
    }

    #[test]
    fn pin_id_serializes_transparently() {
        let id = PinId::new(99);
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "99");
    }
}
