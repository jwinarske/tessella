// SPDX-License-Identifier: BSD-2-Clause
// Copyright (c) 2026 Joel Winarske

//! One frame, written to bytes and read back.
//!
//! The seam between the halves has one gap: a producer can build a real frame -- a style, its
//! tiles, their buckets, through `frame::emit` into a ring -- and nothing can hand that frame to a
//! consumer in another process or another run. `tools/capture-render` is the only thing that
//! consumes one, and it does so in-process by holding the `SlabArena` the producer wrote.
//!
//! So a consumer's fixtures are hand-written rings, which is what makes them fixtures: they carry
//! the records their author thought of. A capture carries what a producer actually emitted.
//!
//! # What a consumer needs, and nothing else
//!
//! Exactly three things, which is what [`decode()`](crate::capture::decode) returns:
//!
//! - the **records**, as `(kind, record, payload)` triples;
//! - the **region**, one flat run of bytes;
//! - the **slab table**, each slab's offset and length within that region.
//!
//! That is the shape `tessella_consume::slab::resolve` already takes, so a replay hands the last
//! two straight through with no translation. [`Slab`](crate::capture::Slab) is structurally
//! `tessella_consume::slab::Slab` and deliberately not that type: this crate is the one both
//! halves depend on, and depending on the consumer would invert that.
//!
//! # Records, not the ring's bytes
//!
//! A capture holds records, not a copy of the ring. The ring's layout is its own -- a head and a
//! tail over one buffer, with wrap-around in the middle of a record -- so a file mirroring it
//! would pin that layout as a format and break whenever the ring changed. Writing each record's
//! kind, record and payload, and replaying them through `Producer::write`, reconstructs an
//! equivalent stream through the public API on both sides.
//!
//! It also means a capture is not byte-identical to what crossed the ring, and does not claim to
//! be: it is the same *records* in the same order.
//!
//! # No padding, because nothing needs it
//!
//! Records sit flush against each other at whatever offset they land on. Every envelope is read
//! with `WireRecord::from_bytes`, which is a `read_unaligned`, so an aligned record buys nothing
//! and padding to reach alignment would be bytes written for a requirement that does not exist.
//!
//! # Layout
//!
//! Little-endian throughout, as the wire is.
//!
//! | offset | field | width |
//! |---|---|---|
//! | 0 | [`MAGIC`](crate::capture::MAGIC) | 8 |
//! | 8 | record count | 4 |
//! | 12 | slab count | 4 |
//! | 16 | length of the records section | 8 |
//! | 24 | length of the region | 8 |
//! | 32 | the records | *as stated* |
//! | | the slab table, 16 bytes each | *count* x 16 |
//! | | the region | *as stated* |
//!
//! Each record in the records section is a 16-byte head -- kind, record length, payload length, a
//! reserved word -- then the record's bytes, then its payload's.

extern crate alloc;

use alloc::vec::Vec;

/// What a capture file starts with, and the whole of its versioning.
///
/// The trailing digits are the format's version. A reader checks the lot and refuses anything
/// else rather than trying to interpret an older layout: a capture is a development artifact with
/// no compatibility promise, and the failure that matters is reading one as though it were this
/// version.
pub const MAGIC: [u8; 8] = *b"TSLCAP01";

/// Bytes before the first record.
const HEAD: usize = 32;

/// Bytes of head per record.
const PER_RECORD: usize = 16;

/// Bytes per slab-table entry.
const PER_SLAB: usize = 16;

/// Where a slab sits within the region.
///
/// Structurally `tessella_consume::slab::Slab`, which is the type a consumer resolves against.
/// Not that type, because this crate is the one both halves depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slab {
    /// Byte offset of the slab within the region.
    pub offset: u64,
    /// Length of the slab in bytes.
    pub length: u64,
}

/// One record, as a capture holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    /// The `EnvelopeKind` discriminant, carried as it was written rather than decoded here: a
    /// capture of a newer producer may hold a kind this build does not know, and refusing to read
    /// the file for it would lose every other record too.
    pub kind: u32,
    /// The fixed-size record.
    pub record: &'a [u8],
    /// The variable-length payload, empty where the record has none.
    pub payload: &'a [u8],
}

/// Builds a capture.
///
/// Records are pushed in the order they were read, then [`Self::finish`] takes the slab table and
/// the region. That order is the useful one: a caller drains a ring first and has the arena to
/// hand afterwards.
#[derive(Debug, Default)]
pub struct Builder {
    records: Vec<u8>,
    count: u32,
}

