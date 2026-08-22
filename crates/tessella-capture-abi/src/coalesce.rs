//! Coalescing for the state envelopes (§4).
//!
//! # Why this is not a slot table
//!
//! The obvious structure for latest-wins is a shared-memory table with one slot per key,
//! overwritten in place. It is the wrong one here, and the reason is payload size. Slot storage
//! has to be sized up front, and these payloads span three orders of magnitude: a
//! [`CameraUpdate`](crate::envelope::CameraUpdate) is 272 fixed bytes, while an
//! [`OrderUpdate`](crate::envelope::OrderUpdate) can carry thousands of entries and a
//! [`TextureUpdate`](crate::envelope::TextureUpdate) an atlas region. Sizing every slot for its
//! worst case wastes the region; sizing for the common case makes the worst case unrepresentable.
//! On top of that a table needs a seqlock or double buffering so the consumer cannot read a slot
//! mid-write, and it is a second transport whose ordering against the ring has to be reasoned
//! about separately.
//!
//! # What this is instead
//!
//! Coalescing happens on the producer side, and only one transport exists.
//!
//! The producer holds each key's current state in its own memory, where it is absolute and
//! always current. A key is written to the ring only when no earlier envelope for that key is
//! still unconsumed. While one is in flight, updates accumulate in the producer rather than
//! queueing behind it. So the ring holds at most one envelope per key, which is the occupancy
//! bound §4 asks coalescing to provide, and the bound is a function of live keys rather than of
//! how long the consumer stalls.
//!
//! "Still unconsumed" is a comparison, not a protocol: the producer records the ring position
//! its envelope ended at, and the consumer's `tail` passing that position means the envelope is
//! gone. The reverse channel is not involved.
//!
//! Two consequences worth stating plainly, because they are real trade-offs and not free:
//!
//! - Under stall, a consumer can see one superseded value before the current one, where a slot
//!   table would have shown only the current one. This is harmless precisely because every
//!   coalescable envelope is an absolute state write — the stale value is overwritten by the
//!   next drain, not accumulated. It is one drain of staleness, not a queue of it.
//! - State envelopes share the ring's ordering with geometry, which is a benefit rather than a
//!   cost: a slot table would have needed its own answer for whether a uniform write can be
//!   applied before the geometry that indexes it.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::EnvelopeKind;
use crate::envelope::{Rect16, TEXTURE_RECT_CAP};
use crate::ring::{Full, Producer};

/// Identifies the state one coalescable envelope describes.
///
/// The fields are the §4 table's key column, flattened. What goes in them is per kind:
///
/// | kind | primary | secondary |
/// |---|---|---|
/// | `UboUpdate` | view | layer and slot, packed |
/// | `TextureUpdate` | texture | 0 |
/// | `CameraUpdate` | view | 0 |
/// | `OrderUpdate` | view | 0 |
/// | `StencilTiles` | view | layer |
///
/// Constructors below build these, so a caller cannot pack a key two different ways and end up
/// with two slots for one piece of state — which would silently defeat coalescing rather than
/// fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoalesceKey {
    kind: EnvelopeKind,
    primary: u64,
    secondary: u64,
}

impl CoalesceKey {
    /// Key for a uniform buffer: `(view, layer, slot)`.
    ///
    /// `layer` is `-1` for the frame-wide buffers that belong to no layer.
    #[must_use]
    pub fn ubo(view: u32, layer: i32, slot: u32) -> Self {
        Self {
            kind: EnvelopeKind::UboUpdate,
            primary: u64::from(view),
            // Sign-extending would make -1 collide with a large positive layer index; casting
            // the bit pattern keeps every distinct layer distinct.
            secondary: (u64::from(layer as u32) << 32) | u64::from(slot),
        }
    }

    /// Key for a texture's pixels.
    #[must_use]
    pub fn texture(texture: u64) -> Self {
        Self {
            kind: EnvelopeKind::TextureUpdate,
            primary: texture,
            secondary: 0,
        }
    }

