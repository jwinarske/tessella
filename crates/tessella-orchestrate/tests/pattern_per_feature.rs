//! A pattern that varies with the feature, resolved at bucket build.
//!
//! # What this covers that the encoding tests do not
//!
//! `pattern_uniforms` checks that per-vertex rectangles reach the wire in the shape the capture
//! gives — two attributes, ids 4 and 5, `UShort4` at a stride of eight. It supplies those
//! rectangles itself. This checks the half that produces them: evaluating each feature's pattern
//! expression against the atlas, and filling that feature's own vertices with what it resolved.
//!
//! The distinction matters because a data-driven pattern is the only case where two features in
//! one bucket draw different sprites. Everything else about a pattern — the shaders, the
//! uniforms, the atlas — is identical whether the expression varies with the feature or not, so
//! nothing else in the suite would notice the values being uniform when they should not be.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::TextureId;
use tessella_glyph::atlas::Rect;
use tessella_glyph::sprite::IconPosition;
use tessella_orchestrate::frame::Patterns;
use tessella_orchestrate::tile::{TileId, build_tile_with_patterns};
use tessella_source::geojson::{GeoJsonFeature, Geometry};
use tessella_source::tiling::TilingOptions;
use tessella_style::crossfade::ZoomHistory;
use tessella_style::{Style, Value};

/// Two squares, one of each `kind`, far enough apart to be separate features in one tile.
fn features() -> Vec<GeoJsonFeature> {
    let square = |x: f64| {
        Geometry::Polygon(vec![vec![vec![
            [x, 0.0],
            [x, 0.02],
            [x + 0.02, 0.02],
            [x + 0.02, 0.0],
            [x, 0.0],
        ]]])
    };
    vec![
        GeoJsonFeature {
            id: None,
            properties: BTreeMap::from([("kind".to_owned(), Value::String("a".into()))]),
            geometry: square(0.0),
        },
        GeoJsonFeature {
            id: None,
            properties: BTreeMap::from([("kind".to_owned(), Value::String("b".into()))]),
            geometry: square(0.1),
        },
    ]
}

fn style() -> Style {
    Style::parse(
        r#"{"version": 8,
            "sources": {"probe": {"type": "geojson", "data": {"type": "FeatureCollection",
                                                              "features": []}}},
            "layers": [{"id": "f", "type": "fill", "source": "probe",
                        "paint": {"fill-pattern":
                          ["match", ["get", "kind"], "a", "sand_noise", "grass_pattern"]}}]}"#,
    )
    .expect("the style parses")
}

fn atlas() -> BTreeMap<String, IconPosition> {
    let mut positions = BTreeMap::new();
    // Two sprites at different origins, so which one a feature resolved to is visible in the
    // rectangle rather than inferred. Fifty-pixel sprites, reported as fifty-two.
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

/// Two features, two sprites, and each feature's own vertices carry its own rectangle.
#[test]
fn each_feature_gets_the_sprite_its_expression_names() {
    let positions = atlas();
    let pixels = vec![0u8; 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

    let buckets = build_tile_with_patterns(
        &style(),
        "probe",
        TileId::new(0, 0, 0),
        &features(),
        TilingOptions::default(),
        Some(&patterns),
    )
    .expect("the tile builds");

    let bucket = buckets.first().expect("one layer");
    let vertices = &bucket.pattern_vertices;
    let count = match &bucket.content {
        tessella_orchestrate::Content::Fill(fill) => fill.vertices.len(),
        other => panic!("expected a fill: {other:?}"),
    };
    assert!(
        vertices.covers(count),
        "one pair per vertex: {} pairs for {count} vertices",
        vertices.from.len()
    );

    // `sand_noise` sits at x = 1 and `grass_pattern` at x = 200, so their rectangles begin at
    // 2 and 201. Both must appear, or the expression was evaluated once for the layer rather
    // than once per feature — which is exactly the failure a uniform would be.
    let origins: std::collections::BTreeSet<u16> =
        vertices.from.iter().map(|rect| rect[0]).collect();
    assert_eq!(
        origins,
        std::collections::BTreeSet::from([2, 201]),
        "both sprites should appear, one per feature: {origins:?}"
    );

    // And every rectangle is fifty wide, whichever sprite it is.
    for rect in &vertices.from {
        assert_eq!([rect[2] - rect[0], rect[3] - rect[1]], [50, 50]);
    }
}

/// With no atlas, no rectangles — and the layer still builds.
#[test]
fn no_sprites_means_no_rectangles() {
    let buckets = build_tile_with_patterns(
        &style(),
        "probe",
        TileId::new(0, 0, 0),
        &features(),
        TilingOptions::default(),
        None,
    )
    .expect("the tile builds");

    let bucket = buckets.first().expect("one layer");
    assert!(
        bucket.pattern_vertices.from.is_empty(),
        "nothing to resolve against"
    );
}

/// A pattern naming sprites the atlas does not hold resolves to nothing at all.
///
/// Not to zeroes: zeroes are for a feature that failed among features that did not, so the
/// buffer stays the length the shader reads. When *no* feature resolved there is no pattern on
/// the layer, and writing a buffer of zeroed rectangles would have every vertex sample the
/// atlas's top-left corner.
#[test]
fn an_atlas_without_the_sprites_resolves_to_nothing() {
    let positions = BTreeMap::new();
    let pixels = vec![0u8; 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

    let buckets = build_tile_with_patterns(
        &style(),
        "probe",
        TileId::new(0, 0, 0),
        &features(),
        TilingOptions::default(),
        Some(&patterns),
    )
    .expect("the tile builds");

    assert!(
        buckets
            .first()
            .expect("one layer")
            .pattern_vertices
            .from
            .is_empty()
    );
}
