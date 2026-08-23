//! In-memory pin registry.
//!
//! Owns the authoritative set of live pins for the daemon process. IDs
//! are minted from a monotonically increasing `u64` and never reused —
//! this lets a CLI print an id, the user act on it later, and us be
//! certain we are addressing the same pin (or `Close` on an unknown id
//! returns the idempotent `Ok` that the contract promises).
//!
//! Storage is `std::sync::Mutex` rather than `tokio::sync::Mutex`
//! because every operation is non-async and the critical section is a
//! few HashMap ops.
//!
//! The registry is also the daemon's change bus. Every mutating method
//! publishes a full [`PinSummary`] snapshot on a `tokio::broadcast`
//! channel, which is what lets the Omarchy bar widget render live
//! without polling. Putting the publish here rather than at the call
//! sites means both writers are covered by construction: the IPC
//! handlers on the tokio thread *and* the keyboard transforms on the
//! iced thread.

use crate::thumbnail;
use image::RgbaImage;
use osnip_core::{PinId, PinSummary};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// Backlog depth for the change bus.
///
/// A subscriber that falls this far behind is not going to catch up in
/// any useful sense, and every message is a full snapshot — so lagging
/// is recoverable by resending the current state rather than replaying.
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Metadata + pixels for a single live pin. The image is held under
/// `Arc` so the GUI thread (phase 5b) can clone a handle without
/// touching the mutex.
///
/// `width` / `height` are physical pixel dimensions of the captured
/// image. `logical_width` / `logical_height` are what the window
/// should be sized to in compositor logical units — this matches the
/// region the user dragged on a HiDPI output, so the pin renders at
/// the same on-screen size as the source. For clipboard pins (where
/// logical info isn't available) we fall back to physical dimensions.
#[derive(Debug, Clone)]
struct PinEntry {
    width: u32,
    height: u32,
    logical_width: u32,
    logical_height: u32,
    created_at_unix_ms: u64,
    image: Arc<RgbaImage>,
    /// Bumped on every pixel replacement so a viewer caching the
    /// thumbnail by path can tell the file changed underneath it.
    revision: u64,
    /// Where this pin's thumbnail was written, if thumbnails are on and
    /// the write succeeded.
    thumbnail: Option<PathBuf>,
}

/// Process-wide pin registry. Cheap to clone the `Arc` wrapper; the
/// lock is fine-grained.
#[derive(Debug)]
pub struct PinRegistry {
    next_id: AtomicU64,
    /// Where to write thumbnails, or `None` to skip generating them.
    thumb_dir: Option<PathBuf>,
    /// Change bus. See [`PinRegistry::subscribe`].
    events: broadcast::Sender<Vec<PinSummary>>,
    // INVARIANT: the lock is only held for the duration of a single
    // HashMap operation (insert/remove/snapshot). No `.await` ever
    // happens with this lock held — keeps `std::sync::Mutex` safe under
    // the multi-threaded runtime and rules out cross-task deadlocks.
    pins: Mutex<HashMap<PinId, PinEntry>>,
}

