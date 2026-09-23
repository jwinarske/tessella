// SPDX-License-Identifier: BSD-2-Clause
//! `fill-sort-key`, `line-sort-key` and `circle-sort-key` reorder a layer's features.
//!
//! # What the oracle does
//!
//! mbgl permutes the *vertex buffer* at layout time, not the segments at draw time. Settled with
//! `mbgl-capture-probe --dump-vertices` on two squares whose keys reverse their source order,
//! tile 13/4093/2723:
//!
//! ```text
//! no key:   (2206,10240) (2206,9612) (2952,9612) (2952,10240) (2206,10240)  <- feature 0, key 10
//!           (3698,9013)  (3698,7815) (4443,7815) (4443,9013)  (3698,9013)   <- feature 1, key 1
//! with key: (3698,9013)  (3698,7815) (4443,7815) (4443,9013)  (3698,9013)   <- the lower key first
//!           (2206,10240) (2206,9612) (2952,9612) (2952,10240) (2206,10240)
//! ```
//!
//! Same coordinates, permuted. `mbgl-render` agrees on what that means: the overlap of two
//! opaque squares reads `(0,0,255)` without the key and `(255,0,0)` with it, so the highest key
//! is laid out last and draws on top.
//!
//! Checked for determinism first, because a capture diff is worthless otherwise -- three
//! consecutive runs of the unsorted style are byte-identical.
//!
//! # Why it was missed
//!
//! The property was declared in `FILL_LAYOUT` and read by nothing. No fixture in the tree set it,
//! so no camera in the parity sweep could see it -- the sweep tops out at 0.004% and this was
//! never in it. It is the kind of gap a property table makes easy: the name is present, the
//! parse test passes, and the behavior is absent.
//!
//! # What is deliberately *not* sorted
//!
//! `fill-extrusion` shares the fill arm but mbgl declares no `fill-extrusion-sort-key`, so an
//! extrusion keeps source order whatever the fill beside it does. `symbol-sort-key` is applied by
//! `SymbolLayout` instead, because it orders placement rather than the vertex buffer.

use std::sync::Arc;

use tessella_orchestrate::Content;
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

/// The tile the oracle's two squares land in.
const TILE: (u8, u32, u32) = (13, 4093, 2723);

/// Two squares, the first with the higher key, as the oracle capture used.
const SQUARES: &str = r#"[
  { "type": "Feature", "properties": { "k": 10 },
    "geometry": { "type": "Polygon", "coordinates":
      [[[-0.120,51.500],[-0.116,51.500],[-0.116,51.504],[-0.120,51.504],[-0.120,51.500]]] } },
  { "type": "Feature", "properties": { "k": 1 },
    "geometry": { "type": "Polygon", "coordinates":
      [[[-0.112,51.506],[-0.108,51.506],[-0.108,51.510],[-0.112,51.510],[-0.112,51.506]]] } }
]"#;

/// A one-layer style over `features`, with `layout` spliced into the layer.
fn style_of(kind: &str, features: &str, layout: &str, extra_paint: &str) -> Style {
    let text = format!(
        r##"{{
 "version": 8,
 "sources": {{ "probe": {{ "type": "geojson", "data": {{
   "type": "FeatureCollection", "features": {features} }} }} }},
 "layers": [
  {{ "id": "sorted", "type": "{kind}", "source": "probe", {layout}
     "paint": {{ {extra_paint} }} }}
 ]
}}"##
    );
    Style::parse(&text).expect("the style parses")
}

/// The x of every vertex the layer built, in buffer order.
fn xs(style: &Style) -> Vec<i16> {
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let (z, x, y) = TILE;
    let buckets = build_tile(
        style,
        "probe",
        TileId::new(z, x, y),
        &features,
        TilingOptions::default(),
    )
    .expect("the tile builds");

    for bucket in &buckets {
        match &bucket.content {
            Content::Fill(fill) => return fill.vertices.iter().map(|v| v[0]).collect(),
            Content::Fill3d(ext) => return ext.vertices.iter().map(|v| v.position[0]).collect(),
            Content::Line(line) => return line.vertices.iter().map(|v| v.pos_normal[0]).collect(),
            Content::Circle(circle) => return circle.vertices.iter().map(|v| v[0]).collect(),
            _ => {}
        }
    }
    panic!("the layer built no geometry")
}

