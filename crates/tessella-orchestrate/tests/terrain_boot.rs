//! A terrain's source is fetched, though no layer draws from it.
//!
//! The one thing about plumbing a terrain in that is not obvious. Every other source a style has
//! is named by a layer, and the resolve plan collects sources by walking the layers; a terrain is
//! named at the top level of the document and is drawn from by nothing. So a style whose only use
//! of a DEM is its terrain fetched no tiles at all -- and drew a perfectly good flat map, with
//! every other part of the build working and nothing anywhere to say why the ground was flat.

#![cfg(feature = "image")]

use std::sync::Arc;

use tessella_orchestrate::boot::{Boot, BootError, ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::ViewTransform;

const TILE_PNG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.png");
const MVT: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// Serves a DEM and a vector tile, and counts what was asked for.
struct Origin {
    asked: Arc<std::sync::Mutex<Vec<String>>>,
}

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.asked
            .lock()
            .expect("the counter is not poisoned")
            .push(url.to_string());
        let body = if url.contains("/dem/") {
            TILE_PNG.to_vec()
        } else if url.contains("/vector/") {
            MVT.to_vec()
        } else {
            return Ok(Response {
                status: 404,
                ..Response::default()
            });
        };
        Ok(Response {
            status: 200,
            body,
            ..Response::default()
        })
    }
}

fn view() -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    }
}

fn boot(style: &str) -> (Result<Boot, BootError>, Vec<String>) {
    let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pool = Pool::new(Workers::serial());
    let booted = tessella_orchestrate::boot::cold_start(&ColdStart {
        style,
        view: &view(),
        files: Arc::new(Coalescing::new(Origin {
            asked: Arc::clone(&asked),
        })),
        cache: Arc::new(TileCache::new(64)),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    });
    let urls = asked.lock().expect("the counter is not poisoned").clone();
    (booted, urls)
}

/// A style whose DEM is used by the terrain and by no layer.
fn style(terrain: &str) -> String {
    format!(
        r#"{{"version": 8,
            "sources": {{
              "dem": {{"type": "raster-dem", "tiles": ["https://o/dem/{{z}}/{{x}}/{{y}}.png"],
                       "tileSize": 256}},
              "base": {{"type": "vector", "tiles": ["https://o/vector/{{z}}/{{x}}/{{y}}.mvt"]}}
            }},
            {terrain}
            "layers": [{{"id": "roads", "type": "line", "source": "base",
                         "source-layer": "road"}}]}}"#
    )
}

/// The terrain's DEM is fetched even though no layer names it.
#[test]
fn a_terrain_source_is_fetched() {
    let (booted, urls) = boot(&style(
        r#""terrain": {"source": "dem", "exaggeration": 1.4},"#,
    ));
    booted.expect("boots");
    assert!(
        urls.iter().any(|url| url.contains("/dem/")),
        "no DEM was asked for: {urls:?}"
    );
    // And the vector source is still there, so this did not replace the walk it added to.
    assert!(urls.iter().any(|url| url.contains("/vector/")), "{urls:?}");
}

/// The DEM's tiles carry a terrain bucket, which is the ground itself.
///
/// `synthesize_terrain` makes a layer over the DEM source and `build_terrain_tile_on` gives it a
/// bucket per tile, so a booted terrain style should hold one per DEM tile. Checked here rather
/// than through a render, because everything between this and a picture -- the cover walk, the
/// encoder, the material -- can only be reasoned about once this much is known.
#[test]
fn a_dem_tile_carries_the_ground() {
    let (booted, _) = boot(&style(
        r#""terrain": {"source": "dem", "exaggeration": 1.4},"#,
    ));
    let booted = booted.expect("boots");

    let dem_tiles: Vec<&tessella_orchestrate::boot::BuiltTile> = booted
        .tiles
        .iter()
        .filter(|built| built.source == "dem")
        .collect();
    assert!(
        !dem_tiles.is_empty(),
        "the DEM was fetched but built no tiles"
    );

    // The ones at the *surface* cover, which is not all of them: a 256-pixel DEM is covered
    // twice over, once at the view's zoom for the ground and once a level finer for the layers
    // that read it as an image. The finer tiles carry a hillshade or a relief and no ground --
    // see `boot::DemReads` -- so this counts the coarsest zoom the DEM landed at.
    let surface = dem_tiles
        .iter()
        .map(|built| built.tile.z)
        .min()
        .expect("a DEM tile");
    let at_surface: Vec<_> = dem_tiles
        .iter()
        .filter(|built| built.tile.z == surface)
        .collect();
    let ground = at_surface
        .iter()
        .filter(|built| {
            built
                .buckets
                .iter()
                .any(|bucket| matches!(bucket.content, tessella_orchestrate::Content::Terrain(_)))
        })
        .count();
    assert_eq!(
        ground,
        at_surface.len(),
        "{ground} of {} DEM tiles at z{surface} carry the ground",
        at_surface.len()
    );
    // And none of the finer ones does, so the ground is on one grid rather than two.
    assert!(
        dem_tiles
            .iter()
            .filter(|built| built.tile.z != surface)
            .all(|built| {
                !built.buckets.iter().any(|bucket| {
                    matches!(bucket.content, tessella_orchestrate::Content::Terrain(_))
                })
            }),
        "a DEM tile past the surface cover carries a ground"
    );
}

