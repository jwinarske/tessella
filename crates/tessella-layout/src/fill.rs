//! Fill buckets: rings in, vertices and triangles out.
//!
//! Every rule here is transcribed from mbgl — `classifyRings` and `signedArea` in
//! `tile/geometry_tile_data.cpp`, `generateFillBuffers` and `addFillIndices` in
//! `gfx/fill_generator.cpp` — because the bucket is what the oracle diff compares and a rule
//! invented here would diverge in a way the diff reports two steps away from its cause.
//!
//! # What agreement is established, and what is not
//!
//! Coordinate *values* and ring *winding* match the oracle (see `tessella_source::clip`). The
//! *starting vertex* of a clipped ring does not: geojson-vt emits a rotation of what this
//! pipeline produces, and the cause is still open. A rotation changes which index refers to
//! which vertex, so the vertex and index buffers will not be byte-identical until it is
//! resolved — but it does not change what geometry is produced, so everything this module
//! decides is testable today.
//!
//! # Winding decides structure, not appearance
//!
//! `classify_rings` splits a ring list into polygons by the sign of each ring's area. The first
//! non-degenerate ring fixes the sign that means "exterior"; a later ring of that same sign
//! starts a new polygon, and rings of the opposite sign are holes in the current one.
//!
//! Which absolute direction means "exterior" is therefore never assumed — it is whatever the
//! data's first ring uses. That matters because tile space has y increasing downward, so the
//! screen-space sense of a positive area is inverted relative to the mathematical convention,
//! and any rule hardcoding "counter-clockwise is exterior" would be right in one coordinate
//! system and silently wrong in the other, turning every hole into a separate filled polygon.

use alloc::vec::Vec;

/// A tile-local position, in the integer units the vertex buffer carries.
pub type Position = [i16; 2];

/// A closed ring.
pub type Ring = Vec<Position>;

/// A draw segment: a contiguous index range with its own vertex base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Segment {
    /// First vertex.
    pub vertex_offset: u32,
    /// First index.
    pub index_offset: u32,
    /// Vertices in this segment.
    pub vertex_length: u32,
    /// Indices in this segment.
    pub index_length: u32,
}

/// A built fill bucket.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillBucket {
    /// Vertex positions, in the order rings were added.
    pub vertices: Vec<Position>,
    /// Triangle indices, relative to their segment's vertex base.
    pub indices: Vec<u16>,
    /// Draw segments.
    pub segments: Vec<Segment>,
    /// Line indices for the outline, over the same vertices.
    ///
    /// A fill draws twice — its triangles and then its outline — and the oracle gives the two
    /// different shaders, `FillShader` and `FillOutlineShader`, over one vertex buffer. What
    /// differs is the indices: triangles from earcut, and a line loop per ring.
    pub line_indices: Vec<u16>,
    /// Draw segments for [`Self::line_indices`].
    ///
    /// Its own vector because a line segment's index range is not a triangle segment's, and a
    /// segment carries both offsets. The vertex ranges do coincide, which is what lets the
    /// outline share the fill's vertices.
    pub line_segments: Vec<Segment>,
}

/// Largest vertex index a segment can address.
///
/// Indices are u16, so a segment cannot span more than this many vertices and a new one opens
/// when the next polygon would cross the limit. §12.4 allows a u32 spill per segment; until
/// something needs it, splitting is the same thing mbgl does.
const MAX_SEGMENT_VERTICES: usize = u16::MAX as usize;

/// Twice a ring's signed area, in mbgl's formulation.
///
/// Not divided by two, and not absolute: only the sign is read, and keeping the doubled integer
/// form avoids a division whose only effect would be to introduce rounding. The exact
/// expression is mbgl's — `(p2.x - p1.x) * (p1.y + p2.y)` summed over edges — because a
/// different but algebraically equivalent shoelace can disagree in sign on a degenerate ring,
/// and that is precisely where the classification turns.
#[must_use]
pub fn signed_area(ring: &[Position]) -> i64 {
    let len = ring.len();
    if len < 3 {
        return 0;
    }
    let mut sum = 0i64;
    let mut j = len - 1;
    for i in 0..len {
        let p1 = ring[i];
        let p2 = ring[j];
        sum += (i64::from(p2[0]) - i64::from(p1[0])) * (i64::from(p1[1]) + i64::from(p2[1]));
        j = i;
    }
    sum
}

