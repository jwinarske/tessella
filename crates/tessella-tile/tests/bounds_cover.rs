//! Covering a geographic box, which is how an offline region names its tiles.
//!
//! The counts here are what a download is sized by, so being wrong is not cosmetic: too few
//! means a region incomplete offline, too many means downloading tiles nobody asked for — at
//! zoom 14 over a country, a great many of them.

use tessella_tile::cover::{Bounds, CoverError, TileCoord};

/// The whole world is one tile at zoom zero and four at zoom one.
#[test]
fn the_world_covers_the_pyramid() {
    let world = Bounds::world();
    assert_eq!(world.tile_count(0), 1);
    assert_eq!(world.tile_count(1), 4);
    assert_eq!(world.tile_count(2), 16);

    let mut tiles = world.tiles(1, 100).expect("covers");
    tiles.sort_unstable();
    assert_eq!(
        tiles,
        [
            TileCoord {
                z: 1,
                x: 0,
                y: 0,
                wrap: 0
            },
            TileCoord {
                z: 1,
                x: 0,
                y: 1,
                wrap: 0
            },
            TileCoord {
                z: 1,
                x: 1,
                y: 0,
                wrap: 0
            },
            TileCoord {
                z: 1,
                x: 1,
                y: 1,
                wrap: 0
            },
        ]
    );
}

/// The count and the enumeration agree, at every zoom.
///
/// They are computed by different code — one closes a formula, the other walks a loop — and a
/// caller sizes a download with the first and performs it with the second.
#[test]
fn the_count_matches_what_is_enumerated() {
    let berlin = Bounds::new(13.0, 52.3, 13.8, 52.7);
    for z in 0..=12 {
        let counted = berlin.tile_count(z);
        let listed = berlin.tiles(z, 1_000_000).expect("covers").len() as u64;
        assert_eq!(counted, listed, "at zoom {z}");
    }
}

/// A box ending exactly on a tile boundary does not pull in the tile beyond it.
///
/// mbgl's asymmetry: the west edge floors and the east edge ceils-then-subtracts-one. At zoom
/// 14 over a city the difference is a whole column of tiles nobody asked to download.
#[test]
fn an_edge_aligned_box_does_not_overreach() {
    // At zoom 1 the meridian is the boundary between columns 0 and 1.
    let western = Bounds::new(-180.0, -85.0, 0.0, 85.0);
    assert_eq!(western.tile_count(1), 2, "one column, two rows");
    let tiles = western.tiles(1, 100).expect("covers");
    assert!(tiles.iter().all(|tile| tile.x == 0), "{tiles:?}");
}

/// A box crossing the antimeridian wraps rather than covering the world backwards.
///
/// Written `west > east`, which is not a mistake to reject but the only way two numbers say
/// "from 170°E to 170°W". Read naively the span is negative and the count nonsense.
#[test]
fn a_box_crossing_the_antimeridian_wraps() {
    let fiji = Bounds::new(170.0, -20.0, -170.0, -15.0);
    assert!(fiji.crosses_antimeridian());

    let counted = fiji.tile_count(4);
    let listed = fiji.tiles(4, 1_000).expect("covers");
    assert_eq!(counted, listed.len() as u64);

    // A narrow strip on both sides of the line, not most of the planet.
    assert!(counted < 20, "{counted} tiles");
    let columns: std::collections::BTreeSet<u32> = listed.iter().map(|tile| tile.x).collect();
    assert!(columns.contains(&15), "the eastern edge: {columns:?}");
    assert!(columns.contains(&0), "and the western one: {columns:?}");
    assert!(!columns.contains(&8), "and nothing between: {columns:?}");
}

/// Latitudes past the Mercator limit clamp rather than escaping the world.
#[test]
fn poles_clamp_into_the_world() {
    let polar = Bounds::new(-180.0, -90.0, 180.0, 90.0);
    for z in 0..=8 {
        let world = u64::from(1u32 << z);
        assert_eq!(polar.tile_count(z), world * world, "at zoom {z}");
        let tiles = polar.tiles(z, 100_000).expect("covers");
        assert!(
            tiles.iter().all(|tile| tile.y < 1 << z && tile.x < 1 << z),
            "at zoom {z}"
        );
    }
}

/// A region is sized before it is downloaded, so the count must not allocate it.
///
/// At zoom 16 over a country the answer is in the millions. A caller asking "how big is this"
/// is asking precisely so it can decline, and answering by building the list would make the
/// question as expensive as the answer.
#[test]
fn the_count_does_not_allocate() {
    let france = Bounds::new(-5.0, 41.0, 9.0, 51.0);
    let huge = france.tile_count(16);
    assert!(huge > 1_000_000, "{huge} tiles");

    // Enumerating it is refused at a limit the caller sets, rather than attempted.
    match france.tiles(16, 100_000) {
        Err(CoverError::TooLarge { tiles }) => assert_eq!(tiles, huge),
        other => panic!("{other:?}"),
    }
}

/// A degenerate box is one tile, not none.
///
/// A user dropping a pin and asking for "here" states a box of zero area, and the tile under it
/// is what they meant.
#[test]
fn a_point_sized_box_is_one_tile() {
    let pin = Bounds::new(13.405, 52.52, 13.405, 52.52);
    for z in 0..=14 {
        assert_eq!(pin.tile_count(z), 1, "at zoom {z}");
        assert_eq!(pin.tiles(z, 10).expect("covers").len(), 1);
    }
}
