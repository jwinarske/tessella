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

use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_orchestrate::ubo::{self, DrawableEntry, GlobalPaintParams, LineDrawableEntry};
use tessella_style::property::Color;

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
        let buckets = build_mvt_tile(
            &style,
            "world",
            TileId::overscaled(z, x, y, tile.z),
            &decoded,
        )
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

// --- Uniform buffers, the other half of R1's exit criterion ---
//
// The tests above compare geometry: what the buffers hold. These compare the uniforms that tell
// a shader where to put it and what colour to make it. Both halves have to hold on a real style
// for the exit to mean anything — geometry alone is a map drawn correctly in the wrong place.

/// `(layer, slot)` → `(size, sorted 16-byte blocks)`, from the dump's `ubo` lines.
///
/// The same parse `ubo_buffers.rs` does against the hermetic dump; the dump format does not vary
/// by style, which is what lets the two share a shape.
fn oracle_ubos() -> BTreeMap<(i32, u32), (usize, Vec<String>)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("ubo ") else {
            continue;
        };
        let mut fields = rest.split(' ');
        let key = fields.next().expect("a key");
        let (kind, index) = key.split_once(':').expect("kind:index");
        let layer = if kind == "global" {
            -1
        } else {
            index.parse::<i32>().expect("layer number")
        };
        let field = |name: &str, fields: &mut std::str::Split<'_, char>| -> String {
            fields
                .next()
                .and_then(|f| f.strip_prefix(name))
                .unwrap_or_else(|| panic!("a {name} field"))
                .to_string()
        };
        let slot: u32 = field("slot=", &mut fields).parse().expect("slot number");
        let size: usize = field("size=", &mut fields).parse().expect("size number");
        let bytes = field("bytes=", &mut fields);

        let mut blocks: Vec<String> = bytes
            .as_bytes()
            .chunks(32)
            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
            .collect();
        blocks.sort();
        out.insert((layer, slot), (size, blocks));
    }
    out
}

/// Sorted 16-byte blocks of a packed buffer, in the dump's spelling.
fn ubo_blocks(bytes: &[u8]) -> Vec<String> {
    let mut blocks: Vec<String> = bytes
        .chunks(16)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    blocks.sort();
    blocks
}

/// The drawables the oracle emitted for a layer, read out of their ids.
///
/// A drawable id spells `L{layer}.S{sublayer}.t{z}_{x}_{y}_o{overscaled}_w{wrap}`, so the dump
/// states exactly which tiles produced a drawable for which layer — which is not "every tile",
/// and assuming otherwise is what the first version of this got wrong. Layer 1 covers twelve
/// drawables where layer 2 covers eighteen: a layer with no features in a tile draws nothing
/// there, and the oracle's buffer is sized to what it drew.
///
/// Taking the list from the dump rather than from a built cover keeps these tests independent of
/// the tile server. What tiles the *cover* contains is already asserted by
/// `the_cover_matches_the_oracle`; what this needs is which of them each layer drew in.
fn oracle_drawables(layer: i32) -> Vec<(u8, u32, u32, i32, i32)> {
    let mut out = Vec::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable ") else {
            continue;
        };
        let id = rest.split(' ').next().expect("an id");
        let mut parts = id.split('.');
        let l: i32 = parts
            .next()
            .and_then(|f| f.strip_prefix('L'))
            .expect("a layer")
            .parse()
            .expect("layer number");
        if l != layer {
            continue;
        }
        let sub: i32 = parts
            .next()
            .and_then(|f| f.strip_prefix('S'))
            .expect("a sublayer")
            .parse()
            .expect("sublayer number");
        let tile = parts.next().expect("a tile");
        let mut coords = tile.strip_prefix('t').expect("t-prefixed").split('_');
        let z: u8 = coords.next().expect("z").parse().expect("z number");
        let x: u32 = coords.next().expect("x").parse().expect("x number");
        let y: u32 = coords.next().expect("y").parse().expect("y number");
        let wrap: i32 = coords
            .nth(1)
            .and_then(|f| f.strip_prefix('w'))
            .map(|w| w.replace('+', ""))
            .expect("a wrap")
            .parse()
            .expect("wrap number");

        out.push((z, x, y, wrap, sub));
    }
    assert!(!out.is_empty(), "the oracle drew nothing for layer {layer}");
    out
}

