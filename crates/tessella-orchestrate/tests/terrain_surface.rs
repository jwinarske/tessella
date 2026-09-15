//! Building a tile for terrain: the grid its relief chose, honored by the tessellator.
//!
//! `Surface` is part of the tile key, so a tile built for one surface is a different cache entry
//! from the same tile built for another. Terrain is the first surface whose grid is not a function
//! of the tile's address: a sphere's follows from the zoom and agrees between two builds by
//! construction, while a terrain's follows from elevation that arrives separately and later. So
//! one tile is genuinely built twice -- flat while its DEM is in flight, split once it lands --
//! and the cell count travels in the key to keep those two apart.

use tessella_orchestrate::tile::{TileId, build_mvt_tile_on};
use tessella_orchestrate::{Content, LayerBucket};
use tessella_style::Style;
use tessella_tile::store::Surface;

const MVT: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r##"{
 "version": 8,
 "sources": {"src": {"type": "vector", "tiles": ["https://o/{z}/{x}/{y}.mvt"]}},
 "layers": [{"id": "water", "type": "fill", "source": "src", "source-layer": "water",
             "paint": {"fill-color": "#12243a"}}]
}"##;

fn build(surface: Surface) -> Vec<LayerBucket> {
    let style = Style::parse(STYLE).expect("style parses");
    let decoded = tessella_source::mvt::Tile::decode(MVT).expect("the fixture decodes");
    build_mvt_tile_on(&style, "src", TileId::new(0, 0, 0), &decoded, surface).expect("builds")
}

fn triangles(buckets: &[LayerBucket]) -> usize {
    buckets
        .iter()
        .filter_map(|bucket| match &bucket.content {
            Content::Fill(fill) => Some(fill.indices.len() / 3),
            _ => None,
        })
        .sum()
}

fn vertices(buckets: &[LayerBucket]) -> Vec<[i16; 2]> {
    buckets
        .iter()
        .filter_map(|bucket| match &bucket.content {
            Content::Fill(fill) => Some(fill.vertices.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// A terrain whose ground is flat builds the plane's tile, vertex for vertex.
///
/// One cell is the answer for flat ground and for a tile whose DEM has not arrived, which between
/// them is most tiles of most frames. If that cost anything over the flat path, every map with a
/// terrain in its style would pay it everywhere -- so it is not merely equal output, it is the
/// same code path: `grid_step_or_none` answers zero, which is the branch that copies the triangle
/// list through untouched.
#[test]
fn one_cell_is_the_flat_build() {
    let plane = build(Surface::Plane);
    let flat = build(Surface::Terrain { cells: 1 });
    assert!(!vertices(&plane).is_empty(), "the fixture has water");
    assert_eq!(vertices(&plane), vertices(&flat));
    assert_eq!(triangles(&plane), triangles(&flat));
    // And zero cells is the same answer rather than a division by nothing.
    assert_eq!(
        vertices(&build(Surface::Terrain { cells: 0 })),
        vertices(&plane)
    );
}

/// A finer grid cuts more triangles, monotonically.
#[test]
fn a_finer_grid_cuts_more() {
    let mut previous = triangles(&build(Surface::Terrain { cells: 1 }));
    for cells in [2, 4, 8, 16, 32] {
        let split = triangles(&build(Surface::Terrain { cells }));
        assert!(
            split > previous,
            "{cells} cells cut {split} against {previous}"
        );
        previous = split;
    }
}

/// Every vertex of a split tile stays inside the tile's own buffered range.
///
/// Subdivision clips against a grid; it must not invent geometry outside what the tessellator
/// produced. A fill is buffered past its own edge and those coordinates are real, so the bound is
/// the buffer rather than the extent.
#[test]
fn splitting_does_not_move_geometry() {
    let flat = build(Surface::Terrain { cells: 1 });
    let split = build(Surface::Terrain { cells: 16 });

    let bounds = |verts: &[[i16; 2]]| {
        verts.iter().fold(
            [i16::MAX, i16::MIN, i16::MAX, i16::MIN],
            |[lx, hx, ly, hy], point| {
                [
                    lx.min(point[0]),
                    hx.max(point[0]),
                    ly.min(point[1]),
                    hy.max(point[1]),
                ]
            },
        )
    };
    let [flat_lx, flat_hx, flat_ly, flat_hy] = bounds(&vertices(&flat));
    let [lx, hx, ly, hy] = bounds(&vertices(&split));
    assert!(
        lx >= flat_lx && hx <= flat_hx,
        "{lx}..{hx} vs {flat_lx}..{flat_hx}"
    );
    assert!(
        ly >= flat_ly && hy <= flat_hy,
        "{ly}..{hy} vs {flat_ly}..{flat_hy}"
    );
}

/// The cell count is part of the tile's identity, so two grids are two cache entries.
///
/// Without it, a tile built flat while its DEM was in flight would be found in the cache and
/// reused after the DEM landed -- a terrain that never rose, on a map that had every elevation it
/// needed.
#[test]
fn the_cell_count_is_part_of_the_key() {
    use tessella_tile::store::TileKey;
    let flat = TileKey::new("src", 14, 8802, 5373, 1).on(Surface::Terrain { cells: 1 });
    let split = TileKey::new("src", 14, 8802, 5373, 1).on(Surface::Terrain { cells: 32 });
    assert_ne!(flat, split);
    // And neither is the plane's, though the flat one builds the same geometry: a map that turns
    // its terrain off should not be served tiles built for one.
    assert_ne!(flat, TileKey::new("src", 14, 8802, 5373, 1));
}
