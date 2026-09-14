//! A heatmap layer's tiles, and the view its drawables bind into.
//!
//! The geometry is the circle bucket's, already byte-checked in `heatmap_uniforms.rs` against
//! the oracle's segment lengths. What this test is for is the part that has no counterpart in
//! any other layer: a heatmap's kernels are bound into an *offscreen* view of their own
//! (DR-25), and the view they are not bound into is the one the map is drawn in.
//!
//! Getting that wrong is not subtle in the frame and is very subtle in the code. The kernels
//! would draw straight onto the map — full-intensity white quads over the basemap, never
//! reaching the color ramp — and every count, every buffer and every uniform in the stream
//! would still be right.

use tessella_capture_abi::envelope::{DrawFlags, ViewId};
use tessella_orchestrate::order::bindings_for;
use tessella_orchestrate::tile::{TileId, bucket_for, build_tile};
use tessella_orchestrate::view;
use tessella_orchestrate::{Content, LayerBucket};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const HEATMAP: &str = include_str!("../../tessella-style/tests/heatmap_style.json");
const DUMP: &str = include_str!("../../../tests/golden/heatmap_style.dump");

const VIEW: ViewId = ViewId(0);

fn style() -> Style {
    Style::parse(HEATMAP).expect("style parses")
}

fn build(x: u32, y: u32) -> Vec<LayerBucket> {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("the heatmap style has one geojson source");
    };
    let features = geojson::read(&source.data).expect("features read");
    build_tile(
        &style,
        "probe",
        TileId::new(13, x, y),
        &features,
        TilingOptions::default(),
    )
    .expect("tile builds")
}

/// The tiles the oracle draws heatmap kernels in, and how many vertices each holds.
///
/// `sh0020` is the kernel shader; its `seg` lines carry the counts. Two layers over the same
/// eight points, so each tile appears twice and the pair agrees.
fn oracle_kernels() -> Vec<((u32, u32), u32)> {
    let mut out = Vec::new();
    for line in DUMP.lines() {
        if !line.starts_with("  seg ") || !line.contains(".sh0020.") {
            continue;
        }
        let key = line.split_whitespace().nth(1).expect("a key");
        let tile = key.split(".t13_").nth(1).expect("a tile");
        let mut parts = tile.split('_');
        let x: u32 = parts.next().expect("x").parse().expect("an x");
        let y: u32 = parts.next().expect("y").parse().expect("a y");
        let vlen: u32 = line
            .split_whitespace()
            .find_map(|field| field.strip_prefix("vlen="))
            .expect("a vlen")
            .parse()
            .expect("a vertex count");
        out.push(((x, y), vlen));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The build puts kernels in the tiles the oracle does, with the vertex counts it has.
#[test]
fn the_kernel_tiles_and_counts_match_the_oracle() {
    let want = oracle_kernels();
    assert!(!want.is_empty(), "the golden carries kernel segments");

    for ((x, y), vlen) in want {
        let buckets = build(x, y);
        let bucket = bucket_for(&buckets, "heatmap-constant").expect("the constant layer");
        let Content::Heatmap(ref heatmap) = bucket.content else {
            panic!("a heatmap layer builds a heatmap bucket");
        };
        assert_eq!(
            heatmap.vertices.len() as u32,
            vlen,
            "tile {x}/{y} holds the oracle's vertex count"
        );
        assert_eq!(heatmap.indices.len() as u32, vlen / 4 * 6);
    }
}

/// A tile with no points in it builds an empty bucket and binds nothing, the way a circle does.
#[test]
fn a_tile_without_points_binds_nothing() {
    let buckets = build(4092, 2723);
    let bucket = bucket_for(&buckets, "heatmap-constant").expect("the constant layer");
    assert!(!bucket.content.has_data(), "no points fall in this tile");

    let mut next_id = 1;
    let bindings = bindings_for(
        VIEW,
        tessella_orchestrate::order::tile_of(13, 4092, 2723),
        &buckets,
        &mut next_id,
        false,
    );
    assert!(
        bindings.iter().all(|binding| binding.view == VIEW),
        "an empty heatmap bucket produces no binding at all"
    );
}

/// The kernels bind into the layer's offscreen view, and nothing binds them into the map's.
///
/// This is the assertion the whole test exists for.
#[test]
fn the_kernels_bind_into_the_offscreen_view() {
    let buckets = build(4093, 2724);
    let mut next_id = 1;
    let bindings = bindings_for(
        VIEW,
        tessella_orchestrate::order::tile_of(13, 4093, 2724),
        &buckets,
        &mut next_id,
        false,
    );

    let offscreen: Vec<_> = bindings
        .iter()
        .filter(|binding| view::is_offscreen(binding.view))
        .collect();
    assert_eq!(offscreen.len(), 2, "one per heatmap layer");

    for binding in &offscreen {
        assert_eq!(
            binding.view,
            view::offscreen_view(VIEW, binding.layer_index as u32).expect("encodes"),
            "the id is derived from the layer it draws"
        );
        // Color and nothing else: the oracle's `flags=0001`.
        assert_eq!(binding.flags, DrawFlags::ENABLE_COLOR);
        assert_eq!(binding.sub_layer_index, 0);
    }

    // And the map's own view carries the background and no kernels.
    let onscreen: Vec<_> = bindings
        .iter()
        .filter(|binding| !view::is_offscreen(binding.view))
        .collect();
    assert!(
        onscreen.iter().all(|binding| binding.layer_index == 0),
        "only the background draws into the map's view"
    );
}