/// Those drawables as fill/background entries.
fn fill_entries(layer: i32) -> Vec<DrawableEntry> {
    let view = probe_view();
    oracle_drawables(layer)
        .into_iter()
        .map(|(z, x, y, wrap, sub)| {
            DrawableEntry::for_tile(&view, z, x, y, wrap, layer, sub).expect("an unrotated camera")
        })
        .collect()
}

/// The frame-wide paint parameters match on the real style's camera.
///
/// Different zoom and different viewport from the hermetic capture, so this is the arithmetic
/// rather than a constant that happened to be right once.
#[test]
fn the_live_global_params_match_the_oracle() {
    let oracle = oracle_ubos();
    let (size, want) = oracle
        .get(&(-1, ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO))
        .expect("the oracle writes global paint params");

    let packed = GlobalPaintParams::for_view(&probe_view(), [64.0, 64.0], 1.0).pack();
    assert_eq!(packed.len(), *size);

    // The dump elides `symbol_fade_change` as dashes, for the reason `ubo_buffers.rs` gives.
    let mine = ubo_blocks(&packed);
    assert_eq!(mine.len(), want.len());
    let mut compared = 0;
    for (got, expected) in mine.iter().zip(want) {
        if expected.contains('-') {
            continue;
        }
        assert_eq!(got, expected);
        compared += 1;
    }
    assert!(compared >= 2, "only {compared} blocks were comparable");
}

/// Every layer's drawable buffer matches: nine tiles of tile matrices, per layer, per sublayer.
///
/// This is where a real cover differs from the hermetic one in the way that matters — nine tiles
/// at zoom 5 rather than six at zoom 4, so the matrices are different numbers arrived at by the
/// same route.
#[test]
fn the_live_drawable_buffers_match_the_oracle() {
    let oracle = oracle_ubos();

    let cases: &[(i32, u32, u32)] = &[
        (
            0,
            ubo_slots::ID_BACKGROUND_DRAWABLE_UBO,
            ubo_layouts::BACKGROUND_DRAWABLE_UNION_UBO.stride,
        ),
        (
            1,
            ubo_slots::ID_FILL_DRAWABLE_UBO,
            ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride,
        ),
        (
            2,
            ubo_slots::ID_FILL_DRAWABLE_UBO,
            ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride,
        ),
    ];

    for (layer, slot, stride) in cases {
        let (size, want) = oracle
            .get(&(*layer, *slot))
            .unwrap_or_else(|| panic!("the oracle writes a drawable buffer for layer {layer}"));

        let packed = ubo::pack_drawable_buffer(&fill_entries(*layer), *stride);

        assert_eq!(packed.len(), *size, "layer {layer} buffer size");
        assert_eq!(ubo_blocks(&packed), *want, "layer {layer} drawable bytes");
    }
}

/// The background layer's colour reaches the uniform the shader reads.
#[test]
fn the_live_background_props_match_the_oracle() {
    let oracle = oracle_ubos();
    let (size, want) = oracle
        .get(&(0, ubo_slots::ID_BACKGROUND_PROPS_UBO))
        .expect("the oracle writes background props");

    let packed = ubo::pack_background_props(Color::parse("#a8c9e0").expect("a color"), 1.0);
    assert_eq!(packed.len(), *size);
    assert_eq!(ubo_blocks(&packed), *want);
}

