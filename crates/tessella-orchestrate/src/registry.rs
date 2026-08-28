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
    /// Ring position just past the `GeometryAdd` that announced it.
    ///
    /// §13.2's acknowledgement, made answerable. The consumer publishes how far it has uploaded
    /// through [`ReverseChannel::acked_geometry`](tessella_capture_abi::reverse::ReverseChannel::acked_geometry),
    /// and a drawable whose announcement sits at or before that has bytes on the GPU rather than
    /// merely bytes on the wire.
    ///
    /// Re-announcing moves it forward, which is right: a displaced drawable's bytes are in a
    /// different slab and the consumer has to upload them again.
    announced_at: u64,
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
    /// Slab ranges this frame has decided to let go, applied on [`Self::retire`].
    releasing: Vec<SlabRef>,
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
                // Nothing has been written yet; `record_refs` fills it in when the announcement
                // lands. Until then the drawable is not acknowledged, which is correct: an id
                // handed out in the binding pass names nothing the consumer has seen.
                announced_at: u64::MAX,
            },
        );
        id
    }

    /// Records where a newly announced drawable's bytes live, so eviction can release them.
    ///
    /// Called only for a drawable that was new: one already known was not re-encoded, and its
    /// references are the ones recorded when it was.
    pub fn record_refs(&mut self, key: DrawableKey, refs: alloc::vec::Vec<SlabRef>, at: u64) {
        if let Some(entry) = self.live.get_mut(&key) {
            entry.refs = refs;
            entry.announced_at = at;
        }
    }

    /// How far the stream must be consumed before this tile's geometry is on the GPU.
    ///
    /// The furthest of its drawables' announcements: a tile is not drawable because most of it
    /// arrived. `None` for a tile this registry knows nothing about.
    ///
    /// # Why the producer can answer this at all
    ///
    /// Because it wrote the records and knows where each one landed. The consumer publishes one
    /// number — how far it has uploaded through — and the comparison is the whole of §13.2's
    /// acknowledgement. mbgl has no equivalent: it retains an ancestor until its descendants are
    /// *built*, and the gap between built and uploaded is exactly where its single-frame holes
    /// come from.
    #[must_use]
    pub fn announced_through(&self, tile: TileId) -> Option<u64> {
        self.live
            .iter()
            .filter(|(key, _)| key.tile == Some(tile))
            .map(|(_, entry)| entry.announced_at)
            .max()
    }

    /// Whether every drawable of this tile has been uploaded by the consumer.
    ///
    /// `through` is the reverse channel's acknowledged ring position. A tile nothing has
    /// announced is not acknowledged — there is nothing on the GPU to draw, which is the answer
    /// the substitution wants.
    #[must_use]
    pub fn is_acknowledged(&self, tile: TileId, through: u64) -> bool {
        self.announced_through(tile)
            .is_some_and(|announced| through >= announced)
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
    ///
    /// Returns the slab ranges those drawables held, together with any staged by
    /// [`Self::displace`], for the caller to release on the arena.
    ///
    /// # Why the caller releases and this does not
    ///
    /// Because a frame can fail after deciding what to retire and before finishing the records
    /// that say so. Releasing as each retirement was decided left a failed frame with bytes the
    /// arena thought free and the registry still holding — and the retry, working from the
    /// registry, released them a second time. The count is saturating, so the second release
    /// took it to zero and the next sweep freed a slab whose geometry was still announced and
    /// still being drawn. Staging them here means the arena moves only when the frame commits,
    /// which is the same discipline the ring, the registry and the session already keep.
    pub fn retire(&mut self) -> Vec<SlabRef> {
        let view = self.view;
        for (key, entry) in &mut self.live {
            if !self.seen.contains(key) {
                entry.users.remove(&view);
            }
        }
        let mut releasing = core::mem::take(&mut self.releasing);
        self.live.retain(|_, entry| {
            if entry.users.is_empty() {
                releasing.extend_from_slice(&entry.refs);
                return false;
            }
            true
        });
        releasing
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
        // Nothing this frame staged for release happens, because nothing this frame said so
        // reached the consumer.
        self.releasing.clear();
        self.next = self.next_at_frame_start;
    }

    /// Drawables to displace so their slabs can be freed, from those this view alone holds.
    ///
    /// DR-21's compaction: a slab whose live fraction has fallen far enough is worth emptying,
    /// because the bytes it still holds are a fraction of the bytes it occupies. Emptying it
    /// means re-announcing its survivors — they land in the current slab, and the old one sweeps.
    ///
    /// # Only what this view alone holds
    ///
    /// Displacing a drawable means telling every view that uses it, and a `ViewRelease` names one
    /// view: a frame for view A cannot release view B's use. So a drawable two views draw is left
    /// where it is, however poorly packed its slab. That is a real limit rather than a temporary
    /// one — coordinating a displacement across views needs a caller that runs all of them, which
    /// is §5.4's orchestrator and not a frame.
    ///
    /// In the common case it costs nothing: views sharing a cover share their slabs' fate, and
    /// views with different covers hold different slabs.
    ///
    /// # Why a threshold rather than a count
    ///
    /// What matters is the ratio of bytes held to bytes occupied. A slab with one live geometry
    /// out of two hundred is worth emptying; one with one out of two is not, and neither is
    /// decided by how many drawables are in it.
    #[must_use]
    pub fn displaceable(
        &self,
        arena: &crate::emit::SlabArena,
        threshold: f64,
    ) -> Vec<(DrawableKey, GeometryId)> {
        let view = self.view;
        self.live
            .iter()
            .filter(|(_, entry)| entry.users.len() == 1 && entry.users.contains(&view))
            .filter(|(_, entry)| {
                entry.refs.iter().any(|reference| {
                    arena
                        .live_fraction(reference.slab)
                        .is_some_and(|fraction| fraction > 0.0 && fraction < threshold)
                })
            })
            .map(|(key, entry)| (*key, entry.id))
            .collect()
    }

    /// Drops a displaced drawable, so the next frame announces it afresh.
    ///
    /// The caller emits its release and removal first: this is the bookkeeping, and doing it
    /// before the records were written would leave a consumer holding geometry nothing will ever
    /// mention again.
    pub fn displace(&mut self, key: &DrawableKey) {
        if let Some(entry) = self.live.remove(key) {
            // Staged rather than released, for [`Self::retire`]'s reason: a displacement decided
            // by a frame that then fails must not have moved the arena.
            self.releasing.extend_from_slice(&entry.refs);
        }
        self.seen.remove(key);
    }

    /// Every known drawable's geometry and the slab ranges it holds.
    ///
    /// The arena's counterpart to [`Self::len`]: what the registry believes is still wanted, for
    /// a caller checking the arena agrees.
    pub fn live_refs(&self) -> impl Iterator<Item = (GeometryId, &[SlabRef])> {
        self.live
            .values()
            .map(|entry| (entry.id, entry.refs.as_slice()))
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

/// Everything a stream remembers between frames.
///
/// # Why one type
///
/// Two lifetimes meet here and they are not the same. Geometry is shared across views — §5.3's
/// reason for a process-scoped id — so the registry is one. Draw order and the camera are per
/// view, and a view that has not moved should say nothing at all. Keeping them in separate
/// values left the caller to pair them up correctly for every view, every frame, which is a
/// thing to get wrong once and never notice: the symptom is traffic, not a wrong picture.
///
/// # What a parked view emits
///
/// Nothing. `DrawOrder` already suppresses an order identical to the last one it sent, and
/// always did — but the emitter built a fresh one every frame, so its memory was thrown away
/// before it could be used and every frame looked like a change. Holding it here is what makes
/// the suppression real. The camera is gated the same way, on the key `damage` already defines.
#[derive(Debug, Default)]
pub struct Session {
    /// Geometry, shared by every view.
    registry: GeometryRegistry,
    /// Per-view memory: what order was last sent, and where the camera was.
    views: BTreeMap<u32, ViewMemory>,
}

/// What one view remembers.
#[derive(Debug, Default)]
struct ViewMemory {
    order: crate::order::DrawOrder,
    camera: Option<crate::damage::CameraKey>,
    /// Whether the view has been declared and the constant textures sent.
    declared: bool,
}

impl Session {
    /// A session that has emitted nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared geometry registry, to read.
    #[must_use]
    pub fn geometry(&self) -> &GeometryRegistry {
        &self.registry
    }

    /// The shared geometry registry.
    pub fn registry(&mut self) -> &mut GeometryRegistry {
        &mut self.registry
    }

    /// The registry and this view's draw order at once.
    ///
    /// One call because a frame needs both for its whole length and they are different fields:
    /// handing them out separately means two overlapping borrows of the session, which the
    /// caller can only resolve by cloning something it should not.
    pub fn split(
        &mut self,
        view: ViewId,
        layer_count: u32,
    ) -> (&mut GeometryRegistry, &mut crate::order::DrawOrder) {
        let memory = self.views.entry(view.0).or_default();
        if memory.order.layer_count() != layer_count {
            memory.order = crate::order::DrawOrder::new(layer_count);
        }
        memory.order.clear();
        (&mut self.registry, &mut memory.order)
    }

    /// The draw order this view last sent, ready to be rebuilt against.
    ///
    /// Cleared of its bindings and keeping its emitted list, which is what lets an unchanged
    /// frame recognise itself. `DrawOrder::clear` documents that asymmetry.
    pub fn order_for(&mut self, view: ViewId, layer_count: u32) -> &mut crate::order::DrawOrder {
        let memory = self.views.entry(view.0).or_default();
        if memory.order.layer_count() != layer_count {
            memory.order = crate::order::DrawOrder::new(layer_count);
        }
        memory.order.clear();
        &mut memory.order
    }

    /// Whether this view's camera differs from the one it last *sent*.
    ///
    /// True for a view that has sent none, because a consumer with no camera cannot draw.
    ///
    /// A query, with [`Self::record_camera`] separate, for the reason the registry separates
    /// reporting from retiring: a frame that fails must not have recorded a camera it never
    /// sent, or the next frame sees no change and stays silent about a camera the consumer
    /// does not have.
    #[must_use]
    pub fn camera_differs(&self, view: ViewId, camera: &crate::damage::CameraKey) -> bool {
        self.views
            .get(&view.0)
            .and_then(|memory| memory.camera.as_ref())
            .is_none_or(|last| !last.same_as(camera))
    }

    /// Records the camera a frame has just sent.
    pub fn record_camera(&mut self, view: ViewId, camera: crate::damage::CameraKey) {
        self.views.entry(view.0).or_default().camera = Some(camera);
    }

    /// Whether this view still needs its declaration and the constant textures.
    ///
    /// A query rather than a mark, and [`Self::record_declared`] is the other half — for the
    /// reason the camera is split the same way: a frame that fails must not have recorded a
    /// declaration it never sent, or the consumer is left with a `ViewUse` naming a view it was
    /// never told about, which DR-18 calls a protocol fault.
    #[must_use]
    pub fn needs_declaring(&self, view: ViewId) -> bool {
        self.views
            .get(&view.0)
            .is_none_or(|memory| !memory.declared)
    }

    /// Records that a frame has declared this view.
    pub fn record_declared(&mut self, view: ViewId) {
        self.views.entry(view.0).or_default().declared = true;
    }

    /// Forgets a view entirely, for one that has been undeclared.
    pub fn forget(&mut self, view: ViewId) {
        self.views.remove(&view.0);
    }
}