/// The oracle's permutation, vertex for vertex.
///
/// Not just "the order changed": these are the coordinates `--dump-vertices` printed, so a
/// reordering that got the direction backwards would still fail.
#[test]
fn a_sort_key_reorders_a_fill_the_way_the_oracle_does() {
    let unsorted = xs(&style_of("fill", SQUARES, "", r#""fill-color": "white""#));
    assert_eq!(
        unsorted,
        [2206, 2206, 2952, 2952, 2206, 3698, 3698, 4443, 4443, 3698],
        "without a key the source order stands"
    );

    let sorted = xs(&style_of(
        "fill",
        SQUARES,
        r#""layout": { "fill-sort-key": ["get", "k"] },"#,
        r#""fill-color": "white""#,
    ));
    assert_eq!(
        sorted,
        [3698, 3698, 4443, 4443, 3698, 2206, 2206, 2952, 2952, 2206],
        "the lower key lays out first, which is the oracle's buffer"
    );
}

/// Ascending: the highest key is laid out last, so it draws on top.
///
/// Stated separately from the permutation above because that one would also pass if both this
/// build and the recorded oracle had the direction wrong together.
#[test]
fn the_highest_key_is_laid_out_last() {
    let sorted = xs(&style_of(
        "fill",
        SQUARES,
        r#""layout": { "fill-sort-key": ["get", "k"] },"#,
        r#""fill-color": "white""#,
    ));
    // The square with key 10 starts at x 2206; it must be the one at the end of the buffer.
    assert_eq!(sorted.last(), Some(&2206), "key 10 draws last: {sorted:?}");
}

/// Equal keys keep the order the source gave them.
#[test]
fn equal_keys_keep_source_order() {
    let tied = SQUARES
        .replace("\"k\": 10", "\"k\": 5")
        .replace("\"k\": 1 ", "\"k\": 5 ");
    let sorted = xs(&style_of(
        "fill",
        &tied,
        r#""layout": { "fill-sort-key": ["get", "k"] },"#,
        r#""fill-color": "white""#,
    ));
    assert_eq!(
        sorted,
        [2206, 2206, 2952, 2952, 2206, 3698, 3698, 4443, 4443, 3698],
        "a stable sort leaves a tie alone: {sorted:?}"
    );
}

/// A constant sort key is not a reason to permute anything.
#[test]
fn a_constant_key_is_still_source_order() {
    let sorted = xs(&style_of(
        "fill",
        SQUARES,
        r#""layout": { "fill-sort-key": 3 },"#,
        r#""fill-color": "white""#,
    ));
    assert_eq!(
        sorted,
        [2206, 2206, 2952, 2952, 2206, 3698, 3698, 4443, 4443, 3698],
        "every feature keys the same, so nothing moves: {sorted:?}"
    );
}

/// Lines and circles sort too, and by their own property name.
#[test]
fn lines_and_circles_sort_by_their_own_key() {
    const LINES: &str = r#"[
      { "type": "Feature", "properties": { "k": 10 },
        "geometry": { "type": "LineString", "coordinates": [[-0.120,51.502],[-0.112,51.502]] } },
      { "type": "Feature", "properties": { "k": 1 },
        "geometry": { "type": "LineString", "coordinates": [[-0.116,51.506],[-0.108,51.506]] } }
    ]"#;
    const POINTS: &str = r#"[
      { "type": "Feature", "properties": { "k": 10 },
        "geometry": { "type": "Point", "coordinates": [-0.120,51.502] } },
      { "type": "Feature", "properties": { "k": 1 },
        "geometry": { "type": "Point", "coordinates": [-0.112,51.506] } }
    ]"#;

    let plain = xs(&style_of(
        "line",
        LINES,
        "",
        r#""line-color": "white", "line-width": 4"#,
    ));
    let sorted = xs(&style_of(
        "line",
        LINES,
        r#""layout": { "line-sort-key": ["get", "k"] },"#,
        r#""line-color": "white", "line-width": 4"#,
    ));
    assert_eq!(
        swap_halves(&plain),
        sorted,
        "line-sort-key swaps the two runs"
    );

    let plain = xs(&style_of(
        "circle",
        POINTS,
        "",
        r#""circle-color": "white""#,
    ));
    let sorted = xs(&style_of(
        "circle",
        POINTS,
        r#""layout": { "circle-sort-key": ["get", "k"] },"#,
        r#""circle-color": "white""#,
    ));
    assert_eq!(
        swap_halves(&plain),
        sorted,
        "circle-sort-key swaps the two quads"
    );
}