/// Groups rings into polygons by winding.
///
/// A polygon is an exterior ring followed by its holes. Degenerate rings — zero area — are
/// dropped: they contribute nothing to draw and would otherwise take the place of the sign that
/// decides what "exterior" means.
#[must_use]
pub fn classify_rings(rings: &[Ring]) -> Vec<Vec<Ring>> {
    // One ring cannot be a hole in anything, so no classification is needed and none is done.
    // mbgl short-circuits here too, and the difference is observable: it keeps a lone
    // zero-area ring that the loop below would drop.
    if rings.len() <= 1 {
        return alloc::vec![rings.to_vec()];
    }

    let mut polygons: Vec<Vec<Ring>> = Vec::new();
    let mut polygon: Vec<Ring> = Vec::new();
    // 0 until the first non-degenerate ring fixes it.
    let mut exterior_sign = 0i8;

    for ring in rings {
        let area = signed_area(ring);
        if area == 0 {
            continue;
        }
        let sign = if area < 0 { -1 } else { 1 };

        if exterior_sign == 0 {
            exterior_sign = sign;
        }

        if sign == exterior_sign && !polygon.is_empty() {
            polygons.push(core::mem::take(&mut polygon));
        }
        polygon.push(ring.clone());
    }

    if !polygon.is_empty() {
        polygons.push(polygon);
    }
    polygons
}

/// The most interior rings a polygon is triangulated with. mbgl's `limitHoles(polygon, 500)`.
pub const MAX_HOLES: usize = 500;

/// Drops all but the largest holes, as mbgl does before triangulating.
///
/// # Why a cap at all
///
/// earcut's hole elimination is quadratic in the hole count: each hole is joined to the outer
/// ring by a bridge found by scanning, so a polygon with thousands of interior rings costs
/// seconds and produces a triangulation nobody can see the difference in. mbgl caps it at five
/// hundred, and the cap is not an optimization this build chose — a bucket built without it has
/// *more* geometry than the oracle's for the same feature, which fails the diff.
///
/// # Why the largest, and why the order after is not defined
///
/// Largest by absolute area, because a dropped hole is a hole that gets painted over and the
/// visible cost of that scales with its size. mbgl selects them with `std::nth_element`, which
/// partitions rather than sorts: everything before the nth is no smaller than everything after,
/// and nothing more is promised. So the kept holes' relative order is unspecified there, and
/// matching it exactly is neither possible nor necessary — earcut's output does not depend on
/// the order holes are presented in, only on which ones are.
pub fn limit_holes(polygon: &mut Vec<Ring>) {
    if polygon.len() <= 1 + MAX_HOLES {
        return;
    }
    // The exterior stays put; only the holes are ranked.
    polygon[1..].sort_by(|a, b| {
        signed_area(b)
            .abs()
            .cmp(&signed_area(a).abs())
            // A stable tiebreak so a rebuild of the same tile keeps the same holes, which a
            // partition does not guarantee and a diff against a previous frame relies on.
            .then_with(|| b.len().cmp(&a.len()))
    });
    polygon.truncate(1 + MAX_HOLES);
}

/// Builds a fill bucket from a feature's rings.
///
/// Rings are added to the vertex buffer in order, then each polygon is triangulated and its
/// indices appended relative to the current segment's vertex base.
#[must_use]
pub fn build(rings: &[Ring]) -> FillBucket {
    build_features(core::slice::from_ref(&rings))
}

/// Builds a bucket from several features, each classified on its own.
///
/// # Why the feature boundary matters
///
/// `classify_rings` decides exterior from hole by winding, walking a ring list and starting a
/// new polygon at each exterior. Handed every feature's rings at once it will happily attach one
/// feature's hole to another feature's exterior, because from inside the list there is nothing
/// to say where one feature ended. mbgl calls `classifyRings` per feature for exactly this
/// reason.
///
/// It is also what makes the difference between tessellating a real tile and appearing to hang:
/// a water layer with 47 features and 4875 rings becomes one polygon with 4874 holes, and earcut
/// is not linear in holes.
///
/// The single-feature case is `build`, which is this with one entry — so there is one
/// implementation and the boundary is explicit at every call site rather than implied by
/// whichever `Vec` the caller happened to build.
#[must_use]
pub fn build_features(features: &[&[Ring]]) -> FillBucket {
    build_features_tracked(features).0
}

