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
//! # Where the rotation is not
//!
//! Three hypotheses tested, all refuted:
//!
//! - **Axis order.** Clipping y-before-x is identical to x-before-y. There is a test.
//! - **The clip algorithm.** `clip_ring` below is now a faithful port of geojson-vt's
//!   `clipRing`, nested cases and last-point rule included, rather than the textbook
//!   Sutherland-Hodgman it started as. The rotation survives the correction, so the clip is
//!   not the cause. The port is kept because it is the real algorithm and the reconstruction
//!   was only accidentally equivalent.
//! - **The pyramid recursion.** Simulating geojson-vt's recursive split — clipping once per
//!   level from z0 to z13 rather than once against the target tile — lands on the same
//!   rotation. This was first tested with the reconstructed clip, which made the result
//!   worthless; it was retested with the faithful one and the conclusion holds.
//!
//! And mbgl's `addRingVertices` emits whatever ring it is given, verbatim, so nothing
//! downstream introduces it.
//!
//! # The open lead, narrowed
//!
//! Two further hypotheses tested since, both refuted:
//!
//! - **The `z` significance filter.** `tile.hpp`'s `transform(vt_linear_ring)` keeps only
//!   points whose `z` exceeds the tile's squared tolerance, which looked like it could drop a
//!   ring's first point and so rotate it. It cannot, here: intersection points are created
//!   with `z = 1.0`, and the tile tolerance at z13 is `6 / (2^13 * 8192)`, so `sq_tolerance` is
//!   about `8e-15`. Nothing is filtered.
//! - **A rotated or reflected input ring.** Running the clip from each of the four starting
//!   points, in both directions, produces the oracle's sequence from none of them. The oracle's
//!   ring is not this clip applied to a differently-ordered input.
//!
//! What that leaves is the recursive split, which is the one step whose *model* here is a
//! reconstruction rather than a port. Every other stage has now been read from the source and
//! matched. The earlier "recursion refuted" result therefore says the reconstruction of it
//! reproduces the rotation, not that the recursion is innocent — the suspect is the model.
//!
//! Concretely: geojson-vt splits a parent into four children by clipping left/right and then
//! top/bottom, and a ring surviving thirteen levels of that is clipped twenty-six times. The
//! simulation here descends one path and clips twice per level, which is the same *shape* but
//! not demonstrably the same *sequence*. Resolving this means running geojson-vt itself and
//! comparing, rather than reasoning about it further.
//!
//! # Why this cannot be normalized away
//!
//! A ring is cyclic, so its starting vertex carries no geometric meaning — but unlike triangle
//! emission order, it is not free to normalize. The index buffer refers to vertices by
//! position, so a rotation changes both buffers, and the flat vertex buffer does not record
//! where one ring ends and the next begins, so the oracle cannot canonicalize rings the way it
//! canonicalizes triangles. Triangles are self-delimiting; rings are not.
//!
//! Simplification is not implicated in the *values*: at a tolerance of 6 tile units a
//! rectangle's corners are all significant, so nothing is dropped from this style. Its role in
//! the *ordering*, through the `z` filter above, is the open question.

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
    let last_edge = ring.len() - 2;
    for (i, pair) in ring.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        let (ak, bk) = (axis.of(a), axis.of(b));
        let at = |k: f64| {
            let t = (k - ak) / (bk - ak);
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
        };

        // The three cases are geojson-vt's, nested exactly as it nests them. An earlier
        // version here checked entering and leaving independently, which is the obvious
        // reading and produces the same ring for simple shapes but not the same sequence.
        if ak < lo {
            if bk > lo {
                // Enters through the low edge.
                out.push(at(lo));
                if bk > hi {
                    // And leaves through the high one: crosses the box entirely.
                    out.push(at(hi));
                } else if i == last_edge {
                    out.push(b);
                }
            }
        } else if ak > hi {
            if bk < hi {
                out.push(at(hi));
                if bk < lo {
                    out.push(at(lo));
                } else if i == last_edge {
                    out.push(b);
                }
            }
        } else {
            out.push(a);
            if bk < lo {
                out.push(at(lo));
            } else if bk > hi {
                out.push(at(hi));
            }
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
