// SPDX-License-Identifier: BSD-2-Clause
//! GeoJSON past the antimeridian, carried into this world the way geojson-vt's `wrap` carries it.
//!
//! display-line-that-crosses-180th-meridian draws a route from Peru west to Tokyo written as
//! longitude -219.7 rather than 140.3. mbgl draws all of it in every world copy; the tile builder
//! projected it straight into the tile and clipped away everything west of -180, so the route
//! stopped at the meridian and started again nowhere.

use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::{EXTENT, TilingOptions};
use tessella_style::{Source, Style};

/// Builds `tile` from a style with one GeoJSON source and one layer of `kind` over it.
fn build(data: &str, kind: &str, tile: TileId) -> Content {
    let style: Style = serde_json::from_str(&format!(
        r#"{{"version": 8,
            "sources": {{"data": {{"type": "geojson", "data": {data}}}}},
            "layers": [{{"id": "layer", "type": "{kind}", "source": "data"}}]}}"#
    ))
    .expect("a style");
    let Some(Source::Geojson(source)) = style.source("data") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");
    let mut buckets = build_tile(&style, "data", tile, &features, TilingOptions::default())
        .expect("the tile builds");
    buckets.remove(0).content
}

/// The route's western end is past -180. Its piece from the meridian to Tokyo belongs at the
/// eastern side of the world's only zoom-0 tile.
#[test]
fn a_line_past_the_antimeridian_draws_in_this_world_too() {
    let route = r#"{"type": "Feature", "properties": {},
        "geometry": {"type": "LineString", "coordinates": [[-72.42187, -16.59408], [-219.72657, 35.67514]]}}"#;
    let Content::Line(line) = build(route, "line", TileId::new(0, 0, 0)) else {
        panic!("a line bucket");
    };
    let east = line
        .vertices
        .iter()
        .map(|vertex| vertex.pos_normal[0] >> 1)
        .max()
        .expect("vertices");
    // Tokyo is at 140.3 degrees, seven eighths of the way across; without the copy the line
    // ended at Peru's side of the tile, a third of the way.
    assert!(
        i32::from(east) > EXTENT * 3 / 4,
        "the line reaches only {east} of {EXTENT}"
    );
}

/// A point at longitude 190 is the point at -170.
#[test]
fn a_point_past_the_antimeridian_draws_where_it_wraps_to() {
    let point = r#"{"type": "Feature", "properties": {},
        "geometry": {"type": "Point", "coordinates": [190, 0]}}"#;
    let Content::Circle(circle) = build(point, "circle", TileId::new(0, 0, 0)) else {
        panic!("a circle bucket");
    };
    let xs: Vec<i16> = circle
        .vertices
        .iter()
        .map(|vertex| vertex[0] >> 1)
        .collect();
    // Ten degrees east of the western edge: 10 / 360 of the tile.
    let expected = (f64::from(EXTENT) * 10.0 / 360.0).round();
    assert_eq!(xs.len(), 4, "one circle: {xs:?}");
    assert!(
        xs.iter().all(|&x| f64::from(x) == expected),
        "{xs:?} is not at {expected}"
    );
}

/// Away from the antimeridian nothing is copied: one point is still one circle.
#[test]
fn a_feature_inside_the_world_is_drawn_once() {
    let point = r#"{"type": "Feature", "properties": {},
        "geometry": {"type": "Point", "coordinates": [-45, 30]}}"#;
    let Content::Circle(circle) = build(point, "circle", TileId::new(2, 1, 1)) else {
        panic!("a circle bucket");
    };
    assert_eq!(circle.vertices.len(), 4);
}