    /// Key for a view's camera.
    #[must_use]
    pub fn camera(view: u32) -> Self {
        Self {
            kind: EnvelopeKind::CameraUpdate,
            primary: u64::from(view),
            secondary: 0,
        }
    }

    /// Key for a view's draw order.
    #[must_use]
    pub fn order(view: u32) -> Self {
        Self {
            kind: EnvelopeKind::OrderUpdate,
            primary: u64::from(view),
            secondary: 0,
        }
    }

    /// Key for a layer's clip set within a view.
    #[must_use]
    pub fn stencil(view: u32, layer: i32) -> Self {
        Self {
            kind: EnvelopeKind::StencilTiles,
            primary: u64::from(view),
            secondary: u64::from(layer as u32),
        }
    }

    /// The envelope kind this key belongs to.
    #[must_use]
    pub fn kind(&self) -> EnvelopeKind {
        self.kind
    }
}

/// One key's current state.
#[derive(Debug)]
struct Slot {
    record: Vec<u8>,
    payload: Vec<u8>,
    /// Set when the state changed since it was last written to the ring.
    dirty: bool,
    /// Ring position the last envelope for this key ended at. The envelope is still in flight
    /// until the consumer's tail reaches it.
    in_flight_until: u64,
}

/// What a flush did, for the §9.3 counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushStats {
    /// Envelopes written to the ring.
    pub written: usize,
    /// Keys left dirty because an earlier envelope was still in flight. These are the ones
    /// coalescing absorbed rather than queued.
    pub deferred: usize,
    /// Keys left dirty because the ring was full. Distinct from `deferred`: this is
    /// backpressure, and a watchdog should care (R-4).
    pub blocked: usize,
}

/// Producer-side coalescing table.
///
/// Staging is cheap and can happen as often as state changes; [`flush`](Self::flush) is what
/// touches the ring, and is called once per producer frame.
#[derive(Debug, Default)]
pub struct Coalescer {
    slots: BTreeMap<CoalesceKey, Slot>,
}

impl Coalescer {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
        }
    }

    /// Records a key's current state, replacing whatever was there.
    ///
    /// Replacement is the whole point: these are absolute writes, so the newest one is the only
    /// one that matters and older ones are not merely redundant but wrong to send. Staging the
    /// byte-identical state twice still marks the key dirty, so the producer should suppress
    /// no-op rewrites before calling — §6.1's memcmp-before-dirty, which is what makes
    /// "dirty-only" true rather than aspirational.
    pub fn stage(&mut self, key: CoalesceKey, record: &[u8], payload: &[u8]) {
        let slot = self.slots.entry(key).or_insert_with(|| Slot {
            record: Vec::new(),
            payload: Vec::new(),
            dirty: false,
            in_flight_until: 0,
        });
        slot.record.clear();
        slot.record.extend_from_slice(record);
        slot.payload.clear();
        slot.payload.extend_from_slice(payload);
        slot.dirty = true;
    }

    /// Number of keys holding state, in flight or not.
    ///
    /// This is the occupancy bound: the ring can hold at most this many coalescable envelopes,
    /// whatever the consumer is doing.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// True when no key holds state.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// True when no key is waiting to be written.
    ///
    /// The §6.5 still-frame guarantee is this returning true with an empty ring: a parked view
    /// emits zero bytes because there is nothing dirty to emit, not because something
    /// downstream suppressed it.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        !self.slots.values().any(|slot| slot.dirty)
    }

    /// Drops a key's state entirely, for a view or texture that no longer exists.
    ///
    /// Leaving dead keys in the table would inflate the occupancy bound without bounding
    /// anything, which is R-11's shape one level down.
    pub fn forget(&mut self, key: CoalesceKey) {
        self.slots.remove(&key);
    }

    /// Writes every dirty key whose previous envelope the consumer has already taken.
    ///
    /// A key whose envelope is still in flight stays dirty and is picked up by a later flush,
    /// which is where the coalescing happens: the intermediate states between the two flushes
    /// were absorbed by [`stage`](Self::stage) overwriting the slot and never reached the ring.
    pub fn flush(&mut self, producer: &mut Producer) -> FlushStats {
        let consumed_through = producer.consumed_through();
        let mut stats = FlushStats::default();

        for (key, slot) in &mut self.slots {
            if !slot.dirty {
                continue;
            }
            if slot.in_flight_until > consumed_through {
                stats.deferred += 1;
                continue;
            }
            match producer.write(key.kind, &slot.record, &slot.payload) {
                Ok(()) => {
                    slot.in_flight_until = producer.head();
                    slot.dirty = false;
                    stats.written += 1;
                }
                Err(Full) => stats.blocked += 1,
            }
        }
        stats
    }
}