/// As [`build_features`], reporting the bucket's vertex count after each input feature.
///
/// The paint binder needs to know which vertices belong to which feature, and cannot work it
/// out from the geometry: `classify_rings` may split one feature into several polygons and drops
/// degenerate ones, so a feature's vertex count is not the sum of its rings' lengths. Taking the
/// count from the bucket after each feature is the only reading that stays right when a ring is
/// dropped — and a binder that guessed instead would paint every feature after the first
/// dropped ring with its neighbour's colour.
#[must_use]
pub fn build_features_tracked(features: &[&[Ring]]) -> (FillBucket, Vec<usize>) {
    let mut bucket = FillBucket::default();
    let mut ends = Vec::with_capacity(features.len());

    for rings in features {
        build_polygons(&mut bucket, classify_rings(rings));
        ends.push(bucket.vertices.len());
    }

    (bucket, ends)
}

fn build_polygons(bucket: &mut FillBucket, polygons: Vec<Vec<Ring>>) {
    for mut polygon in polygons {
        // Before anything is counted: the cap changes the vertex count as well as the
        // triangulation, and a segment sized against the uncapped count is a segment whose
        // length does not describe its own buffer.
        limit_holes(&mut polygon);
        let total_vertices: usize = polygon.iter().map(Vec::len).sum();
        if total_vertices == 0 {
            continue;
        }
        let start_vertices = bucket.vertices.len();

        // A segment opens when there is none, or when this polygon would push the current one
        // past what a u16 index can reach. The check is against the whole polygon rather than
        // each ring, because a polygon's triangles index across its rings.
        let needs_segment = bucket.segments.last().is_none_or(|segment| {
            segment.vertex_length as usize + total_vertices > MAX_SEGMENT_VERTICES
        });
        if needs_segment {
            #[allow(clippy::cast_possible_truncation)]
            bucket.segments.push(Segment {
                vertex_offset: start_vertices as u32,
                index_offset: bucket.indices.len() as u32,
                vertex_length: 0,
                index_length: 0,
            });
        }

        // Indices are relative to the segment's vertex base, not to the buffer's start. That
        // is what lets a segment be drawn with its own vertex offset and keeps indices in u16
        // however large the buffer grows.
        let base = bucket
            .segments
            .last()
            .map_or(0, |segment| segment.vertex_length);

        let mut flat: Vec<f64> = Vec::with_capacity(total_vertices * 2);
        let mut holes: Vec<usize> = Vec::new();
        for (index, ring) in polygon.iter().enumerate() {
            if index > 0 {
                holes.push(flat.len() / 2);
            }
            let ring_base = bucket.vertices.len() - start_vertices + base as usize;
            for point in ring {
                bucket.vertices.push(*point);
                flat.push(f64::from(point[0]));
                flat.push(f64::from(point[1]));
            }
            outline_indices(bucket, ring_base, ring.len());
        }
        debug_assert_eq!(flat.len(), total_vertices * 2);

        let triangles = earcutr::earcut(&flat, &holes, 2).unwrap_or_default();

        #[allow(clippy::cast_possible_truncation)]
        for index in &triangles {
            bucket.indices.push(base as u16 + *index as u16);
        }

        if let Some(segment) = bucket.segments.last_mut() {
            #[allow(clippy::cast_possible_truncation)]
            {
                segment.vertex_length += total_vertices as u32;
                segment.index_length += triangles.len() as u32;
            }
        }
    }
}

