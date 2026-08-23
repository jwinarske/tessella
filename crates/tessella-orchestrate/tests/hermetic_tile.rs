//! The whole R0 pipeline over the hermetic style, checked against the oracle.
//!
//! Style → filter → project → clip → tessellate → bucket, run for each tile the oracle covers,
//! and compared against what the oracle actually drew there. Every stage has been checked in
//! isolation; this is the first test that says the sequence produces the right thing.
//!
//! What is compared: which layers produce geometry, how many drawables each becomes, and how
//! many vertices land in each tile. Not the vertex *order*, which differs by a rotation whose
//! cause is still open (see `tessella_source::clip`) — but the counts are what the structure of
//! the tile is made of, and they are exact.

use tessella_orchestrate::Content;
use tessella_orchestrate::tile::{TileId, bucket_for, build_tile, drawable_count};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features() -> Vec<tessella_source::GeoJsonFeature> {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("the probe style has one geojson source");
    };
    geojson::read(&source.data).expect("features read")
}

/// The six tiles the oracle covers, from the drawable keys in the golden dump.
const COVER: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

fn build(x: u32, y: u32) -> Vec<tessella_orchestrate::LayerBucket> {
    build_tile(
        &style(),
        TileId::new(13, x, y),
        &features(),
        TilingOptions::default(),
    )
    .expect("tile builds")
}

/// Vertex counts per tile, read out of the golden dump's fill drawables: five where one
/// polygon overlaps the tile, ten where both do.
///
/// This is the assertion that exercises the whole chain. Getting the projection, the clip box,
/// the buffer scale, or the ring handling wrong changes these numbers.
#[test]
fn fill_vertex_counts_match_the_oracle_per_tile() {
    let expected = [
        ((4092, 2723), 5),
        ((4092, 2724), 5),
        ((4093, 2723), 10),
        ((4093, 2724), 10),
        ((4094, 2723), 5),
        ((4094, 2724), 5),
    ];

    for ((x, y), vertices) in expected {
        let buckets = build(x, y);
        let fill = bucket_for(&buckets, "fill-constant")
            .and_then(|b| b.content.as_fill())
            .unwrap_or_else(|| panic!("a fill bucket at {x}/{y}"));
        assert_eq!(
            fill.vertices.len(),
            vertices,
            "tile 13/{x}/{y} should carry {vertices} fill vertices"
        );
    }
}

/// Both fill layers see the same polygons, because their filters are the same. A difference
/// here would mean the filter or the property resolution had leaked into the geometry.
#[test]
fn both_fill_layers_produce_the_same_geometry() {
    for (x, y) in COVER {
        let buckets = build(x, y);
        let constant = bucket_for(&buckets, "fill-constant")
            .and_then(|b| b.content.as_fill())
            .expect("fill-constant");
        let driven = bucket_for(&buckets, "fill-datadriven")
            .and_then(|b| b.content.as_fill())
            .expect("fill-datadriven");
        assert_eq!(constant.vertices, driven.vertices, "at {x}/{y}");
        assert_eq!(constant.indices, driven.indices, "at {x}/{y}");
    }
}

/// The oracle draws 37 drawables over six tiles: one background per tile, two per fill layer
/// per tile, plus the line and circle layers R0 does not implement.
///
/// R0's share is the background and the two fills: `6 * (1 + 2 + 2)` = 30. The remaining seven
/// are the line layer's six and the circle layer's one.
#[test]
fn the_r0_drawable_count_matches_the_oracles_share() {
    let total: usize = COVER
        .iter()
        .map(|(x, y)| drawable_count(&build(*x, *y)))
        .sum();
    assert_eq!(total, 30);

    // And the layers R0 does not implement are absent rather than empty, so nothing claims to
    // have drawn them.
    let buckets = build(4093, 2724);
    assert!(bucket_for(&buckets, "line-datadriven").is_none());
    assert!(bucket_for(&buckets, "circle-constant").is_none());
}

