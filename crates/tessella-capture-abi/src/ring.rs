//! SPSC transport: the lossless record ring (§4).
//!
//! Producer is the Rust map/orchestrator thread; consumer is the consumer's tick, which on the
//! Fluorite side runs inside the ECS update on the Filament API thread (§3.2). One tick
//! draining several producer frames is normal and correct.
//!
//! # What this ring is, and is not
//!
//! §4 gives every envelope kind a policy, and the two policies want opposite structures. This
//! module implements the **lossless** half — `GeometryAdd`, `GeometryRemove`, `ViewUse`,
//! `ViewRelease` — as an in-order byte FIFO where backpressure blocks the producer by design.
//!
//! The coalescing policies cannot live in a FIFO and are not implemented here. Latest-wins
//! means a superseded envelope is replaced rather than queued, and a FIFO has nowhere to put
//! the replacement: the old record sits at a fixed offset the consumer may already have passed,
//! and the new one may not even be the same length. Worse, queuing replacements is exactly the
//! unbounded-occupancy failure §4 requires coalescing to prevent — a stalled consumer would
//! accumulate every intermediate camera. Latest-wins therefore belongs in a keyed slot table
//! written in place, where occupancy is bounded by the number of live keys rather than by how
//! long the consumer stalls. That table is the next piece and shares this region.
//!
//! # Framing
//!
//! Each record is `[RecordHeader][fixed record][payload]`, and every record starts at a
//! 16-byte boundary. Records are never split across the buffer's wrap: a record that will not
//! fit contiguously is preceded by a skip record covering the remainder, and the real record
//! begins at offset zero. Contiguity is not a convenience — it is what lets the consumer read a
//! record in place rather than reassembling it, which is the whole premise of §11.3.
//!
//! Padding every record to 16 bytes costs up to 15 bytes each and buys the guarantee that the
//! space left before a wrap is either zero or large enough for a skip header. Without it the
//! remainder could be 8 bytes, too small to describe itself, and the wrap would need a second
//! mechanism.
//!
//! # Ordering
//!
//! `head` and `tail` are free-running byte counters, not indices, so full and empty are never
//! ambiguous. The producer publishes with a release store to `head` after writing bytes; the
//! consumer acquires `head` before reading them, and publishes consumption with a release store
//! to `tail`. That is the whole synchronization protocol, and it is why the counters are
//! explicit-width atomics rather than anything richer — R-6 is about riscv64 alignment and
//! atomics behaving the same as everywhere else.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::EnvelopeKind;

/// Alignment of every record's start, and the granularity records are padded to.
pub const RECORD_ALIGN: usize = 16;

/// Padding between the two counters.
///
/// 128 rather than 64: some aarch64 parts have a 128-byte cache line, and the cost of being
/// generous is a few hundred bytes in a region measured in megabytes. `head` and `tail` are
/// written by different threads on every record, so sharing a line between them would turn
/// each publish into a coherence round trip.
const COUNTER_PAD: usize = 128;

/// Marks a record that exists only to cover the space before a wrap.
const FLAG_SKIP: u16 = 1 << 0;

/// Fixed-size prefix of every record on the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct RecordHeader {
    /// [`EnvelopeKind`] discriminant, or zero for a skip record.
    kind: u16,
    /// [`FLAG_SKIP`], or zero.
    flags: u16,
    /// Bytes of fixed envelope record following this header.
    record_len: u32,
    /// Bytes of payload region following the fixed record.
    payload_len: u32,
    /// Total bytes this record occupies, header included, padded to [`RECORD_ALIGN`].
    total_len: u32,
}

