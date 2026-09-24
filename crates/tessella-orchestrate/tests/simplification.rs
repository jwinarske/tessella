// SPDX-License-Identifier: BSD-2-Clause
//! A GeoJSON feature reaches a tile simplified, as geojson-vt simplifies it.
//!
//! # What it was worth
//!
//! mbgl simplifies GeoJSON geometry when it cuts tiles and this build did not, so a line carried
//! every point its author wrote (tessella#274). On a four-hundred-point line of the shape a route
//! or a GPS track has, over the z13 cover of one camera:
//!
//! | | vertices | indices |
//! |---|---|---|
//! | oracle | 44 | 108 |
//! | before | 2,316 | 6,924 |
//! | after | 44 | 108 |
//!
//! Fifty-three times the vertices, gone, and the picture was already the same: that fixture read
//! 14 differing pixels of 786,432 before and the parity sweep and the 59-camera example corpus are
//! unchanged after. Which is the point -- the tolerance is chosen so the picture does not move, so
//! **gross-pixel parity cannot see this either way**, and only a count can.
//!
//! # The three filters
//!
//! Points go by importance, and whole features go by their own extent: mbgl drops a line shorter
//! than the tolerance and a ring smaller than its square. Those two are easy to miss and are why a
//! dense extract loses small features rather than only losing points.

use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

/// The z13 cover of 51.505/-0.11 at 1024x768.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// A one-layer line style over `coordinates`.
fn line_style(coordinates: &str) -> Style {
    let text = format!(
        r#"{{"version": 8,
             "sources": {{"probe": {{"type": "geojson", "data": {{
               "type": "FeatureCollection", "features": [
                 {{"type": "Feature", "properties": {{}},
                   "geometry": {{"type": "LineString", "coordinates": {coordinates}}}}}]}}}}}},
             "layers": [{{"id": "l", "type": "line", "source": "probe",
                          "paint": {{"line-color": "white", "line-width": 4}}}}]}}"#
    );
    Style::parse(&text).expect("the style parses")
}

/// Line vertex and index totals over the cover.
fn line_totals(style: &Style) -> (usize, usize) {
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let mut totals = (0, 0);
    for (x, y) in TILES {
        let built = build_tile(
            style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        for bucket in &built {
            if let Content::Line(line) = &bucket.content {
                totals.0 += line.vertices.len();
                totals.1 += line.indices.len();
            }
        }
    }
    totals
}

/// A route-shaped line reaches the tile at the oracle's own vertex count.
///
/// The same seeded walk the measurement in #274 used: four hundred points, each a small step east
/// with a little jitter north, which is what a recorded track looks like and what no example in the
/// corpus has.
#[test]
fn a_dense_line_simplifies_to_the_oracles_count() {
    // A tiny deterministic generator, so the fixture is the same on every machine.
    let mut state = 7u64;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        ((state >> 33) as f64 / f64::from(u32::MAX >> 1)) - 1.0
    };
    let (mut lon, mut lat) = (-0.128_f64, 51.503_f64);
    let mut points = Vec::with_capacity(400);
    for _ in 0..400 {
        lon += 0.000_04;
        lat += 0.000_008 + next() * 0.000_003;
        points.push(format!("[{lon:.7},{lat:.7}]"));
    }
    let style = line_style(&format!("[{}]", points.join(",")));

    let (vertices, indices) = line_totals(&style);
    assert!(
        vertices < 200,
        "a four-hundred-point track should not reach the tile whole: {vertices} vertices, where \
         unsimplified it was 2,316 and the oracle spends 44"
    );
    assert!(
        indices < 500,
        "and its indices with it: {indices}, where unsimplified it was 6,924"
    );
}

/// A collinear point is dropped and the endpoints are not.
///
/// One tile unit at z13 is `(360 / 2^13) / 8192` degrees, and the threshold is six of them. So a
/// three-point line kinked by four units keeps two points and one kinked by eight keeps three.
#[test]
fn the_threshold_is_the_oracles_six_tile_units() {
    let unit = (360.0 / f64::from(1u32 << 13)) / 8192.0;
    let totals = |deviation: f64| {
        let offset = deviation * unit;
        line_totals(&line_style(&format!(
            "[[-0.110,51.501],[{:.12},51.505],[-0.110,51.509]]",
            -0.110 + offset
        )))
        .0
    };
    let flat = totals(4.0);
    let kinked = totals(8.0);
    assert!(
        kinked > flat,
        "a kink past the tolerance costs a point where one under it does not: {kinked} against \
         {flat}"
    );
    assert!(flat > 0, "the endpoints always survive: {flat}");
}

/// A line shorter than the tolerance is dropped whole, not shortened.
///
/// mbgl's `if (line.dist > tolerance)`, which is one of the two whole-feature filters.
#[test]
fn a_line_under_the_tolerance_is_dropped_whole() {
    let unit = (360.0 / f64::from(1u32 << 13)) / 8192.0;
    // Two points a single tile unit apart, well under the six-unit tolerance.
    let short = line_style(&format!("[[-0.110,51.505],[{:.12},51.505]]", -0.110 + unit));
    assert_eq!(
        line_totals(&short).0,
        0,
        "a line shorter than the tolerance draws nothing"
    );

    // And one comfortably longer does draw.
    let long = line_style("[[-0.120,51.505],[-0.100,51.505]]");
    assert!(
        line_totals(&long).0 > 0,
        "a line over the tolerance still draws"
    );
}

/// Points are never simplified, which is why they carry no annotation.
#[test]
fn a_point_feature_is_untouched() {
    let style = Style::parse(
        r#"{"version": 8,
            "sources": {"probe": {"type": "geojson", "data": {
              "type": "FeatureCollection", "features": [
                {"type": "Feature", "properties": {},
                 "geometry": {"type": "Point", "coordinates": [-0.110, 51.505]}}]}}},
            "layers": [{"id": "c", "type": "circle", "source": "probe",
                        "paint": {"circle-color": "white", "circle-radius": 6}}]}"#,
    )
    .expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    assert!(
        features[0].simplification.is_empty(),
        "a point has nothing to simplify"
    );

    // Summed over the cover rather than guessed at one tile: 51.505 sits within a whisker of the
    // z13 boundary between y 2723 and 2724, and picking the wrong side measures nothing.
    let quads: usize = TILES
        .iter()
        .map(|&(x, y)| {
            build_tile(
                &style,
                "probe",
                TileId::new(13, x, y),
                &features,
                TilingOptions::default(),
            )
            .expect("the tile builds")
            .iter()
            .filter_map(|bucket| match &bucket.content {
                Content::Circle(circle) => Some(circle.vertices.len()),
                _ => None,
            })
            .sum::<usize>()
        })
        .sum();
    assert_eq!(quads, 4, "one point is one quad, simplification or not");
}
