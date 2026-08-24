//! The whole pipeline against a live tile server.
//!
//! Style → source resolution → cover → HTTP fetch → MVT decode → buckets → stream, over a real
//! socket. Every stage of this has a test of its own against a fixture; what this adds is that
//! they compose when the bytes come from somewhere rather than from `include_bytes!`.
//!
//! # What only this can catch
//!
//! A URL templated in a way no server answers. A zoom range read as "no limit" and every tile
//! a 404. A cover whose tiles are addressed at the display zoom rather than the source's. A
//! coalescing table that dedupes within a view and not across them. None of those are visible
//! against an in-memory source, because an in-memory source answers whatever it is asked.
//!
//! The server binds an ephemeral loopback port, so this needs no network and no fixture server
//! running beside it. `tools/tile-server` is the same code with a `main`, for pointing a real
//! client at by hand.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use tessella_orchestrate::tile::{TileId, bucket_for, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{Coalescing, FileSource};
use tessella_storage::{ZoomRange, fetch_zoom, tileset};
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const FIXTURE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// The zooms the server claims, which are narrower than the view will ask for.
const SERVED: (u8, u8) = (0, 6);

fn server() -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some(SERVED)))
        .expect("binds a loopback port")
}

/// A style whose vector source is the running server, given inline rather than by manifest.
fn inline_style(origin: &str) -> Style {
    let text = format!(
        r##"{{"version": 8,
             "sources": {{"live": {{"type": "vector",
                 "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                 "minzoom": {}, "maxzoom": {}}}}},
             "layers": [
               {{"id": "water", "type": "fill", "source": "live", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}},
               {{"id": "admin", "type": "line", "source": "live", "source-layer": "admin",
                 "paint": {{"line-color": "#c04030", "line-width": 1.5}}}}]}}"##,
        SERVED.0, SERVED.1
    );
    Style::parse(&text).expect("style parses")
}

fn view(zoom: f64) -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

fn tileset_of(style: &Style, files: &HttpFileSource) -> tileset::TileSet {
    let Some(Source::Vector(source)) = style.source("live") else {
        panic!("the style has one vector source");
    };
    tileset::resolve(source, files).expect("resolves")
}

/// The pipeline runs end to end and produces geometry from bytes off a socket.
#[test]
fn a_live_source_builds_buckets() {
    let server = server();
    let style = inline_style(&server.origin());
    let files = Coalescing::new(HttpFileSource::default());
    let set = tileset_of(&style, files.inner());
    assert_eq!(set.zooms, ZoomRange { min: 0, max: 6 });

    let view = view(5.0);
    let tiles = cover::cover(&view).expect("covers");
    assert!(!tiles.is_empty(), "the view covers something");

    let mut water = 0usize;
    let mut lines = 0usize;
    for tile in &tiles {
        let z = fetch_zoom(tile.z, set.zooms).expect("within the source's range");
        let url = set.url_for(z, tile.x, tile.y, 1.0).expect("a template");
        let response = files.fetch(&url).expect("fetches");
        assert!(response.is_ok(), "{} for {url}", response.status);

        let decoded = Tile::decode(&response.body).expect("decodes");
        let buckets = build_mvt_tile(
            &style,
            TileId::overscaled(z, tile.x, tile.y, tile.z),
            &decoded,
        )
        .expect("builds");

        water += bucket_for(&buckets, "water")
            .and_then(|b| b.content.as_fill())
            .map_or(0, |b| b.vertices.len());
        lines += bucket_for(&buckets, "admin")
            .and_then(|b| b.content.as_line())
            .map_or(0, |b| b.vertices.len());
    }

    assert!(water > 0, "the water layer tessellated");
    assert!(lines > 0, "the admin layer extruded");
    assert_eq!(
        server.requests() as usize,
        tiles.len(),
        "one request per tile of the cover"
    );
}

/// Above the source's maximum, tiles are fetched at that maximum and *used* deeper.
///
/// This is what `overscaled_z` is for, and it is only exercised against a source that states a
/// range. Addressing the tile at the display zoom instead asks a server that stops at six for a
/// zoom-nine tile, and every request 404s — which looks like a broken server rather than a
/// broken client.
#[test]
fn a_view_past_the_sources_maximum_overscales() {
    let server = server();
    let style = inline_style(&server.origin());
    let files = HttpFileSource::default();
    let set = tileset_of(&style, &files);

    let view = view(9.0);
    let tiles = cover::cover(&view).expect("covers");
    assert!(tiles.iter().all(|tile| tile.z == 9), "the cover is at nine");

    for tile in &tiles {
        let z = fetch_zoom(tile.z, set.zooms).expect("within range");
        assert_eq!(z, 6, "clamped to what the source has");

        // The tile fetched is the ancestor: the display tile's index shifted by the difference.
        let shift = tile.z - z;
        let (x, y) = (tile.x >> shift, tile.y >> shift);
        let url = set.url_for(z, x, y, 1.0).expect("a template");
        let response = files.fetch(&url).expect("fetches");
        assert!(response.is_ok(), "{} for {url}", response.status);

        // And the bucket it builds is identified by both zooms.
        let id = TileId::overscaled(z, x, y, tile.z);
        assert_eq!(id.z, 6);
        assert_eq!(id.bucket_zoom(), 9);
        assert_eq!(id.to_string(), format!("6/{x}/{y}@9"));
    }
}

/// Below the source's minimum there is nothing to fetch.
#[test]
fn a_view_below_the_sources_minimum_fetches_nothing() {
    let range = ZoomRange { min: 4, max: 6 };
    assert_eq!(fetch_zoom(3, range), None);
    assert_eq!(fetch_zoom(0, range), None);
}

/// A source given by manifest resolves to the same thing as one given inline.
#[test]
fn a_manifest_resolves_to_the_same_tileset() {
    let inline = {
        let server = server();
        let style = inline_style(&server.origin());
        tileset_of(&style, &HttpFileSource::default())
    };

    // A second server that serves a TileJSON pointing at itself.
    let probe = tile_server::Server::start(tile_server::Routes::new()).expect("binds");
    let origin = probe.origin();
    drop(probe);
    let manifest = format!(
        r#"{{"tilejson":"3.0.0","tiles":["{origin}/{{z}}/{{x}}/{{y}}.pbf"],"minzoom":{},"maxzoom":{}}}"#,
        SERVED.0, SERVED.1
    );
    let server = tile_server::Server::start(
        tile_server::Routes::new()
            .at("/tiles.json", "application/json", manifest.into_bytes())
            .tiles(FIXTURE.to_vec(), Some(SERVED)),
    )
    .expect("binds");

    let style = Style::parse(&format!(
        r#"{{"version": 8,
             "sources": {{"live": {{"type": "vector", "url": "{}/tiles.json"}}}},
             "layers": []}}"#,
        server.origin()
    ))
    .expect("style parses");

    let set = tileset_of(&style, &HttpFileSource::default());
    assert_eq!(set.zooms, inline.zooms, "the manifest states the range");
    assert_eq!(set.templates.len(), 1);
    assert!(
        set.templates[0].ends_with("/{z}/{x}/{y}.pbf"),
        "{:?}",
        set.templates
    );
    assert_eq!(server.paths(), ["/tiles.json"], "the manifest, once");
}

