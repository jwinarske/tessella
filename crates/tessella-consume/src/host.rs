//! Reading the stream, and planning what a frame draws.
//!
//! This runs the other three modules as one thing: records come off the ring, geometry and uses
//! are joined, an order is held until the camera that commits it arrives, and the result is a plan
//! of batches.
//!
//! # What it does not do
//!
//! It does not upload, and it does not decide when the producer may reuse anything.
//!
//! Two positions matter and they are not the same. The ring's tail is how far the stream has been
//! *read*, and it moves as records are consumed. The acknowledged position is how far the consumer
//! has *uploaded*, and §13.2 holds the producer to not reusing a slab until the consumer has
//! acknowledged past the point the geometry in it was announced. Only a backend knows when its
//! driver's copy completed, so [`Plan::announced_through`] reports the position to acknowledge and
//! the acknowledging is the backend's call.
//!
//! A backend that never acknowledges stalls the producer. That is the correct failure: the
//! alternative is the producer reusing bytes the GPU is still reading.
//!
//! # Pull, not push
//!
//! Reading updates state; planning reads it. Nothing is handed to a callback, so a backend decides
//! when to resolve geometry and in what order to do its work — a Vulkan one wants its transfers
//! staged before the render pass, which an inverted drain would deny it. The plan owns ids and
//! indexes and borrows nothing, so holding it does not borrow the host and does not stop the next
//! read.
//!
//! [`Host::plan`] keeps each view's batches and returns them, so an unchanged frame is served from
//! the cache rather than re-collapsed. Planning the quad's order measured 375 microseconds; serving
//! it cached measures 0.02.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use tessella_capture_abi::envelope::{
    AttributeDesc, CameraUpdate, Extent, GeometryAdd, GeometryRemove, OrderEntry, OrderEpoch,
    OrderUpdate, Segment, Span, StencilTile, StencilTiles, TextureId, TextureRef, TextureUpdate,
    TileId, UboUpdate, ViewDeclare, ViewId, ViewRelease, ViewTarget, ViewUndeclare, ViewUse,
    WireRecord,
};
use tessella_capture_abi::ring::Consumer;
use tessella_capture_abi::{CameraMode, EnvelopeKind, TextureChannelDataType, TexturePixelType};

use crate::batch::{Batches, Program, collapse_into};
use crate::join::{Announcement, Joiner};
use crate::stencil::{self, Partition};
use crate::upload::{self, Uploads};

/// What one read of the stream did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    /// Records consumed.
    pub records: u64,
    /// Records whose kind this build does not know.
    ///
    /// Not an error: the ABI is revisioned and a newer producer may send a kind this consumer
    /// predates. Counted rather than ignored, because a stream that is *mostly* unknown is a
    /// version mismatch presenting as a blank map, and a blank map has many other explanations.
    pub unknown: u64,
    /// Records naming a view that was never declared, and were dropped for it.
    ///
    /// A protocol fault rather than a malformed record: the bytes parse. The ABI states the rule --
    /// a `ViewUse` "naming a view the consumer has not seen declared is a protocol fault", and
    /// `ViewDeclare` is "ordered ahead of any `ViewUse` naming the view" -- so a use that arrives
    /// first is a producer out of order, not a producer sending rubbish, and the two are worth
    /// telling apart when a map comes out blank.
    pub undeclared: u64,
    /// Records this build knows but could not read, the bytes being malformed.
    ///
    /// Separate from `unknown` on purpose. An unknown kind is a newer producer; a known kind that
    /// will not parse is a corrupt stream or a size disagreement, and the two want different
    /// answers from whoever is looking.
    pub malformed: u64,
}

impl Progress {
    fn merge(&mut self, other: Self) {
        self.records += other.records;
        self.unknown += other.unknown;
        self.undeclared += other.undeclared;
        self.malformed += other.malformed;
    }
}

/// An order held until its camera arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Order {
    epoch: OrderEpoch,
    entries: Vec<OrderEntry>,
}

/// A frame ready to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The view this draws.
    pub view: ViewId,
    /// The epoch the camera and the order agree on.
    pub epoch: OrderEpoch,
    /// Acknowledge this position once the uploads for this frame have completed.
    ///
    /// The highest ring position at which any geometry this plan draws was announced. Below it the
    /// producer may reuse slabs; at or above it, it may not until this is acknowledged.
    pub announced_through: u64,
}

