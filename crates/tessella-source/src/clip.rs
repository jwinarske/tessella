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
//! # Where the rotation comes from: wagyu, not the clip
//!
//! Resolved. mbgl runs *every* GeoJSON polygon through `fixupPolygons` before it reaches a
//! bucket — `geojson_tile_data.hpp` calls it unconditionally on any feature whose type is
//! Polygon, citing geojson-vt-cpp issue 44 — and `fixupPolygons` hands the rings to wagyu and
//! takes a `clip_type_union` of them. Wagyu does not preserve input order: it rebuilds each ring
//! from its own sweep and chooses its own starting vertex. That is the whole of it.
//!
//! So this is not a clipping question and never was. The clip below is a faithful port of
//! geojson-vt's `clipRing` and produces the right ring; a later normalization pass rotates it.
//!
//! # What was ruled out getting there
//!
//! Every earlier suspect is refuted, and the last of them by simulation rather than by argument:
//!
//! - **Axis order.** Clipping y-before-x is identical to x-before-y. There is a test.
//! - **The clip algorithm.** `clip_ring` is a faithful port of `clipRing`, nested cases and
//!   last-point rule included, rather than the textbook Sutherland-Hodgman it started as. The
//!   rotation survived the correction. The port is kept because it is the real algorithm and the
//!   reconstruction was only accidentally equivalent.
//! - **The pyramid recursion.** This was the last hypothesis standing, on the grounds that the
//!   model of it here was a reconstruction rather than a port. It has since been simulated
//!   properly — descending z0 to z13 with geojson-vt's own `splitTile` bounds, `p = 0.5 * buffer
//!   / extent`, clipping x then y at every level, twenty-six clips in total — and it lands on
//!   exactly the ring this module produces in one pass. The recursion is innocent.
//! - **The `z` significance filter.** `tile.hpp` keeps only points whose `z` exceeds the tile's
//!   squared tolerance, which looked like it could drop a ring's first point. It cannot here:
//!   intersections are created with `z = 1.0` and `sq_tolerance` is about `8e-15`.
//! - **A rotated or reflected input ring.** Running the clip from each of the four starting
//!   points, in both directions, produces the oracle's sequence from none of them.
//!
//! # Why the port stops here
//!
//! Wagyu is a full polygon-clipping library — a sweep-line union with its own topology
//! structures — and porting it would buy a vertex *order*, not a different polygon. On
//! well-formed input its union is geometrically an identity: same rings, same winding, same
//! area, same triangulation up to a permutation of indices. mbgl runs it because GeoJSON is
//! allowed to be self-intersecting and wrongly-wound and it has to cope with that; the hermetic
//! style's rectangles are neither.
//!
//! What that costs is byte-exact vertex-buffer comparison against the oracle for GeoJSON
//! polygon sources, which is why the test below compares rings as cycles. It is worth revisiting
//! if a real style turns up geometry where wagyu is not an identity — self-intersecting rings are
//! where it would show, because there the union genuinely changes the polygon and a cycle
//! comparison would stop being enough.
//!
//! # What the rotation costs
//!
//! A ring is cyclic, so its starting vertex carries no geometric meaning — but unlike triangle
//! emission order, it is not free for the *oracle* to normalize away. The index buffer refers to
//! vertices by position, so a rotation moves both buffers, and the flat vertex buffer does not
//! record where one ring ends and the next begins, so the probe cannot canonicalize rings the
//! way it canonicalizes triangles. Triangles are self-delimiting; rings are not.
//!
//! This side can compare as cycles, because it knows its own ring boundaries. That is what the
//! test below does, and it is a real comparison: it still catches a wrong vertex, a missing one,
//! a reversed winding, or a different entry point in the *cycle*. What it gives up is the
//! sequence, which wagyu owns.
//!
//! Simplification is not implicated in the values: at a tolerance of 6 tile units a rectangle's
//! corners are all significant, so nothing is dropped from this style.

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

    /// The clipped ring is the oracle's, as a cycle.
    ///
    /// The sequences differ because mbgl passes every GeoJSON polygon through wagyu's union
    /// before it reaches a bucket, and wagyu picks its own starting vertex — see the note at the
    /// top of this file. What is asserted is what survives that: the same vertices, in the same
    /// cyclic order, entered at some rotation.
    ///
    /// This still fails on a wrong coordinate, a missing or extra vertex, or a reversed winding.
    /// Only the entry point is given up.
    #[test]
    fn the_clipped_ring_is_the_oracles_cycle() {
        let (lo, hi) = box_bounds();
        let clipped = round_to_tile_units(&clip_ring_to_box(&hermetic_polygon(), lo, hi));

        // Both are closed; compare the open cycles.
        let ours = &clipped[..clipped.len() - 1];
        let theirs = &ORACLE[..ORACLE.len() - 1];
        assert_eq!(ours.len(), theirs.len(), "{ours:?} vs {theirs:?}");

        let offset = (0..theirs.len())
            .find(|offset| {
                (0..theirs.len()).all(|i| ours[i] == theirs[(i + offset) % theirs.len()])
            })
            .unwrap_or_else(|| panic!("{ours:?} is not a rotation of {theirs:?}"));

        // The rotation is real, so the test is not passing by the sequences being equal.
        assert_ne!(offset, 0, "if this is ever zero, wagyu has been ported");
    }

    /// And the winding survives, which a cycle comparison alone would not establish.
    ///
    /// A ring traversed backwards visits the same vertices in a cyclic order too — the reverse
    /// cycle — so the rotation check above would reject it, but only because the sequence differs.
    /// Signed area says it directly, and it is what decides whether the polygon is an exterior or
    /// a hole.
    #[test]
    fn the_winding_survives_the_clip() {
        let (lo, hi) = box_bounds();
        let clipped = round_to_tile_units(&clip_ring_to_box(&hermetic_polygon(), lo, hi));

        let signed = |ring: &[[i32; 2]]| -> i64 {
            let n = ring.len() - 1;
            (0..n)
                .map(|i| {
                    let (a, b) = (ring[i], ring[(i + 1) % n]);
                    i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1])
                })
                .sum()
        };
        let ours = signed(&clipped);
        let theirs = signed(&ORACLE);
        assert_eq!(ours.signum(), theirs.signum(), "{ours} vs {theirs}");
        assert_eq!(ours, theirs, "and the same area, not merely the same sign");
    }

    /// The tiling pyramid produces the same ring as one clip against the target tile.
    ///
    /// This is the evidence that retired the last hypothesis before wagyu was found. geojson-vt
    /// does not clip once against the tile you ask for: it splits a parent into four children by
    /// clipping left/right and then top/bottom, so a ring reaching z13 has been clipped
    /// twenty-six times. That looked like somewhere a rotation could accumulate.
    ///
    /// It does not. Descending z0 to z13 along the path to 4092/2723, with geojson-vt's own
    /// bounds — `(x - p) / 2^z` to `(x + 0.5 + p) / 2^z` for a left child and the mirror for a
    /// right one, `p = 0.5 * buffer / extent` — lands on the ring this module produces in a
    /// single pass. Twenty-six clips, no rotation.
    #[test]
    fn the_tiling_pyramid_does_not_rotate_the_ring() {
        // The polygon in normalized mercator, which is the space geojson-vt splits in.
        let world: Vec<[f64; 2]> = [
            [-0.16, 51.49],
            [-0.16, 51.52],
            [-0.12, 51.52],
            [-0.12, 51.49],
            [-0.16, 51.49],
        ]
        .iter()
        .map(|[lon, lat]| {
            let sin = (lat * core::f64::consts::PI / 180.0).sin();
            [
                lon / 360.0 + 0.5,
                0.5 - 0.25 * ((1.0 + sin) / (1.0 - sin)).ln() / core::f64::consts::PI,
            ]
        })
        .collect();

        // geojson-vt's `p = 0.5 * buffer / extent`, both in tile units.
        let options = TilingOptions::default();
        let (lo_bound, hi_bound) = options.clip_range();
        let extent = f64::from(hi_bound + lo_bound);
        let p = 0.5 * f64::from(-lo_bound) / extent;
        let (target_z, target_x, target_y) = (13u32, 4092u32, 2723u32);

        let mut ring = world;
        for z in 0..target_z {
            let scale = f64::from(1u32 << z);
            let step = 1u32 << (target_z - z);
            let (x, y) = (target_x / step, target_y / step);
            let (child_x, child_y) = (target_x / (step / 2), target_y / (step / 2));

            let (x_lo, x_hi) = if child_x == 2 * x {
                ((f64::from(x) - p) / scale, (f64::from(x) + 0.5 + p) / scale)
            } else {
                (
                    (f64::from(x) + 0.5 - p) / scale,
                    (f64::from(x) + 1.0 + p) / scale,
                )
            };
            ring = clip_ring(&ring, x_lo, x_hi, Axis::X);

            let (y_lo, y_hi) = if child_y == 2 * y {
                ((f64::from(y) - p) / scale, (f64::from(y) + 0.5 + p) / scale)
            } else {
                (
                    (f64::from(y) + 0.5 - p) / scale,
                    (f64::from(y) + 1.0 + p) / scale,
                )
            };
            ring = clip_ring(&ring, y_lo, y_hi, Axis::Y);
            assert!(!ring.is_empty(), "the ring vanished at z{}", z + 1);
        }

        // Into tile-local units, the space the single-pass clip works in.
        let scale = f64::from(1u32 << target_z);
        let descended: Vec<[f64; 2]> = ring
            .iter()
            .map(|point| {
                [
                    (point[0] * scale - f64::from(target_x)) * extent,
                    (point[1] * scale - f64::from(target_y)) * extent,
                ]
            })
            .collect();

        let (lo, hi) = box_bounds();
        let single_pass = clip_ring_to_box(&hermetic_polygon(), lo, hi);
        assert_eq!(
            round_to_tile_units(&descended),
            round_to_tile_units(&single_pass),
            "twenty-six clips and one clip disagree"
        );
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
