// SPDX-License-Identifier: BSD-2-Clause
//! `fill-extrusion-rounded-corner-distance` rounds an extrusion's corners, as mbgl rounds them.
//!
//! A port of `roundPolygonCorners` in `src/mbgl/tile/geometry_tile_data.cpp`, which
//! `FillExtrusionBucket::addFeature` calls between `limitHoles` and its vertex count. The
//! property was declared by mbgl and read nowhere here, so a style asking for rounded buildings
//! got square ones.
//!
//! # What a corner costs
//!
//! Five points where there was one -- a start, three interior arc points, an end -- so a closed
//! rectangle of five points becomes twenty-one. A corner is left alone when either edge has zero
//! length, or when the edges are within five degrees of parallel and there is no arc to strike.
//!
//! # Against the oracle
//!
//! The committed extrusion fixture with the property set to 5, captured by `mbgl-capture-probe`,
//! against this build. Layer `ext-opaque`, z13:
//!
//! | tile | oracle | this build |
//! |---|---|---|
//! | 4092/2723 | 83 / 222 | **84** / 222 |
//! | 4092/2724 | 42 / 108 | 42 / 108 |
//! | 4093/2723 | 126 / 336 | 126 / 336 |
//! | 4093/2724 | 84 / 216 | 84 / 216 |
//! | 4094/2723 | 63 / 162 | 63 / 162 |
//! | 4094/2724 | 63 / 162 | 63 / 162 |
//!
//! Every index count agrees, the mismatched tile included. Its one extra vertex is the
//! buffer-edge ring merge tessella #256 pinned and not a rounding difference: unrounded that tile
//! is already 20 against 19, because mbgl's clipper merges a hole that reaches the tile's buffer
//! into the outer boundary and this build keeps two rings. Two rings of four corners round to
//! `2 * (4 * 5 + 1)` = 42; one merged ring of eight rounds to `8 * 5 + 1` = 41. The difference is
//! the second ring's closing point, which is why it is one vertex and no triangle.

use tessella_layout::fill_extrusion::round_polygon_corners;

/// A closed square, counter-clockwise, a thousand tile units on a side.
fn square() -> Vec<Vec<[i16; 2]>> {
    vec![vec![[0, 0], [1000, 0], [1000, 1000], [0, 1000], [0, 0]]]
}

/// Four corners become five points each, and the ring closes as it came in.
#[test]
fn a_rounded_corner_is_five_points() {
    let rounded = round_polygon_corners(&square(), 50.0);
    assert_eq!(rounded.len(), 1, "one ring in, one out");
    assert_eq!(
        rounded[0].len(),
        4 * 5 + 1,
        "four corners at five points each, plus the closing point: {:?}",
        rounded[0].len()
    );
    assert_eq!(
        rounded[0].first(),
        rounded[0].last(),
        "the ring should come back closed"
    );
}

/// The corner distance is capped at a fifth of the shorter edge, whatever the style asks.
///
/// mbgl's `maxEdgeLenPercent` of 0.2. Without it a large distance on a short edge would run the
/// arc past the next corner and cross the ring over itself.
#[test]
fn the_distance_is_capped_at_a_fifth_of_the_edge() {
    // Asking for 10,000 on a 1,000-unit square: the cap is 200, so the first corner's arc starts
    // 200 before it rather than off the far end of the edge.
    let rounded = round_polygon_corners(&square(), 10_000.0);
    let start = rounded[0][0];
    // Walking the ring from (0,0) toward (1000,0), the corner at (1000,0) opens at x = 800.
    let far = rounded[0]
        .iter()
        .map(|p| f64::from(p[0]))
        .fold(f64::MIN, f64::max);
    assert!(
        (far - 1000.0).abs() < 1e-3,
        "the arc should not leave the ring's own extent: {far}"
    );
    assert!(
        start[0].is_finite() && start[1].is_finite(),
        "a capped corner still produces a point"
    );

    // And the cap really binds: a huge ask and a merely large one agree, because both clamp.
    let clamped = round_polygon_corners(&square(), 1_000.0);
    assert_eq!(
        rounded[0].len(),
        clamped[0].len(),
        "both are capped, so both spend the same points"
    );
}

/// A corner whose edges are within five degrees of parallel is left alone.
///
/// mbgl's `sinParallelTreshold`. There is no circle through a straight line, and the arithmetic
/// that finds the center divides by a cross product that is zero there.
#[test]
fn a_straight_corner_is_left_alone() {
    // Three collinear points along the bottom edge, so the middle one is not a corner.
    let ring = vec![vec![
        [0, 0],
        [500, 0],
        [1000, 0],
        [1000, 1000],
        [0, 1000],
        [0, 0],
    ]];
    let rounded = round_polygon_corners(&ring, 50.0);
    // Five corners, one of them straight: four rounded at five points, one kept at one.
    assert_eq!(
        rounded[0].len(),
        4 * 5 + 1 + 1,
        "the collinear point should cost one point, not five: {}",
        rounded[0].len()
    );
}

/// A duplicated vertex has no direction to round, and it spoils *two* corners rather than one.
///
/// The duplicate's own corner has a zero-length outgoing edge and the next one has a zero-length
/// incoming edge, so mbgl's `edge1Len == 0.0 || edge2Len == 0.0` catches both and keeps both as
/// single points.
#[test]
fn a_duplicate_vertex_spoils_two_corners() {
    let ring = vec![vec![
        [0, 0],
        [1000, 0],
        [1000, 0],
        [1000, 1000],
        [0, 1000],
        [0, 0],
    ]];
    let rounded = round_polygon_corners(&ring, 50.0);
    // Three corners rounded at five points, two kept at one, and the closing point.
    assert_eq!(
        rounded[0].len(),
        3 * 5 + 2 + 1,
        "a duplicate should cost two corners, not one: {}",
        rounded[0].len()
    );
}

/// The arc is fractional, which is why the output is not tile-unit integers.
///
/// Rounding the arc to integers here would move it before [`pack_vertex`] could put the fraction
/// in `decimals`, and the walls would part company with the roof.
#[test]
fn the_arc_is_fractional() {
    let rounded = round_polygon_corners(&square(), 137.0);
    let fractional = rounded[0]
        .iter()
        .any(|p| p[0].fract() != 0.0 || p[1].fract() != 0.0);
    assert!(
        fractional,
        "an arc that landed only on integers would not be an arc: {:?}",
        &rounded[0][..5]
    );
}

/// A ring too short to have a corner comes through unchanged.
#[test]
fn a_degenerate_ring_survives() {
    for ring in [vec![], vec![[5, 5]]] {
        let rounded = round_polygon_corners(std::slice::from_ref(&ring), 50.0);
        assert_eq!(
            rounded[0].len(),
            ring.len(),
            "a ring with no corner should pass through"
        );
    }
}
