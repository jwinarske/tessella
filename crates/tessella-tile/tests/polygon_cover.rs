//! Tile cover over an arbitrary shape.

use std::collections::BTreeSet;

use tessella_tile::cover::{Bounds, TileCoord};
use tessella_tile::polygon::{Cover, Polygon};

/// The tiles a polygon covers at one zoom, deduplicated and ordered.
fn cover(polygon: &Polygon, z: u8) -> BTreeSet<(u32, u32)> {
    Cover::polygon(polygon, z)
        .map(|tile| (tile.x, tile.y))
        .collect()
}

/// A rectangle as a ring, wound clockwise in screen terms (south-west first, going north).
fn rectangle(west: f64, south: f64, east: f64, north: f64) -> Polygon {
    Polygon::new(vec![
        [west, south],
        [west, north],
        [east, north],
        [east, south],
        [west, south],
    ])
}

fn box_tiles(bounds: Bounds, z: u8) -> BTreeSet<(u32, u32)> {
    bounds
        .tiles(z, 1_000_000)
        .expect("enumerates")
        .into_iter()
        .map(|tile: TileCoord| (tile.x, tile.y))
        .collect()
}

/// A polygon that is a rectangle covers exactly what the rectangle does.
///
/// The strongest check available: `Bounds` is already verified against the oracle, so agreeing
/// with it over many boxes and zooms exercises projection, row indexing, span merging and the
/// non-zero fill against something known good — rather than against my own arithmetic.
#[test]
fn a_rectangular_polygon_agrees_with_the_box() {
    let cases = [
        (13.0, 52.3, 13.8, 52.7),
        (-0.5, 51.2, 0.3, 51.7),
        (-122.6, 37.6, -122.2, 37.9),
        (-5.0, 41.0, 9.0, 51.0),
        (100.0, -8.0, 141.0, 6.0),
        (-74.1, 40.5, -73.7, 40.9),
    ];
    for (west, south, east, north) in cases {
        for z in 0..=10u8 {
            let expected = box_tiles(Bounds::new(west, south, east, north), z);
            let actual = cover(&rectangle(west, south, east, north), z);
            assert_eq!(
                actual, expected,
                "box ({west}, {south}, {east}, {north}) at z{z}"
            );
        }
    }
}

/// Winding order does not change what a shape covers.
///
/// A user's drawn ring arrives in whichever direction they dragged. The non-zero rule reads
/// direction only to tell a hole from the ring around it, so an outline alone must cover the
/// same tiles either way round.
#[test]
fn winding_order_does_not_change_the_cover() {
    let forward = rectangle(13.0, 52.3, 13.8, 52.7);
    let mut reversed = forward.clone();
    reversed.exterior.reverse();

    for z in 0..=10u8 {
        assert_eq!(cover(&forward, z), cover(&reversed, z), "z{z}");
    }
}

/// An open ring and a closed one describe the same shape.
#[test]
fn a_ring_may_arrive_open_or_closed() {
    let closed = rectangle(13.0, 52.3, 13.8, 52.7);
    let mut open = closed.clone();
    open.exterior.pop();

    for z in 0..=10u8 {
        assert_eq!(cover(&closed, z), cover(&open, z), "z{z}");
    }
}

/// A diagonal shape covers fewer tiles than its bounding box.
///
/// The whole reason polygons exist here. A triangle over half a box should not download the
/// other half, and at street zoom that half is the difference between a download a user accepts
/// and one they cancel.
#[test]
fn a_triangle_costs_less_than_its_box() {
    let triangle = Polygon::new(vec![[13.0, 52.3], [13.8, 52.3], [13.0, 52.7], [13.0, 52.3]]);
    let z = 11;
    let inside = cover(&triangle, z);
    let around = box_tiles(Bounds::new(13.0, 52.3, 13.8, 52.7), z);

    assert!(!inside.is_empty());
    assert!(
        inside.is_subset(&around),
        "the triangle stays inside its own box"
    );
    assert!(
        inside.len() * 3 < around.len() * 2,
        "and covers markedly less: {} of {}",
        inside.len(),
        around.len()
    );
}

/// A shape's interior is covered, not just its outline.
///
/// The non-zero fill. Without it a polygon downloads a hollow ring of tiles and the middle of
/// the city the user selected is missing.
#[test]
fn the_interior_is_filled() {
    let z = 9;
    let square = rectangle(0.0, 0.0, 10.0, 10.0);
    let tiles = cover(&square, z);

    // Every row the shape touches is contiguous in x — no gaps where the interior should be.
    let mut rows: std::collections::BTreeMap<u32, Vec<u32>> = std::collections::BTreeMap::new();
    for (x, y) in &tiles {
        rows.entry(*y).or_default().push(*x);
    }
    assert!(rows.len() > 2, "several rows, so there is an interior");
    for (row, mut xs) in rows {
        xs.sort_unstable();
        let span = (xs[xs.len() - 1] - xs[0] + 1) as usize;
        assert_eq!(xs.len(), span, "row {row} has a hole: {xs:?}");
    }
}