/// Control block at the head of the shared region.
///
/// Laid out so the two counters never share a cache line with each other or with the immutable
/// fields, which are read by both sides on every operation.
#[derive(Debug)]
#[repr(C)]
pub struct RingControl {
    /// ABI revision the producer wrote this region with.
    pub abi_rev: u32,
    /// Padding. Must be zero.
    pub _pad0: u32,
    /// Bytes in the data region. Always a power of two.
    pub capacity: u64,
    _pad1: [u8; COUNTER_PAD - 16],
    /// Bytes ever written. Producer writes, consumer reads.
    head: AtomicU64,
    _pad2: [u8; COUNTER_PAD - 8],
    /// Bytes ever consumed. Consumer writes, producer reads.
    tail: AtomicU64,
    _pad3: [u8; COUNTER_PAD - 8],
}

const _: () = {
    assert!(size_of::<RecordHeader>() == 16);
    assert!(align_of::<RecordHeader>() == 4);
    assert!(size_of::<RingControl>() == COUNTER_PAD * 3);
    assert!(align_of::<RingControl>() == 8);
    // A record header must fit in the smallest gap the wrap rule can leave.
    assert!(size_of::<RecordHeader>() <= RECORD_ALIGN);
};

/// The producer could not fit a record.
///
/// Lossless envelopes are never dropped, so this is backpressure: the caller retries after the
/// consumer drains. §4 makes that block by design and sizes the ring for worst-case tile
/// turnover; R-4 is the risk that a consumer pause turns the block into a stall, which the
/// watchdog counter rather than the transport is meant to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// Rounds `value` up to a multiple of `align`, which must be a power of two.
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// Total bytes a record with these lengths occupies on the ring.
#[must_use]
pub const fn record_size(record_len: usize, payload_len: usize) -> usize {
    align_up(
        size_of::<RecordHeader>() + align_up(record_len, 8) + payload_len,
        RECORD_ALIGN,
    )
}

/// Bytes a region must provide for a ring of `capacity` data bytes.
#[must_use]
pub const fn region_size(capacity: usize) -> usize {
    size_of::<RingControl>() + capacity
}

/// Producer half of the ring.
///
/// Not `Sync`: §4 has exactly one producer, and the whole ordering argument depends on it.
#[derive(Debug)]
pub struct Producer {
    base: *mut u8,
    capacity: usize,
    /// Cached `tail`, refreshed only when a reservation does not obviously fit. Reading the
    /// consumer's counter pulls its cache line, so the common case avoids it entirely.
    cached_tail: u64,
}

/// Consumer half of the ring.
#[derive(Debug)]
pub struct Consumer {
    base: *mut u8,
    capacity: usize,
    cached_head: u64,
}

// SAFETY: each half owns one counter and touches only the bytes that counter grants it. They
// are `Send` so the producer can be moved onto the map thread and the consumer onto the tick
// thread, and deliberately not `Sync`, because two producers or two consumers would break the
// single-writer-per-counter invariant the ordering argument rests on.
unsafe impl Send for Producer {}
unsafe impl Send for Consumer {}

/// Initializes a region and returns both halves.
///
/// # Safety
///
/// `base` must point to at least [`region_size(capacity)`](region_size) writable bytes, aligned
/// to at least 8, and must outlive both halves. `capacity` must be a power of two of at least
/// [`RECORD_ALIGN`] bytes. Nothing else may touch the region for as long as the halves live.
///
/// This writes the control block, so exactly one side calls it; the other calls [`attach`].
pub unsafe fn init(base: *mut u8, capacity: usize) -> (Producer, Consumer) {
    assert!(
        capacity.is_power_of_two(),
        "capacity must be a power of two"
    );
    assert!(
        capacity >= RECORD_ALIGN,
        "capacity is smaller than a record"
    );

    // SAFETY: the caller guarantees `base` is writable, aligned, and large enough.
    unsafe {
        let control = base.cast::<RingControl>();
        (&raw mut (*control).abi_rev).write(crate::ABI_REV);
        (&raw mut (*control)._pad0).write(0);
        (&raw mut (*control).capacity).write(capacity as u64);
        // Written through raw pointers: `head.store(..)` would take a reference to memory
        // that is not an initialized `AtomicU64` yet.
        (&raw mut (*control).head).write(AtomicU64::new(0));
        (&raw mut (*control).tail).write(AtomicU64::new(0));
    }

    (
        Producer {
            base,
            capacity,
            cached_tail: 0,
        },
        Consumer {
            base,
            capacity,
            cached_head: 0,
        },
    )
}