/// What makes a view draw into a texture rather than onto the screen.
///
/// DR-25's offscreen view. A heatmap draws its kernels into a half-resolution target and then draws
/// that target through a color ramp; a hillshade prepare pass has the same shape. Two of the
/// eighteen families cannot draw at all without one.
///
/// # Why nesting is forbidden, and what that buys
///
/// The producer refuses a target whose parent is itself offscreen -- `ViewError::NestedTarget` --
/// and this refuses one too. So the children of a view are a flat set rather than a tree, and a
/// consumer drawing a frame runs them in any order and then draws the parent. No recursion, no
/// cycle to detect, and no depth to bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The view this is sized against, and in whose frame it is drawn first.
    pub parent: ViewId,
    /// The id this view's output is bound by, in `TextureUpdate`'s id space.
    ///
    /// Never the subject of one: nothing uploads pixels to a render target. So a consumer holds an
    /// image under this id that no `Upload::Texture` will ever name.
    pub texture: TextureId,
    /// Numerator of the size against the parent.
    pub scale_num: u16,
    /// Denominator of the size against the parent.
    pub scale_den: u16,
    /// What channels its texels carry.
    pub format: TexturePixelType,
    /// What one channel holds.
    ///
    /// Not derivable from the format: a heatmap target is `RGBA` *and* `HalfFloat` together,
    /// because "the kernel sum runs past one, and an 8-bit target clips it to a flat cap over every
    /// dense cluster -- which reads as a ramp that has lost its top stop rather than as a format
    /// bug".
    pub channel: TextureChannelDataType,
}

impl Target {
    /// The target's size, from the parent's.
    ///
    /// Rounded up, so a parent of an odd width still gives a half-resolution target that covers it:
    /// 65 at one half is 33 and not 32, and a target one pixel short of its parent samples its own
    /// edge where the parent's last column reads.
    ///
    /// At least one pixel each way. A scale that rounds to nothing is a target `vkCreateImage`
    /// refuses, and the caller should not have to know which call says so.
    #[must_use]
    pub fn size(&self, parent: Extent) -> Extent {
        let scale = |at: u32| {
            let num = u64::from(at).saturating_mul(u64::from(self.scale_num));
            let den = u64::from(self.scale_den).max(1);
            u32::try_from(num.div_ceil(den)).unwrap_or(u32::MAX).max(1)
        };
        Extent {
            width: scale(parent.width),
            height: scale(parent.height),
        }
    }
}

/// A view's clip masks: one per tile, and which tiles each layer group clips.
///
/// `StencilTiles` is "the tile set a layer group wants clipped, emitted on change only", one record
/// per (view, layer group), each carrying a run of [`StencilTile`] -- a tile and the column-major
/// matrix its mask quad is drawn by.
///
/// Both halves are kept because both are needed and the record carries them together: the tile sets
/// decide the [`Partition`], and the matrices are what a mask is actually drawn with. Dropping the
/// matrices would mean decoding the same record again later.
///
/// # The partition is computed here, not per frame
///
/// `StencilTiles` arrives on change, so the partition changes only then -- recomputing it per frame
/// would be work for a map that is not moving. It is rebuilt when a group's tiles are replaced,
/// which is the only thing that can change it.
///
/// Per *record*, which means a zoom crossing that moves every group pays one rebuild per group
/// rather than one for the frame. That is the cost of `clips` being a shared accessor: a staleness
/// flag would need `&mut` to bring level, and a `&self` reader would then have to be able to hand
/// back a stale partition, which is a worse thing to have than a few extra rebuilds.
///
/// A rebuild between two groups' records is over a mixed state -- one group's new tiles beside
/// another's old ones -- and is replaced by the next one. No frame sees it: [`Host::read`] drains
/// what is available before anything calls [`Host::plan`].
#[derive(Debug, Clone, Default)]
pub struct Clips {
    /// One mask per tile, whichever group named it, with the matrix it is drawn by.
    masks: BTreeMap<TileId, [f32; 16]>,
    /// Each layer group's own tile set, which is what decides whose field clears whose.
    groups: BTreeMap<i32, BTreeSet<TileId>>,
    /// The assignments, rebuilt whenever the groups change.
    partition: Partition,
}

