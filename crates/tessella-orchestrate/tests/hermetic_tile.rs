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
/// per tile, one per line layer per tile, plus the circle layer this build does not implement.
///
/// This build's share is `6 * (1 + 2 + 2 + 1)` = 36. The one remaining is the circle layer's,
/// which the oracle draws in a single tile.
#[test]
fn the_drawable_count_matches_the_oracles_share() {
    let total: usize = COVER
        .iter()
        .map(|(x, y)| drawable_count(&build(*x, *y)))
        .sum();
    assert_eq!(total, 36);

    // And the layer this build does not implement is absent rather than empty, so nothing
    // claims to have drawn it.
    let buckets = build(4093, 2724);
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
    assert_eq!(
        indices,
        [0, 1, 2, 3],
        "bg, fill-constant, fill-datadriven, line-datadriven"
    );

    // The oracle's keys agree: background is L00000, the fills are L00001 and L00002, the line
    // is L00003, and the circle layer this build skips is L00004 — an index left unoccupied
    // rather than renumbered away.
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

/// Line vertex and index counts per tile, read out of the golden dump's `L00003` drawables.
///
/// These are the assertion that the join and cap decisions are mbgl's. Two vertices are emitted
/// per centreline point and two triangles per segment, so 4/6 is a single segment, 6/12 is two
/// segments joined, and 8/12 is *two separate pieces* of two vertices each — that last one is
/// what says the line clip splits rather than bridging the gap, and it is the number a
/// ring-style clip would get wrong.
#[test]
fn line_vertex_counts_match_the_oracle_per_tile() {
    let expected = [
        ((4092, 2723), (4, 6)),
        ((4092, 2724), (4, 6)),
        ((4093, 2723), (6, 12)),
        ((4093, 2724), (6, 12)),
        ((4094, 2723), (8, 12)),
        ((4094, 2724), (6, 12)),
    ];

    for ((x, y), (vertices, indices)) in expected {
        let buckets = build(x, y);
        let bucket = bucket_for(&buckets, "line-datadriven")
            .and_then(|b| b.content.as_line())
            .unwrap_or_else(|| panic!("tile {x}/{y} has a line bucket"));
        assert_eq!(
            (bucket.vertices.len(), bucket.indices.len()),
            (vertices, indices),
            "tile {x}/{y}"
        );
    }
}

/// FNV-1a 64, the hash the probe uses over a raw buffer.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The line vertex and index buffers are byte-identical to the oracle's.
///
/// This is a stronger claim than anything the fill path can make, and the reason is that
/// `fixupPolygons` — the wagyu union that rotates a fill's rings and costs that path its
/// byte comparison (see `tessella_source::clip`) — runs on polygons only. A LineString reaches
/// the bucket in the order the source wrote it, so the whole chain is comparable: projection,
/// clip, rounding, join selection, extrusion, and the bit-packing of the vertex.
///
/// The hashes are the probe's own, read out of the golden dump, over `count * stride` bytes.
#[test]
fn line_buffers_are_byte_identical_to_the_oracle() {
    let expected = [
        (
            (4092, 2723),
            0xe81e_b541_9b13_9fbdu64,
            0x165c_900a_f128_06ceu64,
        ),
        ((4092, 2724), 0x6cda_c8da_b7de_969d, 0x165c_900a_f128_06ce),
        ((4093, 2723), 0x5ad1_f8be_3e7d_95de, 0x671c_58d9_9b7d_8781),
        ((4093, 2724), 0xdca5_04a7_b256_0d9e, 0x671c_58d9_9b7d_8781),
        ((4094, 2723), 0x1a8d_c925_a30d_d65d, 0xdba4_ba8d_651c_5355),
        ((4094, 2724), 0x95cc_f74d_3b20_b49e, 0x671c_58d9_9b7d_8781),
    ];

    for ((x, y), vertex_hash, index_hash) in expected {
        let buckets = build(x, y);
        let bucket = bucket_for(&buckets, "line-datadriven")
            .and_then(|b| b.content.as_line())
            .unwrap_or_else(|| panic!("tile {x}/{y} has a line bucket"));

        let mut vertex_bytes = Vec::new();
        for v in &bucket.vertices {
            vertex_bytes.extend_from_slice(&v.pos_normal[0].to_le_bytes());
            vertex_bytes.extend_from_slice(&v.pos_normal[1].to_le_bytes());
            vertex_bytes.extend_from_slice(&v.data);
        }
        assert_eq!(fnv1a(&vertex_bytes), vertex_hash, "vertices at {x}/{y}");

        let index_bytes: Vec<u8> = bucket
            .indices
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect();
        assert_eq!(fnv1a(&index_bytes), index_hash, "indices at {x}/{y}");
    }
}

/// A line layer over polygon features draws their outlines.
///
/// mbgl takes the *feature's* geometry type, not the layer's, so a style can stroke a fill
/// without a second source. Dropping polygons here would leave such a layer silently blank —
/// and silently is the problem: the layer is present, its paint resolves, and it draws nothing.
#[test]
fn a_line_layer_strokes_polygon_features() {
    let style = Style::parse(
        r##"{"version": 8,
             "sources": {"probe": {"type": "geojson", "data":
               {"type": "Feature", "properties": {},
                "geometry": {"type": "Polygon", "coordinates":
                  [[[-0.2, 51.45], [-0.05, 51.45], [-0.05, 51.55], [-0.2, 51.55], [-0.2, 51.45]]]}}}},
             "layers": [{"id": "stroke", "type": "line", "source": "probe",
                         "paint": {"line-color": "#ff0000"}}]}"##,
    )
    .expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("one geojson source");
    };
    let features = geojson::read(&source.data).expect("features read");

    let buckets = build_tile(
        &style,
        TileId::new(13, 4093, 2723),
        &features,
        TilingOptions::default(),
    )
    .expect("tile builds");
    let line = bucket_for(&buckets, "stroke")
        .and_then(|b| b.content.as_line())
        .expect("a line bucket");

    assert!(!line.vertices.is_empty(), "the outline is drawn");
    assert_eq!(line.indices.len() % 3, 0, "whole triangles");
    // A closed ring joins at the seam rather than capping, so every vertex is extruded off the
    // centreline; an unextruded pair would mean a cap was emitted where a join belongs.
    assert!(
        line.vertices
            .iter()
            .all(|v| v.data[0] != 128 || v.data[1] != 128),
        "no unextruded vertex"
    );
}
