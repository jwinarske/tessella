//! A `line-pattern` that varies with the feature, and where its two streams sit (§R3).
//!
//! # What was missing
//!
//! Cross-faded pattern binders were built for fills and only for fills: `PatternVertices` was
//! produced for `LayerKind::Fill` and nothing else, and only the fill attribute ids were ever
//! written. A data-driven `line-pattern` therefore parsed, resolved, chose the pattern shader,
//! bound the atlas — and bound no per-vertex rectangles at all, so every feature in the layer
//! drew whatever the uniform pair happened to say. That is right at an integer zoom and wrong
//! between them, and right for one feature and wrong for the rest, which is the sort of
//! difference that reads as working.
//!
//! # Why the slots came from the oracle
//!
//! Because they are not the fill's, and nothing about reading the binder classes suggests that.
//! A capture with a data-driven `line-pattern` layer binds ids **9 and 10** at bindings **7 and
//! 8**, beside the line's own position and normal at 0 and 1 — where a fill puts the same two
//! streams at ids 4 and 5, bindings 1 and 2. The line shader has already spent its low bindings
//! on colour, blur, opacity, gapwidth, offset and width. Everything else is identical: `UShort4`,
//! stride eight, one pair per vertex.
//!
//! The layer that settles it is `line-pattern-data-driven` in `pattern_style.json`, added for
//! this with a second line feature beside it — one line could not tell a per-vertex stream from
//! a uniform that happened to be right.

use std::collections::{BTreeMap, BTreeSet};

use tessella_capture_abi::envelope::{GeometryId, TextureId};
use tessella_glyph::atlas::Rect;
use tessella_glyph::sprite::IconPosition;
use tessella_orchestrate::emit::{LineDraw, SlabArena, encode_line};
use tessella_orchestrate::frame::Patterns;
use tessella_orchestrate::tile::{TileId, build_tile_with_patterns};
use tessella_capture_abi::BuiltIn;
use tessella_capture_abi::generated::shader_attributes::declared_for;
use tessella_orchestrate::binder::{LINE_FAMILY, attribute_ids, layout, permutation_key};
use tessella_orchestrate::Content;
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_source::tiling::TilingOptions;
use tessella_style::crossfade::ZoomHistory;
use tessella_style::{Style, Value};

/// The oracle's numbers for `line-pattern-data-driven`, read off `pattern_style.dump`.
const FROM_ID: u32 = 9;
const TO_ID: u32 = 10;
const FROM_BINDING: i32 = 7;
const TO_BINDING: i32 = 8;
/// `UShort4`, as the capture's `dt=15`.
const USHORT4: u8 = 15;

/// Two lines, one of each `kind`, far enough apart to be separate features in one tile.
fn features() -> Vec<GeoJsonFeature> {
    // Whole degrees, not fractions of one: at zoom zero a hundredth of a degree is under half a
    // tile unit, so both ends round to the same integer point and the line is dropped as
    // degenerate. The first version of this test lost a feature that way and read as a pattern
    // that had not been resolved per feature.
    let line = |x: f64| Geometry::LineString(vec![vec![[x, 0.0], [x + 4.0, 4.0]]]);
    let mut properties = BTreeMap::new();
    properties.insert("kind".to_owned(), Value::String("a".to_owned()));
    let mut other = BTreeMap::new();
    other.insert("kind".to_owned(), Value::String("b".to_owned()));
    vec![
        GeoJsonFeature {
            id: None,
            properties,
            geometry: line(0.0),
        },
        GeoJsonFeature {
            id: None,
            properties: other,
            geometry: line(20.0),
        },
    ]
}

fn style() -> Style {
    Style::parse(
        r#"{"version": 8,
            "sources": {"probe": {"type": "geojson", "data": {"type": "FeatureCollection",
                                                              "features": []}}},
            "layers": [{"id": "l", "type": "line", "source": "probe",
                        "paint": {"line-width": 8, "line-pattern":
                          ["match", ["get", "kind"], "a", "sand_noise", "grass_pattern"]}}]}"#,
    )
    .expect("the style parses")
}

fn atlas() -> BTreeMap<String, IconPosition> {
    let mut positions = BTreeMap::new();
    // Two sprites at different origins, so which one a feature resolved to is visible in the
    // rectangle rather than inferred.
    for (name, x) in [("sand_noise", 1_u32), ("grass_pattern", 200)] {
        positions.insert(
            name.to_owned(),
            IconPosition {
                padded_rect: Rect {
                    x,
                    y: 1,
                    width: 52,
                    height: 52,
                },
                pixel_ratio: 1.0,
                sdf: false,
                content: None,
                text_fit_width: None,
                text_fit_height: None,
            },
        );
    }
    positions
}

