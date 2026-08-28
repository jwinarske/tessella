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

/// Retention: which bytes a slab still owes, and when it can go.
///
/// DR-21 makes a slab the buffer and a geometry a sub-range of it, so a slab outlives any one
/// geometry and is freed when nothing wants any part of it. The arena counts bytes rather than
/// references, because the number that decides whether to compact is how much of the slab is
/// still wanted — a strong count says only whether *anyone* holds it.
mod retention {
    use tessella_orchestrate::SlabArena;

    /// Nothing is retained by default, so an unclaimed slab is swept.
    ///
    /// The right default, and not merely a convenient one: a frame that allocated and then
    /// failed to write has retained nothing, and its bytes go on the next sweep without the
    /// failure path having to say anything.
    #[test]
    fn an_unclaimed_slab_is_swept() {
        let mut arena = SlabArena::new();
        arena.alloc(&[1, 2, 3, 4]);
        arena.seal();
        assert_eq!(arena.slabs().count(), 1);

        let freed = arena.sweep();
        assert_eq!(freed.len(), 1, "nothing wanted it");
        assert!(arena.slabs().next().is_none());
    }

    /// A retained slab survives, and goes once released.
    #[test]
    fn a_retained_slab_survives_until_released() {
        let mut arena = SlabArena::new();
        let reference = arena.alloc(&[1, 2, 3, 4]);
        arena.seal();
        arena.retain(reference);

        assert!(arena.sweep().is_empty(), "something wants it");
        assert_eq!(arena.slabs().count(), 1);

        arena.release(reference);
        assert_eq!(arena.sweep(), vec![reference.slab]);
        assert!(arena.slabs().next().is_none());
    }

    /// A slab holding several geometries lives until the last of them goes.
    ///
    /// This is the case DR-21 is about. A layer's tiles share a slab, so a pan releases some of
    /// them and the slab stays for the rest — which is why the live *fraction* exists, and why
    /// freeing on the first release would be wrong.
    #[test]
    fn a_slab_outlives_any_one_geometry() {
        let mut arena = SlabArena::new();
        let first = arena.alloc(&[0; 40]);
        let second = arena.alloc(&[0; 40]);
        let third = arena.alloc(&[0; 40]);
        arena.seal();
        for reference in [first, second, third] {
            arena.retain(reference);
        }
        assert_eq!(first.slab, third.slab, "one slab holds all three");

        arena.release(first);
        assert!(arena.sweep().is_empty(), "two are still wanted");
        arena.release(second);
        assert!(arena.sweep().is_empty(), "one is");
        arena.release(third);
        assert_eq!(arena.sweep().len(), 1, "and now none");
    }

    /// The live fraction is what a compaction decision reads.
    #[test]
    fn the_live_fraction_falls_as_geometries_go() {
        let mut arena = SlabArena::new();
        let references: Vec<_> = (0..4).map(|_| arena.alloc(&[0; 40])).collect();
        arena.seal();
        let slab = references[0].slab;
        for reference in &references {
            arena.retain(*reference);
        }

        let fraction = arena.live_fraction(slab).expect("a sealed slab");
        assert!(
            (fraction - 1.0).abs() < 1e-9,
            "everything is wanted: {fraction}"
        );

        arena.release(references[0]);
        arena.release(references[1]);
        let fraction = arena.live_fraction(slab).expect("still held");
        assert!((fraction - 0.5).abs() < 1e-9, "half of it is: {fraction}");

        assert_eq!(arena.live_fraction(9999), None, "a slab it does not hold");
    }

    /// A double release does not make a slab outlive its bytes by going negative.
    ///
    /// Saturating rather than panicking: a double release is a producer bug, and the useful
    /// failure is a slab living too long — visible in the fraction — rather than an abort in a
    /// frame loop.
    #[test]
    fn a_double_release_saturates() {
        let mut arena = SlabArena::new();
        let reference = arena.alloc(&[0; 16]);
        arena.seal();
        arena.retain(reference);
        arena.release(reference);
        arena.release(reference);
        assert_eq!(arena.live_fraction(reference.slab), Some(0.0));
        assert_eq!(arena.sweep().len(), 1);
    }

    /// An empty allocation retains nothing, so it cannot pin a slab.
    #[test]
    fn an_empty_reference_pins_nothing() {
        let mut arena = SlabArena::new();
        let real = arena.alloc(&[0; 16]);
        let empty = arena.alloc(&[]);
        arena.seal();
        arena.retain(empty);
        assert_eq!(arena.sweep().len(), 1, "the empty one wanted nothing");

        let _ = real;
    }
}

/// A handle means the same thing after a sweep as before one.
///
/// # The divergence
///
/// `SlabRef::slab` is a slab's id: `SlabArena::slab` looks it up by id, `retain` and `release`
/// key their accounting on it, and the C consumer indexes the region's table with it — the
/// header says the handle indexes the table, and that is the only rule a consumer across a
/// mapping has.
///
/// The table was written in the order the arena held its slabs, which is the same thing only
/// until a slab is freed. After that every id above the hole is one position too high, and a
/// handle resolves to a *different slab's bytes* — not to nothing, which would be a diagnosis,
/// but to plausible geometry belonging to another layer.
///
/// Unreachable until this phase, because nothing swept: retention landed with DR-21 and the
/// frame emitter only began releasing slabs when a drawable left the cover.
#[test]
fn a_handle_survives_a_sweep() {
    let mut arena = SlabArena::new();

    // Three slabs, of which the first is unwanted.
    let first = arena.alloc(&[0xAA; 32]);
    arena.seal();
    let second = arena.alloc(&[0xBB; 32]);
    arena.seal();
    let third = arena.alloc(&[0xCC; 32]);
    arena.seal();
    arena.retain(second);
    arena.retain(third);

    assert_eq!(arena.sweep(), vec![first.slab], "the unwanted one goes");

    let region = arena.pack();
    for (reference, expected) in [(second, 0xBB), (third, 0xCC)] {
        let bytes = resolve(&region, reference).expect("the handle resolves");
        assert_eq!(
            bytes,
            arena.resolve(reference).expect("and in process too"),
            "handle {} resolves to different bytes across the mapping",
            reference.slab
        );
        assert!(
            bytes.iter().all(|byte| *byte == expected),
            "handle {} came back as another slab's bytes",
            reference.slab
        );
    }
}
