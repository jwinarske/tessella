//! MVT version 1 polygons, whose ring winding means nothing.
//!
//! # What went wrong without this
//!
//! Version 1 of the vector tile spec did not define ring winding order, so a v1 polygon's rings
//! cannot be sorted into exteriors and holes by their signed area -- which is the only thing
//! `classify_rings` knows how to do. mbgl repairs them first, with an even-odd union gated on
//! `getVersion() < 2`; this build did not.
//!
//! On a real z0 world tile that put a four-point island at the head of a feature with 118
//! continent-sized rings behind it. All 118 became "holes" of the island, none of them lie inside
//! it, and earcut answers one triangle for that -- correctly. The ocean then painted over both
//! Americas: ten percent of the frame against `mbgl-render`, and a map with a continent missing.
//!
//! The check here is the shape of the failure rather than the pixels, so it needs no oracle: a
//! polygon that will not triangulate produces triangles far out of proportion to the points it
//! was built from.

use tessella_layout::fill;
use tessella_source::mvt;

/// The fixture, which is version 1 and the only one this repository has.
const WORLD: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn water() -> (u32, Vec<Vec<fill::Ring>>) {
    let tile = mvt::Tile::decode(WORLD).expect("the fixture decodes");
    let layer = tile.layer("water").expect("a water layer");
    let features = layer
        .features()
        .map(|feature| {
            feature
                .rings()
                .map(|ring| {
                    ring.iter()
                        .map(|point| [point[0] as i16, point[1] as i16])
                        .collect()
                })
                .collect()
        })
        .collect();
    (layer.version, features)
}

#[test]
fn the_fixture_is_the_version_this_is_about() {
    let (version, _) = water();
    assert_eq!(version, 1, "this test has nothing to say about a v2 tile");
}

#[test]
fn repaired_rings_triangulate_in_proportion_to_their_points() {
    let (_, features) = water();

    let repaired: Vec<Vec<fill::Ring>> = features
        .iter()
        .map(|rings| fill::fixup_polygons(rings))
        .collect();
    let borrowed: Vec<&[fill::Ring]> = repaired.iter().map(Vec::as_slice).collect();
    let bucket = fill::build_features(&borrowed);

    let vertices = bucket.vertices.len();
    let triangles = bucket.indices.len() / 3;
    // A polygon of n points triangulates to about n triangles once its holes are bridged. Half
    // of that is a generous floor and still far above what the broken classification managed:
    // 4,153 vertices produced 1,628 triangles, because the two largest polygons produced one
    // each.
    assert!(
        triangles * 2 > vertices,
        "{vertices} vertices produced only {triangles} triangles; the polygons are not being \
         triangulated"
    );
}

#[test]
fn the_repair_is_what_makes_the_difference() {
    let (_, features) = water();

    let count = |features: &[Vec<fill::Ring>]| {
        let borrowed: Vec<&[fill::Ring]> = features.iter().map(Vec::as_slice).collect();
        let bucket = fill::build_features(&borrowed);
        bucket.indices.len() / 3
    };

    let repaired: Vec<Vec<fill::Ring>> = features
        .iter()
        .map(|rings| fill::fixup_polygons(rings))
        .collect();

    // Asserted as a comparison rather than a number, so the test says what the repair is for
    // instead of pinning a triangle count that any change to the tessellator would move.
    assert!(
        count(&repaired) > count(&features) * 2,
        "the repair changed almost nothing: {} triangles against {}",
        count(&repaired),
        count(&features)
    );
}
