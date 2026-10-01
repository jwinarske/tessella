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
//! [`Host::plan_into`] fills a buffer the caller keeps, so a steady state allocates nothing.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{
    AttributeDesc, CameraUpdate, GeometryAdd, GeometryRemove, OrderEntry, OrderEpoch, OrderUpdate,
    Segment, Span, TextureRef, TextureUpdate, UboUpdate, ViewId, ViewRelease, ViewUse, WireRecord,
};
use tessella_capture_abi::ring::Consumer;

use crate::batch::{Batch, Program, collapse_into};
use crate::join::{Announcement, Joiner};
use crate::upload::Uploads;

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

/// Reads the stream and plans frames from it.
#[derive(Debug, Clone, Default)]
pub struct Host {
    joiner: Joiner,
    orders: BTreeMap<ViewId, Order>,
    cameras: BTreeMap<ViewId, OrderEpoch>,
    read_through: u64,
    progress: Progress,
    uploads: Uploads,
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

    /// Fills `out` with the batches a view draws, if it is ready.
    ///
    /// `out` is cleared first and reused, so a caller keeping one buffer allocates nothing in a
    /// steady state. Returns `None` when the view has no order, no camera, or a camera naming an
    /// epoch the held order does not establish — §11.7's "hold `CameraUpdate` until its
    /// `orderEpoch` is held". Drawing under a camera that does not match the order is drawing this
    /// frame's geometry through the last frame's matrices.
    pub fn plan_into(&self, view: ViewId, out: &mut Vec<Batch>) -> Option<Plan> {
        let order = self.orders.get(&view)?;
        if self.cameras.get(&view) != Some(&order.epoch) {
            return None;
        }

        out.clear();
        let mut announced_through = 0;
        collapse_into(&order.entries, out, |id| {
            let drawable = self.joiner.drawable(id, view)?;
            announced_through = announced_through.max(drawable.geometry.announced_at);
            Some(Program {
                builtin_shader: drawable.geometry.add.builtin_shader,
                permutation_key: drawable.geometry.add.permutation_key,
                texture_refs: &drawable.geometry.texture_refs,
            })
        });

        Some(Plan {
            view,
            epoch: order.epoch,
            announced_through,
        })
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
                // The views already holding it are re-joined by this; a caller that wants to
                // know which asks the joiner, since a read is not the moment to act on it.
                let _ = self.joiner.announce(Announcement {
                    add,
                    announced_at: at,
                    attrs,
                    instance_attrs,
                    segments,
                    texture_refs,
                });
                Outcome::Read
            }
            EnvelopeKind::GeometryRemove => {
                GeometryRemove::from_bytes(bytes).map_or(Outcome::Malformed, |remove| {
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
                self.uploads.push_texture(
                    update.texture,
                    update.size,
                    update.format,
                    update.rects[..count].to_vec(),
                    pixels,
                );
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