impl Default for PinRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PinRegistry {
    /// Construct an empty registry with thumbnails disabled. IDs start
    /// at 1 — `0` is reserved as a sentinel for "unset" if we ever need
    /// one.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            thumb_dir: None,
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            pins: Mutex::new(HashMap::new()),
        }
    }

    /// Construct an empty registry that maintains PNG thumbnails in
    /// `dir`.
    ///
    /// Wipes `dir` first: pin ids restart from 1 every run, so a
    /// previous session's files would otherwise be served as if they
    /// belonged to this session's pins.
    #[must_use]
    pub fn with_thumbnail_dir(dir: PathBuf) -> Self {
        thumbnail::clear(&dir);
        Self {
            thumb_dir: Some(dir),
            ..Self::new()
        }
    }

    /// Subscribe to change notifications.
    ///
    /// Each message is the complete pin list *after* a change. The
    /// current state is not replayed on subscribe, so a new subscriber
    /// should pair this with one [`PinRegistry::list`] call.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<PinSummary>> {
        self.events.subscribe()
    }

    /// Broadcast the current pin list to every subscriber.
    ///
    /// Skipped entirely when nobody is listening, which is the common
    /// case: the bar plugin is the only subscriber, and it is only
    /// connected while the shell is running.
    fn publish(&self) {
        if self.events.receiver_count() == 0 {
            return;
        }
        let _ = self.events.send(self.list());
    }

    /// Write a pin's thumbnail, returning where it landed. `None` when
    /// thumbnails are disabled or the write failed — a missing preview
    /// degrades the panel to a placeholder, which is not worth failing
    /// a capture over.
    fn write_thumbnail(&self, id: PinId, image: &RgbaImage) -> Option<PathBuf> {
        let dir = self.thumb_dir.as_ref()?;
        match thumbnail::write(dir, id, image) {
            Ok(path) => Some(path),
            Err(e) => {
                tracing::warn!(pin_id = %id, error = %e, "could not write thumbnail");
                None
            }
        }
    }

    /// Whether this registry maintains thumbnails, so the daemon can
    /// advertise the capability honestly instead of by assumption.
    pub fn thumbnails_enabled(&self) -> bool {
        self.thumb_dir.is_some()
    }

    fn remove_thumbnail(&self, id: PinId) {
        if let Some(dir) = &self.thumb_dir {
            thumbnail::remove(dir, id);
        }
    }

    /// Snapshot every live pin as a `PinSummary` vector, sorted by id
    /// for stable output. Order matters for tests and for users
    /// scrolling a long list.
    pub fn list(&self) -> Vec<PinSummary> {
        let guard = match self.pins.lock() {
            Ok(g) => g,
            // Mutex poisoning means a previous holder panicked. The
            // map state is still readable; we recover rather than
            // propagate a panic into the IPC loop.
            Err(poisoned) => {
                tracing::error!("pin registry mutex was poisoned; recovering");
                poisoned.into_inner()
            }
        };
        let mut out: Vec<PinSummary> = guard
            .iter()
            .map(|(id, entry)| PinSummary {
                id: *id,
                width: entry.width,
                height: entry.height,
                created_at_unix_ms: entry.created_at_unix_ms,
                thumbnail: entry.thumbnail.clone(),
                revision: entry.revision,
            })
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    /// Remove a pin by id. Returns `true` if the pin existed; the
    /// daemon currently maps both outcomes to `IpcResponse::Ok` per
    /// the idempotent-close contract, but the bool is exposed so
    /// future audit logging can distinguish them.
    pub fn close(&self, id: PinId) -> bool {
        let existed = {
            let mut guard = match self.pins.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.remove(&id).is_some()
        };
        if existed {
            self.remove_thumbnail(id);
            self.publish();
        }
        existed
    }

    /// Drop every pin and return how many were removed.
    pub fn close_all(&self) -> usize {
        let removed: Vec<PinId> = {
            let mut guard = match self.pins.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let ids = guard.keys().copied().collect();
            guard.clear();
            ids
        };
        for id in &removed {
            self.remove_thumbnail(*id);
        }
        if !removed.is_empty() {
            self.publish();
        }
        removed.len()
    }

    /// Insert a new pin (with its captured pixels) and return its
    /// freshly minted id.
    ///
    /// `logical_size` is the size the **window** should open at, in
    /// compositor logical units — usually the size of the region the
    /// user dragged on slurp. Pass `None` when no logical size is
    /// available (e.g. clipboard pins) and the image's pixel
    /// dimensions are used instead.
    pub fn insert(
        &self,
        image: Arc<RgbaImage>,
        logical_size: Option<(u32, u32)>,
        created_at_unix_ms: u64,
    ) -> PinId {
        let id = PinId::new(self.next_id.fetch_add(1, Ordering::Relaxed));
        let width = image.width();
        let height = image.height();
        let (logical_width, logical_height) = logical_size.unwrap_or((width, height));
        // Encode before taking the lock, so the PNG is on disk by the
        // time `publish` advertises its path and no subscriber can race
        // us to a file that does not exist yet.
        let thumbnail = self.write_thumbnail(id, &image);
        {
            let mut guard = match self.pins.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.insert(
                id,
                PinEntry {
                    width,
                    height,
                    logical_width,
                    logical_height,
                    created_at_unix_ms,
                    image,
                    revision: 0,
                    thumbnail,
                },
            );
        }
        self.publish();
        id
    }

    /// Borrow a pin's pixels by cloning the `Arc`. Returns `None` if
    /// the pin is not (or no longer) registered.
    #[allow(dead_code)] // used by the GUI thread
    pub fn image(&self, id: PinId) -> Option<Arc<RgbaImage>> {
        let guard = match self.pins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(&id).map(|e| Arc::clone(&e.image))
    }

    /// Replace a pin's image in place. Used by the keyboard-driven
    /// transforms (rotate / flip) so subsequent operations compose
    /// against the latest pixels rather than the original capture.
    ///
    /// `swap_logical` controls the logical-size book-keeping:
    /// - `true` for 90°/270° rotations — the window's expected aspect
    ///   ratio inverts, so width/height swap.
    /// - `false` for flips — pixel content changes but dimensions
    ///   don't.
    ///
    /// Returns `true` if the pin existed and was updated, `false`
    /// otherwise (caller can decide whether that is a warning or
    /// silent miss).
    #[allow(dead_code)] // used by the GUI thread
    pub fn replace_image(&self, id: PinId, image: Arc<RgbaImage>, swap_logical: bool) -> bool {
        // Same ordering as `insert`: encode outside the lock and before
        // publishing. If the pin turns out to be gone we clean the file
        // back up below — a rarer race than holding the lock across a
        // PNG encode would be worth.
        let thumbnail = self.write_thumbnail(id, &image);
        let updated = {
            let mut guard = match self.pins.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.get_mut(&id) {
                Some(entry) => {
                    entry.width = image.width();
                    entry.height = image.height();
                    if swap_logical {
                        std::mem::swap(&mut entry.logical_width, &mut entry.logical_height);
                    }
                    entry.image = image;
                    entry.revision += 1;
                    if thumbnail.is_some() {
                        entry.thumbnail = thumbnail;
                    }
                    true
                }
                None => false,
            }
        };
        if !updated {
            // The pin was closed while we were encoding; do not leave
            // an orphan file behind for the next pin to inherit.
            self.remove_thumbnail(id);
            return false;
        }
        self.publish();
        true
    }

    /// Logical (compositor-units) size the pin window should open at.
    /// Returns `None` if the pin is not registered.
    #[allow(dead_code)] // used by the GUI thread
    pub fn logical_size(&self, id: PinId) -> Option<(u32, u32)> {
        let guard = match self.pins.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.get(&id).map(|e| (e.logical_width, e.logical_height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(width: u32, height: u32) -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_pixel(
            width,
            height,
            image::Rgba([0, 0, 0, 255]),
        ))
    }

    #[test]
    fn new_registry_is_empty() {
        let r = PinRegistry::new();
        assert!(r.list().is_empty());
    }

    #[test]
    fn insert_then_list_then_close() {
        let r = PinRegistry::new();
        let id1 = r.insert(fixture(800, 600), None, 1_700_000_000_000);
        let id2 = r.insert(fixture(1920, 1080), None, 1_700_000_000_500);
        let listing = r.list();
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[0].id, id1);
        assert_eq!(listing[0].width, 800);
        assert_eq!(listing[1].id, id2);
        assert_eq!(listing[1].width, 1920);
        assert!(r.close(id1));
        assert!(!r.close(id1), "second close on same id is a miss");
        assert_eq!(r.list().len(), 1);
    }

    #[test]
    fn close_all_clears_and_counts() {
        let r = PinRegistry::new();
        r.insert(fixture(1, 1), None, 0);
        r.insert(fixture(2, 2), None, 0);
        assert_eq!(r.close_all(), 2);
        assert!(r.list().is_empty());
        assert_eq!(r.close_all(), 0);
    }

    #[test]
    fn ids_are_not_reused() {
        let r = PinRegistry::new();
        let id1 = r.insert(fixture(1, 1), None, 0);
        r.close(id1);
        let id2 = r.insert(fixture(1, 1), None, 0);
        assert_ne!(id1, id2);
    }

    #[test]
    fn image_returns_pixels_by_id() {
        let r = PinRegistry::new();
        let id = r.insert(fixture(4, 3), None, 0);
        let img = r.image(id).expect("image present");
        assert_eq!(img.width(), 4);
        assert_eq!(img.height(), 3);
        r.close(id);
        assert!(r.image(id).is_none());
    }

    #[test]
    fn logical_size_falls_back_to_image_dimensions() {
        let r = PinRegistry::new();
        let id = r.insert(fixture(1393, 220), None, 0);
        assert_eq!(r.logical_size(id), Some((1393, 220)));
    }

    #[test]
    fn replace_image_swaps_logical_when_requested() {
        let r = PinRegistry::new();
        let id = r.insert(fixture(800, 600), Some((400, 300)), 0);
        let rotated = Arc::new(RgbaImage::from_pixel(600, 800, image::Rgba([0, 0, 0, 255])));
        assert!(r.replace_image(id, rotated, true));
        assert_eq!(r.logical_size(id), Some((300, 400)));
        let img = r.image(id).expect("image present");
        assert_eq!(img.width(), 600);
        assert_eq!(img.height(), 800);
    }

    #[test]
    fn replace_image_keeps_logical_when_not_requested() {
        let r = PinRegistry::new();
        let id = r.insert(fixture(800, 600), Some((400, 300)), 0);
        let flipped = Arc::new(RgbaImage::from_pixel(800, 600, image::Rgba([0, 0, 0, 255])));
        assert!(r.replace_image(id, flipped, false));
        assert_eq!(r.logical_size(id), Some((400, 300)));
    }

    #[test]
    fn replace_image_unknown_id_is_miss() {
        let r = PinRegistry::new();
        let new_img = Arc::new(RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255])));
        assert!(!r.replace_image(PinId::new(99), new_img, false));
    }

    #[test]
    fn list_carries_thumbnail_and_revision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = PinRegistry::with_thumbnail_dir(dir.path().to_path_buf());
        let id = r.insert(fixture(40, 20), None, 0);

        let listed = r.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revision, 0);
        let thumb = listed[0].thumbnail.clone().expect("thumbnail path");
        assert!(thumb.exists(), "thumbnail was advertised but not written");

        // A transform must bump the revision, or a viewer caching by
        // path has no way to know the file changed.
        let rotated = Arc::new(RgbaImage::from_pixel(20, 40, image::Rgba([0, 0, 0, 255])));
        assert!(r.replace_image(id, rotated, true));
        assert_eq!(r.list()[0].revision, 1);
    }

    #[test]
    fn closing_a_pin_removes_its_thumbnail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = PinRegistry::with_thumbnail_dir(dir.path().to_path_buf());
        let id = r.insert(fixture(10, 10), None, 0);
        let thumb = r.list()[0].thumbnail.clone().expect("thumbnail path");
        assert!(thumb.exists());

        assert!(r.close(id));
        assert!(!thumb.exists(), "thumbnail outlived its pin");
    }

    #[test]
    fn close_all_removes_every_thumbnail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = PinRegistry::with_thumbnail_dir(dir.path().to_path_buf());
        r.insert(fixture(10, 10), None, 0);
        r.insert(fixture(10, 10), None, 0);
        let thumbs: Vec<_> = r.list().into_iter().filter_map(|p| p.thumbnail).collect();
        assert_eq!(thumbs.len(), 2);

        assert_eq!(r.close_all(), 2);
        for t in thumbs {
            assert!(!t.exists(), "{} outlived close_all", t.display());
        }
    }

    #[test]
    fn with_thumbnail_dir_wipes_a_previous_session() {
        // Pin ids restart at 1 every run, so a leftover 1.png would be
        // served as if it belonged to this session's first pin.
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = dir.path().join("1.png");
        std::fs::write(&stale, b"not really a png").expect("write stale");

        let _r = PinRegistry::with_thumbnail_dir(dir.path().to_path_buf());
        assert!(!stale.exists(), "stale thumbnail survived startup");
    }

    #[test]
    fn registry_without_thumbnails_reports_none() {
        let r = PinRegistry::new();
        assert!(!r.thumbnails_enabled());
        r.insert(fixture(10, 10), None, 0);
        assert_eq!(r.list()[0].thumbnail, None);
    }

    #[tokio::test]
    async fn every_mutation_publishes_a_snapshot() {
        let r = PinRegistry::new();
        let mut rx = r.subscribe();

        let id = r.insert(fixture(10, 10), None, 0);
        assert_eq!(rx.recv().await.expect("insert event").len(), 1);

        let flipped = Arc::new(RgbaImage::from_pixel(10, 10, image::Rgba([9, 9, 9, 255])));
        assert!(r.replace_image(id, flipped, false));
        let after_replace = rx.recv().await.expect("replace event");
        assert_eq!(after_replace[0].revision, 1);

        assert!(r.close(id));
        assert!(rx.recv().await.expect("close event").is_empty());

        r.insert(fixture(1, 1), None, 0);
        let _ = rx.recv().await.expect("second insert event");
        assert_eq!(r.close_all(), 1);
        assert!(rx.recv().await.expect("close_all event").is_empty());
    }

    #[tokio::test]
    async fn no_op_mutations_do_not_publish() {
        // A subscriber that wakes for nothing makes the bar redraw for
        // nothing; closing an already-closed pin is not a change.
        let r = PinRegistry::new();
        let mut rx = r.subscribe();

        assert!(!r.close(PinId::new(404)));
        assert_eq!(r.close_all(), 0);

        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "a no-op mutation published an event"
        );
    }

    #[test]
    fn logical_size_records_explicit_value() {
        // Mimics the HiDPI capture case: physical 1393x220, logical 929x147.
        let r = PinRegistry::new();
        let id = r.insert(fixture(1393, 220), Some((929, 147)), 0);
        assert_eq!(r.logical_size(id), Some((929, 147)));
        // Image is still physical.
        let img = r.image(id).expect("image present");
        assert_eq!(img.width(), 1393);
        assert_eq!(img.height(), 220);
    }
}
