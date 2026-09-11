//! Every fill polygon in the fixture tiles triangulates the way it did.
//!
//! `vendor/earcutr` is patched, and the triangulation it produces is part of what the oracle
//! compares: `earcut_matches_earcut_hpp` and `earcut_agreement` pin the cases that were measured
//! against `earcut.hpp`, and this pins everything else the fixtures carry. It hashes the output of
//! `earcutr::earcut` over each polygon feature of every vector tile under `tests/`, grouped into
//! polygons the way the fill layer groups them, so a change to the vendored crate that alters
//! any triangle of any of them -- the order of the triangles, or which vertex a triangle starts
//! from, included -- fails here rather than as a pixel count somewhere downstream.
//!
//! The numbers are `earcut.hpp`'s. mbgl's vendored copy, run over the same polygons and hashed
//! the same way, gives exactly these -- every one of the fixtures' fill polygons triangulated as
//! mbgl triangulates it, index for index. A change that is meant to alter the triangulation
//! updates them, and says in its commit what `earcut.hpp` makes of the new ones. A change that is
//! not meant to -- a faster search for the same ears -- must leave them exactly as they are.

use std::path::Path;

use tessella_layout::fill::{Ring, classify_rings};
use tessella_source::mvt::{GeomType, Tile};

/// What the fixtures triangulate to: polygons, triangles, and FNV-1a 64 over the indices.
///
/// Taken from mbgl's vendored `earcut.hpp`, and matched by this crate since it took `earcut.hpp`'s
/// hole bridging and fallback passes. Before that, 18 of these polygons triangulated differently,
/// and the hash was `0x342f_2a43_07e2_aece`.
const POLYGONS: usize = 5030;
const TRIANGLES: usize = 150_661;
const HASH: u64 = 0xa329_e819_8ac2_57cf;

/// The vector tiles the fill layer is exercised on elsewhere in the suite.
const TILES: &[&str] = &[
    "tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt",
    "tests/mvt-fixtures/real-world-0-0-0.mvt",
    "tests/mvt-fixtures/streets-10-163-395.mvt",
    "tests/live-fixtures/world_z7-5-14-9.mvt",
    "tests/live-fixtures/world_z7-5-14-10.mvt",
    "tests/live-fixtures/world_z7-5-14-11.mvt",
    "tests/live-fixtures/world_z7-5-15-9.mvt",
    "tests/live-fixtures/world_z7-5-15-10.mvt",
    "tests/live-fixtures/world_z7-5-15-11.mvt",
    "tests/live-fixtures/world_z7-5-16-9.mvt",
    "tests/live-fixtures/world_z7-5-16-10.mvt",
    "tests/live-fixtures/world_z7-5-16-11.mvt",
];

/// FNV-1a, 64-bit.
struct Fnv(u64);

impl Fnv {
    fn eat(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[test]
fn the_fixture_polygons_triangulate_as_they_did() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut hash = Fnv(0xcbf2_9ce4_8422_2325);
    let (mut polygons, mut triangles) = (0, 0);

    for tile in TILES {
        let bytes = std::fs::read(root.join(tile)).expect("the fixture is there");
        let tile = Tile::decode(&bytes).expect("the fixture decodes");
        for layer in &tile.layers {
            for feature in layer.features() {
                if feature.geom_type() != GeomType::Polygon {
                    continue;
                }
                // Tile coordinates, as the fill layer holds them; a ring past what an `i16`
                // carries is not one the fill layer is handed either.
                let Some(rings): Option<Vec<Ring>> = feature
                    .rings()
                    .map(|ring| {
                        ring.iter()
                            .map(|p| Some([i16::try_from(p[0]).ok()?, i16::try_from(p[1]).ok()?]))
                            .collect()
                    })
                    .collect()
                else {
                    continue;
                };
                for polygon in classify_rings(&rings) {
                    // Flattened as `fill::build` flattens it: the outer ring, then each hole.
                    let mut flat: Vec<f64> = Vec::new();
                    let mut holes: Vec<usize> = Vec::new();
                    for (index, ring) in polygon.iter().enumerate() {
                        if index > 0 {
                            holes.push(flat.len() / 2);
                        }
                        for point in ring {
                            flat.extend([f64::from(point[0]), f64::from(point[1])]);
                        }
                    }
                    let indices = earcutr::earcut(&flat, &holes, 2).unwrap_or_default();
                    polygons += 1;
                    triangles += indices.len() / 3;
                    // Each polygon's indices, then its count, so two lists cannot run together
                    // into the same stream.
                    for index in &indices {
                        hash.eat(*index as u64);
                    }
                    hash.eat(indices.len() as u64);
                }
            }
        }
    }

    assert_eq!(
        (polygons, triangles, hash.0),
        (POLYGONS, TRIANGLES, HASH),
        "the fixtures triangulate differently: {polygons} polygons, {triangles} triangles, \
         hash {:#018x}",
        hash.0
    );
}