/// A fill layer is two drawables — triangles and outline — which is how the oracle emits it.
/// One per fill layer would be half a layer short and would look right until something set an
/// outline color.
#[test]
fn a_fill_layer_is_two_drawables_and_a_background_is_one() {
    let buckets = build(4093, 2723);
    let background = bucket_for(&buckets, "bg").expect("bg");
    assert_eq!(background.drawable_count(), 1);
    assert_eq!(background.content, Content::Background);

    let fill = bucket_for(&buckets, "fill-constant").expect("fill-constant");
    assert_eq!(fill.drawable_count(), 2);
}

/// Layer index is the position in the style document, not a count of layers that drew. A
/// layer that draws nothing still occupies its index, because the index is what painter order
/// is expressed in.
#[test]
fn layer_indices_are_the_styles_own_order() {
    let buckets = build(4093, 2723);
    let indices: Vec<usize> = buckets.iter().map(|b| b.layer_index).collect();
    assert_eq!(indices, [0, 1, 2], "bg, fill-constant, fill-datadriven");

    // The oracle's keys agree: background is L00000, the fills are L00001 and L00002, and the
    // line and circle layers it does draw are L00003 and L00004 — indices this build skips
    // without renumbering.
    assert_eq!(bucket_for(&buckets, "bg").unwrap().layer_index, 0);
    assert_eq!(
        bucket_for(&buckets, "fill-constant").unwrap().layer_index,
        1
    );
    assert_eq!(
        bucket_for(&buckets, "fill-datadriven").unwrap().layer_index,
        2
    );
}

/// The property bindings survive into the bucket, which is what a binder needs to lay out a
/// vertex. Constant paint is a uniform; the `match` on a feature property is an attribute.
#[test]
fn the_buckets_carry_their_property_bindings() {
    use tessella_style::Binding;

    let buckets = build(4093, 2723);

    let constant = bucket_for(&buckets, "fill-constant").expect("fill-constant");
    assert_eq!(constant.paint["fill-color"].binding, Binding::Uniform);

    let driven = bucket_for(&buckets, "fill-datadriven").expect("fill-datadriven");
    assert_eq!(
        driven.paint["fill-color"].binding,
        Binding::Attribute {
            interpolated: false
        }
    );
}

/// Triangles are well formed everywhere in the cover: a multiple of three, and every index
/// addressing a vertex that exists.
#[test]
fn every_tiles_triangles_are_well_formed() {
    for (x, y) in COVER {
        let buckets = build(x, y);
        let fill = bucket_for(&buckets, "fill-constant")
            .and_then(|b| b.content.as_fill())
            .expect("a fill bucket");

        assert_eq!(fill.indices.len() % 3, 0, "at {x}/{y}");
        assert!(!fill.indices.is_empty(), "at {x}/{y}");
        for segment in &fill.segments {
            let start = segment.index_offset as usize;
            let end = start + segment.index_length as usize;
            for index in &fill.indices[start..end] {
                assert!(
                    u32::from(*index) < segment.vertex_length,
                    "index {index} outside its segment at {x}/{y}"
                );
            }
        }
    }
}

/// A tile the features do not reach still builds, with an empty fill bucket rather than a
/// missing layer. A layer with nothing to draw this frame is not the same as a layer that is
/// not in the style.
#[test]
fn a_tile_outside_the_data_builds_empty_rather_than_failing() {
    let buckets = build_tile(
        &style(),
        TileId::new(13, 0, 0),
        &features(),
        TilingOptions::default(),
    )
    .expect("builds");

    let fill = bucket_for(&buckets, "fill-constant")
        .and_then(|b| b.content.as_fill())
        .expect("the layer is still present");
    assert!(fill.vertices.is_empty());
    assert!(fill.indices.is_empty());

    // Background draws everywhere, including where there is no data.
    assert_eq!(
        bucket_for(&buckets, "bg").map(|b| &b.content),
        Some(&Content::Background)
    );
}