/// Four views over one cover cost one fetch per tile, not four.
///
/// The §9.3 flatness assertion, measured at the server rather than in the client's own
/// bookkeeping: this is the number that must not scale with view count.
#[test]
fn four_views_over_one_cover_fetch_each_tile_once() {
    const VIEWS: usize = 4;
    let server = server();
    let style = inline_style(&server.origin());
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let set = tileset_of(&style, files.inner());

    let view = view(5.0);
    let tiles = cover::cover(&view).expect("covers");
    let urls: Vec<String> = tiles
        .iter()
        .map(|tile| {
            let z = fetch_zoom(tile.z, set.zooms).expect("in range");
            set.url_for(z, tile.x, tile.y, 1.0).expect("a template")
        })
        .collect();

    let start = Arc::new(Barrier::new(VIEWS));
    let handles: Vec<_> = (0..VIEWS)
        .map(|_| {
            let files = Arc::clone(&files);
            let start = Arc::clone(&start);
            let urls = urls.clone();
            std::thread::spawn(move || {
                start.wait();
                urls.iter()
                    .map(|url| files.fetch(url).expect("fetches").body.len())
                    .sum::<usize>()
            })
        })
        .collect();

    let totals: Vec<usize> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no panic"))
        .collect();
    assert!(
        totals.iter().all(|total| *total == totals[0]),
        "every view saw the same bytes: {totals:?}"
    );

    // Each view asks for every tile; the server must see far fewer than that.
    let asked = (VIEWS * urls.len()) as u64;
    assert_eq!(files.stats().fetches() + files.stats().waits(), asked);
    assert!(
        server.requests() < asked,
        "{} requests for {asked} asks",
        server.requests()
    );
    assert_eq!(files.stats().fetches(), server.requests());

    // And no tile was fetched more than once, which is the claim that matters.
    let mut per_path: BTreeMap<String, usize> = BTreeMap::new();
    for path in server.paths() {
        *per_path.entry(path).or_default() += 1;
    }
    assert!(
        per_path.values().all(|count| *count == 1),
        "a tile was fetched twice: {per_path:?}"
    );
}
