//! GeoJSON polygon rings arrive wound the way a vector tile winds them.
//!
//! A ring's *order* is geometry to an extrusion in a way it never is to a fill: the wall builder
//! walks the outline and takes each edge's perpendicular as that wall's outward facing, and the
//! quad it winds from the edge is what back-face culling reads. Reverse the ring and both
//! invert, which draws a building as a band across its top with a diagonal sliver down each
//! side.
//!
//! RFC 7946 §3.1.6 asks for counter-clockwise exteriors — the opposite of what a tile carries —
//! so the spec-conformant way to write a building was the way that drew it broken. mbgl
//! normalizes before any bucket sees the ring; so does this.

use tessella_source::{Geometry, geojson};
use tessella_style::Value;

/// Twice a ring's signed area, in longitude and latitude. Only the sign is read.
fn shoelace(ring: &[[f64; 2]]) -> f64 {
    (0..ring.len())
        .map(|index| {
            let here = ring[index];
            let next = ring[(index + 1) % ring.len()];
            here[0] * next[1] - next[0] * here[1]
        })
        .sum()
}

fn polygon(coordinates: &str) -> Vec<Vec<Vec<[f64; 2]>>> {
    let document = format!(
        r#"{{"type": "Feature", "properties": {{}},
             "geometry": {{"type": "Polygon", "coordinates": {coordinates}}}}}"#
    );
    let value: Value = serde_json::from_str(&document).expect("json");
    let features = geojson::read(&value).expect("features read");
    let Geometry::Polygon(polygons) = &features[0].geometry else {
        panic!("a polygon");
    };
    polygons.clone()
}

const COUNTER_CLOCKWISE: &str = "[[[-1,-1],[1,-1],[1,1],[-1,1],[-1,-1]]]";
const CLOCKWISE: &str = "[[[-1,1],[1,1],[1,-1],[-1,-1],[-1,1]]]";

/// Either winding arrives the same way round.
///
/// The counter-clockwise one is RFC 7946's, and the one that drew a building broken: a white
/// cube written that way read 19742 gross pixels against the oracle, and 0 written the other.
#[test]
fn either_winding_arrives_wound_like_a_tile() {
    let from_ccw = polygon(COUNTER_CLOCKWISE);
    let from_cw = polygon(CLOCKWISE);

    // The direction, not the starting corner: normalizing turns a ring round, it does not roll
    // it, so the two agree on which way they go and still begin where they were written.
    for (name, rings) in [("counter-clockwise", &from_ccw), ("clockwise", &from_cw)] {
        let area = shoelace(&rings[0][0]);
        assert!(
            area < 0.0,
            "{name} arrived at {area}, which is not the winding a tile carries -- clockwise in \
             longitude and latitude, since Mercator flips the vertical axis"
        );
    }
}

/// A ring keeps its closing position, wherever it came from.
///
/// Reversing `[a, b, c, d, a]` gives `[a, d, c, b, a]`, so the first and last stay equal — and a
/// ring that lost that property would no longer be closed.
#[test]
fn a_reversed_ring_is_still_closed() {
    for source in [COUNTER_CLOCKWISE, CLOCKWISE] {
        let ring = polygon(source)[0][0].clone();
        assert_eq!(ring.len(), 5, "GeoJSON rings repeat their first position");
        assert_eq!(ring[0], ring[4], "and that is what closes them");
    }
}

/// A hole turns with its exterior, so it stays a hole.
///
/// `classify_rings` tells an interior ring from an exterior one by the sign it does *not* share.
/// Flipping only the exterior would leave the two indistinguishable, and the hole would be
/// filled in.
#[test]
fn a_hole_turns_with_its_exterior() {
    // Exterior counter-clockwise, hole clockwise: RFC 7946's own arrangement.
    let rings =
        polygon("[[[-2,-2],[2,-2],[2,2],[-2,2],[-2,-2]], [[-1,1],[1,1],[1,-1],[-1,-1],[-1,1]]]");
    let exterior = shoelace(&rings[0][0]);
    let hole = shoelace(&rings[0][1]);

    assert!(exterior < 0.0, "the exterior turned, {exterior}");
    assert!(
        hole > 0.0,
        "and the hole turned with it, so the signs still disagree, {hole}"
    );
}

/// A multipolygon's parts are each normalized on their own.
#[test]
fn every_part_of_a_multipolygon_is_wound() {
    let document = r#"{"type": "Feature", "properties": {},
        "geometry": {"type": "MultiPolygon", "coordinates": [
            [[[-1,-1],[1,-1],[1,1],[-1,1],[-1,-1]]],
            [[[5,1],[7,1],[7,-1],[5,-1],[5,1]]]]}}"#;
    let value: Value = serde_json::from_str(document).expect("json");
    let features = geojson::read(&value).expect("features read");
    let Geometry::Polygon(polygons) = &features[0].geometry else {
        panic!("a polygon");
    };

    assert_eq!(polygons.len(), 2, "two parts");
    for (index, part) in polygons.iter().enumerate() {
        assert!(
            shoelace(&part[0]) < 0.0,
            "part {index} is wound like a tile, {}",
            shoelace(&part[0])
        );
    }
}

