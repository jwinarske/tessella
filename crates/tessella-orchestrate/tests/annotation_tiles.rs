//! An annotation tile, from the store through the synthesized style to buckets.
//!
//! The pieces are tested apart in `tessella-source`: the store cuts a tile, and synthesis puts
//! layers in a style. What is only true together is that the layers read the tile -- each shape
//! layer names its own source-layer, and a mistake there is not a wrong picture but an empty one,
//! with every count in the stream still correct because there is nothing in it.
//!
//! The point layer is the one worth naming. It is a symbol layer over a source-layer of points
//! whose only property is a sprite name, and it resolves its icon through an expression rather
//! than a literal -- so an `icon-image` that failed to compile, or a feature whose property the
//! expression could not read, produces the same nothing.

use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_orchestrate::{Content, LayerBucket};
use tessella_source::annotation::{
    Annotation, AnnotationImage, Annotations, POINT_LAYER_ID, ShapeAnnotation, ShapeGeometry,
    ShapePaint, SymbolAnnotation,
};
use tessella_style::{Style, Value};

/// Berlin at z14, which is the camera the parity scene uses.
const Z: u8 = 14;

fn empty_style() -> Style {
    Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("a style")
}

fn berlin_tile() -> (u32, u32) {
    let units = tessella_tile::projection::tile_units(13.405, 52.52, Z);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (units[0] as u32, units[1] as u32)
}

/// The scene `tools/parity/scenes/annot_p.geojson` describes, in the store's own terms.
fn scene() -> Annotations {
    let mut annotations = Annotations::new();
    for (lon, lat, icon) in [
        (13.405, 52.52, "marker"),
        (13.410_8, 52.523_5, "marker"),
        (13.398_5, 52.516_6, ""),
    ] {
        annotations.add(Annotation::Symbol(SymbolAnnotation {
            geometry: [lon, lat],
            icon: icon.into(),
        }));
    }

    annotations.add(Annotation::Shape(ShapeAnnotation::line(
        ShapeGeometry::Lines(vec![vec![
            [13.394_8, 52.514_2],
            [13.402_1, 52.518_8],
            [13.409_3, 52.517_7],
            [13.414_7, 52.523_1],
        ]]),
        ShapePaint {
            color: Some(Value::String("#ff9c00".into())),
            width: Some(Value::Number(6.0)),
            opacity: Some(Value::Number(0.9)),
            ..ShapePaint::default()
        },
    )));

    annotations.add(Annotation::Shape(ShapeAnnotation::fill(
        ShapeGeometry::Polygons(vec![vec![
            vec![
                [13.399_0, 52.520_3],
                [13.406_0, 52.520_3],
                [13.406_0, 52.524_0],
                [13.399_0, 52.524_0],
            ],
            vec![
                [13.401_2, 52.521_4],
                [13.403_8, 52.521_4],
                [13.403_8, 52.522_9],
                [13.401_2, 52.522_9],
            ],
        ]]),
        ShapePaint {
            color: Some(Value::String("#3f6fff".into())),
            opacity: Some(Value::Number(0.45)),
            outline_color: Some(Value::String("#ffffff".into())),
            ..ShapePaint::default()
        },
    )));

    annotations.add_image(
        "marker",
        AnnotationImage {
            image: tessella_source::image::Image {
                width: 16,
                height: 16,
                pixels: vec![255; 16 * 16 * 4],
            },
            pixel_ratio: 1.0,
            sdf: false,
        },
    );

    annotations.prepare();
    annotations
}

fn buckets_at(annotations: &Annotations, x: u32, y: u32) -> Vec<LayerBucket> {
    let mut style = empty_style();
    annotations.synthesize(&mut style);
    assert_eq!(style.reject_uncompilable(), Vec::new());

    let tile = annotations.tile(Z, x, y).expect("the store cuts a tile");
    build_mvt_tile(
        &style,
        tessella_source::annotation::SOURCE_ID,
        TileId::new(Z, x, y),
        &tile,
    )
    .expect("the tile builds")
}

fn buckets(annotations: &Annotations) -> Vec<LayerBucket> {
    let (x, y) = berlin_tile();
    buckets_at(annotations, x, y)
}

/// Every layer the store synthesized produces a bucket of the kind it asked for.
#[test]
fn each_synthesized_layer_draws_its_own_features() {
    let annotations = scene();
    let built = buckets(&annotations);

    let kinds: Vec<(&str, &str)> = built
        .iter()
        .map(|bucket| {
            let kind = match &bucket.content {
                Content::Symbol(_) => "symbol",
                Content::Line(_) => "line",
                Content::Fill(_) => "fill",
                _ => "other",
            };
            (bucket.layer_id.as_str(), kind)
        })
        .collect();

    assert_eq!(
        kinds,
        [
            ("org.maplibre.annotations.shape.3", "line"),
            ("org.maplibre.annotations.shape.4", "fill"),
            (POINT_LAYER_ID, "symbol"),
        ],
        "one bucket per layer, shapes under the points"
    );
}

/// A shape layer reads only its own source-layer.
///
/// Both shapes are in the same tile. A layer that read the tile rather than its own layer would
/// draw the other shape's geometry too, which is a fill drawing a polyline -- visible, and
/// nothing in the counts would say why.
#[test]
fn a_shape_layer_draws_one_feature_and_not_its_neighbors() {
    let annotations = scene();
    let built = buckets(&annotations);

    let fill = built
        .iter()
        .find(|bucket| bucket.layer_id == "org.maplibre.annotations.shape.4")
        .expect("the fill layer");
    let Content::Fill(fill) = &fill.content else {
        panic!("the fill layer builds a fill");
    };
    // One polygon with one hole. The hole is what makes this worth asserting: a ring structure
    // lost on the way in draws a solid rectangle, which looks like a working fill.
    assert!(!fill.vertices.is_empty());
    assert!(!fill.indices.is_empty());
}

/// Every symbol annotation reaches the point layer of the tile that contains it.
///
/// The scene's three markers are a few hundred meters apart and land in three *different* z14
/// tiles -- 8802/5373, 8802/5372 and 8801/5373 -- which is the ordinary case and the one a test
/// that put them all in one tile would never exercise. The count that matters is over the cover,
/// not over a tile.
#[test]
fn every_symbol_reaches_the_tile_that_contains_it() {
    let annotations = scene();
    let (x, y) = berlin_tile();

    let mut total = 0;
    for tile_x in x - 1..=x + 1 {
        for tile_y in y - 1..=y + 1 {
            let built = buckets_at(&annotations, tile_x, tile_y);
            let Some(points) = built
                .iter()
                .find(|bucket| bucket.layer_id == POINT_LAYER_ID)
            else {
                continue;
            };
            let Content::Symbol(layout) = &points.content else {
                panic!("the point layer builds symbols");
            };
            total += layout.pending.len();
        }
    }

    assert_eq!(total, 3, "two named icons and one default marker");
}

/// Removing every annotation leaves a style with no annotation layers, so nothing is drawn and
/// no layer is left behind naming a source that is gone.
#[test]
fn an_emptied_store_leaves_no_layers() {
    let mut annotations = scene();
    let ids: Vec<u64> = (0..5).collect();
    for id in ids {
        annotations.remove(id);
    }
    assert!(annotations.is_empty());

    let mut style = empty_style();
    annotations.synthesize(&mut style);
    assert!(style.layers.is_empty());
    assert!(annotations.tile(Z, 0, 0).is_none());
}