/// Attaches to a region another side has already initialized.
///
/// # Safety
///
/// As [`init`], except the control block must already be valid and `capacity` must match the
/// one it records. Returns `None` if the region's ABI revision is not this crate's, which is
/// the version-skew case §3.5 makes reachable once the halves can be separate processes.
pub unsafe fn attach(base: *mut u8, capacity: usize) -> Option<(Producer, Consumer)> {
    // SAFETY: the caller guarantees the region is valid and initialized.
    let control = unsafe { &*base.cast::<RingControl>() };
    if control.abi_rev != crate::ABI_REV || control.capacity != capacity as u64 {
        return None;
    }
    let head = control.head.load(Ordering::Acquire);
    let tail = control.tail.load(Ordering::Acquire);
    Some((
        Producer {
            base,
            capacity,
            cached_tail: tail,
        },
        Consumer {
            base,
            capacity,
            cached_head: head,
        },
    ))
}

/// Control block of a region.
///
/// # Safety
///
/// `base` must point at an initialized region.
unsafe fn control<'a>(base: *mut u8) -> &'a RingControl {
    // SAFETY: the caller guarantees the region is initialized, and the control block is the
    // first thing in it.
    unsafe { &*base.cast::<RingControl>() }
}

/// Start of the data region.
///
/// # Safety
///
/// `base` must point at a region of at least [`region_size`] bytes.
unsafe fn data_of(base: *mut u8) -> *mut u8 {
    // SAFETY: the caller guarantees the data region follows the control block inside the same
    // allocation.
    unsafe { base.add(size_of::<RingControl>()) }
}

impl Producer {
    /// Bytes currently unread by the consumer.
    #[must_use]
    pub fn occupancy(&self) -> usize {
        // SAFETY: the region outlives this half.
        let control = unsafe { control(self.base) };
        let head = control.head.load(Ordering::Relaxed);
        let tail = control.tail.load(Ordering::Acquire);
        (head - tail) as usize
    }

    /// Writes one lossless record, or reports backpressure.
    ///
    /// `record` is the fixed envelope struct as bytes; `payload` is the variable-length region
    /// its [`Span`](crate::envelope::Span) fields address.
    ///
    /// # Errors
    ///
    /// [`Full`] when the record does not fit. Nothing is written and the caller retries later;
    /// a lossless envelope is never dropped.
    pub fn write(&mut self, kind: EnvelopeKind, record: &[u8], payload: &[u8]) -> Result<(), Full> {
        let total = record_size(record.len(), payload.len());
        if total > self.capacity {
            // Not backpressure: this record can never fit, however long the consumer runs.
            // Reporting it as Full would spin the producer forever.
            return Err(Full);
        }

        // SAFETY: the region outlives this half.
        let control = unsafe { control(self.base) };
        let head = control.head.load(Ordering::Relaxed);
        let offset = (head as usize) & (self.capacity - 1);
        let to_end = self.capacity - offset;

        // A record never straddles the wrap; if it would, a skip record covers the remainder.
        let skip = if total > to_end { to_end } else { 0 };
        let needed = skip + total;

        if !self.has_room(control, head, needed) {
            return Err(Full);
        }

        let mut cursor = head;
        if skip > 0 {
            self.write_header(
                offset,
                RecordHeader {
                    kind: 0,
                    flags: FLAG_SKIP,
                    record_len: 0,
                    payload_len: 0,
                    total_len: skip as u32,
                },
            );
            cursor += skip as u64;
        }

        let start = (cursor as usize) & (self.capacity - 1);
        self.write_header(
            start,
            RecordHeader {
                kind: kind as u16,
                flags: 0,
                record_len: record.len() as u32,
                payload_len: payload.len() as u32,
                total_len: total as u32,
            },
        );
        let record_at = start + size_of::<RecordHeader>();
        self.write_bytes(record_at, record);
        self.write_bytes(record_at + align_up(record.len(), 8), payload);

        // Release: everything written above is visible to a consumer that acquires this.
        control.head.store(cursor + total as u64, Ordering::Release);
        Ok(())
    }

