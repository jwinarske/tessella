//! A raster source, end to end: resolved, covered at its own zoom, fetched, decoded, built.
//!
//! The layer's geometry is tested in `raster_tiles` and the decoder in `tessella-source`. What
//! is left is the pipeline between them, and the part of it that is not obvious is the *cover*:
//! a raster source is not covered at the map's zoom, and getting that wrong draws a satellite
//! basemap at half the resolution of the labels over it.

#![cfg(feature = "image")]

use std::sync::Arc;

use tessella_orchestrate::boot::{Boot, BootError, ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::ViewTransform;

/// maplibre-native's own 256-pixel raster tile.
const TILE_PNG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.png");
const TILE_JPEG: &[u8] = include_bytes!("../../../tests/image-fixtures/tile.jpeg");
const MVT: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// An origin serving one body for tiles and refusing anything it does not recognise.
struct Origin {
    raster: Vec<u8>,
}

impl FileSource for Origin {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let body = if url.contains("/imagery/") {
            self.raster.clone()
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

fn view(zoom: f64) -> ViewTransform {
    ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

fn boot(style: &str, view: &ViewTransform, raster: Vec<u8>) -> Result<Boot, BootError> {
    let pool = Pool::new(Workers::serial());
    tessella_orchestrate::boot::cold_start(&ColdStart {
        style,
        view,
        files: Arc::new(Coalescing::new(Origin { raster })),
        cache: Arc::new(TileCache::new(64)),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    })
}

/// A style with a raster source of the given tile size, and a vector source beside it.
fn overlay(tile_size: u32) -> String {
    format!(
        r#"{{"version": 8,
            "sources": {{
              "sat": {{"type": "raster", "tiles": ["https://o/imagery/{{z}}/{{x}}/{{y}}.png"],
                       "tileSize": {tile_size}}},
              "base": {{"type": "vector", "tiles": ["https://o/vector/{{z}}/{{x}}/{{y}}.mvt"]}}
            }},
            "layers": [{{"id": "imagery", "type": "raster", "source": "sat"}},
                       {{"id": "roads", "type": "line", "source": "base",
                         "source-layer": "road"}}]}}"#
    )
}

/// The tiles one source contributed, as `(z, x, y)`.
fn tiles(boot: &Boot, source: &str) -> Vec<(u8, u32, u32)> {
    let mut found: Vec<(u8, u32, u32)> = boot
        .tiles
        .iter()
        .filter(|built| built.source == source)
        .map(|built| (built.tile.z, built.tile.x, built.tile.y))
        .collect();
    found.sort_unstable();
    found
}

/// A 256-pixel raster source is covered one zoom level deeper than the vector source beside it.
///
/// mbgl's `coveringZoomLevel` shifts by `log2(512 / tileSize)`, so a 256-pixel tile — which is
/// what most imagery services serve — needs one level more to fill the same screen. Covering it
/// at the map's own zoom fetches imagery at half the resolution of everything drawn over it,
/// which reads as a blurry basemap rather than as a cover bug.
#[test]
fn a_256_pixel_raster_source_is_covered_one_level_deeper() {
    let booted = boot(&overlay(256), &view(13.0), TILE_PNG.to_vec()).expect("boots");

    let vector = tiles(&booted, "base");
    let raster = tiles(&booted, "sat");
    assert!(!vector.is_empty() && !raster.is_empty());

    assert!(vector.iter().all(|tile| tile.0 == 13), "{vector:?}");
    assert!(raster.iter().all(|tile| tile.0 == 14), "{raster:?}");

    // And it is four times the tiles, because a level is four times the tiles. Not exactly
    // four — the two covers round their edges independently — but the same ground, so every
    // raster tile's parent is a vector tile of the cover.
    assert!(
        raster.len() > vector.len(),
        "{} vs {}",
        raster.len(),
        vector.len()
    );
    for (z, x, y) in &raster {
        assert!(
            vector.contains(&(z - 1, x >> 1, y >> 1)),
            "raster tile {z}/{x}/{y} is not over the vector cover"
        );
    }
}

/// A 512-pixel raster source is covered exactly as a vector source is.
///
/// The shift is `log2(512 / 512)`, which is zero — so the two covers agree, and the rounding
/// that a raster source does instead of flooring has nothing to round. Worth asserting because
/// it is the case a wrong shift still passes: a build covering every raster source one level
/// deep looks right on a 256 source and wrong here.
#[test]
fn a_512_pixel_raster_source_is_covered_like_a_vector_one() {
    let booted = boot(&overlay(512), &view(13.0), TILE_PNG.to_vec()).expect("boots");
    assert_eq!(tiles(&booted, "sat"), tiles(&booted, "base"));
}

/// The default is 512 when a source states no size, which is the spec's.
#[test]
fn a_raster_source_with_no_stated_size_is_512() {
    let style = r#"{"version": 8,
        "sources": {"sat": {"type": "raster", "tiles": ["https://o/imagery/{z}/{x}/{y}.png"]},
                    "base": {"type": "vector", "tiles": ["https://o/vector/{z}/{x}/{y}.mvt"]}},
        "layers": [{"id": "imagery", "type": "raster", "source": "sat"},
                   {"id": "roads", "type": "line", "source": "base", "source-layer": "road"}]}"#;
    let booted = boot(style, &view(13.0), TILE_PNG.to_vec()).expect("boots");
    assert_eq!(tiles(&booted, "sat"), tiles(&booted, "base"));
}

/// The picture arrives with the bucket, decoded and the size the origin sent.
#[test]
fn the_decoded_picture_rides_with_the_bucket() {
    let booted = boot(&overlay(256), &view(13.0), TILE_PNG.to_vec()).expect("boots");

    let built = booted
        .tiles
        .iter()
        .find(|built| built.source == "sat")
        .expect("a raster tile");
    let content = built.buckets[0]
        .content
        .as_raster()
        .expect("a raster bucket");

    assert_eq!(content.image.size(), (256, 256));
    assert_eq!(content.image.pixels.len(), 256 * 256 * 4);
    assert_eq!(content.bucket.quads(), 1);
}

/// A JPEG imagery tile decodes exactly as a PNG one does.
///
/// Which is the case that matters for a satellite basemap, and the one that used to fail
/// silently: the tile fetches, the bytes arrive, and nothing reads them.
#[test]
fn a_jpeg_imagery_tile_decodes() {
    let booted = boot(&overlay(256), &view(13.0), TILE_JPEG.to_vec()).expect("boots");
    let built = booted
        .tiles
        .iter()
        .find(|built| built.source == "sat")
        .expect("a raster tile");
    let content = built.buckets[0]
        .content
        .as_raster()
        .expect("a raster bucket");
    assert_eq!(content.image.size(), (256, 256));
}

/// A body that is not an image fails the tile rather than drawing something.
///
/// An origin serving an error page with a 200 is a real failure mode — a proxy, an expired
/// token, a rate limit — and the bytes are HTML. Treating an undecodable body as an empty tile
/// would draw a hole and report success, which sends the next person looking at the cover.
#[test]
fn an_undecodable_imagery_tile_is_reported() {
    let failure = boot(
        &overlay(256),
        &view(13.0),
        b"<html>rate limited</html>".to_vec(),
    )
    .expect_err("an html body is not a tile");
    assert!(
        matches!(failure, BootError::Decode { .. }),
        "expected a decode error, got {failure:?}"
    );
}

/// An absent imagery tile is a hole rather than a failure.
///
/// A raster source's coverage is not a rectangle — imagery stops at a country's border, or a
/// zoom level, or the sea — and the cover asks for the whole viewport. mbgl treats a 404 as an
/// empty tile, and so does this: the alternative is a style failing to start because a corner of
/// the screen is outside a survey.
#[test]
fn an_absent_imagery_tile_leaves_a_hole() {
    let style = r#"{"version": 8,
        "sources": {"sat": {"type": "raster", "tiles": ["https://o/missing/{z}/{x}/{y}.png"]}},
        "layers": [{"id": "imagery", "type": "raster", "source": "sat"}]}"#;
    let booted = boot(style, &view(13.0), TILE_PNG.to_vec()).expect("a 404 is not a boot failure");
    assert!(
        booted.tiles.iter().all(|built| built.buckets.is_empty()),
        "a 404 produced a bucket"
    );
}

/// A raster source's zoom range clamps it, as a vector source's does.
///
/// The clamp runs after the covering-zoom shift, not before. A 256-pixel source stopping at
/// zoom 14 is asked for z14 tiles by a z14 *view*, not by a z13 one — reading the range against
/// the map's zoom instead would fetch a level the source does not have and take the 404s for an
/// origin fault.
#[test]
fn a_raster_sources_maxzoom_clamps_the_shifted_level() {
    let style = r#"{"version": 8,
        "sources": {"sat": {"type": "raster", "tiles": ["https://o/imagery/{z}/{x}/{y}.png"],
                            "tileSize": 256, "maxzoom": 13}},
        "layers": [{"id": "imagery", "type": "raster", "source": "sat"}]}"#;

    // A view at zoom 13 covers this source at 14, which it does not have — so every tile is
    // overscaled from 13.
    let booted = boot(style, &view(13.0), TILE_PNG.to_vec()).expect("boots");
    let built: Vec<_> = booted
        .tiles
        .iter()
        .map(|built| (built.tile.z, built.tile.overscaled_z))
        .collect();
    assert!(!built.is_empty());
    assert!(
        built.iter().all(|(z, over)| *z == 13 && *over == 14),
        "{built:?}"
    );
}