impl Builder {
    /// An empty capture.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many records have been pushed.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether nothing has been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Adds one record.
    ///
    /// A record whose lengths do not fit a `u32` is dropped rather than truncated, and the count
    /// does not rise. Nothing on the wire is that large -- the ring's own framing is 32-bit -- so
    /// this is the unreachable arm written as a drop rather than a panic.
    pub fn push(&mut self, kind: u32, record: &[u8], payload: &[u8]) {
        let (Ok(record_len), Ok(payload_len)) =
            (u32::try_from(record.len()), u32::try_from(payload.len()))
        else {
            return;
        };
        self.records.extend_from_slice(&kind.to_le_bytes());
        self.records.extend_from_slice(&record_len.to_le_bytes());
        self.records.extend_from_slice(&payload_len.to_le_bytes());
        self.records.extend_from_slice(&0u32.to_le_bytes());
        self.records.extend_from_slice(record);
        self.records.extend_from_slice(payload);
        self.count += 1;
    }

    /// Finishes into the bytes of a capture.
    #[must_use]
    pub fn finish(self, slabs: &[Slab], region: &[u8]) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(HEAD + self.records.len() + slabs.len() * PER_SLAB + region.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&(slabs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.records.len() as u64).to_le_bytes());
        out.extend_from_slice(&(region.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.records);
        for slab in slabs {
            out.extend_from_slice(&slab.offset.to_le_bytes());
            out.extend_from_slice(&slab.length.to_le_bytes());
        }
        out.extend_from_slice(region);
        out
    }
}

/// A capture, borrowed from the bytes it was decoded from.
#[derive(Debug, Clone, Copy)]
pub struct Capture<'a> {
    records: &'a [u8],
    count: u32,
    slabs: &'a [u8],
    region: &'a [u8],
}

impl<'a> Capture<'a> {
    /// How many records it holds.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.count
    }

    /// Whether it holds no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The region a slab reference resolves against.
    #[must_use]
    pub fn region(&self) -> &'a [u8] {
        self.region
    }

    /// The slab table, in slab-handle order.
    ///
    /// Collected rather than borrowed as a slice: the file holds them little-endian and a `&[Slab]`
    /// would be a cast, which is a promise about alignment and byte order that a file read into a
    /// `Vec<u8>` cannot make.
    #[must_use]
    pub fn slabs(&self) -> Vec<Slab> {
        self.slabs
            .as_chunks::<PER_SLAB>()
            .0
            .iter()
            .map(|entry| Slab {
                offset: u64::from_le_bytes(entry[0..8].try_into().unwrap_or_default()),
                length: u64::from_le_bytes(entry[8..16].try_into().unwrap_or_default()),
            })
            .collect()
    }

    /// The records, in the order they were pushed.
    ///
    /// Stops at the first record whose head or body does not fit, rather than skipping it: the
    /// lengths are what locate the next record, so one that does not fit means the rest of the
    /// section cannot be found either.
    pub fn records(&self) -> impl Iterator<Item = Record<'a>> + '_ {
        let mut at = 0usize;
        core::iter::from_fn(move || {
            let head = self.records.get(at..at.checked_add(PER_RECORD)?)?;
            let kind = u32::from_le_bytes(head[0..4].try_into().ok()?);
            let record_len = u32::from_le_bytes(head[4..8].try_into().ok()?) as usize;
            let payload_len = u32::from_le_bytes(head[8..12].try_into().ok()?) as usize;
            let body = at.checked_add(PER_RECORD)?;
            let middle = body.checked_add(record_len)?;
            let end = middle.checked_add(payload_len)?;
            let record = self.records.get(body..middle)?;
            let payload = self.records.get(middle..end)?;
            at = end;
            Some(Record {
                kind,
                record,
                payload,
            })
        })
    }
}