fn build() -> Vec<tessella_orchestrate::tile::LayerBucket> {
    let positions = atlas();
    let pixels = vec![0u8; 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    build_tile_with_patterns(
        &style(),
        "probe",
        TileId::new(0, 0, 0),
        &features(),
        TilingOptions::default(),
        Some(&patterns),
    )
    .expect("the tile builds")
}

/// The layout and permutation key a line bucket binds with, as `frame` builds them.
fn bind(
    bucket: &tessella_orchestrate::tile::LayerBucket,
    shader: BuiltIn,
) -> (tessella_orchestrate::binder::VertexLayout, u64) {
    let ids = attribute_ids(LINE_FAMILY);
    let key = permutation_key(&bucket.paint, &ids);
    let vertex_layout = layout(&bucket.binder, &ids, |attr_id| {
        declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
    });
    (vertex_layout, key)
}

/// Each line's own vertices carry its own rectangle.
#[test]
fn each_line_gets_the_sprite_its_expression_names() {
    let buckets = build();
    let bucket = buckets.first().expect("one layer");
    let count = match &bucket.content {
        Content::Line(line) => line.vertices.len(),
        other => panic!("expected a line: {other:?}"),
    };
    let vertices = &bucket.pattern_vertices;

    assert!(
        vertices.covers(count),
        "one pair per vertex: {} pairs for {count} vertices",
        vertices.from.len()
    );

    // `sand_noise` sits at x = 1 and `grass_pattern` at x = 200, so their rectangles begin at 2
    // and 201. Both must appear, or the expression was evaluated once for the layer rather than
    // once per feature — which is exactly what a uniform would be.
    let origins: BTreeSet<u16> = vertices.from.iter().map(|rect| rect[0]).collect();
    assert_eq!(
        origins,
        BTreeSet::from([2, 201]),
        "both sprites should appear, one per feature: {origins:?}"
    );
}

/// And they reach the wire at the slots the capture puts them at.
#[test]
fn the_streams_land_where_the_oracle_puts_them() {
    let buckets = build();
    let bucket = buckets.first().expect("one layer");
    let Content::Line(line) = &bucket.content else {
        panic!("expected a line")
    };

    let (layout, key) = bind(bucket, BuiltIn::LinePatternShader);
    let mut arena = SlabArena::new();
    let encoded = encode_line(
        &mut arena,
        GeometryId(1),
        line,
        &LineDraw {
            layout: &layout,
            attributes: bucket.binder.data(),
            permutation_key: key,
            pattern_atlas: Some(TextureId(20)),
            pattern_vertices: Some(&bucket.pattern_vertices),
        },
    );

    let attributes = encoded.attributes();
    let found: BTreeMap<u32, &tessella_capture_abi::envelope::AttributeDesc> = attributes
        .iter()
        .map(|attribute| (attribute.attr_id, attribute))
        .collect();

    for (id, binding) in [(FROM_ID, FROM_BINDING), (TO_ID, TO_BINDING)] {
        let attribute = found
            .get(&id)
            .unwrap_or_else(|| panic!("no attribute {id}: the capture binds it"));
        assert_eq!(attribute.binding, binding, "attribute {id} at the wrong slot");
        assert_eq!(attribute.data_type, USHORT4, "attribute {id} is not UShort4");
        assert_eq!(attribute.stride, 8, "attribute {id} at the wrong stride");
        assert_eq!(attribute.offset, 0);
        assert_eq!(attribute.vertex_offset, 0);
    }

    // The line's own two, undisturbed, at the slots they had before any of this.
    assert_eq!(found.get(&0).map(|a| a.binding), Some(0), "position");
    assert_eq!(found.get(&1).map(|a| a.binding), Some(1), "line data");
}

/// A line whose pattern the atlas does not hold binds no rectangles.
#[test]
fn an_unresolved_pattern_binds_nothing() {
    let mut arena = SlabArena::new();
    let buckets = build();
    let bucket = buckets.first().expect("one layer");
    let Content::Line(line) = &bucket.content else {
        panic!("expected a line")
    };
    let (layout, key) = bind(bucket, BuiltIn::LineShader);
    let encoded = encode_line(
        &mut arena,
        GeometryId(1),
        line,
        &LineDraw {
            layout: &layout,
            attributes: bucket.binder.data(),
            permutation_key: key,
            pattern_atlas: None,
            pattern_vertices: None,
        },
    );
    assert!(
        encoded
            .attributes()
            .iter()
            .all(|attribute| attribute.attr_id != FROM_ID && attribute.attr_id != TO_ID),
        "rectangles for a pattern nothing will bind are bytes no shader reads"
    );
}