    /// True when `needed` bytes are free, refreshing the cached tail only if it has to.
    fn has_room(&mut self, control: &RingControl, head: u64, needed: usize) -> bool {
        let free = |tail: u64| self.capacity - (head - tail) as usize;
        if free(self.cached_tail) >= needed {
            return true;
        }
        self.cached_tail = control.tail.load(Ordering::Acquire);
        free(self.cached_tail) >= needed
    }

    fn write_header(&self, offset: usize, header: RecordHeader) {
        // SAFETY: `offset` is within the data region and 16-byte aligned, and the caller has
        // established that the header's bytes are free. The write is unaligned-safe regardless.
        unsafe {
            data_of(self.base)
                .add(offset)
                .cast::<RecordHeader>()
                .write_unaligned(header);
        }
    }

    fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // SAFETY: the caller has established that `bytes.len()` bytes from `offset` are free
        // and do not cross the wrap, since a record is contiguous by construction.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                data_of(self.base).add(offset),
                bytes.len(),
            );
        }
    }
}

/// A record borrowed from the ring, valid until the consumer advances past it.
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    /// What kind of envelope this is.
    pub kind: EnvelopeKind,
    /// The fixed envelope struct's bytes.
    pub record: &'a [u8],
    /// The payload region its spans address.
    pub payload: &'a [u8],
    /// Bytes this record occupies, including any skip that preceded it.
    consumed: u64,
}

/// How far the tail moves when a record is consumed.
///
/// A token rather than a bare count, because it is only ever meaningful for the record it came
/// from, and because it lets the borrow on the ring end before [`Consumer::advance`] takes it
/// mutably.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consumed(u64);

impl Record<'_> {
    /// How far the tail moves when this record is consumed.
    #[must_use]
    pub fn consumed(&self) -> Consumed {
        Consumed(self.consumed)
    }
}

impl Consumer {
    /// Bytes written but not yet consumed.
    #[must_use]
    pub fn occupancy(&self) -> usize {
        // SAFETY: the region outlives this half.
        let control = unsafe { control(self.base) };
        let head = control.head.load(Ordering::Acquire);
        let tail = control.tail.load(Ordering::Relaxed);
        (head - tail) as usize
    }

    /// Borrows the next record without consuming it.
    ///
    /// Peek and [`advance`](Self::advance) are separate so a drain can stop on a budget rather
    /// than on emptiness. §11.2 requires exactly that: geometry is applied up to a per-tick
    /// ceiling and the rest waits for the next tick, because amortizing buffer creation across
    /// two or three ticks is invisible where a 12 ms tick is not.
    ///
    /// Returns `None` when the ring is empty, or when the next record's framing is
    /// inconsistent — a length that overruns what the producer published means a torn read or
    /// a mismatched build, and the only safe response is to stop rather than to interpret it.
    #[must_use]
    pub fn peek(&mut self) -> Option<Record<'_>> {
        // SAFETY: the region outlives this half.
        let control = unsafe { control(self.base) };
        let tail = control.tail.load(Ordering::Relaxed);
        if self.cached_head == tail {
            // Acquire: pairs with the producer's release, making its bytes visible.
            self.cached_head = control.head.load(Ordering::Acquire);
        }
        let available = self.cached_head - tail;
        if available == 0 {
            return None;
        }

        let mut cursor = tail;
        let mut offset = (cursor as usize) & (self.capacity - 1);
        let mut header = self.read_header(offset);

