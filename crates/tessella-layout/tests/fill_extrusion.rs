//! Extruded polygons, against mbgl's `FillExtrusionBucket`.
//!
//! The branch under test is the **instanced** one. mbgl chooses between two with
//! `MLN_USE_FILL_EXTRUSION_INSTANCING`, which its own header defines as
//! `(MLN_RENDER_BACKEND_METAL || MLN_RENDER_BACKEND_VULKAN)`, and DR-16 put this build on Vulkan
//! — so that is not a choice made here but the one the target backend takes.
//!
//! The difference is the whole shape of the bucket. Without instancing, each edge contributes
//! four extra vertices and six extra indices to build a wall. With it, a building is its ring
//! points and an earcut roof, and the walls are instanced over the same buffer.

use tessella_layout::fill_extrusion::{build, build_features, pack_vertex};

/// A closed square ring, counter-clockwise, in the winding `classify_rings` reads as exterior.
fn square(size: i16) -> Vec<[i16; 2]> {
    vec![[0, 0], [size, 0], [size, size], [0, size], [0, 0]]
}

/// A building is its outline plus a roof, and no wall geometry at all.
///
/// The instanced path's defining property. A five-point square ring gives five vertices — mbgl
/// keeps the repeated closing point — and two triangles of roof. The non-instanced path would
/// give twenty-one vertices and eight triangles for the same square, so a port of the wrong
/// branch is visible here as a count rather than as a rendering fault.
#[test]
fn a_building_is_an_outline_and_a_roof() {
    let bucket = build(&[square(100)]);

    assert_eq!(
        bucket.vertices.len(),
        5,
        "one per ring point, closing point kept"
    );
    assert_eq!(bucket.indices.len(), 6, "two triangles of roof");
    assert_eq!(bucket.segments.len(), 1);
    assert_eq!(bucket.segments[0].vertex_length, 5);
    assert_eq!(bucket.segments[0].index_length, 6);
}

/// The fractional part of a position is kept, packed seven bits per axis.
///
/// A tile coordinate rounded to an integer moves a wall's foot by up to half a unit, which shows
/// as a seam between a building and the ground it stands on. mbgl packs
/// `(frac.x * 256 + frac.y) * 2 + discarded`, and the arithmetic is transcribed rather than
/// reasoned because the two halves have to agree exactly with the shader's unpacking.
#[test]
fn the_fractional_position_survives_the_packing() {
    // A quarter and a half: 0.25 * 128 = 32, 0.5 * 128 = 64.
    let vertex = pack_vertex(10.25, 20.5, false, 0);
    assert_eq!(vertex.position, [10, 20]);
    assert_eq!(vertex.decimals, (32 * 256 + 64) * 2);

    // A whole coordinate packs to zero, which is what an integer tile position gives.
    assert_eq!(pack_vertex(10.0, 20.0, false, 0).decimals, 0);

    // A negative coordinate floors *down*, so its fraction stays positive — the packing has no
    // room for a sign, and `floor` rather than `trunc` is what keeps it that way.
    let negative = pack_vertex(-0.5, -1.25, false, 0);
    assert_eq!(negative.position, [-1, -2]);
    assert_eq!(negative.decimals, (64 * 256 + 96) * 2);
}

/// The discard flag rides in the low bit, which is why the fraction is doubled.
///
/// A ring's closing point has no edge leaving it and therefore no wall to raise. mbgl passes
/// `!p2` — the absence of a next point — and packs it into the same number as the fraction.
#[test]
fn the_closing_point_is_marked_discarded() {
    let bucket = build(&[square(100)]);
    let discarded: Vec<bool> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.decimals & 1 == 1)
        .collect();
    assert_eq!(
        discarded,
        vec![false, false, false, false, true],
        "only the last point of the ring has no edge leaving it"
    );

    // And the flag does not disturb the fraction beside it.
    assert_eq!(
        pack_vertex(10.25, 20.5, true, 0).decimals,
        (32 * 256 + 64) * 2 + 1
    );
}

/// Edge distance accumulates along a ring, for a pattern to wrap against.
///
/// mbgl's `util::dist<uint16_t>` truncates rather than rounds, so a hundred-unit edge
/// contributes a hundred and a diagonal ten-by-ten contributes fourteen. The value accumulates,
/// which is why the rounding matters: taking the nearest integer instead drifts a pattern along
/// a long wall.
#[test]
fn edge_distance_accumulates_along_the_ring() {
    let bucket = build(&[square(100)]);
    let distances: Vec<u16> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.edge_distance)
        .collect();
    assert_eq!(distances, vec![0, 100, 200, 300, 400]);

    // A diagonal truncates: hypot(10, 10) is 14.14.
    let diagonal = build(&[vec![[0, 0], [10, 10], [20, 0], [0, 0]]]);
    assert_eq!(diagonal.vertices[1].edge_distance, 14);
}

