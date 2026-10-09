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
    AttributeDesc, CameraUpdate, GeometryAdd, GeometryRemove, OrderEntry, OrderEpoch, OrderUpdate,
    Segment, Span, TextureRef, TextureUpdate, UboUpdate, ViewId, ViewRelease, ViewUse, WireRecord,
};
use tessella_capture_abi::ring::Consumer;
use tessella_capture_abi::{EnvelopeKind, TextureChannelDataType, TexturePixelType};

use crate::batch::{Batches, Program, collapse_into};
use crate::join::{Announcement, Joiner};
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
}

/// Reads the stream and plans frames from it.
#[derive(Debug, Clone, Default)]
pub struct Host {
    joiner: Joiner,
    orders: BTreeMap<ViewId, Order>,
    cameras: BTreeMap<ViewId, OrderEpoch>,
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
            (Some(order), Some(epoch)) => order.epoch == *epoch,
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
        if self.cameras.get(&view) != Some(&order.epoch) {
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
        })
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
            EnvelopeKind::ViewUse => {
                ViewUse::from_bytes(bytes).map_or(Outcome::Malformed, |use_| {
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
                    self.cameras.insert(camera.view, camera.order_epoch);
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
