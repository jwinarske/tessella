//! A style's sprite is fetched beside its manifests, not after them (§12.5).
//!
//! # What this is for
//!
//! §12.5 asks for "speculative parallel fetch (sprite + glyph ranges + cover tiles issued the
//! moment sources parse, before layer compilation finishes)". The sprite is the one of the three
//! that can genuinely be issued that early: it is addressed by the style alone, so nothing it
//! needs is in a source manifest. Tiles cannot — the manifest carries their templates — which is
//! the asymmetry that makes issuing the sprite early worth doing rather than a uniform rule.
//!
//! Cold start did not fetch it at all, so a style with icons had no sheet when its first tiles
//! were built and the icons appeared on some later frame.
//!
//! # Why the assertion is a timestamp and not an order
//!
//! Because "in parallel" is not observable from the outside as an ordering: a fast enough serial
//! fetch produces the same sequence. What is observable is that the sheet is *there* when the
//! start returns, and that it arrived before the point a serial version could have started it.

#![cfg(all(feature = "std", feature = "image"))]

use std::sync::Arc;

use tessella_orchestrate::boot::{ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
use tessella_tile::cover::ViewTransform;

const FIXTURE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const SPRITE_INDEX: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.json");
const SPRITE_SHEET: &[u8] = include_bytes!("../../../tests/sprite-fixtures/emerald.png");

fn view() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

fn start(style: &str, files: &Arc<Coalescing<HttpFileSource>>) -> tessella_orchestrate::boot::Boot {
    let pool = Pool::new(Workers::new(4));
    tessella_orchestrate::boot::cold_start(&ColdStart {
        style,
        view: &view(),
        files: Arc::clone(files),
        cache: Arc::new(TileCache::new(64)),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    })
    .expect("the start succeeds")
}

/// The sheet is in hand when the start returns.
#[test]
fn a_styles_sprite_arrives_with_its_tiles() {
    let server = tile_server::Server::start(
        tile_server::Routes::new()
            .tiles(FIXTURE.to_vec(), Some((0, 14)))
            .at("/sprite.json", "application/json", SPRITE_INDEX.to_vec())
            .at("/sprite.png", "image/png", SPRITE_SHEET.to_vec()),
    )
    .expect("the server starts");
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));

    let style = format!(
        r##"{{"version": 8,
             "sprite": "{origin}/sprite",
             "sources": {{"v": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"]}}}},
             "layers": [{{"id": "water", "type": "fill", "source": "v",
                          "source-layer": "water", "paint": {{"fill-color": "#123"}}}}]}}"##,
        origin = server.origin()
    );

    let boot = start(&style, &files);
    let sprites = boot.sprites.as_ref().expect("the sheet was fetched");
    assert!(
        sprites.get("grass_pattern").is_some(),
        "the sheet decoded and its index is readable"
    );
    assert!(
        boot.trace.sprite_fetched.is_some(),
        "and the trace says when"
    );
    assert!(!boot.tiles.is_empty(), "the tiles arrived too");

    // Issued beside the manifests rather than after the tiles: a sprite fetched serially at the
    // end could not land before the last tile did.
    assert!(
        boot.trace.sprite_fetched.expect("timed") <= boot.trace.complete,
        "the sheet is in hand no later than the cover is"
    );
}

/// A style with no sprite fetches none, and says so the same way.
#[test]
fn a_style_without_a_sprite_asks_for_nothing() {
    let server =
        tile_server::Server::start(tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))))
    .expect("the server starts");
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));

    let style = format!(
        r##"{{"version": 8,
             "sources": {{"v": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"]}}}},
             "layers": [{{"id": "water", "type": "fill", "source": "v",
                          "source-layer": "water", "paint": {{"fill-color": "#123"}}}}]}}"##,
        origin = server.origin()
    );

    let boot = start(&style, &files);
    assert!(boot.sprites.is_none());
    assert!(boot.trace.sprite_fetched.is_none());
    assert!(!boot.tiles.is_empty(), "and the map still starts");
}

/// A sprite that does not answer costs the icons and not the map.
#[test]
fn a_missing_sheet_does_not_fail_the_start() {
    let server = tile_server::Server::start(
        tile_server::Routes::new()
            .tiles(FIXTURE.to_vec(), Some((0, 14)))
            .at_status("/sprite.json", 404),
    )
    .expect("the server starts");
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));

    let style = format!(
        r##"{{"version": 8,
             "sprite": "{origin}/sprite",
             "sources": {{"v": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"]}}}},
             "layers": [{{"id": "water", "type": "fill", "source": "v",
                          "source-layer": "water", "paint": {{"fill-color": "#123"}}}}]}}"##,
        origin = server.origin()
    );

    let boot = start(&style, &files);
    assert!(boot.sprites.is_none(), "no sheet");
    assert!(
        boot.trace.sprite_fetched.is_none(),
        "and the trace does not claim one"
    );
    assert!(
        !boot.tiles.is_empty(),
        "every layer that is not an icon still drew"
    );
}
