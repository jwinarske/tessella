//! Turning a slab reference into bytes, safely.
//!
//! A `SlabRef` is a slab, an offset and a length. It arrives from another process and nothing
//! about it is trustworthy: the slab may not exist, the offset may be past the end of the region,
//! and the length may run off it. So resolution is bounds-checked and returns `None` rather than
//! a short slice or a panic — a consumer that aborts on a malformed stream is a consumer that can
//! be crashed by the thing it is meant to survive.
//!
//! # Why it lives here and not in each backend
//!
//! Because the check would otherwise be written once per consumer, and two consumers writing the
//! same check can be wrong in the same way and still agree with each other. That is the one class
//! of bug an independent second implementation does not catch, so the check belongs in the crate
//! that owns the reading.
//!
//! # Why it takes the region as an argument
//!
//! The producer repacks its slab table after every frame that allocates, so the region a consumer
//! was handed last frame is not the one to resolve against this frame. Resolution is therefore a
//! function of *(region, reference)* and never a method that remembers a region — which has the
//! happy consequence that the returned lifetime comes from the region rather than from any state,
//! so a resolved slice does not borrow the host and does not stop it being read again.

use tessella_capture_abi::envelope::SlabRef;

/// Where a slab sits within the mapped region.
///
/// The consumer is given this table; it is not derivable from the reference alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slab {
    /// Byte offset of the slab within the region.
    pub offset: u64,
    /// Length of the slab in bytes.
    pub length: u64,
}

/// Resolves a reference into borrowed bytes, or `None` if it does not fit.
///
/// `slabs` indexes by the reference's slab number. A reference naming a slab past the end of the
/// table is as malformed as one running off the end of its slab, and both answer the same way.
///
/// The bytes are valid as long as the region is, which is until the producer is told it may reuse
/// them — not until the next record is read. That is the difference between data that lives in a
/// slab and data that lives in the ring, and it is why geometry can be resolved when a backend is
/// ready to upload it rather than when it was announced.
#[must_use]
pub fn resolve<'a>(region: &'a [u8], slabs: &[Slab], reference: SlabRef) -> Option<&'a [u8]> {
    let slab = slabs.get(reference.slab as usize)?;

    // Every arithmetic step checked, in the region's width rather than the reference's: a 32-bit
    // offset plus a 32-bit length cannot overflow a u64, so the sum is exact and the comparison
    // below is the only thing deciding.
    let within = u64::from(reference.offset);
    let length = u64::from(reference.length);
    let end_in_slab = within.checked_add(length)?;
    if end_in_slab > slab.length {
        return None;
    }

    let start = slab.offset.checked_add(within)?;
    let end = start.checked_add(length)?;
    if end > region.len() as u64 {
        return None;
    }

    // Both fit in the region, so both fit in a usize on any target whose regions this large are
    // addressable at all.
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    region.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::{Slab, resolve};
    use tessella_capture_abi::envelope::SlabRef;

    const REGION: [u8; 32] = [7; 32];

    fn slabs() -> [Slab; 2] {
        [
            Slab {
                offset: 0,
                length: 16,
            },
            Slab {
                offset: 16,
                length: 16,
            },
        ]
    }

    fn reference(slab: u32, offset: u32, length: u32) -> SlabRef {
        SlabRef {
            slab,
            offset,
            length,
        }
    }

    #[test]
    fn a_reference_within_its_slab_resolves() {
        let bytes = resolve(&REGION, &slabs(), reference(1, 4, 8)).expect("resolves");
        assert_eq!(bytes.len(), 8);
    }

    #[test]
    fn a_whole_slab_resolves() {
        assert_eq!(
            resolve(&REGION, &slabs(), reference(0, 0, 16)).map(<[u8]>::len),
            Some(16)
        );
    }

    /// An empty reference is legal and resolves to nothing, rather than to the rest of the slab.
    #[test]
    fn a_zero_length_reference_resolves_to_nothing() {
        assert_eq!(
            resolve(&REGION, &slabs(), reference(0, 4, 0)).map(<[u8]>::len),
            Some(0)
        );
    }

    /// One byte past the end of its slab is refused, even though the region has room.
    ///
    /// The slab bound is the one that matters: the bytes past it belong to the next slab, and
    /// handing them over is one geometry reading another's vertices.
    #[test]
    fn a_reference_running_past_its_slab_is_refused() {
        assert_eq!(resolve(&REGION, &slabs(), reference(0, 8, 9)), None);
        assert_eq!(resolve(&REGION, &slabs(), reference(0, 16, 1)), None);
    }

    #[test]
    fn a_slab_that_is_not_in_the_table_is_refused() {
        assert_eq!(resolve(&REGION, &slabs(), reference(2, 0, 1)), None);
        assert_eq!(resolve(&REGION, &slabs(), reference(u32::MAX, 0, 1)), None);
    }

    /// A slab the table places outside the region is refused when it is read.
    ///
    /// The table comes from the producer too, so it is checked rather than trusted.
    #[test]
    fn a_slab_outside_the_region_is_refused() {
        let beyond = [Slab {
            offset: 64,
            length: 16,
        }];
        assert_eq!(resolve(&REGION, &beyond, reference(0, 0, 1)), None);
    }

    /// Offsets and lengths at the type's limits do not wrap into something that looks valid.
    #[test]
    fn arithmetic_at_the_limits_does_not_wrap() {
        let huge = [Slab {
            offset: 0,
            length: u64::MAX,
        }];
        assert_eq!(
            resolve(&REGION, &huge, reference(u32::MAX, u32::MAX, u32::MAX)),
            None,
            "a slab number past the table"
        );
        assert_eq!(
            resolve(&REGION, &huge, reference(0, u32::MAX, u32::MAX)),
            None,
            "fits the slab the table claims, but not the region"
        );
    }

    /// An empty region resolves nothing, including an empty reference.
    #[test]
    fn an_empty_region_resolves_nothing_addressable() {
        let empty = [Slab {
            offset: 0,
            length: 8,
        }];
        assert_eq!(resolve(&[], &empty, reference(0, 0, 1)), None);
    }
}
