//! A style that names one thing this build does not have still draws every layer that does.
//!
//! mbgl's `Parser::parseLayer` converts each layer on its own and, on failure, logs a warning
//! and returns — the layer never enters `layers` and the rest of the document does. Refusing
//! the whole style instead is the difference between a map with a missing label layer and a
//! blank screen, and on a real vendor style it is not hypothetical: a hundred and fourteen
//! layers, eleven of them filtered on Mapbox Style Spec v3 expressions.
//!
//! Two of those, `["distance-from-center"]` and `["pitch"]`, this build now implements, so they
//! no longer cost their layers — see `the_camera_operators_are_no_longer_rejected` below. The
//! rejection path is not thereby obsolete: `["config"]` and `["measure-light"]` remain, and an
//! unknown operator is the ordinary way a style outruns a renderer.

use tessella_style::Style;

fn style_with(layers: &str) -> Style {
    Style::parse(&format!(
        r#"{{"version": 8, "sources": {{"s": {{"type": "vector", "tiles": []}}}},
            "layers": [{layers}]}}"#
    ))
    .expect("the document parses")
}

/// The layer that will not compile goes; the ones around it stay, in order.
#[test]
fn one_bad_layer_does_not_take_the_style_with_it() {
    let mut style = style_with(
        r#"{"id": "under", "type": "fill", "source": "s", "source-layer": "l"},
           {"id": "vendor", "type": "symbol", "source": "s", "source-layer": "l",
            "filter": ["<=", ["measure-light", "brightness"], 2]},
           {"id": "over", "type": "line", "source": "s", "source-layer": "l"}"#,
    );

    let rejected = style.reject_uncompilable();
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].id, "vendor");
    assert!(
        rejected[0].reason.contains("measure-light"),
        "the reason names the operator: {}",
        rejected[0].reason
    );

    let kept: Vec<&str> = style.layers.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(kept, ["under", "over"], "draw order survives the drop");
}

/// Paint and layout are compiled too, not just the filter.
///
/// All three are asked for at bucket build, so all three have to be asked for here — a layer
/// whose filter compiles and whose paint does not would otherwise fail every tile instead of
/// being dropped once.
#[test]
fn paint_and_layout_are_compiled_as_well() {
    let mut style = style_with(
        r#"{"id": "bad-paint", "type": "fill", "source": "s", "source-layer": "l",
            "paint": {"fill-color": "not-a-color"}},
           {"id": "bad-layout", "type": "line", "source": "s", "source-layer": "l",
            "layout": {"line-cap": ["gett", "x"]}}"#,
    );

    let rejected = style.reject_uncompilable();
    let ids: Vec<&str> = rejected.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(ids, ["bad-paint", "bad-layout"]);
    assert!(style.layers.is_empty());
}

/// A style this build fully understands loses nothing.
#[test]
fn a_compilable_style_is_untouched() {
    let mut style = style_with(
        r##"{"id": "roads", "type": "line", "source": "s", "source-layer": "l",
            "filter": ["all", ["==", ["get", "class"], "street"],
                       ["step", ["zoom"], false, 13, true]],
            "paint": {"line-dasharray": [3, 3], "line-color": "#888"}}"##,
    );
    assert!(style.reject_uncompilable().is_empty());
    assert_eq!(style.layers.len(), 1);
}

/// The two camera operators keep their layers now.
///
/// This is the whole point of implementing them: they appeared only in filters, and a filter
/// that will not compile takes the layer with it. Every one of the layers they cost was a label
/// layer, so the visible effect of the gap was a map with most of its labels missing.
#[test]
fn the_camera_operators_are_no_longer_rejected() {
    let mut style = style_with(
        r#"{"id": "far", "type": "symbol", "source": "s", "source-layer": "l",
            "filter": ["<=", ["distance-from-center"], 2]},
           {"id": "flat", "type": "symbol", "source": "s", "source-layer": "l",
            "filter": [">", ["pitch"], 30]},
           {"id": "both", "type": "symbol", "source": "s", "source-layer": "l",
            "filter": ["all", ["<=", ["distance-from-center"], 2], [">", ["pitch"], 30]]}"#,
    );

    let rejected = style.reject_uncompilable();
    assert!(
        rejected.is_empty(),
        "a camera filter should compile now: {rejected:?}"
    );
    let kept: Vec<&str> = style.layers.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(kept, ["far", "flat", "both"]);
}
