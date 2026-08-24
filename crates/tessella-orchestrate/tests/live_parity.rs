//! Oracle parity on a *real* style, over real tiles — R1's exit criterion (§9.1, §10).
//!
//! Every earlier oracle diff is against the hermetic style: four hand-written features chosen
//! to be tractable. This one is against a Protomaps planet extract at zoom 5 — nine tiles,
//! thousands of features, geometry nobody chose. The golden is captured by running the probe
//! against the same local tile server this test fetches from, so both sides see identical
//! bytes and any difference is in what they do with them.
//!
//! # Why vertex *sequences* can be compared here and not for GeoJSON
//!
//! DR-19: mbgl runs `fixupPolygons` — the wagyu union that rotates a ring's starting vertex —
//! on every GeoJSON polygon, and on a vector tile only for spec version 1, which is extinct.
//! So a vector source's rings reach the bucket in the order the tile wrote them, and the whole
//! chain is comparable as a sequence. This test is where that prediction is checked rather
//! than argued.
//!
//! # Running it
//!
//! Ignored by default: it needs the tile server the golden was captured against.
//!
//! ```sh
//! cd <tileserver> && ./serve.sh
//! cargo test -p tessella-orchestrate --test live_parity -- --ignored --nocapture
//! ```
//!
//! Regenerating the golden needs a maplibre-native checkout as well:
//!
//! ```sh
//! mbgl-capture-probe file://<tessella>/crates/tessella-style/tests/live_style.json \
//!     --zoom=5 --dump=<tessella>/tests/golden/live_protomaps_z5.dump
//! ```

#![allow(clippy::print_stdout)]

use std::collections::BTreeMap;

use tessella_orchestrate::tile::{TileId, bucket_for, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
use tessella_storage::{fetch_zoom, tileset};
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const STYLE: &str = include_str!("../../tessella-style/tests/live_style.json");
const DUMP: &str = include_str!("../../../tests/golden/live_protomaps_z5.dump");

/// FNV-1a 64, the hash the probe uses over a raw buffer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The camera the golden was captured at.
fn probe_view() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 5.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// `(layer, sublayer, x, y)` → the position attribute's `(count, hash)`.
fn oracle_positions() -> BTreeMap<(u32, u32, u32, u32), (usize, u64)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.trim().strip_prefix("attr ") else {
            continue;
        };
        if !rest.contains(" id=0 ") {
            continue;
        }
        let key = rest.split_whitespace().next().expect("a key");
        let layer: u32 = key[1..6].parse().expect("a layer");
        let sublayer: u32 = key[8..13].parse().expect("a sublayer");
        let tile = &key[key.find(".t").expect("a tile") + 2..];
        let mut parts = tile.split('_');
        let _z = parts.next();
        let x: u32 = parts.next().expect("x").parse().expect("an x");
        let y: u32 = parts.next().expect("y").parse().expect("a y");

        let src = rest
            .split_whitespace()
            .find_map(|field| field.strip_prefix("src="))
            .expect("a src");
        let (count, hash) = src.split_once(':').expect("count:hash");
        out.insert(
            (layer, sublayer, x, y),
            (
                count.parse().expect("a count"),
                u64::from_str_radix(hash, 16).expect("a hash"),
            ),
        );
    }
    out
}

/// `(layer, sublayer, x, y)` → the index buffer's `(count, hash)`.
fn oracle_indices() -> BTreeMap<(u32, u32, u32, u32), (usize, u64)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable ") else {
            continue;
        };
        let key = rest.split_whitespace().next().expect("a key");
        let layer: u32 = key[1..6].parse().expect("a layer");
        let sublayer: u32 = key[8..13].parse().expect("a sublayer");
        let tile = &key[key.find(".t").expect("a tile") + 2..];
        let mut parts = tile.split('_');
        let _z = parts.next();
        let x: u32 = parts.next().expect("x").parse().expect("an x");
        let y: u32 = parts.next().expect("y").parse().expect("a y");

        let idx = rest
            .split_whitespace()
            .find_map(|field| field.strip_prefix("idx="))
            .expect("an idx");
        let (count, hash) = idx.split_once(':').expect("count:hash");
        out.insert(
            (layer, sublayer, x, y),
            (
                count.parse().expect("a count"),
                u64::from_str_radix(hash, 16).expect("a hash"),
            ),
        );
    }
    out
}

/// Builds every tile of the cover from the live server.
fn build_cover() -> BTreeMap<(u32, u32), Vec<tessella_orchestrate::LayerBucket>> {
    let style = Style::parse(STYLE).expect("style parses");
    let origin =
        std::env::var("TESSELLA_LIVE_ORIGIN").unwrap_or_else(|_| "http://127.0.0.1:8080".into());

    // The style names the origin the golden was captured against; honour an override by
    // rewriting the manifest URL rather than by keeping two styles in step.
    let Some(Source::Vector(vector)) = style.source("world") else {
        panic!("the style has one vector source");
    };
    let mut vector = vector.clone();
    if let Some(url) = &vector.url {
        vector.url = Some(url.replace("http://127.0.0.1:8080", &origin));
    }

    let files = Coalescing::new(HttpFileSource::default());
    let set = match tileset::resolve(&vector, files.inner()) {
        Ok(set) => set,
        Err(error) => panic!("{origin}: {error}\nis `pmtiles serve` running?"),
    };

    let view = probe_view();
    let mut out = BTreeMap::new();
    for tile in cover::cover(&view).expect("covers") {
        let z = fetch_zoom(tile.z, set.zooms).expect("within range");
        let shift = tile.z - z;
        let (x, y) = (tile.x >> shift, tile.y >> shift);
        let url = set.url_for(z, x, y, 1.0).expect("a template");
        let response = files.fetch(&url).unwrap_or_else(|e| panic!("{url}: {e}"));
        assert!(response.is_ok(), "{} for {url}", response.status);

        let decoded = Tile::decode(&response.body).unwrap_or_else(|e| panic!("{url}: {e}"));
        let buckets = build_mvt_tile(&style, TileId::overscaled(z, x, y, tile.z), &decoded)
            .unwrap_or_else(|e| panic!("{url}: {e}"));
        out.insert((tile.x, tile.y), buckets);
    }
    out
}

