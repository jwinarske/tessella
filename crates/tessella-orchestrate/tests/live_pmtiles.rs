//! The pipeline against a real tile origin, opt-in.
//!
//! `live_tiles` runs the same shape against a server this repository starts, and is what CI
//! checks. This runs against one that is already there — `pmtiles serve` over real Protomaps
//! archives — and so is `#[ignore]`d: it needs something outside the build to be running, and a
//! test that fails because a service is down is not a test of this code.
//!
//! ```sh
//! cd <tileserver> && ./serve.sh          # pmtiles serve . --port 8080
//! cargo test -p tessella-orchestrate --test live_pmtiles -- --ignored --nocapture
//! ```
//!
//! `TESSELLA_LIVE_ORIGIN` overrides the origin. Any origin serving TileJSON and vector tiles
//! works; nothing here knows it is PMTiles, because by the time the bytes reach a client
//! `pmtiles serve` is an ordinary XYZ origin.
//!
//! # What this is for
//!
//! A fixture is one tile that was known to work when it was committed. A real archive is
//! millions, cut from a live planet build, with every layer the schema defines and geometry
//! nobody chose to be convenient. It is the difference between "the decoder handles this tile"
//! and "the decoder handles tiles".

#![allow(clippy::print_stdout)]

use std::time::Instant;

use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
use tessella_storage::{fetch_zoom, tileset};
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

fn origin() -> String {
    std::env::var("TESSELLA_LIVE_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

/// The Protomaps schema, which is what these archives carry — not OpenMapTiles'.
fn style(origin: &str, source: &str, manifest: &str) -> Style {
    let text = format!(
        r##"{{"version": 8,
             "sources": {{"{source}": {{"type": "vector", "url": "{origin}/{manifest}"}}}},
             "layers": [
               {{"id": "earth", "type": "fill", "source": "{source}", "source-layer": "earth",
                 "paint": {{"fill-color": "#e9e4d8"}}}},
               {{"id": "water", "type": "fill", "source": "{source}", "source-layer": "water",
                 "paint": {{"fill-color": "#a8c9e0"}}}},
               {{"id": "landuse", "type": "fill", "source": "{source}", "source-layer": "landuse",
                 "paint": {{"fill-color": "#dfe6cf", "fill-opacity": 0.7}}}},
               {{"id": "buildings", "type": "fill", "source": "{source}",
                 "source-layer": "buildings", "paint": {{"fill-color": "#cbc4b6"}}}},
               {{"id": "roads", "type": "line", "source": "{source}", "source-layer": "roads",
                 "paint": {{"line-color": "#ffffff",
                   "line-width": ["interpolate", ["linear"], ["zoom"],
                      10, ["match", ["get", "kind"], "highway", 2.0, 0.5],
                      16, ["match", ["get", "kind"], "highway", 8.0, 2.0]]}}}},
               {{"id": "boundaries", "type": "line", "source": "{source}",
                 "source-layer": "boundaries",
                 "paint": {{"line-color": "#8c7f95", "line-width": 1.0}}}}]}}"##
    );
    Style::parse(&text).expect("style parses")
}

fn view(longitude: f64, latitude: f64, zoom: f64) -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude,
        latitude,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// Fetches a whole cover and builds every tile, reporting what came out.
fn run(source: &str, manifest: &str, longitude: f64, latitude: f64, zoom: f64) {
    let origin = origin();
    let style = style(&origin, source, manifest);
    let files = Coalescing::new(HttpFileSource::default());

    let Some(Source::Vector(vector)) = style.source(source) else {
        panic!("the style has one vector source");
    };
    let set = match tileset::resolve(vector, files.inner()) {
        Ok(set) => set,
        Err(error) => panic!("{origin}: {error}\nis `pmtiles serve` running?"),
    };
    println!("{manifest}: zooms {:?}, scheme {:?}", set.zooms, set.scheme);

    let view = view(longitude, latitude, zoom);
    let tiles = cover::cover(&view).expect("covers");
    let started = Instant::now();

    let mut fetched = 0usize;
    let mut bytes = 0usize;
    let mut absent = 0usize;
    let mut totals: std::collections::BTreeMap<String, (usize, usize)> = Default::default();

    for tile in &tiles {
        let Some(z) = fetch_zoom(tile.z, set.zooms) else {
            continue;
        };
        let shift = tile.z - z;
        let (x, y) = (tile.x >> shift, tile.y >> shift);
        let url = set.url_for(z, x, y, 1.0).expect("a template");

        let response = files
            .fetch(&url)
            .unwrap_or_else(|error| panic!("{url}: {error}"));
        if response.is_absent() {
            absent += 1;
            continue;
        }
        assert!(response.is_ok(), "{} for {url}", response.status);
        fetched += 1;
        bytes += response.body.len();

        let decoded = Tile::decode(&response.body).unwrap_or_else(|error| panic!("{url}: {error}"));
        let buckets = build_mvt_tile(&style, TileId::overscaled(z, x, y, tile.z), &decoded)
            .unwrap_or_else(|error| panic!("{url}: {error}"));

        for bucket in &buckets {
            let entry = totals.entry(bucket.layer_id.clone()).or_default();
            if let Some(fill) = bucket.content.as_fill() {
                entry.0 += fill.vertices.len();
                entry.1 += fill.indices.len() / 3;
            }
            if let Some(line) = bucket.content.as_line() {
                entry.0 += line.vertices.len();
                entry.1 += line.indices.len() / 3;
            }
        }
    }

    let elapsed = started.elapsed();
    println!(
        "  {} tiles at z{zoom}: {fetched} fetched, {absent} absent, {} KiB, {elapsed:?}",
        tiles.len(),
        bytes / 1024
    );
    for (layer, (vertices, triangles)) in &totals {
        println!("  {layer:<12} {vertices:>8} vertices  {triangles:>8} triangles");
    }

    assert!(fetched > 0, "the cover found tiles");
    let drawn: usize = totals.values().map(|(vertices, _)| vertices).sum();
    assert!(drawn > 0, "and something tessellated");
}