        if header.flags & FLAG_SKIP != 0 {
            let skip = header.total_len as u64;
            if skip == 0 || skip > available {
                return None;
            }
            cursor += skip;
            if cursor == self.cached_head {
                return None;
            }
            offset = (cursor as usize) & (self.capacity - 1);
            header = self.read_header(offset);
        }

        let total = header.total_len as usize;
        let record_len = header.record_len as usize;
        let payload_len = header.payload_len as usize;
        let consumed = (cursor - tail) + total as u64;

        // Framing must describe something that fits both the published bytes and the space the
        // record claims. Anything else is corruption, not a short read.
        if total == 0
            || consumed > available
            || record_size(record_len, payload_len) != total
            || offset + total > self.capacity
        {
            return None;
        }
        let kind = EnvelopeKind::from_repr(header.kind)?;

        let record_at = offset + size_of::<RecordHeader>();
        let payload_at = record_at + align_up(record_len, 8);
        // SAFETY: the checks above put both ranges inside the data region and inside the bytes
        // the producer released, and the producer does not touch them again until the tail
        // advances past them, which only `advance` does.
        let (record, payload) = unsafe {
            let base = data_of(self.base);
            (
                core::slice::from_raw_parts(base.add(record_at), record_len),
                core::slice::from_raw_parts(base.add(payload_at), payload_len),
            )
        };