// --- the repair pass, tessella#255 ---
//
// mbgl runs every GeoJSON polygon through `fixupPolygons` before anything classifies it: a wagyu
// union with `fill_type_even_odd`, which decides hole-ness by *position* rather than by winding.
// So mbgl is winding-insensitive here, and a hand-written hole wound the same way as its exterior
// is still a hole. Turning every ring together preserves relative winding, which is right for
// well-formed input and left a same-wound hole reading as a second exterior.
//
// Measured over the fill of one annulus at z13, 512x512, gross pixels against `mbgl-render`:
//
// | case | before | after |
// |---|---|---|
// | well-formed hole | 0 | 0 |
// | hole wound like its exterior | 21,808 (8.3%) | **0** |
// | the same, both clockwise | 21,808 (8.3%) | **0** |
// | island inside a hole | 41,700 (15.9%) | **0** |
// | two disjoint rings | 0 | 0 |
// | two overlapping rings | 13,950 (5.3%) | 13,950 -- needs the clipper |
// | one self-crossing ring | 28,186 (10.8%) | 28,186 -- needs the clipper |

/// A ring wound the same way as its exterior is still a hole, and arrives wound like one.
#[test]
fn a_same_wound_hole_is_repaired() {
    let rings = polygon(
        "[[[-4,-4],[4,-4],[4,4],[-4,4],[-4,-4]],
          [[-2,-2],[2,-2],[2,2],[-2,2],[-2,-2]]]",
    );
    let exterior = shoelace(&rings[0][0]);
    let hole = shoelace(&rings[0][1]);
    assert!(
        exterior * hole < 0.0,
        "the hole should oppose its exterior: {exterior} and {hole}"
    );
}

/// An island inside a hole is at depth two, and even-odd makes it an exterior again.
///
/// Three rings all written the same way round. The oracle draws the annulus and the island --
/// fifteen vertices and thirty indices -- where three separate exteriors would be eighteen.
#[test]
fn an_island_inside_a_hole_turns_like_the_exterior() {
    let rings = polygon(
        "[[[-6,-6],[6,-6],[6,6],[-6,6],[-6,-6]],
          [[-4,-4],[4,-4],[4,4],[-4,4],[-4,-4]],
          [[-2,-2],[2,-2],[2,2],[-2,2],[-2,-2]]]",
    );
    let [outer, hole, island] = [
        shoelace(&rings[0][0]),
        shoelace(&rings[0][1]),
        shoelace(&rings[0][2]),
    ];
    assert!(outer * hole < 0.0, "depth one opposes: {outer} and {hole}");
    assert!(
        outer * island > 0.0,
        "depth two turns like the exterior again: {outer} and {island}"
    );
}

/// Two rings side by side are two exteriors, and neither is turned to look like a hole.
#[test]
fn disjoint_rings_stay_exteriors() {
    let rings = polygon(
        "[[[-4,-4],[-2,-4],[-2,-2],[-4,-2],[-4,-4]],
          [[2,2],[4,2],[4,4],[2,4],[2,2]]]",
    );
    let (first, second) = (shoelace(&rings[0][0]), shoelace(&rings[0][1]));
    assert!(
        first * second > 0.0,
        "neither contains the other, so both are exteriors: {first} and {second}"
    );
}

/// Two rings that merely overlap are not a nesting, and must be left alone.
///
/// This is the guard on the containment test. Reading "inside" from one vertex made the second
/// ring a hole and punched the overlap out of the picture -- twelve indices to six -- where two
/// overlapping opaque exteriors at least keep the ink. mbgl unions them into twenty-four and
/// neither answer here matches that; what this pins is that the cheaper one is not made worse.
#[test]
fn overlapping_rings_are_not_a_nesting() {
    let rings = polygon(
        "[[[-3,-3],[1,-3],[1,1],[-3,1],[-3,-3]],
          [[-1,-1],[3,-1],[3,3],[-1,3],[-1,-1]]]",
    );
    let (first, second) = (shoelace(&rings[0][0]), shoelace(&rings[0][1]));
    assert!(
        first * second > 0.0,
        "an overlap is not a containment: {first} and {second}"
    );
}

/// A well-formed polygon comes through exactly as it did before the repair existed.
///
/// The repair is meant to be invisible to conformant data: the exterior is at depth zero and the
/// hole at depth one, which is what their directions already said.
#[test]
fn a_well_formed_polygon_is_untouched() {
    let rings = polygon(
        "[[[-4,-4],[4,-4],[4,4],[-4,4],[-4,-4]],
          [[-2,-2],[-2,2],[2,2],[2,-2],[-2,-2]]]",
    );
    let exterior = shoelace(&rings[0][0]);
    let hole = shoelace(&rings[0][1]);
    assert!(
        exterior < 0.0,
        "the exterior still arrives wound like a tile: {exterior}"
    );
    assert!(
        exterior * hole < 0.0,
        "and the hole still opposes it: {exterior} and {hole}"
    );
}

/// A single ring has nothing to nest in and is not walked.
#[test]
fn one_ring_skips_the_repair() {
    let rings = polygon(COUNTER_CLOCKWISE);
    assert_eq!(rings[0].len(), 1);
    assert!(shoelace(&rings[0][0]) < 0.0, "still wound like a tile");
}
