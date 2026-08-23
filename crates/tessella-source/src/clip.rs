//! Clipping rings to the buffered tile box.
//!
//! # What is established, and what is not
//!
//! Measured against `mbgl-capture-probe --dump-vertices` on the hermetic style, for the fill
//! geometry in tile 13/4092/2723:
//!
//! - **Values agree exactly.** Project to tile-local, clip to
//!   `[-buffer, EXTENT + buffer]`, round to nearest, and every coordinate matches the oracle:
//!   `2941.84 → 2942`, `4819.84 → 4820`, `10398.38 → 10240`, `13804.40 → 10240`.
//! - **The vertex set and the winding agree.**
//! - **The starting vertex does not.** The oracle's ring is this one rotated. Same cycle, same
//!   direction, different entry point.
//!
//! Two hypotheses for the rotation were tested and both refuted. Clipping y-before-x gives the
//! identical result to x-before-y, so axis order is not it. And simulating geojson-vt's
//! recursive pyramid split — clipping once per level from z0 down to z13 rather than once
//! against the target tile — also lands on the same rotation, so the recursion is not it
//! either. mbgl's `addRingVertices` emits the ring verbatim, so nothing downstream introduces
//! it. That places the rotation inside geojson-vt's own `clipLine`, whose handling of the
//! first vertex differs from the textbook Sutherland-Hodgman reconstructed here.
//!
//! # Why this is left as it is for now
//!
//! A ring is cyclic, so its starting vertex carries no geometric meaning — but unlike triangle
//! emission order, it is not free to normalize away. The index buffer refers to vertices by
//! position, so a rotation changes both buffers, and the flat vertex buffer does not record
//! where one ring ends and the next begins, so the oracle cannot canonicalize rings the way it
//! canonicalizes triangles.
//!
//! So vertex-exact agreement needs geojson-vt's `clipLine` ported faithfully rather than
//! reconstructed. That is a bounded piece of work and now a precisely located one: not "port
//! geojson-vt", but "match its clip output ordering". Simplification is not implicated —
//! at a tolerance of 6 tile units a rectangle's corners are all significant, so it is a no-op
//! on this style — and neither is the recursion.

use alloc::vec::Vec;

use crate::geojson::{Position, Ring};

/// Which axis a clip pass runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Clip on x.
    X,
    /// Clip on y.
    Y,
}

impl Axis {
    fn of(self, point: Position) -> f64 {
        match self {
            Self::X => point[0],
            Self::Y => point[1],
        }
    }
}

/// Clips a closed ring to `lo..=hi` on one axis.
///
/// The ring is expected closed — first position repeated at the end, as GeoJSON writes it and
/// as the oracle keeps it — and the result is closed the same way. An empty result means the
/// ring lies wholly outside.
#[must_use]
pub fn clip_ring(ring: &[Position], lo: f64, hi: f64, axis: Axis) -> Ring {
    if ring.len() < 2 {
        return Vec::new();
    }

    let mut out: Ring = Vec::with_capacity(ring.len());
    for pair in ring.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (ak, bk) = (axis.of(a), axis.of(b));
        let at = |t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];

        if ak < lo {
            // Entering through the low edge.
            if bk > lo {
                out.push(at((lo - ak) / (bk - ak)));
            }
        } else if ak > hi {
            // Entering through the high edge.
            if bk < hi {
                out.push(at((hi - ak) / (bk - ak)));
            }
        } else {
            out.push(a);
        }

        // Leaving. Checked separately from entering because one edge can do both, crossing
        // the box entirely.
        if bk < lo && ak >= lo {
            out.push(at((lo - ak) / (bk - ak)));
        }
        if bk > hi && ak <= hi {
            out.push(at((hi - ak) / (bk - ak)));
        }
    }

    if let (Some(first), Some(last)) = (out.first().copied(), out.last().copied())
        && first != last
    {
        out.push(first);
    }
    out
}

/// Clips a ring to a box on both axes.
#[must_use]
pub fn clip_ring_to_box(ring: &[Position], lo: f64, hi: f64) -> Ring {
    let clipped = clip_ring(ring, lo, hi, Axis::X);
    if clipped.is_empty() {
        return Vec::new();
    }
    clip_ring(&clipped, lo, hi, Axis::Y)
}

