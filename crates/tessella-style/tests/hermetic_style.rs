//! Parses the style the golden oracle runs.
//!
//! This is the same JSON `mbgl-capture-probe` compiles in, extracted verbatim. Anything the
//! Rust frontend gets wrong about it shows up as a diff against `tests/golden/`, so getting
//! the parse right is a precondition for that comparison meaning anything.

use tessella_style::{LayerKind, Source, Style, Value};

const HERMETIC: &str = include_str!("hermetic_style.json");

fn style() -> Style {
    Style::parse(HERMETIC).expect("the probe's style parses")
}

#[test]
fn parses_the_document() {
    let style = style();
    assert_eq!(style.version, 8);
    assert_eq!(style.name.as_deref(), Some("capture-probe"));
    assert_eq!(style.sources.len(), 1);
    assert_eq!(style.layers.len(), 5);
}

/// Layer order is the document's order, and it is painter order. A parser that sorted layers,
/// or stored them in a map, would silently restack the map.
#[test]
fn layer_order_is_preserved() {
    let style = style();
    let ids: Vec<&str> = style.layers.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "bg",
            "fill-constant",
            "fill-datadriven",
            "line-datadriven",
            "circle-constant"
        ]
    );
}

#[test]
fn layer_kinds_are_typed() {
    let style = style();
    let kinds: Vec<&LayerKind> = style.layers.iter().map(|l| &l.kind).collect();
    assert_eq!(
        kinds,
        [
            &LayerKind::Background,
            &LayerKind::Fill,
            &LayerKind::Fill,
            &LayerKind::Line,
            &LayerKind::Circle
        ]
    );
    // R0 covers background and fill; the rest of this style is already beyond it (§10).
    assert!(style.layer("bg").unwrap().kind.is_r0());
    assert!(style.layer("fill-constant").unwrap().kind.is_r0());
    assert!(!style.layer("line-datadriven").unwrap().kind.is_r0());
}

/// The source is inline GeoJSON, which is what makes the probe hermetic — R0 runs with no
/// network at all (§10).
#[test]
fn the_source_is_inline_geojson() {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("expected a geojson source");
    };
    assert!(source.is_inline(), "the probe style embeds its data");
    assert_eq!(source.url(), None);

    let features = source
        .data
        .get("features")
        .and_then(Value::as_array)
        .expect("a feature collection");
    assert_eq!(features.len(), 4);
}

/// Background is the one layer with no source. A parser requiring one would reject the
/// simplest layer in the spec.
#[test]
fn background_has_no_source() {
    let style = style();
    let background = style.layer("bg").expect("bg");
    assert_eq!(background.source, None);
    assert_eq!(background.filter, None);
    assert_eq!(
        background.paint["background-color"].as_literal(),
        Some(&Value::String("#101418".into()))
    );
}

/// A constant paint property is a literal, and a `match` is an expression. Getting this
/// backwards would send a color string through the evaluator or a call through the
/// constant-folding path.
#[test]
fn constant_and_data_driven_paint_are_distinguished() {
    let style = style();

    let constant = style.layer("fill-constant").expect("fill-constant");
    assert_eq!(
        constant.paint["fill-color"].as_literal(),
        Some(&Value::String("#2f6f4f".into()))
    );
    assert_eq!(
        constant.paint["fill-opacity"].as_literal(),
        Some(&Value::Number(0.8))
    );

    let driven = style.layer("fill-datadriven").expect("fill-datadriven");
    let color = driven.paint["fill-color"]
        .as_expression()
        .expect("a match expression");
    assert_eq!(color.operator(), Some("match"));
    assert_eq!(color.arguments().len(), 4);
    assert!(
        color.arguments()[0].looks_like_expression(),
        "the input is a `get` call"
    );
}

/// The two syntaxes for a filter are not distinguishable by shape, so the parser keeps it raw
/// rather than guessing. This one is the legacy form, where `$type` is bound specially.
#[test]
fn filters_are_kept_raw_for_the_compile_step() {
    let style = style();
    let filter = style
        .layer("fill-constant")
        .expect("fill-constant")
        .filter
        .as_ref()
        .expect("a filter");
    let items = filter.as_array().expect("an array");
    assert_eq!(items[0], Value::String("==".into()));
    assert_eq!(items[1], Value::String("$type".into()));
    assert_eq!(items[2], Value::String("Polygon".into()));
}

