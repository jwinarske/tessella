//! `["within", geojson]`: whether a feature lies inside a polygon.
//!
//! # The two spaces
//!
//! The polygon is written in longitude and latitude, because that is the only space a style can
//! name. A tile's features are in tile units. mbgl moves the *polygon* -- `latLonToTileCoodinates`
//! in `within.cpp`, once per evaluation -- and so does this, when the caller names a tile. A
//! caller that names none is taken to be in degrees already, which is what the spec's own suite
//! hands it and what these tests use except where they say otherwise.

use tessella_style::Value;
use tessella_style::expression::{Expression, Feature, FeatureGeometry};

struct Shape(Option<FeatureGeometry>, &'static str);

impl Feature for Shape {
    fn property(&self, _key: &str) -> Option<Value> {
        None
    }
    fn geometry_type(&self) -> &str {
        self.1
    }
    fn geometry(&self) -> Option<FeatureGeometry> {
        self.0.clone()
    }
}

/// A square from (0, 0) to (5, 5), as the suite writes one.
fn square() -> Expression {
    let value: Value = serde_json::from_str(
        r#"["within", {"type": "Polygon",
            "coordinates": [[[0, 0], [0, 5], [5, 5], [5, 0], [0, 0]]]}]"#,
    )
    .expect("json");
    Expression::parse(&value).expect("parses")
}

fn holds(expression: &Expression, feature: &dyn Feature) -> bool {
    expression.evaluate(None, Some(feature)) == Ok(Value::Bool(true))
}

fn point(x: f64, y: f64) -> Shape {
    Shape(Some(FeatureGeometry::Points(vec![[x, y]])), "Point")
}

#[test]
fn a_point_inside_is_within_and_one_outside_is_not() {
    let within = square();
    assert!(holds(&within, &point(3.0, 3.0)));
    assert!(!holds(&within, &point(6.0, 6.0)));
}

/// A line wholly inside is within; one that leaves is not, even with both ends inside.
///
/// The second is why the segments are tested against the edges rather than only the endpoints: a
/// concave polygon can hold both ends of a line that passes outside between them. This square is
/// convex, so the case here is the simpler one -- a line with an end outside.
#[test]
fn a_line_is_within_only_if_all_of_it_is() {
    let within = square();
    let inside = Shape(
        Some(FeatureGeometry::Lines(vec![vec![[1.0, 1.0], [4.0, 4.0]]])),
        "LineString",
    );
    let leaves = Shape(
        Some(FeatureGeometry::Lines(vec![vec![[1.0, 1.0], [9.0, 9.0]]])),
        "LineString",
    );
    assert!(holds(&within, &inside));
    assert!(!holds(&within, &leaves));
}

/// A polygon feature is outside whatever it overlaps, which is mbgl's answer rather than a gap.
#[test]
fn a_polygon_feature_is_never_within() {
    let within = square();
    let overlapping = Shape(
        Some(FeatureGeometry::Rings(vec![vec![
            [1.0, 1.0],
            [2.0, 1.0],
            [2.0, 2.0],
            [1.0, 1.0],
        ]])),
        "Polygon",
    );
    assert!(!holds(&within, &overlapping));
}

/// A feature that reports no geometry at all is outside, rather than an error.
#[test]
fn a_feature_with_no_geometry_is_outside() {
    assert!(!holds(&square(), &Shape(None, "Unknown")));
}

/// The argument has to be a polygon. A `LineString` there is a parse error, not a test that
/// nothing passes -- the spec's suite asks for exactly that.
#[test]
fn a_non_polygon_argument_is_a_parse_error() {
    let value: Value = serde_json::from_str(
        r#"["within", {"type": "LineString", "coordinates": [[0, 0], [0, 5], [5, 5]]}]"#,
    )
    .expect("json");
    assert!(Expression::parse(&value).is_err());
}

/// With a tile named, the polygon is converted into that tile's units.
///
/// The check is a round trip rather than a constant: the centre of the tile that contains a
/// point is inside a polygon drawn around that point, and the same feature in tile units is
/// inside it only because the polygon moved.
#[test]
fn a_tile_moves_the_polygon_rather_than_the_feature() {
    let value: Value = serde_json::from_str(
        r#"["within", {"type": "Polygon",
            "coordinates": [[[12.0, 51.0], [12.0, 53.0], [15.0, 53.0], [15.0, 51.0], [12.0, 51.0]]]}]"#,
    )
    .expect("json");
    let within = Expression::parse(&value).expect("parses");

    // Berlin's z14 tile, whose centre is inside that box.
    let tile = (14u8, 8802u32, 5373u32);
    let middle = Shape(
        Some(FeatureGeometry::Points(vec![[4096.0, 4096.0]])),
        "Point",
    );
    assert_eq!(
        within.evaluate_on(None, None, Some(&middle), None, Some(tile)),
        Ok(Value::Bool(true)),
        "the tile's own centre is inside a box drawn around Berlin"
    );
    // And without the tile the same coordinates are degrees, which are nowhere near it.
    assert_eq!(within.evaluate(None, Some(&middle)), Ok(Value::Bool(false)));
}