impl Clips {
    /// Replaces one layer group's tiles and rebuilds the partition.
    fn set(&mut self, layer: i32, tiles: &[StencilTile]) {
        for tile in tiles {
            self.masks.insert(tile.tile, tile.matrix);
        }
        self.groups
            .insert(layer, tiles.iter().map(|tile| tile.tile).collect());

        // Every bit of the byte. A consumer holding some back for its own use would pass fewer --
        // this one has its own depth attachment, so the stencil is entirely the clip's.
        let every: BTreeSet<TileId> = self.groups.values().flatten().copied().collect();
        self.partition = stencil::partition(&every, &self.groups, stencil::ALL_BITS);

        // A tile no group names any more has no mask to draw. Dropped rather than kept, because the
        // matrices are a frame's worth of mask draws and a stale one is a quad drawn over the map.
        self.masks.retain(|tile, _| every.contains(tile));
    }

    /// How each tile's mask is painted and how its geometry tests against it.
    ///
    /// A view that has had no `StencilTiles` answers the default, which is `partitioned: false` with
    /// no tiles -- and `Partition`'s own words for that are "a tile absent here has no mask; its
    /// geometry is left unclipped". So a consumer needs no special case for a view with no clips.
    #[must_use]
    pub fn partition(&self) -> &Partition {
        &self.partition
    }

    /// Each tile's mask and the matrix it is drawn by, in tile order.
    pub fn masks(&self) -> impl Iterator<Item = (TileId, &[f32; 16])> + '_ {
        self.masks.iter().map(|(tile, matrix)| (*tile, matrix))
    }

    /// How many masks this view draws.
    #[must_use]
    pub fn len(&self) -> usize {
        self.masks.len()
    }

    /// Whether this view has no masks, and so clips nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }
}

/// One frame's worth of what a backend draws, from a single borrow of the host.
///
/// The batches *and* the joiner, because recording needs both: the batches say which program and
/// which drawables, and the joiner says which tile each drawable is in -- which is the stencil
/// reference it draws with. Returning only the batches made that impossible to express. The
/// batches borrow the host, so asking it for the joiner afterwards is
///
/// ```text
/// error[E0502]: cannot borrow `host` as immutable because it is also borrowed as mutable
/// ```
///
/// and there is no way round it from outside: cloning the batches defeats the cache
/// [`Host::plan`] exists to serve, and taking the joiner first conflicts the other way.
///
/// Both references come from the one borrow, so the guarantee [`Host::plan`] had is kept -- these
/// batches are the ones just planned, and not a cache a caller might have forgotten to bring
/// level.
#[derive(Debug)]
pub struct Frame<'a> {
    /// What this frame is: the view, the epoch, and how far to acknowledge.
    pub plan: Plan,
    /// The batches to walk, in draw order.
    pub batches: &'a Batches,
    /// Where a drawable's tile is looked up.
    pub joiner: &'a Joiner,
    /// The masks to draw and the assignments to draw them with.
    pub clips: &'a Clips,
    /// The camera this frame draws under. Its `order_epoch` is [`Plan::epoch`].
    ///
    /// From the same borrow as the batches, and for the same reason the batches are here rather
    /// than fetched: what a caller gets is the camera this plan was gated on, not whichever one
    /// the host holds by the time it asks. [`Host::camera`] is the other question -- what a view's
    /// camera is when no frame can be planned at all.
    pub camera: &'a CameraUpdate,
}

