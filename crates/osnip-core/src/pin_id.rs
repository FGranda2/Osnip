use serde::{Deserialize, Serialize};
use std::fmt;

/// Opaque identifier for a pinned window.
///
/// IDs are assigned by the daemon and are unique for the lifetime of a
/// daemon process. They are **not** reused after a pin is closed. Wrapped
/// in a newtype so a raw `u64` from another domain can never accidentally
/// be passed where a `PinId` is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PinId(u64);

impl PinId {
    /// Construct from a raw `u64`. Daemon-internal use only.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Underlying numeric value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PinId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for PinId {
    fn from(raw: u64) -> Self {
        Self(raw)
    }
}