/// Reads a capture, or `None` if the bytes are not one.
///
/// Refused rather than repaired, in every case: a capture is produced and consumed by the same
/// tree, so a file that does not fit its own header is a bug upstream and reading what can be
/// salvaged would hide it.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<Capture<'_>> {
    if bytes.get(0..8)? != MAGIC {
        return None;
    }
    let count = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
    let slab_count = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?) as usize;
    let records_len =
        usize::try_from(u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?)).ok()?;
    let region_len =
        usize::try_from(u64::from_le_bytes(bytes.get(24..32)?.try_into().ok()?)).ok()?;

    let slabs_at = HEAD.checked_add(records_len)?;
    let region_at = slabs_at.checked_add(slab_count.checked_mul(PER_SLAB)?)?;
    let end = region_at.checked_add(region_len)?;
    if end != bytes.len() {
        // Equal rather than "fits": trailing bytes mean the header and the file disagree about
        // what is in it, which is the same fault as a short file and worth the same answer.
        return None;
    }

    Some(Capture {
        records: bytes.get(HEAD..slabs_at)?,
        count,
        slabs: bytes.get(slabs_at..region_at)?,
        region: bytes.get(region_at..end)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capture round-trips its records, its slab table and its region.
    #[test]
    fn a_capture_round_trips() {
        let mut builder = Builder::new();
        assert!(builder.is_empty());
        builder.push(7, &[1, 2, 3, 4], &[9, 9]);
        builder.push(8, &[5, 6], &[]);
        assert_eq!(builder.len(), 2);

        let slabs = [
            Slab {
                offset: 0,
                length: 4,
            },
            Slab {
                offset: 4,
                length: 2,
            },
        ];
        let region = [10u8, 11, 12, 13, 14, 15];
        let bytes = builder.finish(&slabs, &region);

        let read = decode(&bytes).expect("a capture");
        assert_eq!(read.len(), 2);
        assert!(!read.is_empty());
        assert_eq!(read.region(), &region);
        assert_eq!(read.slabs(), slabs);

        let records: Vec<Record<'_>> = read.records().collect();
        assert_eq!(records.len(), 2, "and the iterator agrees with the count");
        assert_eq!(records[0].kind, 7);
        assert_eq!(records[0].record, &[1, 2, 3, 4]);
        assert_eq!(records[0].payload, &[9, 9]);
        assert_eq!(records[1].kind, 8);
        assert_eq!(records[1].record, &[5, 6]);
        assert_eq!(
            records[1].payload,
            &[] as &[u8],
            "a record with no payload reads as empty rather than as the next record's bytes"
        );
    }

    /// An empty capture is a capture, and resolves nothing.
    ///
    /// Worth having because it is what a frame that emitted nothing produces, and the shortest
    /// file the format can hold -- so it is where an off-by-one in the header arithmetic shows.
    #[test]
    fn an_empty_capture_is_still_one() {
        let bytes = Builder::new().finish(&[], &[]);
        assert_eq!(bytes.len(), HEAD, "the head and nothing else");
        let read = decode(&bytes).expect("a capture");
        assert!(read.is_empty());
        assert_eq!(read.len(), 0);
        assert_eq!(read.records().count(), 0);
        assert_eq!(read.slabs(), Vec::new());
        assert_eq!(read.region(), &[] as &[u8]);
    }

    /// Anything that is not a capture is refused, and so is one that does not fit its header.
    #[test]
    fn a_capture_that_does_not_fit_is_refused() {
        assert!(decode(&[]).is_none(), "nothing at all");
        assert!(decode(b"TSLCAP01").is_none(), "the magic and no header");

        let mut builder = Builder::new();
        builder.push(1, &[1, 2], &[3]);
        let good = builder.finish(
            &[Slab {
                offset: 0,
                length: 1,
            }],
            &[42],
        );
        assert!(decode(&good).is_some(), "the control");

        // The magic, and nothing else, changed. This has to be a *well-formed* capture or the
        // length checks refuse it first and the magic goes untested -- which is what the three
        // short cases above do, and why they are not enough on their own.
        let mut wrong_magic = good.clone();
        wrong_magic[7] = b'2';
        assert_eq!(
            wrong_magic.len(),
            good.len(),
            "same bytes, so only the magic can decide"
        );
        assert!(
            decode(&wrong_magic).is_none(),
            "another version of the format is refused rather than read as this one"
        );

        // Short by one, long by one, and a header claiming more than is there.
        assert!(decode(&good[..good.len() - 1]).is_none(), "truncated");
        let mut extra = good.clone();
        extra.push(0);
        assert!(
            decode(&extra).is_none(),
            "trailing bytes are a disagreement, not slack"
        );
        let mut lying = good.clone();
        lying[16] = 0xff;
        assert!(decode(&lying).is_none(), "a records section that overruns");
    }

    /// A record claiming more bytes than the section holds stops the walk.
    ///
    /// The lengths are what locate the next record, so a record that does not fit means the rest
    /// cannot be found -- the iterator ends rather than skipping to a guess.
    #[test]
    fn a_record_that_overruns_ends_the_walk() {
        let mut builder = Builder::new();
        builder.push(1, &[1, 2, 3, 4], &[]);
        builder.push(2, &[5, 6, 7, 8], &[]);
        let mut bytes = builder.finish(&[], &[]);

        // The first record's length, made longer than the section.
        let record_len_at = HEAD + 4;
        bytes[record_len_at..record_len_at + 4].copy_from_slice(&999u32.to_le_bytes());
        let read = decode(&bytes).expect("the header still fits");
        assert_eq!(
            read.records().count(),
            0,
            "the first record does not fit, so no record is located"
        );
        assert_eq!(
            read.len(),
            2,
            "and the count still says what was written, which is how the two disagree visibly"
        );
    }
}