/// Both fill layers' evaluated properties match, including the outline inheriting the fill.
///
/// Two layers with different colours, which is the check the hermetic style cannot make: one
/// layer proves the packing, two prove the colour is read from the layer rather than from
/// wherever the first one left it.
#[test]
fn the_live_fill_props_match_the_oracle() {
    let oracle = oracle_ubos();
    for (layer, hex) in [(1, "#e9e4d8"), (2, "#a8c9e0")] {
        let (size, want) = oracle
            .get(&(layer, ubo_slots::ID_FILL_EVALUATED_PROPS_UBO))
            .unwrap_or_else(|| panic!("the oracle writes fill props for layer {layer}"));

        // No `fill-outline-color` in this style either, so the outline inherits.
        let fill = Color::parse(hex).expect("a color");
        let packed = ubo::pack_fill_props(fill, fill, 1.0, 1.0, 0.5, 1.0);

        assert_eq!(packed.len(), *size, "layer {layer}");
        assert_eq!(ubo_blocks(&packed), *want, "layer {layer}");
    }
}

/// The fill layers' tile-properties buffers are present, full size and empty.
#[test]
fn the_live_fill_tile_props_are_sized_and_empty() {
    let oracle = oracle_ubos();
    for layer in [1, 2] {
        let (size, want) = oracle
            .get(&(layer, ubo_slots::ID_FILL_TILE_PROPS_UBO))
            .unwrap_or_else(|| panic!("the oracle writes fill tile props for layer {layer}"));

        // One entry per drawable, whatever the layer actually drew.
        let stride = ubo_layouts::FILL_TILE_PROPS_UNION_UBO.stride;
        let packed = ubo::pack_tile_props_buffer(oracle_drawables(layer).len(), stride);

        assert_eq!(packed.len(), *size, "layer {layer}");
        assert!(packed.iter().all(|byte| *byte == 0), "nothing to put in it");
        assert_eq!(ubo_blocks(&packed), *want, "layer {layer}");
    }
}

/// The line layer's drawable buffer matches, `ratio` included.
///
/// A line drawable is not a fill drawable with a different label. It carries a `ratio` — screen
/// pixels per tile unit, inverted — that the line shader needs to keep a width in pixels while
/// the geometry is in tile units, and the first version of this test packed the layer as a fill
/// and produced a buffer identical except for six zeroes where the oracle had 0.0625. Which is
/// what a line rendered at zero width looks like.
#[test]
fn the_live_line_drawable_buffer_matches_the_oracle() {
    let oracle = oracle_ubos();
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_DRAWABLE_UBO))
        .expect("the oracle writes a line drawable buffer");

    let view = probe_view();
    let entries: Vec<LineDrawableEntry> = oracle_drawables(3)
        .into_iter()
        .map(|(z, x, y, wrap, sub)| {
            // `line-width` is a constant here and `line-color` a `match` on a feature property,
            // so neither varies with zoom and every mix factor is zero.
            LineDrawableEntry::for_tile(&view, z, x, y, wrap, 3, sub, [0.0; 6])
                .expect("an unrotated camera")
        })
        .collect();

    let packed =
        ubo::pack_line_drawable_buffer(&entries, ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride);
    assert_eq!(packed.len(), *size);
    assert_eq!(ubo_blocks(&packed), *want);
}

/// The line layer's evaluated properties match, with a data-driven colour left to the binder.
///
/// `line-color` is a `match` on a feature property, so the uniform carries the default rather
/// than a colour: what each feature is actually painted comes through the per-vertex attribute
/// buffer, which `every_layer_matches_the_oracle` covers. The uniform still has to agree, and a
/// data-driven property that wrote a colour here would be a layer painted one colour throughout.
#[test]
fn the_live_line_props_match_the_oracle() {
    let oracle = oracle_ubos();
    let (size, want) = oracle
        .get(&(3, ubo_slots::ID_LINE_EVALUATED_PROPS_UBO))
        .expect("the oracle writes line evaluated props");

    // The spec's defaults for everything the style does not set, and black for the colour it
    // sets data-driven.
    let packed = ubo::pack_line_props(Color::black(), 0.0, 1.0, 0.0, 0.0, 1.0, 1.0);

    assert_eq!(packed.len(), *size);
    assert_eq!(ubo_blocks(&packed), *want);
}