/// Rounds tile-local coordinates to the integers the vertex buffer carries.
///
/// Round to nearest, not truncate: the oracle turns `2941.84` into `2942`. Truncating would
/// give `2941`, which is a whole unit of drift on every vertex and would fail the diff for a
/// reason that reads as a projection error.
#[must_use]
pub fn round_to_tile_units(ring: &[Position]) -> Vec<[i32; 2]> {
    #[allow(clippy::cast_possible_truncation)]
    ring.iter()
        .map(|p| [p[0].round() as i32, p[1].round() as i32])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiling::TilingOptions;

    fn box_bounds() -> (f64, f64) {
        let (lo, hi) = TilingOptions::default().clip_range();
        (f64::from(lo), f64::from(hi))
    }

    /// The hermetic style's second polygon, projected into tile 13/4092/2723 — the numbers the
    /// projection produces before anything clips them.
    fn hermetic_polygon() -> Ring {
        alloc::vec![
            [2941.84, 13804.40],
            [2941.84, 4819.84],
            [10398.38, 4819.84],
            [10398.38, 13804.40],
            [2941.84, 13804.40],
        ]
    }

    /// What the oracle emits for that polygon.
    const ORACLE: [[i32; 2]; 5] = [
        [10240, 4820],
        [10240, 10240],
        [2942, 10240],
        [2942, 4820],
        [10240, 4820],
    ];

    /// Values, as a set. This is the part that agrees.
    #[test]
    fn the_clipped_values_match_the_oracle() {
        let (lo, hi) = box_bounds();
        let clipped = round_to_tile_units(&clip_ring_to_box(&hermetic_polygon(), lo, hi));

        let mut ours: Vec<[i32; 2]> = clipped[..clipped.len() - 1].to_vec();
        let mut theirs: Vec<[i32; 2]> = ORACLE[..4].to_vec();
        ours.sort_unstable();
        theirs.sort_unstable();
        assert_eq!(ours, theirs);
    }

    /// Winding, which is a real property: a reversed ring is a hole rather than a fill.
    #[test]
    fn the_clipped_ring_winds_the_same_way_as_the_oracles() {
        let (lo, hi) = box_bounds();
        let clipped = round_to_tile_units(&clip_ring_to_box(&hermetic_polygon(), lo, hi));

        let area = |ring: &[[i32; 2]]| {
            let mut sum = 0i64;
            for pair in ring.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                sum += i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1]);
            }
            sum
        };
        assert_eq!(area(&clipped).signum(), area(&ORACLE).signum());
        assert_eq!(
            area(&clipped).abs(),
            area(&ORACLE).abs(),
            "and the same area"
        );
    }

    /// The divergence, recorded rather than hidden: same cycle, different entry point.
    ///
    /// When this starts failing, geojson-vt's clip ordering has been matched and the note at
    /// the top of this file can go.
    #[test]
    fn the_starting_vertex_still_differs_from_the_oracle() {
        let (lo, hi) = box_bounds();
        let clipped = round_to_tile_units(&clip_ring_to_box(&hermetic_polygon(), lo, hi));
        assert_ne!(clipped.as_slice(), ORACLE.as_slice());

        // But it is a rotation of it, not a different ring.
        let ours = &clipped[..clipped.len() - 1];
        let theirs = &ORACLE[..4];
        let rotated = (0..theirs.len()).any(|offset| {
            (0..theirs.len()).all(|i| ours[i] == theirs[(i + offset) % theirs.len()])
        });
        assert!(rotated, "{ours:?} should be a rotation of {theirs:?}");
    }

    /// Rounding to nearest, not truncating. A whole unit of drift on every vertex would read
    /// as a projection error rather than a rounding one.
    #[test]
    fn coordinates_round_rather_than_truncate() {
        assert_eq!(
            round_to_tile_units(&[[2941.84, 4819.84], [-0.6, 0.4]]),
            [[2942, 4820], [-1, 0]]
        );
    }

    #[test]
    fn a_ring_wholly_inside_is_unchanged() {
        let (lo, hi) = box_bounds();
        let ring: Ring = alloc::vec![
            [100.0, 100.0],
            [200.0, 100.0],
            [200.0, 200.0],
            [100.0, 100.0]
        ];
        assert_eq!(clip_ring_to_box(&ring, lo, hi), ring);
    }

    #[test]
    fn a_ring_wholly_outside_clips_to_nothing() {
        let (lo, hi) = box_bounds();
        let ring: Ring = alloc::vec![
            [50_000.0, 50_000.0],
            [50_100.0, 50_000.0],
            [50_100.0, 50_100.0],
            [50_000.0, 50_000.0],
        ];
        assert!(clip_ring_to_box(&ring, lo, hi).is_empty());
    }

    /// A ring crossing the box completely enters and leaves on the same edge pair, which is
    /// the case the enter and leave tests have to handle independently.
    #[test]
    fn a_ring_spanning_the_box_is_clipped_on_both_sides() {
        let clipped = clip_ring(
            &[
                [-10_000.0, 100.0],
                [10_000.0, 100.0],
                [10_000.0, 200.0],
                [-10_000.0, 200.0],
                [-10_000.0, 100.0],
            ],
            0.0,
            1000.0,
            Axis::X,
        );
        assert!(clipped.iter().all(|p| p[0] >= 0.0 && p[0] <= 1000.0));
        assert!(clipped.iter().any(|p| p[0] == 0.0));
        assert!(clipped.iter().any(|p| p[0] == 1000.0));
    }

    /// Clipping keeps the ring closed, because everything downstream assumes it.
    #[test]
    fn the_result_stays_closed() {
        let (lo, hi) = box_bounds();
        let clipped = clip_ring_to_box(&hermetic_polygon(), lo, hi);
        assert_eq!(clipped.first(), clipped.last());
    }

    /// Neither axis order changes the result, which is one of the two hypotheses for the
    /// rotation that this ruled out.
    #[test]
    fn axis_order_does_not_matter() {
        let (lo, hi) = box_bounds();
        let ring = hermetic_polygon();

        let x_then_y = clip_ring(&clip_ring(&ring, lo, hi, Axis::X), lo, hi, Axis::Y);
        let y_then_x = clip_ring(&clip_ring(&ring, lo, hi, Axis::Y), lo, hi, Axis::X);
        assert_eq!(x_then_y, y_then_x);
    }
}