/// The cover is the oracle's, tile for tile.
#[test]
#[ignore = "needs the tile server the golden was captured against"]
fn the_cover_matches_the_oracle() {
    let mine: Vec<(u32, u32)> = build_cover().keys().copied().collect();
    let mut theirs: Vec<(u32, u32)> = oracle_positions()
        .keys()
        .map(|(_, _, x, y)| (*x, *y))
        .collect();
    theirs.sort_unstable();
    theirs.dedup();

    assert_eq!(mine, theirs, "the same nine tiles");
}

/// A fill layer's vertex and index buffers are byte-identical to the oracle's, on real data.
///
/// This is DR-19's prediction discharged: no `fixupPolygons` runs on a vector tile, so the
/// rings reach the bucket in the tile's own order and the sequences compare directly — which
/// the GeoJSON path cannot do.
#[test]
#[ignore = "needs the tile server the golden was captured against"]
fn fill_buffers_are_byte_identical_to_the_oracle() {
    let built = build_cover();
    let positions = oracle_positions();
    let indices = oracle_indices();

    // `earth` is style layer 1, drawn at sublayer 1 (triangles) and 2 (outline).
    let mut compared = 0;
    for ((x, y), buckets) in &built {
        let Some(fill) = bucket_for(buckets, "earth").and_then(|b| b.content.as_fill()) else {
            continue;
        };
        if fill.segments.is_empty() {
            // The oracle draws nothing here either, or the cover assertion would have failed.
            assert!(
                !positions.contains_key(&(1, 1, *x, *y)),
                "the oracle drew earth at {x}/{y} and this did not"
            );
            continue;
        }

        let (want_vertices, want_hash) = positions
            .get(&(1, 1, *x, *y))
            .unwrap_or_else(|| panic!("the oracle has no earth at {x}/{y}"));
        let vertex_bytes: Vec<u8> = fill
            .vertices
            .iter()
            .flat_map(|v| [v[0].to_le_bytes(), v[1].to_le_bytes()])
            .flatten()
            .collect();
        assert_eq!(
            fill.vertices.len(),
            *want_vertices,
            "vertex count at {x}/{y}"
        );
        assert_eq!(fnv1a(&vertex_bytes), *want_hash, "vertices at {x}/{y}");

        let (want_indices, _) = indices
            .get(&(1, 1, *x, *y))
            .unwrap_or_else(|| panic!("the oracle has no earth indices at {x}/{y}"));
        assert_eq!(fill.indices.len(), *want_indices, "index count at {x}/{y}");

        compared += 1;
    }

    println!("earth: {compared} tiles compared byte for byte");
    assert!(compared >= 6, "only {compared} tiles had geometry");
}

/// Every layer the oracle draws, this build draws — and with the same vertex counts.
#[test]
#[ignore = "needs the tile server the golden was captured against"]
fn every_layer_matches_the_oracle() {
    let built = build_cover();
    let positions = oracle_positions();
    // Style order: 0 background, 1 earth, 2 water, 3 boundaries.
    let by_index = ["bg", "earth", "water", "boundaries"];

    let mut checked = 0;
    let mut mismatched = Vec::new();
    for ((layer, sublayer, x, y), (want_vertices, want_hash)) in &positions {
        // The background's geometry is synthesized by the consumer, not carried.
        if *layer == 0 {
            continue;
        }
        // A fill's outline shares the triangles' vertices; compare each buffer once.
        if *sublayer == 2 {
            continue;
        }
        let buckets = built.get(&(*x, *y)).expect("the cover matched");
        let bucket = bucket_for(buckets, by_index[*layer as usize]).expect("the layer");

        let (got_vertices, bytes): (usize, Vec<u8>) = if let Some(fill) = bucket.content.as_fill() {
            (
                fill.vertices.len(),
                fill.vertices
                    .iter()
                    .flat_map(|v| [v[0].to_le_bytes(), v[1].to_le_bytes()])
                    .flatten()
                    .collect(),
            )
        } else if let Some(line) = bucket.content.as_line() {
            // Interleaved per vertex — position then data — which is what the buffer holds and
            // what the oracle hashed. Grouping them would produce the right length and the
            // wrong hash.
            let mut bytes = Vec::with_capacity(line.vertices.len() * 8);
            for vertex in &line.vertices {
                bytes.extend_from_slice(&vertex.pos_normal[0].to_le_bytes());
                bytes.extend_from_slice(&vertex.pos_normal[1].to_le_bytes());
                bytes.extend_from_slice(&vertex.data);
            }
            (line.vertices.len(), bytes)
        } else {
            continue;
        };

        checked += 1;
        if got_vertices != *want_vertices {
            mismatched.push(format!(
                "{}@{x}/{y}: {got_vertices} vertices, oracle {want_vertices}",
                by_index[*layer as usize]
            ));
            continue;
        }
        if fnv1a(&bytes) != *want_hash {
            mismatched.push(format!(
                "{}@{x}/{y}: {} vertices agree, bytes differ",
                by_index[*layer as usize], got_vertices
            ));
        }
    }

    println!("{checked} drawables compared");
    assert!(mismatched.is_empty(), "{mismatched:#?}");
    assert!(checked > 10, "only {checked} drawables were comparable");
}
