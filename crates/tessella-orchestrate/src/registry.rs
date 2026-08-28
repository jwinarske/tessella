//! Geometry ids that outlive the frame that first announced them.
//!
//! # What this is for
//!
//! §5.3 describes geometry as process-scoped and refcounted, and the ABI is written to it:
//! `GeometryAdd` announces, `ViewUse` binds, `ViewRelease` and `GeometryRemove` let go. The
//! producer has not implemented any of it — it hands out ids dense from zero every frame, so the
//! same id names a different tile after a pan and nothing is ever released. `GeometryId`'s own
//! documentation now says so, and `geometry_ids.rs` holds it to that.
//!
//! This is the first piece of closing that gap: an id that belongs to a *drawable* rather than
//! to a position in this frame's cover.
//!
//! # What identifies a drawable across frames
//!
//! The tile it covers, the layer it belongs to, and its order within that layer. Not its
//! position in the cover, which is what the current allocator uses and what makes an id move
//! when the camera pans. A fill's triangles and its outline are two drawables over one bucket
//! and differ in the sub-layer, which is why that is in the key rather than the bucket index.
//!
//! A drawable that covers no tile — a background — is keyed with `None`, and there is one per
//! tile of the cover, so the tile is part of its key too and is simply absent.
//!
//! # What it deliberately does not do yet
//!
//! Nothing here decides *when* to emit. Knowing an id is stable is what makes emitting only new
//! geometry possible; doing it also needs the arena to keep a retained geometry's slabs alive
//! across frames, which it cannot — it is rebuilt per frame, and `frame::emit` seals it as it
//! goes. That is the next piece, and it is a change to the arena's lifetime rather than to this.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use tessella_capture_abi::envelope::GeometryId;

use crate::tile::TileId;

/// What names one drawable across frames.
///
/// Ordered so a registry's iteration is stable, which keeps the removals it reports in a fixed
/// order rather than in a hash's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DrawableKey {
    /// The tile it covers, or `None` for a viewport-filling drawable.
    pub tile: Option<TileId>,
    /// Position of the layer in the style document.
    pub layer_index: i32,
    /// Order within the layer: 1 for a fill's triangles, 2 for its outline.
    pub sub_layer_index: i32,
}

/// Hands out geometry ids and remembers which drawable each belongs to.
///
/// An id is allocated the first time its drawable is seen and returned unchanged afterwards, so
/// a tile that stays in the cover across a pan keeps the id it had. Ids are never reused: a
/// removed drawable's id is retired rather than handed to the next allocation, because a
/// consumer that has been told to remove one and then meets it again has no way to know the
/// second is a different thing.
#[derive(Debug, Default)]
pub struct GeometryRegistry {
    live: BTreeMap<DrawableKey, GeometryId>,
    /// Keys seen since [`Self::begin_frame`], so the rest can be reported as gone.
    seen: BTreeSet<DrawableKey>,
    next: u64,
}

impl GeometryRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a frame, forgetting which drawables the last one used.
    pub fn begin_frame(&mut self) {
        self.seen.clear();
    }

    /// The id for a drawable, allocating one if it is new.
    ///
    /// Records the key as seen, so a drawable this frame did not ask for is reported by
    /// [`Self::retired`].
    pub fn id_for(&mut self, key: DrawableKey) -> GeometryId {
        self.seen.insert(key);
        if let Some(existing) = self.live.get(&key) {
            return *existing;
        }
        let id = GeometryId(self.next);
        self.next += 1;
        self.live.insert(key, id);
        id
    }

    /// Whether this drawable was already known before the current frame asked for it.
    ///
    /// This is what lets an emitter send `GeometryAdd` only for what is new. It answers about
    /// the *registry*, not about the wire: a caller that asked and then failed to emit has told
    /// the registry a thing it did not do, which is why `frame::emit` rewinds on failure.
    #[must_use]
    pub fn is_new(&self, key: &DrawableKey) -> bool {
        !self.live.contains_key(key)
    }

    /// The drawables the current frame did not ask for, and their ids.
    ///
    /// Reported rather than removed, so a caller can emit `GeometryRemove` for each before
    /// [`Self::retire`] drops them. Splitting the two is what makes the failure case tractable:
    /// a frame that could not be written must not have retired anything.
    #[must_use]
    pub fn retired(&self) -> Vec<(DrawableKey, GeometryId)> {
        self.live
            .iter()
            .filter(|(key, _)| !self.seen.contains(*key))
            .map(|(key, id)| (*key, *id))
            .collect()
    }

    /// Drops everything [`Self::retired`] reported.
    pub fn retire(&mut self) {
        self.live.retain(|key, _| self.seen.contains(key));
    }

    /// How many drawables are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// True when nothing is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}