/// Without the terrain, nothing asks for that source -- which is what makes the test above a
/// test of the terrain rather than of a style that would have fetched it anyway.
#[test]
fn an_unused_dem_is_not_fetched() {
    let (booted, urls) = boot(&style(""));
    booted.expect("boots");
    assert!(
        !urls.iter().any(|url| url.contains("/dem/")),
        "a source no layer and no terrain uses was fetched: {urls:?}"
    );
}

/// A terrain naming something that is not a DEM adds no ask, because there is nothing to fetch.
///
/// It is inert rather than an error: the rest of the style is still a map, and the tiles it does
/// name are still fetched.
#[test]
fn an_inert_terrain_asks_for_nothing() {
    for terrain in [
        r#""terrain": {"source": "missing"},"#,
        r#""terrain": {"source": "base"},"#,
    ] {
        let (booted, urls) = boot(&style(terrain));
        booted.expect("boots");
        assert!(
            !urls.iter().any(|url| url.contains("/dem/")),
            "{terrain}: {urls:?}"
        );
        assert!(
            urls.iter().any(|url| url.contains("/vector/")),
            "{terrain}: {urls:?}"
        );
    }
}

/// The ground lands on the same tiles the layers standing on it do.
///
/// A layer is raised by finding a ground tile that *contains* it, so a ground one zoom finer than
/// the vector tiles has no tile in common with them and raises nothing. That is what a 256-pixel
/// DEM does under the imagery rule -- it covers at `view.zoom + 1` -- and why a DEM read as a
/// surface covers at the view's own zoom instead. See `boot::DemReads`.
///
/// Checked as a set comparison rather than a count: the two covers have to be the same tiles, not
/// merely the same number of them.
#[test]
fn the_ground_shares_the_cover_with_what_stands_on_it() {
    let (booted, _) = boot(&style(
        r#""terrain": {"source": "dem", "exaggeration": 1.4},"#,
    ));
    let booted = booted.expect("boots");

    let at = |source: &str, ground: bool| -> std::collections::BTreeSet<(u8, u32, u32)> {
        booted
            .tiles
            .iter()
            .filter(|built| built.source == source)
            .filter(|built| {
                !ground
                    || built.buckets.iter().any(|bucket| {
                        matches!(bucket.content, tessella_orchestrate::Content::Terrain(_))
                    })
            })
            .map(|built| (built.tile.z, built.tile.x, built.tile.y))
            .collect()
    };

    let vector = at("base", false);
    let ground = at("dem", true);
    assert!(!vector.is_empty(), "no vector tiles were built");
    assert!(!ground.is_empty(), "no ground was built");
    assert_eq!(
        ground, vector,
        "the ground and the tiles standing on it are on different grids"
    );
}

/// The ground's grid comes from its own relief, not from the mesh's ceiling.
///
/// `MESH_SIZE` is the floor a *steep* tile falls back to, and splitting every tile at it is what
/// makes terrain unaffordable: 16,384 cells a tile, and every fill covering one becomes tens of
/// thousands of triangles. Smooth ground has to reach one cell — a ground drawn as two triangles,
/// and the layers standing on it split not at all.
///
/// The fixture is a photographic PNG read as a DEM, so its "elevation" is noise at every scale
/// and it lands at the ceiling. What this pins is that the number is *derived* rather than
/// constant: it is a power of two no larger than the mesh's grid, which is the shape
/// `Relief::cells_within` promises and the shape everything downstream indexes by.
#[test]
fn the_grounds_grid_is_derived_from_its_relief() {
    let (booted, _) = boot(&style(
        r#""terrain": {"source": "dem", "exaggeration": 1.4},"#,
    ));
    let booted = booted.expect("boots");
    let base = u32::from(tessella_layout::terrain::MESH_SIZE);

    let mut seen = 0;
    for built in &booted.tiles {
        for bucket in built.buckets.iter() {
            if let tessella_orchestrate::Content::Terrain(ground) = &bucket.content {
                seen += 1;
                assert!(ground.cells >= 1, "a ground with no cells at all");
                assert!(
                    ground.cells <= base,
                    "{} cells is finer than the mesh's own {base}",
                    ground.cells
                );
                assert!(
                    ground.cells.is_power_of_two(),
                    "{} cells is not a halving of the mesh's grid",
                    ground.cells
                );
            }
        }
    }
    assert!(seen > 0, "no ground was built");
}