/// Merges a dirty rect into a texture's rect list, spilling to the union past the cap (§6.4).
///
/// Returns the new count.
///
/// The list exists because a union over two small updates in opposite atlas corners uploads the
/// whole atlas. Four rects is where §6.4 puts the threshold, and the producer's shelf allocator
/// keeps insertions clustered so the list rarely reaches it.
///
/// Spilling collapses everything to one bounding rect rather than dropping the oldest. Dropping
/// would lose pixels the consumer never uploads — a permanently stale region of the atlas,
/// which is far worse than uploading more than necessary.
pub fn merge_rect(rects: &mut [Rect16; TEXTURE_RECT_CAP], count: u8, incoming: Rect16) -> u8 {
    let count = (count as usize).min(TEXTURE_RECT_CAP);

    // An incoming rect already covered by one on the list is free to drop.
    for existing in rects.iter().take(count) {
        if contains(*existing, incoming) {
            return count as u8;
        }
    }

    // Absorb any rect the incoming one covers, so a growing region does not consume slots.
    let mut kept = 0usize;
    for i in 0..count {
        if !contains(incoming, rects[i]) {
            rects[kept] = rects[i];
            kept += 1;
        }
    }

    if kept < TEXTURE_RECT_CAP {
        rects[kept] = incoming;
        for slot in rects.iter_mut().skip(kept + 1) {
            *slot = Rect16::default();
        }
        return (kept + 1) as u8;
    }

    // Full: collapse to the bounding rect of everything.
    let mut union = incoming;
    for existing in rects.iter().take(kept) {
        union = bounding(union, *existing);
    }
    rects[0] = union;
    for slot in rects.iter_mut().skip(1) {
        *slot = Rect16::default();
    }
    1
}

/// True when `outer` fully covers `inner`.
fn contains(outer: Rect16, inner: Rect16) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && u32::from(outer.x) + u32::from(outer.w) >= u32::from(inner.x) + u32::from(inner.w)
        && u32::from(outer.y) + u32::from(outer.h) >= u32::from(inner.y) + u32::from(inner.h)
}

