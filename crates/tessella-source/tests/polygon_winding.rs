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