        Some(Record {
            kind,
            record,
            payload,
            consumed,
        })
    }

    /// Consumes the record the given token came from.
    ///
    /// Take the token with [`Record::consumed`], let the record's borrow end, then call this.
    /// Advancing twice for one record, or with a token from a record that was not at the head
    /// of the ring, desynchronizes the stream.
    pub fn advance(&mut self, consumed: Consumed) {
        // SAFETY: the region outlives this half.
        let control = unsafe { control(self.base) };
        let tail = control.tail.load(Ordering::Relaxed);
        // Release: the producer may reuse these bytes as soon as it observes this.
        control.tail.store(tail + consumed.0, Ordering::Release);
    }

    fn read_header(&self, offset: usize) -> RecordHeader {
        // SAFETY: `offset` is inside the data region, and a header's worth of bytes there was
        // published by the producer before it released `head`.
        unsafe {
            data_of(self.base)
                .add(offset)
                .cast::<RecordHeader>()
                .read_unaligned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 8-aligned region for a ring of `capacity` data bytes.
    ///
    /// Backed by `u64` so the allocator hands back something the control block's atomics can
    /// live in; a `Vec<u8>` is only guaranteed 1-aligned.
    struct Region {
        buf: Vec<u64>,
        capacity: usize,
    }

    impl Region {
        fn new(capacity: usize) -> Self {
            let words = region_size(capacity).div_ceil(8);
            Self {
                buf: vec![0u64; words],
                capacity,
            }
        }

        fn base(&mut self) -> *mut u8 {
            self.buf.as_mut_ptr().cast()
        }

        fn open(&mut self) -> (Producer, Consumer) {
            let capacity = self.capacity;
            // SAFETY: `buf` is large enough for `region_size(capacity)`, 8-aligned, and
            // outlives the halves within each test.
            unsafe { init(self.base(), capacity) }
        }
    }

    /// Reads one record, checks it, and consumes it.
    fn expect(consumer: &mut Consumer, kind: EnvelopeKind, record: &[u8], payload: &[u8]) {
        let consumed = {
            let got = consumer.peek().expect("a record");
            assert_eq!(got.kind, kind);
            assert_eq!(got.record, record);
            assert_eq!(got.payload, payload);
            got.consumed()
        };
        consumer.advance(consumed);
    }

    #[test]
    fn round_trips_one_record() {
        let mut region = Region::new(256);
        let (mut producer, mut consumer) = region.open();

        assert!(consumer.peek().is_none(), "a fresh ring is empty");

        let record = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let payload = [9u8; 24];
        producer
            .write(EnvelopeKind::GeometryAdd, &record, &payload)
            .unwrap();

        expect(&mut consumer, EnvelopeKind::GeometryAdd, &record, &payload);
        assert!(consumer.peek().is_none());
        assert_eq!(consumer.occupancy(), 0);
    }

    #[test]
    fn preserves_order_across_many_records() {
        let mut region = Region::new(4096);
        let (mut producer, mut consumer) = region.open();

        for i in 0u8..16 {
            producer
                .write(EnvelopeKind::ViewUse, &[i; 8], &[i; 16])
                .unwrap();
        }
        for i in 0u8..16 {
            expect(&mut consumer, EnvelopeKind::ViewUse, &[i; 8], &[i; 16]);
        }
        assert!(consumer.peek().is_none());
    }

    #[test]
    fn empty_record_and_empty_payload_are_legal() {
        let mut region = Region::new(256);
        let (mut producer, mut consumer) = region.open();

        // GeometryRemove is 8 bytes with nothing variable-length behind it.
        producer
            .write(EnvelopeKind::GeometryRemove, &[7u8; 8], &[])
            .unwrap();
        expect(&mut consumer, EnvelopeKind::GeometryRemove, &[7u8; 8], &[]);

        producer.write(EnvelopeKind::ViewRelease, &[], &[]).unwrap();
        expect(&mut consumer, EnvelopeKind::ViewRelease, &[], &[]);
    }

    /// A record that will not fit before the wrap is preceded by a skip record, and the
    /// consumer steps over it transparently. The sizes here are chosen so 128 bytes holds two
    /// 48-byte records with 32 left over — too little for a third, so the third wraps.
    #[test]
    fn wraps_without_splitting_a_record() {
        let mut region = Region::new(128);
        let (mut producer, mut consumer) = region.open();
        assert_eq!(record_size(24, 0), 48);

        producer
            .write(EnvelopeKind::ViewUse, &[1u8; 24], &[])
            .unwrap();
        producer
            .write(EnvelopeKind::ViewUse, &[2u8; 24], &[])
            .unwrap();
        expect(&mut consumer, EnvelopeKind::ViewUse, &[1u8; 24], &[]);
        expect(&mut consumer, EnvelopeKind::ViewUse, &[2u8; 24], &[]);

        // 32 bytes to the end, 48 needed: skip then wrap.
        producer
            .write(EnvelopeKind::ViewUse, &[3u8; 24], &[])
            .unwrap();
        expect(&mut consumer, EnvelopeKind::ViewUse, &[3u8; 24], &[]);
        assert!(consumer.peek().is_none());
    }

    /// Many wraps in a row, with the ring never more than a couple of records deep. This is
    /// the shape that catches an off-by-one in the skip accounting, which a single wrap can
    /// miss.
    #[test]
    fn survives_repeated_wraps() {
        let mut region = Region::new(128);
        let (mut producer, mut consumer) = region.open();

        for i in 0u8..200 {
            producer
                .write(EnvelopeKind::ViewUse, &[i; 24], &[])
                .unwrap();
            expect(&mut consumer, EnvelopeKind::ViewUse, &[i; 24], &[]);
        }
        assert_eq!(consumer.occupancy(), 0);
    }

    /// Backpressure: the producer is refused rather than overwriting, and recovers once the
    /// consumer drains. §4 makes this block by design — a lossless envelope is never dropped.
    #[test]
    fn reports_backpressure_and_recovers() {
        let mut region = Region::new(128);
        let (mut producer, mut consumer) = region.open();

        // Two 48-byte records fit; the third cannot, because it would need a 32-byte skip
        // plus 48 bytes and only 32 are free.
        producer
            .write(EnvelopeKind::ViewUse, &[1u8; 24], &[])
            .unwrap();
        producer
            .write(EnvelopeKind::ViewUse, &[2u8; 24], &[])
            .unwrap();
        assert_eq!(
            producer.write(EnvelopeKind::ViewUse, &[3u8; 24], &[]),
            Err(Full)
        );
        assert_eq!(producer.occupancy(), 96);

        expect(&mut consumer, EnvelopeKind::ViewUse, &[1u8; 24], &[]);
        producer
            .write(EnvelopeKind::ViewUse, &[3u8; 24], &[])
            .unwrap();

        expect(&mut consumer, EnvelopeKind::ViewUse, &[2u8; 24], &[]);
        expect(&mut consumer, EnvelopeKind::ViewUse, &[3u8; 24], &[]);
    }

    /// A record larger than the whole ring is refused permanently. It shares the `Full` result
    /// with backpressure, and a producer that retried it forever would spin — so the check
    /// exists before any space arithmetic, and the test pins that it does not depend on how
    /// empty the ring is.
    #[test]
    fn refuses_a_record_larger_than_the_ring() {
        let mut region = Region::new(64);
        let (mut producer, mut consumer) = region.open();

        let oversized = [0u8; 256];
        assert_eq!(
            producer.write(EnvelopeKind::GeometryAdd, &oversized, &[]),
            Err(Full)
        );
        assert!(
            consumer.peek().is_none(),
            "a refused write must leave nothing behind"
        );
        assert_eq!(consumer.occupancy(), 0);
    }

    /// Peek is non-destructive, which is what lets §11.2's drain stop on a budget rather than
    /// on emptiness.
    #[test]
    fn peek_without_advance_leaves_the_record() {
        let mut region = Region::new(256);
        let (mut producer, mut consumer) = region.open();
        producer
            .write(EnvelopeKind::ViewUse, &[5u8; 8], &[])
            .unwrap();

        for _ in 0..3 {
            let got = consumer.peek().expect("still there");
            assert_eq!(got.record, &[5u8; 8]);
        }
        assert_eq!(consumer.occupancy(), record_size(8, 0));
        expect(&mut consumer, EnvelopeKind::ViewUse, &[5u8; 8], &[]);
    }

    #[test]
    fn attach_rejects_a_mismatched_region() {
        let mut region = Region::new(256);
        let (_producer, _consumer) = region.open();

        // SAFETY: the region is initialized and outlives the call.
        assert!(
            unsafe { attach(region.base(), 512) }.is_none(),
            "a capacity disagreement means the two sides do not share a layout"
        );

        // SAFETY: as above.
        let control = unsafe { &*region.base().cast::<RingControl>() };
        assert_eq!(control.abi_rev, crate::ABI_REV);
        assert_eq!(control.capacity, 256);
    }

    /// The real shape: producer and consumer on different threads, with content that proves
    /// nothing was reordered, torn, or skipped. Backpressure is exercised on purpose by making
    /// the ring far smaller than the traffic.
    #[test]
    fn spsc_across_threads() {
        const COUNT: u32 = 20_000;
        let mut region = Region::new(1024);
        let (mut producer, mut consumer) = region.open();

        std::thread::scope(|scope| {
            scope.spawn(move || {
                for i in 0..COUNT {
                    let record = i.to_le_bytes();
                    // Payload length varies so record framing is exercised at many sizes,
                    // including the ones that force a wrap at an awkward offset.
                    let payload = vec![i as u8; (i % 97) as usize];
                    while producer
                        .write(EnvelopeKind::GeometryAdd, &record, &payload)
                        .is_err()
                    {
                        std::thread::yield_now();
                    }
                }
            });

            scope.spawn(move || {
                let mut next = 0u32;
                while next < COUNT {
                    let consumed = {
                        let Some(got) = consumer.peek() else {
                            std::thread::yield_now();
                            continue;
                        };
                        assert_eq!(got.kind, EnvelopeKind::GeometryAdd);
                        assert_eq!(got.record, next.to_le_bytes());
                        assert_eq!(got.payload.len(), (next % 97) as usize);
                        assert!(got.payload.iter().all(|&b| b == next as u8));
                        got.consumed()
                    };
                    consumer.advance(consumed);
                    next += 1;
                }
                assert_eq!(consumer.occupancy(), 0);
            });
        });
    }
}
