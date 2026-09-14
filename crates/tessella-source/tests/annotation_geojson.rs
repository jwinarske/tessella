//! Reading the parity scene's own GeoJSON as annotations.
//!
//! The file this reads is the file the oracle is handed. `mbgl-render --annotations` applies the
//! same rule -- geometry type picks the class -- so a difference between the two readers is a
//! difference the parity number would report with nothing saying where it came from.

use tessella_source::annotation::{self, Annotations, ShapeKind};
use tessella_style::Value;

const SCENE: &str = include_str!("../../../tools/parity/scenes/annot_p.geojson");

fn scene() -> Annotations {
    let document: Value = serde_json::from_str(SCENE).expect("the scene is JSON");
    annotation::from_geojson(&document).expect("the scene reads as annotations")
}

/// Seven features: three symbols, two lines, two fills.
#[test]
fn the_scene_reads_as_the_classes_it_names() {
    let annotations = scene();

    let shapes: Vec<ShapeKind> = annotations.shapes().map(|(_, shape)| shape.kind).collect();
    assert_eq!(
        shapes,
        [
            ShapeKind::Line,
            ShapeKind::Line,
            ShapeKind::Fill,
            ShapeKind::Fill
        ]
    );

    // The three symbols are the ids the shapes are not, and ids count up in file order.
    let mut style =
        tessella_style::Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("a style");
    annotations.synthesize(&mut style);
    let ids: Vec<&str> = style.layers.iter().map(|layer| layer.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "org.maplibre.annotations.shape.3",
            "org.maplibre.annotations.shape.4",
            "org.maplibre.annotations.shape.5",
            "org.maplibre.annotations.shape.6",
            annotation::POINT_LAYER_ID,
        ]
    );
}

/// Paint written in the file reaches the synthesized layer, and paint left out stays out.
#[test]
fn the_scenes_paint_reaches_its_layers() {
    let annotations = scene();
    let mut style =
        tessella_style::Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("a style");
    annotations.synthesize(&mut style);

    let line = style
        .layer("org.maplibre.annotations.shape.3")
        .expect("the first line");
    assert_eq!(
        line.paint
            .get("line-color")
            .and_then(|value| value.as_literal().and_then(Value::as_str)),
        Some("#ff9c00")
    );
    assert_eq!(
        line.paint
            .get("line-width")
            .and_then(|value| value.as_literal().and_then(Value::as_number)),
        Some(6.0)
    );
    // A line annotation has no outline, and nothing invented one.
    assert!(!line.paint.contains_key("fill-outline-color"));

    let fill = style
        .layer("org.maplibre.annotations.shape.5")
        .expect("the polygon with a hole");
    assert_eq!(
        fill.paint
            .get("fill-opacity")
            .and_then(|value| value.as_literal().and_then(Value::as_number)),
        Some(0.45)
    );
    assert_eq!(
        fill.paint
            .get("fill-outline-color")
            .and_then(|value| value.as_literal().and_then(Value::as_str)),
        Some("#ffffff")
    );

    // The last fill is a multi-polygon with no outline written, so the property is absent and the
    // spec's default applies -- which is the annotation class's default too.
    let multi = style
        .layer("org.maplibre.annotations.shape.6")
        .expect("the multi-polygon");
    assert!(!multi.paint.contains_key("fill-outline-color"));
}

/// The polygon with a hole keeps both rings, and every ring is closed.
#[test]
fn a_polygons_rings_survive_and_are_closed() {
    let annotations = scene();
    let (_, shape) = annotations
        .shapes()
        .find(|(id, _)| *id == 5)
        .expect("the polygon with a hole");

    let annotation::ShapeGeometry::Polygons(polygons) = &shape.geometry else {
        panic!("a fill annotation holds polygons");
    };
    assert_eq!(polygons.len(), 1);
    assert_eq!(polygons[0].len(), 2, "an exterior ring and one hole");
    for ring in &polygons[0] {
        assert_eq!(ring.first(), ring.last(), "construction closes every ring");
    }
}

/// A multi-point is skipped, because no annotation class holds one and the oracle's reader drops
/// it. Accepting it would draw markers the oracle does not and the parity number would say so
/// without saying why.
#[test]
fn a_multi_point_is_skipped() {
    let document: Value = serde_json::from_str(
        r#"{"type":"FeatureCollection","features":[
             {"type":"Feature","properties":{},
              "geometry":{"type":"MultiPoint","coordinates":[[0,0],[1,1]]}},
             {"type":"Feature","properties":{},
              "geometry":{"type":"Point","coordinates":[2,2]}}]}"#,
    )
    .expect("JSON");
    let annotations = annotation::from_geojson(&document).expect("reads");

    let mut style =
        tessella_style::Style::parse(r#"{"version":8,"sources":{},"layers":[]}"#).expect("a style");
    annotations.synthesize(&mut style);
    // One point layer and no shape layers: the multi-point contributed nothing.
    assert_eq!(style.layers.len(), 1);

    let tile = annotations.tile(0, 0, 0).expect("a tile");
    assert_eq!(tile.layers[0].len(), 1, "only the single point drew");
}

/// A store read from the scene is prepared, so the first tile it cuts uses the index.
#[test]
fn reading_leaves_the_store_prepared() {
    assert!(scene().is_prepared());
}