/// Adds one ring's outline, as mbgl's `addOutlineIndices` adds it.
///
/// # A line per vertex, and the closing one first
///
/// `count` vertices give `count` segments and `count * 2` indices — one more segment than the
/// gaps between the points, because the ring closes. mbgl emits that closing segment *first*,
/// from the last vertex back to the first, and only then walks the consecutive pairs. A natural
/// implementation appends it last instead, which draws the same outline and does not match the
/// oracle's index buffer.
///
/// The count includes a ring's repeated closing point, which this build keeps. So a square
/// arrives as five vertices and produces five segments, the first of them from that repeat back
/// to the point it repeats — zero length, and drawn. That is mbgl's arithmetic and the golden
/// carries it: five vertices, ten indices.
fn outline_indices(bucket: &mut FillBucket, base: usize, count: usize) {
    if count == 0 {
        return;
    }

    let needs_segment = bucket
        .line_segments
        .last()
        .is_none_or(|segment| segment.vertex_length as usize + count > MAX_SEGMENT_VERTICES);
    if needs_segment {
        #[allow(clippy::cast_possible_truncation)]
        bucket.line_segments.push(Segment {
            vertex_offset: base as u32,
            index_offset: bucket.line_indices.len() as u32,
            vertex_length: 0,
            index_length: 0,
        });
    }

    let Some(segment) = bucket.line_segments.last_mut() else {
        return;
    };
    #[allow(clippy::cast_possible_truncation)]
    let first = segment.vertex_length as u16;

    #[allow(clippy::cast_possible_truncation)]
    {
        bucket.line_indices.push(first + count as u16 - 1);
        bucket.line_indices.push(first);
        for offset in 1..count as u16 {
            bucket.line_indices.push(first + offset - 1);
            bucket.line_indices.push(first + offset);
        }
        segment.vertex_length += count as u32;
        segment.index_length += count as u32 * 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(points: &[(i16, i16)]) -> Ring {
        points.iter().map(|(x, y)| [*x, *y]).collect()
    }

    /// A closed square, wound as the hermetic style's clipped fill is.
    fn square() -> Ring {
        ring(&[
            (10240, 4820),
            (10240, 10240),
            (2942, 10240),
            (2942, 4820),
            (10240, 4820),
        ])
    }

    // --- winding ---

    #[test]
    fn signed_area_reports_a_consistent_sign_per_direction() {
        let forward = square();
        let mut backward = square();
        backward.reverse();

        assert_ne!(signed_area(&forward), 0);
        assert_eq!(signed_area(&forward), -signed_area(&backward));
    }

    /// A ring with fewer than three points encloses nothing.
    #[test]
    fn degenerate_rings_have_no_area() {
        assert_eq!(signed_area(&ring(&[])), 0);
        assert_eq!(signed_area(&ring(&[(0, 0)])), 0);
        assert_eq!(signed_area(&ring(&[(0, 0), (1, 1)])), 0);
        // Three collinear points enclose nothing either, and this one the formula has to
        // work out rather than reject on length.
        assert_eq!(signed_area(&ring(&[(0, 0), (1, 1), (2, 2), (0, 0)])), 0);
    }

    /// Which direction means "exterior" is taken from the data's first ring, never assumed.
    /// Tile space has y increasing downward, so a hardcoded rule would be right in one
    /// coordinate system and silently turn every hole into a filled polygon in the other.
    #[test]
    fn the_exterior_direction_comes_from_the_data() {
        let outer = square();
        let mut hole = ring(&[
            (4000, 6000),
            (6000, 6000),
            (6000, 8000),
            (4000, 8000),
            (4000, 6000),
        ]);
        if signed_area(&hole).signum() == signed_area(&outer).signum() {
            hole.reverse();
        }

        let polygons = classify_rings(&[outer.clone(), hole.clone()]);
        assert_eq!(polygons.len(), 1, "one polygon with a hole");
        assert_eq!(polygons[0].len(), 2);

        // Flip both, and the classification is unchanged: it is the relative sign that
        // carries the meaning.
        let mut flipped_outer = outer;
        let mut flipped_hole = hole;
        flipped_outer.reverse();
        flipped_hole.reverse();
        let polygons = classify_rings(&[flipped_outer, flipped_hole]);
        assert_eq!(polygons.len(), 1);
        assert_eq!(polygons[0].len(), 2);
    }

    #[test]
    fn two_exteriors_become_two_polygons() {
        let a = square();
        let b: Ring = square().iter().map(|p| [p[0] - 9000, p[1]]).collect();
        let polygons = classify_rings(&[a, b]);
        assert_eq!(polygons.len(), 2);
        assert_eq!(polygons[0].len(), 1);
        assert_eq!(polygons[1].len(), 1);
    }

    /// A degenerate ring must not get to decide what "exterior" means, so it is dropped
    /// before the sign is fixed.
    #[test]
    fn a_degenerate_ring_does_not_set_the_exterior_sign() {
        let flat = ring(&[(0, 0), (100, 0), (200, 0), (0, 0)]);
        let real = square();
        let polygons = classify_rings(&[flat, real.clone()]);
        assert_eq!(polygons.len(), 1);
        assert_eq!(polygons[0], alloc::vec![real]);
    }

    /// mbgl short-circuits a single ring without classifying, and the difference is
    /// observable: a lone degenerate ring survives here where the loop would drop it.
    #[test]
    fn a_single_ring_is_passed_through_unclassified() {
        let flat = ring(&[(0, 0), (100, 0), (200, 0), (0, 0)]);
        let polygons = classify_rings(core::slice::from_ref(&flat));
        assert_eq!(polygons.len(), 1);
        assert_eq!(polygons[0], alloc::vec![flat], "kept, despite zero area");
    }

    // --- building ---

    #[test]
    fn builds_a_square_into_two_triangles() {
        let bucket = build(&[square()]);

        assert_eq!(bucket.vertices.len(), 5, "the closing vertex is kept");
        assert_eq!(bucket.indices.len(), 6, "two triangles");
        assert_eq!(bucket.segments.len(), 1);

        let segment = bucket.segments[0];
        assert_eq!(segment.vertex_offset, 0);
        assert_eq!(segment.index_offset, 0);
        assert_eq!(segment.vertex_length, 5);
        assert_eq!(segment.index_length, 6);
    }

    /// The oracle's fill drawable for this tile: five vertices, six indices, one segment.
    /// The counts are checkable today even though the vertex *order* is not.
    #[test]
    fn the_counts_match_the_oracles_fill_drawable() {
        let bucket = build(&[square()]);
        assert_eq!(bucket.vertices.len(), 5);
        assert_eq!(bucket.indices.len(), 6);
        assert_eq!(bucket.segments.len(), 1);
    }

    /// Every index must address a vertex that exists, relative to its segment.
    #[test]
    fn indices_stay_within_their_segment() {
        let outer = square();
        let mut hole = ring(&[
            (4000, 6000),
            (6000, 6000),
            (6000, 8000),
            (4000, 8000),
            (4000, 6000),
        ]);
        if signed_area(&hole).signum() == signed_area(&outer).signum() {
            hole.reverse();
        }
        let bucket = build(&[outer, hole]);

        for segment in &bucket.segments {
            let start = segment.index_offset as usize;
            let end = start + segment.index_length as usize;
            for index in &bucket.indices[start..end] {
                assert!(
                    u32::from(*index) < segment.vertex_length,
                    "index {index} outside a {}-vertex segment",
                    segment.vertex_length
                );
            }
        }
    }

    /// Two polygons share a segment, and the second's indices are offset past the first's
    /// vertices rather than restarting at zero.
    #[test]
    fn a_second_polygon_indexes_past_the_first() {
        let a = square();
        let b: Ring = square().iter().map(|p| [p[0] - 9000, p[1]]).collect();
        let bucket = build(&[a, b]);

        assert_eq!(bucket.segments.len(), 1, "both fit in one segment");
        assert_eq!(bucket.vertices.len(), 10);
        assert_eq!(bucket.indices.len(), 12);

        let second_half = &bucket.indices[6..];
        assert!(
            second_half.iter().all(|i| *i >= 5),
            "the second polygon's indices start past the first's five vertices: {second_half:?}"
        );
    }

    #[test]
    fn a_hole_is_triangulated_as_part_of_its_polygon() {
        let outer = square();
        let mut hole = ring(&[
            (4000, 6000),
            (6000, 6000),
            (6000, 8000),
            (4000, 8000),
            (4000, 6000),
        ]);
        if signed_area(&hole).signum() == signed_area(&outer).signum() {
            hole.reverse();
        }
        let bucket = build(&[outer, hole]);

        assert_eq!(bucket.vertices.len(), 10, "both rings' vertices");
        assert_eq!(bucket.segments.len(), 1, "one polygon, one segment entry");
        assert!(
            bucket.indices.len() > 6,
            "a square with a hole needs more than two triangles, got {}",
            bucket.indices.len() / 3
        );
        assert_eq!(bucket.indices.len() % 3, 0);
    }

    #[test]
    fn an_empty_input_builds_an_empty_bucket() {
        assert_eq!(build(&[]), FillBucket::default());
        assert_eq!(build(&[Vec::new()]), FillBucket::default());
    }
}