/// A hole is not downloaded.
///
/// A region drawn around a city with a lake in it should not fetch the lake, and at street zoom
/// that is a visible fraction of the total.
#[test]
fn a_hole_is_subtracted() {
    let z = 10;
    let solid = rectangle(0.0, 0.0, 10.0, 10.0);
    let holed = rectangle(0.0, 0.0, 10.0, 10.0).with_hole(vec![
        [3.0, 3.0],
        [7.0, 3.0],
        [7.0, 7.0],
        [3.0, 7.0],
        [3.0, 3.0],
    ]);

    let full = cover(&solid, z);
    let punched = cover(&holed, z);
    assert!(punched.len() < full.len(), "the hole removed tiles");
    assert!(punched.is_subset(&full), "and added none");

    // The very middle of the hole is gone.
    let middle = tessella_tile::projection::tile_units(5.0, 5.0, z);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let centre = (middle[0] as u32, middle[1] as u32);
    assert!(full.contains(&centre), "solid covers the middle");
    assert!(!punched.contains(&centre), "holed does not");
}

/// A shape in two pieces covers both, and nothing between them.
///
/// Two disjoint parts, not a ring with a hole — a hole must lie inside the ring that contains
/// it, so a second lobe is a second *part*. Getting the ocean between two selected cities is
/// the failure worth guarding: it is invisible in the count and enormous in the download.
#[test]
fn separate_parts_are_covered_separately() {
    let z = 8;
    let west = rectangle(-10.0, 40.0, -8.0, 42.0);
    let east = rectangle(10.0, 40.0, 12.0, 42.0);

    let covered: BTreeSet<(u32, u32)> = Cover::shape(&[west.clone(), east.clone()], z)
        .map(|tile| (tile.x, tile.y))
        .collect();

    assert!(covered.is_superset(&cover(&west, z)), "the western part");
    assert!(covered.is_superset(&cover(&east, z)), "the eastern part");

    let middle = tessella_tile::projection::tile_units(0.0, 41.0, z);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let between = (middle[0] as u32, middle[1] as u32);
    assert!(!covered.contains(&between), "and nothing in between");
}

/// A degenerate shape terminates, and covers nothing much.
///
/// User input, so all of these arrive: a ring of one point from a stray tap, a ring of
/// coincident points from a drag that never moved, an empty selection, a drag along a straight
/// line. What matters is that none of them hangs, panics, or covers the world — a zero-area
/// ring covering the tiles its own edge passes through is a thin strip and perfectly defensible.
#[test]
fn degenerate_shapes_terminate() {
    for (ring, most) in [
        (vec![], 0),
        (vec![[13.0, 52.0]], 0),
        (vec![[13.0, 52.0], [13.0, 52.0]], 1),
        (
            vec![[13.0, 52.0], [13.0, 52.0], [13.0, 52.0], [13.0, 52.0]],
            1,
        ),
        // Collinear along a parallel. Every edge is horizontal, so nothing a scanline can
        // cross, so nothing covered — which is right for a ring with no area.
        (
            vec![[13.0, 52.0], [14.0, 52.0], [15.0, 52.0], [13.0, 52.0]],
            0,
        ),
        // Collinear along a meridian. Zero area too, but the edges do cross rows, so it covers
        // the strip of tiles the line passes through.
        (
            vec![[13.0, 52.0], [13.0, 53.0], [13.0, 54.0], [13.0, 52.0]],
            20,
        ),
    ] {
        let tiles = cover(&Polygon::new(ring.clone()), 10);
        assert!(
            tiles.len() <= most,
            "{ring:?} produced {} tiles, expected at most {most}",
            tiles.len()
        );
    }
}

/// Every tile a cover names is inside the world.
#[test]
fn covers_stay_inside_the_world() {
    let polygon = rectangle(-179.0, -84.0, 179.0, 84.0);
    for z in 0..=5u8 {
        let limit = 1u32 << z;
        for tile in Cover::polygon(&polygon, z) {
            assert!(tile.x < limit && tile.y < limit, "{tile:?} at z{z}");
            assert_eq!(tile.wrap, 0);
        }
    }
}

/// A point-sized polygon covers the one tile it is in.
#[test]
fn a_tiny_shape_covers_one_tile() {
    let z = 14;
    let tiles = cover(&rectangle(13.4049, 52.5200, 13.4050, 52.5201), z);
    assert_eq!(tiles.len(), 1, "{tiles:?}");
    let expected = tessella_tile::projection::tile_units(13.40495, 52.52005, z);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let expected = (expected[0] as u32, expected[1] as u32);
    assert!(tiles.contains(&expected));
}
