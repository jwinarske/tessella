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

/// A map whose style has a usable terrain keys its tiles on the terrain surface.
///
/// Terrain is not a third projection: a caller picks a globe or a plane, and the ground being
/// raised follows from the style. So the surface reads both, and a globe wins -- this build has no
/// terrain on one.
#[test]
fn the_surface_follows_the_style() {
    use tessella_capture_abi::ProjectionMode;
    use tessella_capture_abi::envelope::ViewId;
    use tessella_orchestrate::map::Map;
    use tessella_tile::cover::ViewTransform;

    let view = ViewTransform {
        longitude: 13.405,
        latitude: 52.52,
        zoom: 14.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    };
    let with_terrain = r##"{"version":8,
      "sources":{"dem":{"type":"raster-dem","url":"http://x/d.json"}},
      "terrain":{"source":"dem"},
      "layers":[{"id":"bg","type":"background","paint":{"background-color":"#101014"}}]}"##;
    let flat = r##"{"version":8,"sources":{},
      "layers":[{"id":"bg","type":"background","paint":{"background-color":"#101014"}}]}"##;

    let mut map = Map::new(
        Style::parse(with_terrain).expect("style parses"),
        view,
        ViewId(0),
    );
    // One cell until a ground lands, and that is not a placeholder: a terrain with no DEM in
    // hand draws no ground, so there is no surface to follow and nothing to split for. The count
    // steps up when the first ground arrives and its relief asks for more, which re-keys the
    // cover and rebuilds it -- `Surface` is part of `TileKey` precisely so that is safe.
    //
    // Split at the mesh's ceiling regardless, every tile of a terrain style carries 16,384 cells
    // and a fill covering one becomes tens of thousands of triangles: measured on the parity
    // harness, a z14 cover went from settling in 224 ticks to 1,200 and stopped settling to the
    // same picture twice.
    assert_eq!(
        map.surface(),
        Surface::Terrain { cells: 1 },
        "a style with a usable terrain, before any ground has landed"
    );

    // The ground is drawn as the terrain mesh, so anything drawn on it takes the same grid --
    // finer chords against the surface's own chords, coarser floats above it.
    let mut plain = Map::new(Style::parse(flat).expect("style parses"), view, ViewId(0));
    assert_eq!(plain.surface(), Surface::Plane, "no terrain");

    // A globe wins: there is no terrain on one here.
    map.project_on(ProjectionMode::Globe);
    assert_eq!(map.surface(), Surface::Sphere);
    plain.project_on(ProjectionMode::Globe);
    assert_eq!(plain.surface(), Surface::Sphere);
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