/// Reads the stream and plans frames from it.
#[derive(Debug, Clone, Default)]
pub struct Host {
    joiner: Joiner,
    orders: BTreeMap<ViewId, Order>,
    /// Each view's camera, whole.
    ///
    /// The epoch alone was kept here once, because gating `plan` on it was all anything did with a
    /// camera. Nothing else could then be done with one: `depth_range_size` and
    /// `opaque_pass_cutoff` arrive nowhere else, so no consumer could honor §11.7's
    /// opaque/translucent split, and a `CameraMode::Consumer` view -- which owns its own placement
    /// -- got no matrices to place anything with.
    cameras: BTreeMap<ViewId, CameraUpdate>,
    read_through: u64,
    progress: Progress,
    uploads: Uploads,
    /// The batches each view last planned, kept so an unchanged frame costs nothing.
    plans: BTreeMap<ViewId, Batches>,
    /// Views whose cached batches no longer describe what they draw.
    ///
    /// The order's epoch is not enough on its own. A geometry re-announced with a different
    /// permutation is a different program, which changes where the collapse breaks, and it arrives
    /// without a new order. So anything that changes what a view draws marks it here: a new order,
    /// a use, a release, an announcement a view already holds, and a retire of geometry it uses.
    stale: BTreeSet<ViewId>,
    /// Where each view's last plan was announced through, beside the batches it belongs to.
    announced: BTreeMap<ViewId, u64>,
    /// The views the producer has declared, and which side owns each one's camera.
    ///
    /// DR-18's per-view state. A view is in here from its `ViewDeclare` until its `ViewUndeclare`,
    /// and a record naming one that is not is dropped -- the ABI calls that a protocol fault.
    views: BTreeMap<ViewId, CameraMode>,
    /// The offscreen views, and what each draws into.
    targets: BTreeMap<ViewId, Target>,
    /// Each view's clip masks and the partition over them.
    clips: BTreeMap<ViewId, Clips>,
    /// What a view with no `StencilTiles` gets: no masks, and a partition that clips nothing.
    ///
    /// Held rather than returned by value because [`Frame`] carries a reference, and an empty one
    /// is the same for every view. `Partition`'s own words for it are "a tile absent here has no
    /// mask; its geometry is left unclipped", so a consumer needs no special case.
    unclipped: Clips,
}

impl Host {
    /// A host that has read nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drains everything the producer has published.
    ///
    /// Returns what this call did; [`Host::progress`] has the running totals.
    pub fn read(&mut self, consumer: &mut Consumer) -> Progress {
        let mut did = Progress::default();
        loop {
            // Where this record starts, which is what a geometry announced in it is acknowledged
            // against. Read before peeking because the token `peek` hands back is opaque -- it is
            // only spendable on `advance`, deliberately, so the byte count is not available from
            // it and the ring's own monotonic position is the answer.
            //
            // The start rather than the end, and that is conservative in the safe direction: a
            // producer comparing an announcement's position against the acknowledged one treats
            // an equal position as acknowledged, and anything later as not.
            let at = consumer.position();
            let Some(record) = consumer.peek() else {
                break;
            };
            let consumed = record.consumed();
            let outcome = self.dispatch(record.kind, record.record, record.payload, at);
            match outcome {
                Outcome::Read => {}
                Outcome::Unknown => did.unknown += 1,
                Outcome::Malformed => did.malformed += 1,
                Outcome::Undeclared => did.undeclared += 1,
            }
            did.records += 1;
            consumer.advance(consumed);
            self.read_through = consumer.position();
        }
        self.progress.merge(did);
        did
    }