/// The smallest rect covering both.
fn bounding(a: Rect16, b: Rect16) -> Rect16 {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (u32::from(a.x) + u32::from(a.w)).max(u32::from(b.x) + u32::from(b.w));
    let bottom = (u32::from(a.y) + u32::from(a.h)).max(u32::from(b.y) + u32::from(b.h));
    Rect16 {
        x,
        y,
        // Saturating rather than wrapping: a texture cannot exceed u16 in either axis, so a
        // saturated edge is already the whole texture, and wrapping would produce a rect that
        // uploads nothing.
        w: (right - u32::from(x)).min(u32::from(u16::MAX)) as u16,
        h: (bottom - u32::from(y)).min(u32::from(u16::MAX)) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring::{Consumer, region_size};

    struct Region {
        buf: Vec<u64>,
        capacity: usize,
    }

    impl Region {
        fn new(capacity: usize) -> Self {
            Self {
                buf: alloc::vec![0u64; region_size(capacity).div_ceil(8)],
                capacity,
            }
        }

        fn open(&mut self) -> (Producer, Consumer) {
            let capacity = self.capacity;
            // SAFETY: `buf` is large enough, 8-aligned, and outlives the halves.
            unsafe { crate::ring::init(self.buf.as_mut_ptr().cast(), capacity) }
        }
    }

    /// Takes one record, copying it out so the ring's borrow ends before the advance that
    /// lets the producer reuse those bytes. Splitting this out of `drain` is what lets the
    /// loop be a plain `while let`.
    fn take(consumer: &mut Consumer) -> Option<(EnvelopeKind, Vec<u8>)> {
        let (kind, record, consumed) = consumer
            .peek()
            .map(|record| (record.kind, record.record.to_vec(), record.consumed()))?;
        consumer.advance(consumed);
        Some((kind, record))
    }

    fn drain(consumer: &mut Consumer) -> Vec<(EnvelopeKind, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(item) = take(consumer) {
            out.push(item);
        }
        out
    }

    #[test]
    fn keys_distinguish_the_state_they_name() {
        assert_ne!(CoalesceKey::camera(0), CoalesceKey::camera(1));
        assert_ne!(CoalesceKey::camera(0), CoalesceKey::order(0));
        assert_ne!(CoalesceKey::stencil(0, 1), CoalesceKey::stencil(0, 2));
        assert_ne!(CoalesceKey::ubo(0, 1, 0), CoalesceKey::ubo(0, 1, 1));
        assert_ne!(CoalesceKey::ubo(0, 1, 0), CoalesceKey::ubo(0, 2, 0));
        assert_eq!(CoalesceKey::ubo(3, -1, 7), CoalesceKey::ubo(3, -1, 7));
    }

    /// The global-buffer layer index is -1, and packing it by sign extension would make it
    /// collide with a large positive layer. Distinct layers must stay distinct.
    #[test]
    fn the_global_layer_index_does_not_collide() {
        assert_ne!(CoalesceKey::ubo(0, -1, 0), CoalesceKey::ubo(0, i32::MAX, 0));
        assert_ne!(CoalesceKey::ubo(0, -1, 0), CoalesceKey::ubo(0, 0, 0));
        assert_ne!(CoalesceKey::ubo(0, -1, 0), CoalesceKey::ubo(0, -2, 0));
    }

    #[test]
    fn flush_writes_dirty_state_once() {
        let mut region = Region::new(4096);
        let (mut producer, mut consumer) = region.open();
        let mut coalescer = Coalescer::new();

        coalescer.stage(CoalesceKey::camera(0), &[1u8; 16], &[]);
        assert!(!coalescer.is_quiescent());

        let stats = coalescer.flush(&mut producer);
        assert_eq!(stats.written, 1);
        assert!(coalescer.is_quiescent(), "a flushed key is no longer dirty");

        // Nothing changed, so a second flush emits nothing. This is the §6.5 still-frame
        // guarantee at the transport layer: no change, no bytes.
        assert_eq!(coalescer.flush(&mut producer), FlushStats::default());

        let drained = drain(&mut consumer);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, EnvelopeKind::CameraUpdate);
        assert_eq!(drained[0].1, [1u8; 16]);
    }

    /// The property §4 actually needs: however many times state changes while the consumer is
    /// stalled, the ring holds one envelope per key, and what the consumer eventually sees is
    /// the newest value rather than a queue of every intermediate.
    #[test]
    fn a_stalled_consumer_sees_one_envelope_per_key_and_the_latest_value() {
        let mut region = Region::new(4096);
        let (mut producer, mut consumer) = region.open();
        let mut coalescer = Coalescer::new();

        for frame in 0u8..100 {
            coalescer.stage(CoalesceKey::camera(0), &[frame; 16], &[]);
            coalescer.flush(&mut producer);
        }

        // One hundred camera changes, one envelope.
        let drained = drain(&mut consumer);
        assert_eq!(drained.len(), 1, "coalescing absorbed the intermediates");
        assert_eq!(
            drained[0].1, [0u8; 16],
            "the in-flight envelope carries the value staged when it was written"
        );

        // Now that the consumer has taken it, the accumulated latest state goes out, and it is
        // the newest value rather than the next one in a queue.
        coalescer.flush(&mut producer);
        let drained = drain(&mut consumer);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1, [99u8; 16], "the newest value, not the second");
    }

    /// Occupancy is bounded by live keys rather than by how long the consumer stalls, which is
    /// the whole reason §4 requires coalescing.
    #[test]
    fn occupancy_is_bounded_by_key_count() {
        let mut region = Region::new(8192);
        let (mut producer, _consumer) = region.open();
        let mut coalescer = Coalescer::new();

        let keys = [
            CoalesceKey::camera(0),
            CoalesceKey::order(0),
            CoalesceKey::stencil(0, 0),
            CoalesceKey::ubo(0, 0, 0),
        ];

        // The consumer never drains, and the producer runs a thousand frames.
        for frame in 0u8..250 {
            for key in keys {
                coalescer.stage(key, &[frame; 32], &[]);
            }
            coalescer.flush(&mut producer);
        }

        let bound = keys.len() * crate::ring::record_size(32, 0);
        assert!(
            producer.occupancy() <= bound,
            "occupancy {} exceeded the {}-key bound {}",
            producer.occupancy(),
            keys.len(),
            bound
        );
        assert_eq!(coalescer.len(), keys.len());
    }

    /// Deferral and backpressure are different failures and must not be conflated: one is
    /// coalescing working, the other is R-4's stall pathology starting.
    #[test]
    fn deferral_is_distinguished_from_backpressure() {
        let mut region = Region::new(64);
        let (mut producer, mut consumer) = region.open();
        let mut coalescer = Coalescer::new();

        coalescer.stage(CoalesceKey::camera(0), &[1u8; 16], &[]);
        assert_eq!(coalescer.flush(&mut producer).written, 1);

        // Same key again, previous envelope unconsumed: deferred, not blocked.
        coalescer.stage(CoalesceKey::camera(0), &[2u8; 16], &[]);
        let stats = coalescer.flush(&mut producer);
        assert_eq!(stats.deferred, 1);
        assert_eq!(stats.blocked, 0);
        assert_eq!(stats.written, 0);

        // A different key with nothing in flight, into a ring with no room: blocked.
        coalescer.stage(CoalesceKey::order(0), &[3u8; 32], &[]);
        let stats = coalescer.flush(&mut producer);
        assert_eq!(stats.blocked, 1);
        assert_eq!(stats.deferred, 1, "the camera key is still in flight");

        // Draining frees the camera envelope, so the camera's newer value goes out. The order
        // envelope still does not fit — this ring holds 64 bytes and the two records need 80
        // between them — so it stays blocked rather than being dropped.
        drain(&mut consumer);
        let stats = coalescer.flush(&mut producer);
        assert_eq!(stats.written, 1);
        assert_eq!(stats.blocked, 1);

        // Draining again makes room, and the blocked key goes. Nothing was lost: backpressure
        // delays a lossless write, it never discards one.
        let drained = drain(&mut consumer);
        assert_eq!(drained[0].1, [2u8; 16], "the camera's newer value");
        let stats = coalescer.flush(&mut producer);
        assert_eq!(stats.written, 1);
        assert_eq!(stats.blocked, 0);
        let drained = drain(&mut consumer);
        assert_eq!(drained[0].0, EnvelopeKind::OrderUpdate);
        assert_eq!(drained[0].1, [3u8; 32]);
        assert!(coalescer.is_quiescent());
    }

    #[test]
    fn forget_drops_a_key() {
        let mut region = Region::new(1024);
        let (mut producer, _consumer) = region.open();
        let mut coalescer = Coalescer::new();

        coalescer.stage(CoalesceKey::camera(7), &[1u8; 16], &[]);
        assert_eq!(coalescer.len(), 1);
        coalescer.forget(CoalesceKey::camera(7));
        assert!(coalescer.is_empty());
        assert_eq!(coalescer.flush(&mut producer), FlushStats::default());
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect16 {
        Rect16 { x, y, w, h }
    }

    #[test]
    fn rects_accumulate_up_to_the_cap() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let mut count = 0u8;
        for i in 0..TEXTURE_RECT_CAP as u16 {
            count = merge_rect(&mut rects, count, rect(i * 10, 0, 4, 4));
        }
        assert_eq!(count, TEXTURE_RECT_CAP as u8);
        assert_eq!(rects[0], rect(0, 0, 4, 4));
        assert_eq!(rects[3], rect(30, 0, 4, 4));
    }

    /// Opposite corners are the pathology §6.4 exists to fix: a union would upload the whole
    /// atlas, a list uploads two small regions.
    #[test]
    fn opposite_corners_stay_separate() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let count = merge_rect(&mut rects, 0, rect(0, 0, 8, 8));
        let count = merge_rect(&mut rects, count, rect(1016, 1016, 8, 8));
        assert_eq!(count, 2);
        assert_eq!(rects[0], rect(0, 0, 8, 8));
        assert_eq!(rects[1], rect(1016, 1016, 8, 8));
    }

    #[test]
    fn a_covered_rect_is_dropped() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let count = merge_rect(&mut rects, 0, rect(0, 0, 100, 100));
        let count = merge_rect(&mut rects, count, rect(10, 10, 5, 5));
        assert_eq!(count, 1, "a rect inside an existing one adds nothing");
        assert_eq!(rects[0], rect(0, 0, 100, 100));
    }

    #[test]
    fn a_covering_rect_absorbs_the_ones_it_covers() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let count = merge_rect(&mut rects, 0, rect(0, 0, 4, 4));
        let count = merge_rect(&mut rects, count, rect(8, 8, 4, 4));
        assert_eq!(count, 2);
        let count = merge_rect(&mut rects, count, rect(0, 0, 64, 64));
        assert_eq!(count, 1, "one rect covering both replaces them");
        assert_eq!(rects[0], rect(0, 0, 64, 64));
    }

    /// Past the cap the list collapses to a bounding rect. It must cover everything that was
    /// on it: dropping the oldest instead would leave a region of the atlas permanently stale,
    /// which is worse than uploading more than necessary.
    #[test]
    fn spilling_covers_every_rect_it_replaces() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let mut count = 0u8;
        let inputs = [
            rect(0, 0, 4, 4),
            rect(100, 0, 4, 4),
            rect(0, 100, 4, 4),
            rect(100, 100, 4, 4),
            rect(200, 200, 4, 4),
        ];
        for r in inputs {
            count = merge_rect(&mut rects, count, r);
        }
        assert_eq!(count, 1, "the fifth rect spills the list to its union");
        for r in inputs {
            assert!(
                contains(rects[0], r),
                "union {:?} must cover {r:?}",
                rects[0]
            );
        }
        assert_eq!(rects[0], rect(0, 0, 204, 204));
        for slot in rects.iter().skip(1) {
            assert_eq!(*slot, Rect16::default(), "stale entries must be cleared");
        }
    }

    /// A rect reaching the far edge of a maximum-sized texture must not wrap its width to
    /// something that uploads nothing.
    #[test]
    fn spilling_saturates_rather_than_wrapping() {
        let mut rects = [Rect16::default(); TEXTURE_RECT_CAP];
        let mut count = 0u8;
        for r in [
            rect(0, 0, 1, 1),
            rect(10, 10, 1, 1),
            rect(20, 20, 1, 1),
            rect(30, 30, 1, 1),
            rect(u16::MAX - 1, u16::MAX - 1, 1, 1),
        ] {
            count = merge_rect(&mut rects, count, r);
        }
        assert_eq!(count, 1);
        assert_eq!(rects[0].x, 0);
        assert_eq!(rects[0].y, 0);
        assert_eq!(rects[0].w, u16::MAX);
        assert_eq!(rects[0].h, u16::MAX);
    }
}