/// The whole planet at low zoom.
#[test]
#[ignore = "needs a tile server; see the module docs"]
fn the_world_archive_builds() {
    run("world", "world_z7.json", 0.0, 20.0, 3.0);
}

/// Berlin at a zoom the world archive cannot reach, where buildings and roads exist.
///
/// This is the case a low-zoom fixture cannot stand in for: at z14 a tile is dense, the road
/// network is real, and `line-width` is a composite expression that has to be evaluated per
/// feature at both ends of the tile's zoom range.
#[test]
#[ignore = "needs a tile server; see the module docs"]
fn the_berlin_archive_builds_at_high_zoom() {
    run("berlin", "berlin_z15.json", 13.405, 52.52, 14.0);
}

/// Above the archive's maximum, tiles are fetched at the maximum and used deeper.
#[test]
#[ignore = "needs a tile server; see the module docs"]
fn the_berlin_archive_overscales_past_its_maximum() {
    run("berlin", "berlin_z15.json", 13.405, 52.52, 17.0);
}

/// Cold-boot-to-first-tile against real archives — R1's exit metric.
///
/// Reports the stage trace at several zooms and both serially and fanned out. The number that
/// matters is `first_bucket`: the moment there is something to draw, which is what a person
/// perceives as the map appearing.
#[test]
#[ignore = "needs a tile server; see the module docs"]
fn cold_boot_to_first_tile() {
    use tessella_orchestrate::boot::{BootError, ColdStart, Workers};
    use tessella_orchestrate::cache::TileCache;

    let origin = origin();
    for (name, manifest, lon, lat, zoom) in [
        ("world", "world_z7.json", 0.0, 20.0, 3.0),
        ("berlin", "berlin_z15.json", 13.405, 52.52, 14.0),
    ] {
        let text = format!(
            r##"{{"version": 8,
                 "sources": {{"{name}": {{"type": "vector", "url": "{origin}/{manifest}"}}}},
                 "layers": [
                   {{"id": "bg", "type": "background",
                     "paint": {{"background-color": "#a8c9e0"}}}},
                   {{"id": "earth", "type": "fill", "source": "{name}",
                     "source-layer": "earth", "paint": {{"fill-color": "#e9e4d8"}}}},
                   {{"id": "water", "type": "fill", "source": "{name}",
                     "source-layer": "water", "paint": {{"fill-color": "#a8c9e0"}}}},
                   {{"id": "roads", "type": "line", "source": "{name}",
                     "source-layer": "roads", "paint": {{"line-color": "#ffffff",
                       "line-width": ["interpolate", ["linear"], ["zoom"],
                          10, ["match", ["get", "kind"], "highway", 2.0, 0.5],
                          16, ["match", ["get", "kind"], "highway", 8.0, 2.0]]}}}}]}}"##
        );

        for workers in [Workers::serial(), Workers::default()] {
            let files = Coalescing::new(HttpFileSource::default());
            // A fresh cache per run: a shared one would make the second a cache hit and report
            // a startup time that no cold start ever sees.
            let cache: TileCache<BootError> = TileCache::new(256);
            let boot = ColdStart {
                style: &text,
                view: &view(lon, lat, zoom),
                files: &files,
                cache: &cache,
                workers,
                style_rev: 1,
            }
            .run()
            .unwrap_or_else(|error| panic!("{error}\nis `pmtiles serve` running?"));
            let t = boot.trace;
            println!(
                "{name} z{zoom} x{}: parse {:?}  sources {:?}  cover {:?}  \
                 first fetch {:?}  FIRST BUCKET {:?}  complete {:?}  \
                 ({} tiles, {} KiB, {} vertices)",
                workers.get(),
                t.style_parsed,
                t.sources_resolved - t.style_parsed,
                t.cover_computed - t.sources_resolved,
                t.first_fetch - t.cover_computed,
                t.first_bucket,
                t.complete,
                boot.tiles.len(),
                boot.bytes / 1024,
                boot.vertices()
            );
            assert!(boot.vertices() > 0);
            assert!(t.first_bucket <= t.complete);
        }
    }
}