    /// Running totals since this host was made.
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.progress
    }

    /// How far the stream has been read, in bytes.
    #[must_use]
    pub fn read_through(&self) -> u64 {
        self.read_through
    }

    /// The joined geometry, for a backend resolving what a batch names.
    #[must_use]
    pub fn joiner(&self) -> &Joiner {
        &self.joiner
    }

    /// The bytes waiting to reach the device, and where they are.
    ///
    /// Uniform blocks and texture pixels are spans into the ring, so they are copied here during
    /// the read -- the ring's bytes belong to the producer again as soon as the tail passes, and
    /// reading the next record is what moves it. Geometry is not copied, because a slab stays
    /// resolvable until it is acknowledged.
    #[must_use]
    pub fn uploads(&self) -> &Uploads {
        &self.uploads
    }

    /// Drops the upload work and its bytes once the backend has finished with them.
    ///
    /// Keeps the buffer's capacity, so a steady state copies into the same allocation every frame.
    pub fn uploads_done(&mut self) {
        self.uploads.clear();
    }

    /// Whether a view's camera and order agree, so it has something to draw.
    #[must_use]
    pub fn ready(&self, view: ViewId) -> bool {
        match (self.orders.get(&view), self.cameras.get(&view)) {
            (Some(order), Some(camera)) => order.epoch == camera.order_epoch,
            _ => false,
        }
    }

    /// One frame of a view: what to draw, the batches to walk and the joiner to resolve them.
    ///
    /// Planned if the batches are stale and returned from the cache if not.
    ///
    /// `None` when the view has no order, no camera, or a camera naming an epoch the held order
    /// does not establish -- §11.7's "hold `CameraUpdate` until its `orderEpoch` is held". Drawing
    /// under a camera that does not match the order is this frame's geometry through the last
    /// frame's matrices.
    ///
    /// # Why this borrows the host
    ///
    /// Because the host keeps the batches. A still map re-collapsed an identical order every
    /// frame, which measured 371 microseconds at the quad's entry count -- for byte-identical
    /// output, while the producer that fed it was emitting nothing at all. The cache is what makes
    /// the consumer's parked frame as cheap as the producer's.
    ///
    /// The cost is this signature: drawing holds a borrow, so acknowledging has to follow it
    /// rather than interleave. That is the order a backend works in anyway -- draw, then say the
    /// copies are done -- but it is a real constraint and not a free one.
    pub fn plan(&mut self, view: ViewId) -> Option<Frame<'_>> {
        let order = self.orders.get(&view)?;
        let camera = self.cameras.get(&view)?;
        if camera.order_epoch != order.epoch {
            return None;
        }

        if self.stale.remove(&view) || !self.plans.contains_key(&view) {
            let batches = self.plans.entry(view).or_default();
            let joiner = &self.joiner;
            let mut announced_through = 0;
            collapse_into(&order.entries, batches, |id| {
                let drawable = joiner.drawable(id, view)?;
                announced_through = announced_through.max(drawable.geometry.announced_at);
                Some(Program {
                    builtin_shader: drawable.geometry.add.builtin_shader,
                    permutation_key: drawable.geometry.add.permutation_key,
                    texture_refs: &drawable.geometry.texture_refs,
                })
            });
            self.announced.insert(view, announced_through);
        }

        Some(Frame {
            plan: Plan {
                view,
                epoch: order.epoch,
                announced_through: self.announced.get(&view).copied().unwrap_or_default(),
            },
            batches: self.plans.get(&view).expect("planned above"),
            joiner: &self.joiner,
            clips: self.clips.get(&view).unwrap_or(&self.unclipped),
            camera,
        })
    }

    /// What an offscreen view draws into, or `None` for one that draws on the screen.
    #[must_use]
    pub fn target(&self, view: ViewId) -> Option<&Target> {
        self.targets.get(&view)
    }

    /// The offscreen views that feed `view`, which a frame draws before it.
    ///
    /// Flat rather than recursive, because nesting is refused: a target whose parent is itself
    /// offscreen is malformed here and `ViewError::NestedTarget` on the producer's side. So this is
    /// the whole of what a frame has to draw first, and the order among them does not matter --
    /// they write different textures and none samples another.
    pub fn feeding(&self, view: ViewId) -> impl Iterator<Item = ViewId> + '_ {
        self.targets
            .iter()
            .filter(move |(_, target)| target.parent == view)
            .map(|(child, _)| *child)
    }

    /// Which side owns a view's camera, or `None` for a view that is not declared.
    ///
    /// What DR-9 settles per view, and the thing a consumer cannot guess: in consumer mode every
    /// per-drawable matrix on the wire is advisory and a tile's placement comes from its own id, so
    /// a backend that read them as authoritative would place geometry through the producer's camera
    /// instead of its own.
    #[must_use]
    pub fn camera_mode(&self, view: ViewId) -> Option<CameraMode> {
        self.views.get(&view).copied()
    }

    /// A view's last readable camera, whatever epoch it names.
    ///
    /// [`Frame::camera`] is what a frame draws under, and is the one to reach for while drawing --
    /// this one may name an epoch no held order establishes. It is here for what is true of a view
    /// between frames: whether a camera has arrived at all, and what the producer last said about
    /// the light and the projection.
    #[must_use]
    pub fn camera(&self, view: ViewId) -> Option<&CameraUpdate> {
        self.cameras.get(&view)
    }

    /// Whether a view is declared.
    #[must_use]
    pub fn declared(&self, view: ViewId) -> bool {
        self.views.contains_key(&view)
    }

    /// Drops a view and everything held for it.
    ///
    /// What a `ViewUndeclare` costs. Every map here keyed by a view loses its entry, and the
    /// joiner loses that view's uses -- but not the announcements behind them, because geometry is
    /// shared and another view may still hold it. A consumer that dropped only the order would keep
    /// the clips, the camera and the uses of a view that no longer exists, which on a cluster
    /// adding and removing insets is a leak per inset.
    fn forget(&mut self, view: ViewId) {
        // The views that fed it go too, and wholly: an offscreen view exists to be sampled by its
        // parent, so one whose parent is gone is a pass nothing will ever read. Dropping only its
        // *target* would leave it declared and drawing, onto a screen it was never meant to reach.
        //
        // Collected first, and one level deep by construction: nesting is refused, so a child has
        // no children and this cannot recurse.
        let orphaned: Vec<ViewId> = self.feeding(view).collect();
        for child in orphaned {
            self.forget(child);
        }

        self.views.remove(&view);
        self.targets.remove(&view);
        self.orders.remove(&view);
        self.cameras.remove(&view);
        self.clips.remove(&view);
        self.plans.remove(&view);
        self.announced.remove(&view);
        self.stale.remove(&view);
        self.joiner.release_view(view);
    }

    /// A view's clip masks, whether or not it has a frame to draw.
    ///
    /// [`Frame`] carries the same thing for a view that is ready; this is for a caller that wants
    /// them before the camera and the order agree -- a backend sizing its mask buffer, which is a
    /// function of the tile count rather than of the frame.
    #[must_use]
    pub fn clips(&self, view: ViewId) -> &Clips {
        self.clips.get(&view).unwrap_or(&self.unclipped)
    }

    /// A view's batches, as the last [`Host::plan`] of it left them.
    ///
    /// `None` for a view that has never been planned, or whose camera and order did not agree when
    /// it was. [`Host::plan`] is what brings them level; this only reads them.
    ///
    /// # Why this exists beside `Frame`
    ///
    /// Because a frame can have more than one view, and `plan` cannot serve two. It takes `&mut
    /// self` and hands back references into the host, so the second call is refused while the first
    /// one's `Frame` is alive -- and DR-25's offscreen views mean a backend needs the child's
    /// batches *and* the parent's in one recording.
    ///
    /// So a caller drawing several views plans each in turn, dropping each `Frame`, and then reads
    /// them back through this. What it gives up is the guarantee `Frame` carries -- that these are
    /// the batches just planned rather than a cache someone forgot to bring level -- which is why
    /// `Frame` is still what a single-view caller should use.
    #[must_use]
    pub fn batches(&self, view: ViewId) -> Option<&Batches> {
        self.plans.get(&view)
    }

    /// Marks a view's plan stale, so the next [`Host::plan`] rebuilds it.
    ///
    /// Reading the stream does this where it has to. This is for a caller that knows something the
    /// stream did not say -- and for measuring what planning costs when the cache is not serving
    /// it, which is otherwise unobservable from outside.
    pub fn invalidate(&mut self, view: ViewId) {
        self.stale.insert(view);
    }

    /// Whether a view's cached batches would be rebuilt by the next [`Host::plan`].
    ///
    /// For a consumer asserting that a parked frame does no work, which is the whole point of the
    /// cache and is otherwise invisible.
    #[must_use]
    pub fn stale(&self, view: ViewId) -> bool {
        self.stale.contains(&view) || !self.plans.contains_key(&view)
    }

    fn dispatch(&mut self, kind: EnvelopeKind, bytes: &[u8], payload: &[u8], at: u64) -> Outcome {
        match kind {
            EnvelopeKind::GeometryAdd => {
                let Some(add) = GeometryAdd::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                // The runs are read out here and not when a use arrives: they are spans into the
                // ring, and the producer reuses it as soon as the tail passes.
                let (Some(attrs), Some(instance_attrs), Some(segments), Some(texture_refs)) = (
                    read_run::<AttributeDesc>(payload, add.attrs),
                    read_run::<AttributeDesc>(payload, add.instance_attrs),
                    read_run::<Segment>(payload, add.segments),
                    read_run::<TextureRef>(payload, add.texture_refs),
                ) else {
                    return Outcome::Malformed;
                };
                let announcement = Announcement {
                    add,
                    announced_at: at,
                    attrs,
                    instance_attrs,
                    segments,
                    texture_refs,
                };
                // Every view already holding it now draws something else.
                let touched: alloc::vec::Vec<ViewId> = self.joiner.announce(announcement).collect();
                self.stale.extend(touched);
                Outcome::Read
            }
            EnvelopeKind::GeometryRemove => {
                GeometryRemove::from_bytes(bytes).map_or(Outcome::Malformed, |remove| {
                    // Read before retiring: afterwards nothing remembers who was drawing it.
                    let touched: Vec<ViewId> = self.joiner.views(remove.geometry).collect();
                    self.stale.extend(touched);
                    self.joiner.retire(remove.geometry);
                    Outcome::Read
                })
            }
            EnvelopeKind::ViewDeclare => {
                let Some(declare) = ViewDeclare::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                // The mode decides what a per-drawable matrix *means* -- in consumer mode every one
                // of them is advisory and a tile's placement comes from its own id -- so a
                // discriminant this build does not know is refused rather than defaulted. Guessing
                // it is how geometry gets placed through a camera that is not the one drawing.
                let Some(mode) = CameraMode::from_repr(declare.camera_mode) else {
                    return Outcome::Malformed;
                };
                self.views.insert(declare.view, mode);
                Outcome::Read
            }
            EnvelopeKind::ViewTarget => {
                let Some(target) = ViewTarget::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                // Both views declared, and the offscreen one before its target -- the producer
                // writes the pair in that order, "because a target naming an undeclared view is the
                // same protocol fault a use would be".
                if !self.views.contains_key(&target.view)
                    || !self.views.contains_key(&target.parent)
                {
                    return Outcome::Undeclared;
                }
                // Nesting is what `ViewError::NestedTarget` refuses on the producer's side, and
                // refusing it here is what lets a consumer draw a frame without recursion: the
                // children of a view are a flat set.
                if self.targets.contains_key(&target.parent) {
                    return Outcome::Malformed;
                }
                // A denominator of zero is a protocol fault the ABI names, and a view that is its
                // own parent is a frame that draws before itself.
                if target.scale_den == 0 || target.view == target.parent {
                    return Outcome::Malformed;
                }
                let Some(format) = TexturePixelType::from_repr(target.format) else {
                    return Outcome::Malformed;
                };
                let Some(channel) = TextureChannelDataType::from_repr(target.channel_type) else {
                    return Outcome::Malformed;
                };
                self.targets.insert(
                    target.view,
                    Target {
                        parent: target.parent,
                        texture: target.texture,
                        scale_num: target.scale_num,
                        scale_den: target.scale_den,
                        format,
                        channel,
                    },
                );
                Outcome::Read
            }
            EnvelopeKind::ViewUndeclare => {
                let Some(undeclare) = ViewUndeclare::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                self.forget(undeclare.view);
                Outcome::Read
            }
            EnvelopeKind::ViewUse => {
                ViewUse::from_bytes(bytes).map_or(Outcome::Malformed, |use_| {
                    // A use of a view that was never declared is a protocol fault, which the ABI
                    // says of this record: `ViewDeclare` is "ordered ahead of any `ViewUse` naming
                    // the view". Dropped rather than joined -- a view with no declaration has no
                    // camera mode, so nothing downstream knows what its matrices mean.
                    if !self.views.contains_key(&use_.view) {
                        return Outcome::Undeclared;
                    }
                    self.joiner.used(use_);
                    Outcome::Read
                })
            }
            EnvelopeKind::ViewRelease => {
                ViewRelease::from_bytes(bytes).map_or(Outcome::Malformed, |release| {
                    self.joiner.release(release.geometry, release.view);
                    self.stale.insert(release.view);
                    Outcome::Read
                })
            }
            EnvelopeKind::OrderUpdate => {
                let Some(update) = OrderUpdate::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                let Some(entries) = read_run::<OrderEntry>(payload, update.entries) else {
                    return Outcome::Malformed;
                };
                self.orders.insert(
                    update.view,
                    Order {
                        epoch: update.order_epoch,
                        entries,
                    },
                );
                self.stale.insert(update.view);
                Outcome::Read
            }
            EnvelopeKind::StencilTiles => {
                let Some(update) = StencilTiles::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                // All or nothing, as for an order's entries: a short run is a layer clipped to some
                // of its tiles, which draws the rest unclipped rather than not at all.
                let Some(tiles) = read_run::<StencilTile>(payload, update.tiles) else {
                    return Outcome::Malformed;
                };
                self.clips
                    .entry(update.view)
                    .or_default()
                    .set(update.layer_index, &tiles);
                // Not `stale`: the batches do not depend on the clips. What changed is which tile
                // each drawable tests against, and that is read per draw rather than collapsed.
                Outcome::Read
            }
            EnvelopeKind::UboUpdate => {
                let Some(update) = UboUpdate::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                let Some(data) = run_bytes(payload, update.data, 1) else {
                    return Outcome::Malformed;
                };
                self.uploads
                    .push_uniforms(update.view, update.layer_index, update.slot, data);
                Outcome::Read
            }
            EnvelopeKind::TextureUpdate => {
                let Some(update) = TextureUpdate::from_bytes(bytes) else {
                    return Outcome::Malformed;
                };
                // A rect count past the array's end is a malformed record, not a clamp: the rects
                // say which pixels these bytes are, and guessing at that writes them somewhere.
                let count = update.rect_count as usize;
                if count > update.rects.len() {
                    return Outcome::Malformed;
                }
                let Some(pixels) = run_bytes(payload, update.pixels, 1) else {
                    return Outcome::Malformed;
                };
                // Both halves of mbgl's two-part format, decoded here rather than carried as
                // discriminants: a value neither enum knows is a producer this consumer cannot
                // read, and passing it on would have the backend choose an image format from it.
                let Some(format) = TexturePixelType::from_repr(update.format) else {
                    return Outcome::Malformed;
                };
                let Some(channel) = TextureChannelDataType::from_repr(update.channel_type) else {
                    return Outcome::Malformed;
                };
                // The rects and the bytes have to describe the same thing. The producer tests this
                // before it packs and sends the texture whole when it fails; `TextureUpdate::packed`
                // calls the check here "the second half of the same guard", and without it a
                // backend reads past what it was given.
                let rects = update.rects[..count].to_vec();
                // Zero or one, and nothing else. The field was taken from the padding, so a
                // producer that has never heard of it writes zero -- but a value that is neither
                // is a producer this consumer does not understand, and reading it as `!= 0` would
                // guess at the payload's shape. Refused for the reason an unknown format is.
                let packed = match update.packed {
                    0 => false,
                    1 => true,
                    _ => return Outcome::Malformed,
                };
                let shape = upload::Shape {
                    format,
                    channel,
                    packed,
                };
                if upload::rows(update.size, shape, &rects, pixels.len()).is_err() {
                    return Outcome::Malformed;
                }
                self.uploads
                    .push_texture(update.texture, update.size, shape, rects, pixels);
                Outcome::Read
            }
            EnvelopeKind::CameraUpdate => {
                CameraUpdate::from_bytes(bytes).map_or(Outcome::Malformed, |camera| {
                    // The field says to: an unknown projection "must refuse the camera rather
                    // than fall back to the plane", because falling back draws a flat map where a
                    // round one was asked for and looks like a bug in the style. The same answer
                    // tessella#364 settled for the other discriminants on this wire, and the same
                    // one `ViewDeclare`'s `camera_mode` gets above.
                    //
                    // Refused whole -- not stored -- so the view keeps the last camera it could
                    // read and draws stale rather than blank.
                    if camera.projection().is_none() {
                        return Outcome::Malformed;
                    }
                    self.cameras.insert(camera.view, camera);
                    Outcome::Read
                })
            }
            _ => Outcome::Unknown,
        }
    }
}

