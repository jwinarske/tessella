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