/// The two features contribute the same number of vertices, so sorting them swaps the halves.
///
/// Exact rather than `assert_ne`: a permutation that merely differed would pass that, including
/// one that reversed within a feature.
fn swap_halves(xs: &[i16]) -> Vec<i16> {
    let half = xs.len() / 2;
    let (first, second) = xs.split_at(half);
    second.iter().chain(first).copied().collect()
}

/// An extrusion keeps source order, because mbgl declares no `fill-extrusion-sort-key`.
///
/// It shares the fill arm, so without the kind check it would inherit fill's behavior and
/// diverge from the oracle on a property the oracle does not have.
#[test]
fn an_extrusion_ignores_a_sort_key() {
    let plain = xs(&style_of(
        "fill-extrusion",
        SQUARES,
        "",
        r#""fill-extrusion-color": "white", "fill-extrusion-height": 10"#,
    ));
    let keyed = xs(&style_of(
        "fill-extrusion",
        SQUARES,
        r#""layout": { "fill-sort-key": ["get", "k"] },"#,
        r#""fill-extrusion-color": "white", "fill-extrusion-height": 10"#,
    ));
    assert_eq!(
        plain, keyed,
        "an extrusion has no sort key to honor: {plain:?} against {keyed:?}"
    );
}

/// A vector tile sorts the same way a GeoJSON source does.
///
/// The two take different paths -- a slice against an indexed layer -- and the second is the one
/// real styles use, so it is worth its own arm rather than trusting the shared helper.
#[test]
fn a_vector_tile_sorts_the_same_way() {
    use tessella_source::mvt::{GeomType, Geometry, Layer, Tile, Value};

    let square = |x: i32| -> Geometry {
        Geometry::from_rings([vec![
            [x, 1000],
            [x + 400, 1000],
            [x + 400, 1400],
            [x, 1400],
            [x, 1000],
        ]])
    };
    let mut layer = Layer::new("shapes".into(), 4096, 2);
    let key: Arc<str> = "k".into();
    layer.push_feature(
        Some(1),
        GeomType::Polygon,
        [(Arc::clone(&key), Value::Number(10.0))],
        &square(500),
    );
    layer.push_feature(
        Some(2),
        GeomType::Polygon,
        [(Arc::clone(&key), Value::Number(1.0))],
        &square(2000),
    );
    let decoded = Tile {
        layers: vec![layer],
    };

    let build = |layout: &str| {
        let text = format!(
            r##"{{
 "version": 8,
 "sources": {{ "probe": {{ "type": "vector", "tiles": ["http://example.invalid/{{z}}/{{x}}/{{y}}"] }} }},
 "layers": [
  {{ "id": "sorted", "type": "fill", "source": "probe", "source-layer": "shapes", {layout}
     "paint": {{ "fill-color": "white" }} }}
 ]
}}"##
        );
        let style = Style::parse(&text).expect("the style parses");
        let (z, x, y) = TILE;
        let buckets = build_mvt_tile(&style, "probe", TileId::new(z, x, y), &decoded)
            .expect("the tile builds");
        buckets
            .iter()
            .find_map(|bucket| match &bucket.content {
                Content::Fill(fill) => Some(fill.vertices.iter().map(|v| v[0]).collect::<Vec<_>>()),
                _ => None,
            })
            .expect("a fill bucket")
    };

    let plain = build("");
    let sorted = build(r#""layout": { "fill-sort-key": ["get", "k"] },"#);
    assert_ne!(
        plain, sorted,
        "a vector tile's features should permute too: {plain:?}"
    );
    assert_eq!(
        plain.last(),
        sorted.first(),
        "the key-1 square moves to the front, as it does for GeoJSON"
    );
}