/// What dispatching one record did.
enum Outcome {
    Read,
    Unknown,
    Malformed,
    /// The record parsed and named a view that was never declared.
    Undeclared,
}

/// The bytes a span names, or `None` if the payload does not hold them.
///
/// `width` is the size of one element; a span of bytes has width one. Checked rather than trusted,
/// because the span arrives from another process and a length past the payload is how a consumer
/// reads whatever follows it in the ring.
fn run_bytes(payload: &[u8], span: Span, width: usize) -> Option<&[u8]> {
    let start = span.offset as usize;
    let length = (span.count as usize).checked_mul(width)?;
    let end = start.checked_add(length)?;
    payload.get(start..end)
}

/// Reads a run of records out of a payload, or `None` if any of it does not fit.
///
/// All or nothing, deliberately. A short attribute run is a drawable missing a binding and a short
/// segment run is geometry missing its later triangles — both draw something, and what they draw
/// is wrong rather than absent. Refusing the announcement is the louder failure and the right one.
fn read_run<T: WireRecord>(payload: &[u8], span: Span) -> Option<Vec<T>> {
    let size = core::mem::size_of::<T>();
    let mut out = Vec::with_capacity(span.count as usize);
    for index in 0..span.count as usize {
        let start = (span.offset as usize).checked_add(index.checked_mul(size)?)?;
        out.push(T::from_bytes(payload.get(start..)?)?);
    }
    Some(out)
}