/// A number written without a decimal point is the same style value as one written with it,
/// so `circle-radius: 8` must not become something different from `8.0`.
#[test]
fn integer_literals_are_numbers() {
    let style = style();
    let circle = style.layer("circle-constant").expect("circle-constant");
    assert_eq!(
        circle.paint["circle-radius"].as_literal(),
        Some(&Value::Number(8.0))
    );
}

/// Parsing then serializing then parsing again must reach the same document. This is what
/// makes §12.5's compiled-style cache possible: a cache is only sound if the thing it stores
/// is the whole of what was parsed, and an unrecognized key silently dropped here would be a
/// key silently dropped from every cached style.
#[test]
fn round_trips_losslessly() {
    let first = style();
    let json = serde_json::to_string(&first).expect("serializes");
    let second = Style::parse(&json).expect("re-parses");
    assert_eq!(first, second);
}

#[test]
fn rejects_an_unsupported_version() {
    let bumped = HERMETIC.replace(r#""version": 8"#, r#""version": 9"#);
    assert!(matches!(
        Style::parse(&bumped),
        Err(tessella_style::Error::UnsupportedVersion(9))
    ));
}

#[test]
fn rejects_malformed_json() {
    assert!(matches!(
        Style::parse("{ not json"),
        Err(tessella_style::Error::Json(_))
    ));
}

/// The spec's camelCase source fields reach their fields rather than falling into `extra`.
///
/// A source's multi-word keys are camelCase where a layer's paint properties are kebab-case, and
/// serde needs told per field because most source keys are single words that need no rename. A
/// missing one is not a parse error — the key lands in `extra` and the field reads `None` — so
/// it looks exactly like a style that did not state the value. `tileSize` was that for a while,
/// which covered every raster basemap at the wrong zoom.
#[test]
fn a_sources_camel_case_keys_are_read() {
    use tessella_style::Source;

    let style = tessella_style::Style::parse(
        r#"{"version": 8,
            "sources": {
              "sat": {"type": "raster", "tiles": ["https://o/{z}/{x}/{y}.png"],
                      "tileSize": 256, "minzoom": 2, "maxzoom": 19},
              "points": {"type": "geojson", "data": "https://o/points.json",
                         "cluster": true, "clusterRadius": 60, "clusterMaxZoom": 11}
            },
            "layers": []}"#,
    )
    .expect("the style parses");

    let Some(Source::Raster(raster)) = style.source("sat") else {
        panic!("the raster source is missing");
    };
    assert_eq!(raster.tile_size, Some(256), "tileSize fell into `extra`");
    assert_eq!(raster.minzoom, Some(2.0));
    assert_eq!(raster.maxzoom, Some(19.0));
    assert!(
        !raster.extra.contains_key("tileSize"),
        "tileSize was read twice"
    );

    let Some(Source::Geojson(points)) = style.source("points") else {
        panic!("the geojson source is missing");
    };
    assert_eq!(points.cluster, Some(true));
    assert_eq!(points.cluster_radius, Some(60.0));
    assert_eq!(points.cluster_max_zoom, Some(11.0));
}

/// And they go back out under the same names.
///
/// A style is round-tripped rather than only read — an offline region records the style it
/// pinned — so a rename that reads correctly and writes `tile_size` produces a document nothing
/// else can read back.
#[test]
fn a_sources_camel_case_keys_are_written_back() {
    let style = tessella_style::Style::parse(
        r#"{"version": 8,
            "sources": {"sat": {"type": "raster", "tiles": [], "tileSize": 256}},
            "layers": []}"#,
    )
    .expect("the style parses");

    let Some(source) = style.source("sat") else {
        panic!("the source is missing");
    };
    let written = serde_json::to_string(source).expect("serializes");
    assert!(written.contains(r#""tileSize":256"#), "{written}");
    assert!(!written.contains("tile_size"), "{written}");
}
