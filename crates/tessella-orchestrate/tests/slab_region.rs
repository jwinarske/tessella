//! A slab handle has to mean something to a consumer that is not this process.
//!
//! # The gap this closes
//!
//! §3.5 records that nothing in the ABI precludes a consumer in another process, because
//! "slab handles are offsets" rather than pointers. That is true of the handle. What it offsets
//! *into* was undefined: in process a slab is an `Arc` and a handle indexes a `Vec` of them, and
//! a consumer across a mapping has neither. So a C consumer could read every envelope on the
//! ring and reach not one vertex.
//!
//! `SlabArena::pack` is the region that makes a handle resolvable — a header, a table, then the
//! bytes. An in-process Rust consumer still holds `Arc`s and never packs one; this is for the
//! side of the seam that cannot.

use tessella_capture_abi::envelope::{SlabEntry, SlabRef, SlabRegion, WireRecord as _};
use tessella_orchestrate::SlabArena;

/// Resolves a reference the way a consumer with only the bytes must: through the table.
fn resolve(region: &[u8], reference: SlabRef) -> Option<&[u8]> {
    let header = SlabRegion::from_bytes(region)?;
    if header.abi_rev != tessella_capture_abi::ABI_REV {
        return None;
    }
    // Every bound checked against the region's own length, because these arrive from the far
    // side: a table entry that overruns is an out-of-bounds read, not a short one.
    if header.total_len as usize > region.len() {
        return None;
    }
    if reference.slab >= header.count {
        return None;
    }
    let at = size_of::<SlabRegion>() + reference.slab as usize * size_of::<SlabEntry>();
    let entry = SlabEntry::from_bytes(region.get(at..)?)?;

    let start = usize::try_from(entry.offset).ok()?;
    let end = start.checked_add(usize::try_from(entry.length).ok()?)?;
    let slab = region.get(start..end)?;

    let from = reference.offset as usize;
    let to = from.checked_add(reference.length as usize)?;
    slab.get(from..to)
}

/// Every reference the arena hands out resolves to the same bytes through the packed region.
#[test]
fn a_packed_region_resolves_what_the_arena_does() {
    let mut arena = SlabArena::new();
    let payloads: Vec<Vec<u8>> = (0..12u8)
        .map(|n| (0..=n).map(|b| b.wrapping_mul(7).wrapping_add(n)).collect())
        .collect();

    let refs: Vec<SlabRef> = payloads.iter().map(|bytes| arena.alloc(bytes)).collect();
    arena.seal();
    let region = arena.pack();

    for (reference, expected) in refs.iter().zip(&payloads) {
        let in_process = arena.resolve(*reference).expect("the arena resolves it");
        assert_eq!(in_process, expected.as_slice());

        let mapped = resolve(&region, *reference).expect("the region resolves it");
        assert_eq!(
            mapped,
            expected.as_slice(),
            "slab {} offset {} resolved differently through the region",
            reference.slab,
            reference.offset
        );
    }
}

/// Each slab starts eight-aligned, so a consumer can read its contents at natural alignment.
///
/// A vertex buffer of `i16` pairs read from an odd offset is an unaligned load — tolerated on
/// x86 and a fault on some of the targets §16 cross-compiles for.
#[test]
fn every_slab_starts_aligned() {
    let mut arena = SlabArena::new();
    // Lengths that are deliberately not multiples of eight, so padding is what aligns them.
    for length in [1usize, 3, 7, 13, 31] {
        arena.alloc(&vec![0xa5; length]);
    }
    arena.seal();
    let region = arena.pack();

    let header = SlabRegion::from_bytes(&region).expect("a header");
    for index in 0..header.count {
        let at = size_of::<SlabRegion>() + index as usize * size_of::<SlabEntry>();
        let entry = SlabEntry::from_bytes(&region[at..]).expect("an entry");
        assert_eq!(
            entry.offset % 8,
            0,
            "slab {index} begins at {} , which is not eight-aligned",
            entry.offset
        );
    }
    assert_eq!(
        region.len() % 8,
        0,
        "the region itself is not a whole number of words"
    );
}

/// The region states its own revision, because it is mapped separately from the ring.
///
/// A consumer that checked only the ring would accept a slab region from another build — the two
/// regions may be handed over by different means and there is nothing tying them together but
/// this.
#[test]
fn the_region_states_its_revision() {
    let mut arena = SlabArena::new();
    arena.alloc(b"x");
    arena.seal();
    let region = arena.pack();

    let header = SlabRegion::from_bytes(&region).expect("a header");
    assert_eq!(header.abi_rev, tessella_capture_abi::ABI_REV);
    assert_eq!(header.total_len as usize, region.len());

    // A region claiming another revision is refused rather than read.
    let mut wrong = region.clone();
    wrong[0] = wrong[0].wrapping_add(1);
    assert!(
        resolve(
            &wrong,
            SlabRef {
                slab: 0,
                offset: 0,
                length: 1
            }
        )
        .is_none()
    );
}

/// A reference past the end of its slab, or naming a slab that is not there, resolves to nothing.
#[test]
fn an_overrunning_reference_is_refused() {
    let mut arena = SlabArena::new();
    let good = arena.alloc(b"abcd");
    arena.seal();
    let region = arena.pack();

    assert!(resolve(&region, good).is_some());
    for bad in [
        SlabRef {
            slab: good.slab,
            offset: 0,
            length: 5,
        },
        SlabRef {
            slab: good.slab,
            offset: 4,
            length: 1,
        },
        SlabRef {
            slab: good.slab + 1,
            offset: 0,
            length: 1,
        },
        SlabRef {
            slab: u32::MAX,
            offset: 0,
            length: 1,
        },
    ] {
        assert!(
            resolve(&region, bad).is_none(),
            "slab {} offset {} length {} was resolved",
            bad.slab,
            bad.offset,
            bad.length
        );
    }
}

/// An arena with nothing sealed packs a header, a revision and no slabs.
#[test]
fn an_empty_arena_packs_a_valid_region() {
    let region = SlabArena::new().pack();
    let header = SlabRegion::from_bytes(&region).expect("a header");
    assert_eq!(header.count, 0);
    assert_eq!(header.total_len as usize, region.len());
    assert!(
        resolve(
            &region,
            SlabRef {
                slab: 0,
                offset: 0,
                length: 1
            }
        )
        .is_none()
    );
}