/// The distance resets rather than wrapping when it would leave a `u16`.
///
/// A wrapped distance restarts a pattern at an arbitrary phase; a reset restarts it at its
/// beginning, which is a repeat rather than a glitch. mbgl chooses the reset explicitly.
///
/// The ordering is the part worth pinning, and it is not what a first reading suggests. A vertex
/// is written with the distance *so far*, and only then is the next edge's length added — with
/// the reset checked before that addition. So the point after a reset does not carry zero: it
/// carries the length of the edge that triggered it. Asserting zero there would be asserting a
/// pattern that restarts one vertex too early.
#[test]
fn a_long_ring_resets_its_edge_distance() {
    // Four edges of 20,000 units: the fourth would take the total past 65,535.
    let long = vec![[0, 0], [20_000, 0], [20_000, 20_000], [0, 20_000], [0, 0]];
    let bucket = build(&[long]);
    let distances: Vec<u16> = bucket
        .vertices
        .iter()
        .map(|vertex| vertex.edge_distance)
        .collect();
    assert_eq!(
        distances,
        vec![0, 20_000, 40_000, 60_000, 20_000],
        "the reset happens before the fourth edge is added, not after"
    );

    // And nothing wrapped: every value is below what a u16 holds, which is the property the
    // reset exists to keep.
    assert!(distances.iter().all(|distance| *distance < u16::MAX));
}

/// A hole is tessellated into the roof and contributes wall points of its own.
#[test]
fn a_hole_is_part_of_the_roof_and_has_its_own_walls() {
    // An exterior square with a clockwise inner ring, which `classify_rings` reads as a hole.
    let outer = square(100);
    let inner = vec![[25, 25], [25, 75], [75, 75], [75, 25], [25, 25]];
    let bucket = build(&[outer, inner]);

    assert_eq!(
        bucket.vertices.len(),
        10,
        "both rings contribute their points"
    );
    assert!(
        bucket.indices.len() > 6,
        "the roof is more than two triangles"
    );
    assert_eq!(bucket.segments.len(), 1, "one polygon, one segment");

    // The hole's own distance starts again: it is a separate wall.
    assert_eq!(bucket.vertices[5].edge_distance, 0);
}

/// Each feature is classified on its own.
///
/// The reason `build_features` exists. `classify_rings` decides exterior from hole by winding,
/// and handed two features' rings at once it will attach one building's courtyard to another
/// building's footprint.
#[test]
fn two_features_do_not_share_a_classification() {
    let first = square(100);
    let second = vec![[200, 200], [200, 300], [300, 300], [300, 200], [200, 200]];

    let together = build_features(&[std::slice::from_ref(&first), std::slice::from_ref(&second)]);
    assert_eq!(together.vertices.len(), 10);

    // Handed as one feature, the second ring's winding makes it a hole in the first — a
    // different polygon and a different roof.
    let confused = build_features(&[&[first, second]]);
    assert_ne!(
        confused.indices.len(),
        together.indices.len(),
        "the classification boundary made no difference, so it is not being applied"
    );
}

/// An empty or degenerate input produces an empty bucket rather than a segment with nothing in it.
#[test]
fn nothing_in_gives_nothing_out() {
    assert_eq!(build(&[]).vertices.len(), 0);
    assert_eq!(build(&[vec![]]).vertices.len(), 0);
    assert!(build(&[vec![]]).segments.is_empty());
}

/// Roof triangles index within their segment, not into the whole buffer.
///
/// What keeps indices in `u16` however large a tile's geometry grows, and what a consumer
/// believes when it draws a segment with its own vertex offset.
#[test]
fn indices_are_relative_to_their_segment() {
    let bucket = build_features(&[&[square(100)], &[square(50)]]);
    assert_eq!(bucket.vertices.len(), 10);
    for index in &bucket.indices {
        assert!(
            usize::from(*index) < bucket.vertices.len(),
            "index {index} is outside the buffer"
        );
    }
    // Both polygons share one segment here, so the second's indices are offset by the first's
    // five vertices rather than starting again at zero.
    assert!(
        bucket.indices.iter().any(|index| *index >= 5),
        "the second feature's roof indexes its own vertices"
    );
}
