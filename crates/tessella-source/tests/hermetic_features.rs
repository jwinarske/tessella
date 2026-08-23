//! Reads the hermetic style's inline GeoJSON, and runs the style's own filters over it.
//!
//! The first test that spans crates: the style document, the filter compiler and the feature
//! reader together, on the exact data the oracle runs. Each has been right in isolation; this
//! is where they have to agree on what a feature is.

use tessella_source::{GeoJsonError, Geometry, geojson};
use tessella_style::expression::Feature;
use tessella_style::{Filter, Style, Value};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

fn features() -> Vec<tessella_source::GeoJsonFeature> {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(tessella_style::Source::Geojson(source)) = style.source("probe") else {
        panic!("the probe style has one geojson source");
    };
    geojson::read(&source.data).expect("features read")
}

fn filter_of(layer: &str) -> Filter {
    let style = Style::parse(HERMETIC).expect("style parses");
    let layer = style.layer(layer).expect("layer exists");
    match &layer.filter {
        Some(value) => Filter::parse(value).expect("filter compiles"),
        None => Filter::always(),
    }
}

fn read(json: &str) -> Result<Vec<tessella_source::GeoJsonFeature>, GeoJsonError> {
    let value: Value = serde_json::from_str(json).expect("valid json");
    geojson::read(&value)
}

#[test]
fn reads_the_hermetic_features() {
    let features = features();
    assert_eq!(features.len(), 4);

    let types: Vec<&str> = features.iter().map(|f| f.geometry.type_name()).collect();
    assert_eq!(types, ["Polygon", "Polygon", "LineString", "Point"]);

    let kinds: Vec<Option<Value>> = features.iter().map(|f| f.property("kind")).collect();
    assert_eq!(
        kinds,
        [
            Some(Value::String("a".into())),
            Some(Value::String("b".into())),
            Some(Value::String("a".into())),
            Some(Value::String("b".into())),
        ]
    );
}

/// The style, the filter compiler and the feature reader agreeing on what a feature is. The
/// fill layers filter to Polygons, and there are exactly two.
#[test]
fn the_styles_own_filters_select_the_right_features() {
    let features = features();

    let fills: Vec<_> = features
        .iter()
        .filter(|f| filter_of("fill-constant").matches(*f, None))
        .collect();
    assert_eq!(fills.len(), 2);
    assert!(fills.iter().all(|f| f.geometry.type_name() == "Polygon"));

    let lines: Vec<_> = features
        .iter()
        .filter(|f| filter_of("line-datadriven").matches(*f, None))
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].geometry.type_name(), "LineString");

    let circles: Vec<_> = features
        .iter()
        .filter(|f| filter_of("circle-constant").matches(*f, None))
        .collect();
    assert_eq!(circles.len(), 1);
    assert_eq!(circles[0].geometry.type_name(), "Point");

    // The background layer has no filter and draws from no source at all.
    assert!(features.iter().all(|f| filter_of("bg").matches(f, None)));
}

/// The polygons are the shapes the oracle tessellates, so their rings have to come through
/// intact: closed, four distinct corners, exterior only.
#[test]
fn polygon_rings_survive_intact() {
    let features = features();
    let Geometry::Polygon(polygons) = &features[0].geometry else {
        panic!("the first feature is a polygon");
    };

    assert_eq!(polygons.len(), 1, "one polygon, no holes");
    let ring = &polygons[0][0];
    assert_eq!(ring.len(), 5, "GeoJSON rings repeat their first position");
    assert_eq!(ring[0], ring[4], "and that is what closes them");
    assert_eq!(ring[0], [-0.10, 51.49]);
}

// --- singular and multi are one type ---

