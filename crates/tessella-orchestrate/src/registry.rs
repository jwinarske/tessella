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

use tessella_capture_abi::envelope::{GeometryId, SlabRef, TileId, ViewId};

/// What names one drawable across frames.
///
/// Ordered so a registry's iteration is stable, which keeps the removals it reports in a fixed
/// order rather than in a hash's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DrawableKey {
    /// The tile it covers, or `None` for a viewport-filling drawable.
    ///
    /// The wire's `TileId`, not the tile builder's: it carries the world copy and the overscaled
    /// zoom, and two tiles that differ only in the wrap are two drawables.
    pub tile: Option<TileId>,
    /// Position of the layer in the style document.
    pub layer_index: i32,
    /// Order within the layer: 1 for a fill's triangles, 2 for its outline.
    pub sub_layer_index: i32,
}

/// What a registry remembers about one drawable.
#[derive(Debug, Clone)]
struct Entry {
    id: GeometryId,
    /// The slab ranges its geometry occupies, so eviction can release them.
    ///
    /// Held here because nothing else survives the frame that encoded them: an unchanged
    /// geometry is not re-encoded, so by the time it is evicted the `Encoded` that named its
    /// bytes is long gone. The arena cannot hold them either — it counts bytes per slab and
    /// knows nothing about which drawable they belong to, which is the split that keeps it
    /// usable by a caller with a different lifecycle.
    refs: alloc::vec::Vec<SlabRef>,
    /// Which views are using it.
    ///
    /// §5.3's "removed when the last view releases", made countable. Geometry is shared and a
    /// use is not: two views over one style hold the same buffers and each has its own draw
    /// list, so a tile leaving one view's cover releases that view and removes nothing.
    users: BTreeSet<u32>,
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
    live: BTreeMap<DrawableKey, Entry>,
    /// Keys the view of the current frame has asked for, so the rest of *its* uses can be
    /// reported as released.
    seen: BTreeSet<DrawableKey>,
    /// The view the current frame is for.
    view: u32,
    /// Keys this frame allocated, so a frame that fails can give them back.
    added: BTreeSet<DrawableKey>,
    /// What `next` was when the frame began, for the same reason.
    next_at_frame_start: u64,
    next: u64,
}

impl GeometryRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a frame, forgetting which drawables the last one used.
    pub fn begin_frame(&mut self, view: ViewId) {
        self.seen.clear();
        self.added.clear();
        self.view = view.0;
        self.next_at_frame_start = self.next;
    }

    /// The id for a drawable, allocating one if it is new.
    ///
    /// Records the key as seen, so a drawable this frame did not ask for is reported by
    /// [`Self::retired`].
    pub fn id_for(&mut self, key: DrawableKey) -> GeometryId {
        self.seen.insert(key);
        let view = self.view;
        if let Some(existing) = self.live.get_mut(&key) {
            existing.users.insert(view);
            return existing.id;
        }
        let id = GeometryId(self.next);
        self.next += 1;
        self.added.insert(key);
        self.live.insert(
            key,
            Entry {
                id,
                refs: alloc::vec::Vec::new(),
                users: BTreeSet::from([view]),
            },
        );
        id
    }

    /// Records where a newly announced drawable's bytes live, so eviction can release them.
    ///
    /// Called only for a drawable that was new: one already known was not re-encoded, and its
    /// references are the ones recorded when it was.
    pub fn record_refs(&mut self, key: DrawableKey, refs: alloc::vec::Vec<SlabRef>) {
        if let Some(entry) = self.live.get_mut(&key) {
            entry.refs = refs;
        }
    }

    /// Whether no view holds this drawable yet, so its geometry has to be announced.
    ///
    /// Distinct from [`Self::is_unused_by`], and the distinction is §5.3's: geometry is shared
    /// and a use is per view. A drawable a second view picks up needs a `ViewUse` and *not* a
    /// `GeometryAdd`, because the first view already had the bytes sent. Collapsing the two
    /// re-announces every geometry every time a second view draws it.
    #[must_use]
    pub fn is_new(&self, key: &DrawableKey) -> bool {
        !self.live.contains_key(key)
    }

    /// Whether the current frame's view is not already using this drawable.
    ///
    /// This is what gates a `ViewUse`.
    #[must_use]
    pub fn is_unused_by(&self, key: &DrawableKey) -> bool {
        self.live
            .get(key)
            .is_none_or(|entry| !entry.users.contains(&self.view))
    }

    /// The drawables the current frame did not ask for, and their ids.
    ///
    /// Reported rather than removed, so a caller can emit `GeometryRemove` for each before
    /// [`Self::retire`] drops them. Splitting the two is what makes the failure case tractable:
    /// a frame that could not be written must not have retired anything.
    #[must_use]
    pub fn released(&self) -> Vec<(DrawableKey, GeometryId)> {
        self.live
            .iter()
            .filter(|(key, entry)| entry.users.contains(&self.view) && !self.seen.contains(*key))
            .map(|(key, entry)| (*key, entry.id))
            .collect()
    }

    /// The drawables no view will hold once [`Self::retire`] applies this frame's releases.
    ///
    /// §5.3's "removed when the last view releases": a drawable one view drops but another still
    /// draws keeps its geometry, and only its use goes.
    #[must_use]
    pub fn retired(&self) -> Vec<(DrawableKey, GeometryId)> {
        self.live
            .iter()
            .filter(|(key, entry)| {
                let releasing = entry.users.contains(&self.view) && !self.seen.contains(*key);
                releasing && entry.users.len() == 1
            })
            .map(|(key, entry)| (*key, entry.id))
            .collect()
    }

    /// The slab ranges a retired drawable held, for the arena to release.
    #[must_use]
    pub fn refs_of(&self, key: &DrawableKey) -> &[SlabRef] {
        self.live.get(key).map_or(&[], |entry| &entry.refs)
    }

    /// Applies this frame's releases, dropping whatever no view holds afterwards.
    pub fn retire(&mut self) {
        let view = self.view;
        for (key, entry) in &mut self.live {
            if !self.seen.contains(key) {
                entry.users.remove(&view);
            }
        }
        self.live.retain(|_, entry| !entry.users.is_empty());
    }

    /// Undoes everything the current frame allocated.
    ///
    /// An id is handed out during the binding pass, before a single record is written, so a
    /// frame that then fails has already put its new drawables here. Leaving them makes the
    /// retry believe the consumer holds geometry the failed attempt never sent — and the tile is
    /// missing until the cover changes again, which is a fault appearing one pan after its
    /// cause.
    ///
    /// The counter rewinds too. Ids are retired rather than reused when a drawable is *removed*,
    /// because a consumer was told about them; a frame that failed told nobody anything, so its
    /// numbers are free.
    pub fn rollback(&mut self) {
        for key in &self.added {
            self.live.remove(key);
        }
        self.added.clear();
        self.next = self.next_at_frame_start;
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
