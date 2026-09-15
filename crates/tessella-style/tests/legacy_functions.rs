// SPDX-License-Identifier: BSD-2-Clause
//! Pre-expression functions reach the evaluator rather than the literal type check.
//!
//! demotiles, the style MapLibre's own examples start from, writes `line-width`, `line-opacity`,
//! `text-size` and the halo properties as `{"stops": …}`. Classified as literals, the object met
//! the number check and three of its eight layers were refused as uncompilable -- the coastline,
//! the country boundaries and the country labels -- where mbgl converts every object property as
//! a function.

use tessella_style::property::{Binding, layout_value, resolve_paint};
use tessella_style::{PropertyValue, Style, Value};

fn layer_from(json: &str) -> tessella_style::Layer {
    serde_json::from_str(json).expect("valid layer")
}

fn number(value: &Value) -> f64 {
    value.as_number().expect("a number")
}

/// A zoom function resolves, binds as a uniform because it reads only the zoom, and interpolates
/// between its stops: halfway from zoom 0 to 6 is halfway from 2 to 6.
#[test]
fn a_legacy_zoom_function_resolves_and_interpolates() {
    let layer = layer_from(
        r#"{
            "id": "coastline", "type": "line", "source": "s", "source-layer": "countries",
            "paint": {"line-width": {"stops": [[0, 2], [6, 6], [14, 9], [22, 18]]}}
        }"#,
    );
    assert!(matches!(
        layer.paint.get("line-width"),
        Some(PropertyValue::Expression(_))
    ));

    let paint = resolve_paint(&layer).expect("a legacy function resolves");
    let width = &paint["line-width"];
    assert_eq!(width.binding, Binding::Uniform);
    let at = |zoom| {
        number(
            &width
                .expression
                .evaluate(Some(zoom), None)
                .expect("evaluates"),
        )
    };
    assert!((at(3.0) - 4.0).abs() < 1e-9, "{}", at(3.0));
    assert!((at(0.0) - 2.0).abs() < 1e-9, "{}", at(0.0));
    assert!(
        (at(30.0) - 18.0).abs() < 1e-9,
        "clamped past the last stop: {}",
        at(30.0)
    );
}

/// A symbol layer's layout has no spec table, so `text-size` is read through `layout_value`,
/// which used to hand the literal object back and leave every caller on its default size.
#[test]
fn a_legacy_text_size_reaches_layout_value() {
    let layer = layer_from(
        r#"{
            "id": "countries-label", "type": "symbol", "source": "s", "source-layer": "centroids",
            "layout": {"text-size": {"stops": [[2, 10], [4, 12], [6, 16]]}}
        }"#,
    );
    let size = layout_value(&layer, "text-size", 3.0, None).expect("evaluates");
    assert!((number(&size) - 11.0).abs() < 1e-9, "{size:?}");
}

/// The shape of demotiles' layers compiles whole: nothing is refused.
#[test]
fn a_style_written_with_legacy_functions_rejects_no_layer() {
    let mut style = Style::parse(
        r##"{
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["http://127.0.0.1/{z}/{x}/{y}.pbf"]}},
            "layers": [
                {"id": "coastline", "type": "line", "source": "s", "source-layer": "countries",
                 "paint": {"line-color": "#198EC8",
                           "line-width": {"stops": [[0, 2], [6, 6], [14, 9], [22, 18]]}}},
                {"id": "countries-boundary", "type": "line", "source": "s", "source-layer": "countries",
                 "paint": {"line-width": {"stops": [[1, 1], [6, 2], [14, 6], [22, 12]]},
                           "line-opacity": {"stops": [[3, 0.5], [6, 1]]}}},
                {"id": "countries-label", "type": "symbol", "source": "s", "source-layer": "centroids",
                 "layout": {"text-field": {"stops": [[2, "{ABBREV}"], [4, "{NAME}"]]},
                            "text-size": {"stops": [[2, 10], [4, 12], [6, 16]]}},
                 "paint": {"text-halo-width": {"stops": [[2, 1], [6, 1.6]]}}}
            ]
        }"##,
    )
    .expect("parses");
    let rejected = style.reject_uncompilable();
    assert!(rejected.is_empty(), "{rejected:?}");
    assert_eq!(style.layers.len(), 3);
}

/// An `identity` function has no stops and is still a function.
#[test]
fn an_identity_function_is_evaluated() {
    let value: Value =
        serde_json::from_str(r#"{"type": "identity", "property": "width"}"#).expect("json");
    assert!(value.looks_like_function());
    assert!(matches!(
        PropertyValue::from_value(value),
        PropertyValue::Expression(_)
    ));
}

/// An object that is not a function stays data, so the classification is the parser's rule and
/// not "every object".
#[test]
fn an_object_without_stops_stays_a_literal() {
    let value: Value = serde_json::from_str(r#"{"a": 1}"#).expect("json");
    assert!(!value.looks_like_function());
    assert!(matches!(
        PropertyValue::from_value(value),
        PropertyValue::Literal(_)
    ));
}