/// The style spec has three feature types, not six. A filter asking for `Polygon` must match a
/// MultiPolygon, because mbgl's feature type collapses them — and a reader that kept them
/// distinct would silently drop every multi-geometry from every filtered layer.
#[test]
fn multi_geometries_report_the_singular_type() {
    let point = read(r#"{"type": "Point", "coordinates": [1, 2]}"#).unwrap();
    let multi_point = read(r#"{"type": "MultiPoint", "coordinates": [[1, 2], [3, 4]]}"#).unwrap();
    assert_eq!(point[0].geometry.type_name(), "Point");
    assert_eq!(multi_point[0].geometry.type_name(), "Point");

    let line = read(r#"{"type": "LineString", "coordinates": [[1, 2], [3, 4]]}"#).unwrap();
    let multi_line =
        read(r#"{"type": "MultiLineString", "coordinates": [[[1, 2], [3, 4]]]}"#).unwrap();
    assert_eq!(line[0].geometry.type_name(), "LineString");
    assert_eq!(multi_line[0].geometry.type_name(), "LineString");

    let polygon =
        read(r#"{"type": "Polygon", "coordinates": [[[0,0],[1,0],[1,1],[0,0]]]}"#).unwrap();
    let multi_polygon =
        read(r#"{"type": "MultiPolygon", "coordinates": [[[[0,0],[1,0],[1,1],[0,0]]]]}"#).unwrap();
    assert_eq!(polygon[0].geometry.type_name(), "Polygon");
    assert_eq!(multi_polygon[0].geometry.type_name(), "Polygon");
}

/// And the singular form is normalized into the multi one, so downstream code has a single
/// shape per type rather than two.
#[test]
fn singular_geometries_are_normalized_into_multi() {
    let polygon =
        read(r#"{"type": "Polygon", "coordinates": [[[0,0],[1,0],[1,1],[0,0]]]}"#).unwrap();
    let Geometry::Polygon(polygons) = &polygon[0].geometry else {
        panic!("a polygon");
    };
    assert_eq!(polygons.len(), 1);
    assert_eq!(polygons[0].len(), 1, "one ring");
    assert_eq!(polygons[0][0].len(), 4);

    let multi = read(
        r#"{"type": "MultiPolygon", "coordinates": [
            [[[0,0],[1,0],[1,1],[0,0]]],
            [[[5,5],[6,5],[6,6],[5,5]]]
        ]}"#,
    )
    .unwrap();
    let Geometry::Polygon(polygons) = &multi[0].geometry else {
        panic!("a polygon");
    };
    assert_eq!(polygons.len(), 2);
}

/// Interior rings follow the exterior one, which is the order earcut expects and the order
/// holes are declared in.
#[test]
fn polygon_holes_follow_the_exterior_ring() {
    let with_hole = read(
        r#"{"type": "Polygon", "coordinates": [
            [[0,0],[10,0],[10,10],[0,10],[0,0]],
            [[3,3],[7,3],[7,7],[3,7],[3,3]]
        ]}"#,
    )
    .unwrap();
    let Geometry::Polygon(polygons) = &with_hole[0].geometry else {
        panic!("a polygon");
    };
    assert_eq!(polygons[0].len(), 2, "exterior plus one hole");
    assert_eq!(polygons[0][0][0], [0.0, 0.0]);
    assert_eq!(polygons[0][1][0], [3.0, 3.0]);
}

// --- shapes a style is allowed to write ---

#[test]
fn a_bare_feature_and_a_bare_geometry_both_read() {
    let feature = read(
        r#"{"type": "Feature", "properties": {"a": 1},
            "geometry": {"type": "Point", "coordinates": [1, 2]}}"#,
    )
    .unwrap();
    assert_eq!(feature.len(), 1);
    assert_eq!(feature[0].property("a"), Some(Value::Number(1.0)));

    let geometry = read(r#"{"type": "Point", "coordinates": [1, 2]}"#).unwrap();
    assert_eq!(geometry.len(), 1);
    assert!(geometry[0].properties.is_empty());
    assert_eq!(geometry[0].id(), None);
}

#[test]
fn feature_ids_are_read() {
    let numeric = read(
        r#"{"type": "Feature", "id": 7, "properties": {},
            "geometry": {"type": "Point", "coordinates": [0, 0]}}"#,
    )
    .unwrap();
    assert_eq!(numeric[0].id(), Some(Value::Number(7.0)));

    let textual = read(
        r#"{"type": "Feature", "id": "seven", "properties": {},
            "geometry": {"type": "Point", "coordinates": [0, 0]}}"#,
    )
    .unwrap();
    assert_eq!(textual[0].id(), Some(Value::String("seven".into())));
}

/// `properties` is required by the spec but nullable, and styles write it both ways.
#[test]
fn null_or_absent_properties_are_an_empty_set() {
    for json in [
        r#"{"type": "Feature", "properties": null, "geometry": {"type": "Point", "coordinates": [0,0]}}"#,
        r#"{"type": "Feature", "geometry": {"type": "Point", "coordinates": [0,0]}}"#,
    ] {
        let feature = read(json).unwrap();
        assert!(feature[0].properties.is_empty());
        assert_eq!(feature[0].property("anything"), None);
    }
}

/// Elevation is legal and unread. Dropping it keeps every position two wide, rather than
/// carrying a third value to the GPU that nothing consumes.
#[test]
fn elevation_is_accepted_and_dropped() {
    let feature = read(r#"{"type": "Point", "coordinates": [1, 2, 300]}"#).unwrap();
    let Geometry::Point(points) = &feature[0].geometry else {
        panic!("a point");
    };
    assert_eq!(points[0], [1.0, 2.0]);
}

// --- rejections ---

/// A null geometry is legal GeoJSON and means a feature with no location. Nothing can be drawn
/// from it, so it is refused rather than carried as an empty shape that silently contributes
/// no vertices later.
#[test]
fn a_null_geometry_is_refused() {
    assert!(
        read(r#"{"type": "Feature", "properties": {}, "geometry": null}"#).is_err(),
        "a feature with no location cannot be drawn"
    );
}

/// GeometryCollection is not carried into tiles by mbgl either. Refusing it by name beats
/// dropping its contents without saying so.
#[test]
fn geometry_collection_is_refused_by_name() {
    let error = read(
        r#"{"type": "GeometryCollection", "geometries": [
            {"type": "Point", "coordinates": [0, 0]}
        ]}"#,
    )
    .expect_err("not implemented");
    assert!(format!("{error}").contains("GeometryCollection"), "{error}");
}

#[test]
fn malformed_coordinates_are_reported() {
    for (json, what) in [
        (
            r#"{"type": "Point", "coordinates": [1]}"#,
            "a one-element position",
        ),
        (
            r#"{"type": "Point", "coordinates": ["a", "b"]}"#,
            "non-numeric",
        ),
        (r#"{"type": "Point", "coordinates": 5}"#, "not an array"),
        (
            r#"{"type": "LineString", "coordinates": [[1, 2], 5]}"#,
            "a non-position",
        ),
        (r#"{"coordinates": [1, 2]}"#, "no type"),
    ] {
        assert!(read(json).is_err(), "should reject {what}");
    }
}
