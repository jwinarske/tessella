//! Property resolution and the binding decision.
//!
//! The binding is the point. A property's dependency decides whether it reaches the GPU as a
//! uniform or as a vertex attribute, and that is the distinction §2.2's attribute-descriptor
//! contract is built on.

use tessella_style::property::{
    Binding, Color, PropertyError, attribute_properties, is_all_uniform, resolve_layout,
    resolve_paint,
};
use tessella_style::{Style, Value};

const HERMETIC: &str = include_str!("hermetic_style.json");

fn hermetic() -> Style {
    Style::parse(HERMETIC).expect("parses")
}

fn layer_from(json: &str) -> tessella_style::Layer {
    serde_json::from_str(json).expect("valid layer")
}

// --- colors ---

/// Straight sRGB over 255, not premultiplied and not linearized. Taken from the golden dump
/// rather than assumed: the oracle carries `#2f6f4f` as exactly these components, with the
/// layer's opacity as a separate scalar beside them.
#[test]
fn colors_match_what_the_oracle_carries() {
    let color = Color::parse("#2f6f4f").expect("a color");
    assert!((color.r - 0.184_314).abs() < 1e-6, "{}", color.r);
    assert!((color.g - 0.435_294).abs() < 1e-6, "{}", color.g);
    assert!((color.b - 0.309_804).abs() < 1e-6, "{}", color.b);
    assert!((color.a - 1.0).abs() < 1e-6);
}

#[test]
fn colors_parse_in_every_css_form() {
    let hex = Color::parse("#101418").expect("hex");
    let rgb = Color::parse("rgb(16, 20, 24)").expect("rgb");
    assert_eq!(hex, rgb);

    let named = Color::parse("white").expect("named");
    assert_eq!(named, Color::parse("#ffffff").expect("hex white"));

    let alpha = Color::parse("rgba(0, 0, 0, 0.5)").expect("rgba");
    assert!((alpha.a - 0.5).abs() < 1e-6);
}

#[test]
fn a_non_color_is_reported() {
    assert!(matches!(
        Color::parse("not-a-color"),
        Err(PropertyError::Color { .. })
    ));
}

// --- binding ---

/// A constant paint property is one value for the layer, so it is a uniform.
#[test]
fn a_constant_property_binds_as_a_uniform() {
    let layer = hermetic();
    let fill = layer.layer("fill-constant").expect("fill-constant");
    let paint = resolve_paint(fill).expect("resolves");

    assert_eq!(paint["fill-color"].binding, Binding::Uniform);
    assert_eq!(paint["fill-opacity"].binding, Binding::Uniform);
    assert!(is_all_uniform(&paint), "nothing here varies per feature");
    assert!(attribute_properties(&paint).is_empty());
}

/// A `match` on a feature property varies per feature, so it becomes a vertex attribute — and
/// not an interpolated one, because it does not read the zoom. That is the case where the
/// binder supplies half the declared width and the tweaker zeroes the mix factor (§2.2).
#[test]
fn a_data_driven_property_binds_as_an_uninterpolated_attribute() {
    let style = hermetic();
    let fill = style.layer("fill-datadriven").expect("fill-datadriven");
    let paint = resolve_paint(fill).expect("resolves");

    assert_eq!(
        paint["fill-color"].binding,
        Binding::Attribute {
            interpolated: false
        }
    );
    assert_eq!(
        paint["fill-opacity"].binding,
        Binding::Attribute {
            interpolated: false
        }
    );
    assert!(!is_all_uniform(&paint));
    assert_eq!(
        attribute_properties(&paint),
        ["fill-color", "fill-opacity"],
        "in spec order"
    );
}

/// Reading the zoom as well as the feature is what makes an attribute interpolated: the
/// binder supplies a packed min/max pair for the shader to mix.
#[test]
fn zoom_and_feature_together_make_an_interpolated_attribute() {
    let layer = layer_from(
        r#"{
            "id": "l", "type": "fill", "source": "s",
            "paint": {
                "fill-opacity": ["interpolate", ["linear"], ["zoom"], 10, ["get", "o"], 16, 1]
            }
        }"#,
    );
    let paint = resolve_paint(&layer).expect("resolves");
    assert_eq!(
        paint["fill-opacity"].binding,
        Binding::Attribute { interpolated: true }
    );
}

/// A camera-only expression is the same for every feature at a given zoom, so it is a uniform
/// that changes when the zoom interval does — not an attribute. Binding it as an attribute
/// would put an identical value on every vertex in the layer.
#[test]
fn a_camera_only_property_stays_a_uniform() {
    let layer = layer_from(
        r#"{
            "id": "l", "type": "fill", "source": "s",
            "paint": {
                "fill-opacity": ["interpolate", ["linear"], ["zoom"], 10, 0.2, 16, 1]
            }
        }"#,
    );
    let paint = resolve_paint(&layer).expect("resolves");
    assert_eq!(paint["fill-opacity"].binding, Binding::Uniform);
    assert!(is_all_uniform(&paint));
}

// --- defaults ---

/// Unset properties are present with their defaults, so nothing downstream distinguishes
/// "unset" from "set to the default". The spec does not, and a binder that did would produce
/// different uniforms for two styles that mean the same thing.
#[test]
fn unset_properties_carry_their_defaults() {
    let style = hermetic();
    let background = style.layer("bg").expect("bg");
    let paint = resolve_paint(background).expect("resolves");

    // The style sets the color and nothing else.
    assert_eq!(
        paint["background-color"].as_constant(),
        Some(Value::String("#101418".into()))
    );
    assert_eq!(
        paint["background-opacity"].as_constant(),
        Some(Value::Number(1.0))
    );
    assert_eq!(paint["background-pattern"].as_constant(), Some(Value::Null));
}

/// mbgl defaults fill-outline-color to a default-constructed Color, which is transparent
/// rather than black. The spec's "defaults to fill-color" is resolved by the layer at draw
/// time, not by the property table — getting this backwards would outline every fill in black.
#[test]
fn fill_outline_color_defaults_to_transparent_not_black() {
    let layer = layer_from(r#"{"id": "l", "type": "fill", "source": "s"}"#);
    let paint = resolve_paint(&layer).expect("resolves");

    let outline = paint["fill-outline-color"]
        .as_constant()
        .expect("a constant");
    let color = tessella_style::property::as_color(&outline).expect("a color");
    assert_eq!(color, Color::transparent());

    let fill = paint["fill-color"].as_constant().expect("a constant");
    assert_eq!(
        tessella_style::property::as_color(&fill).expect("a color"),
        Color::black(),
        "fill-color itself does default to black"
    );
}

#[test]
fn fill_defaults_match_mbgl() {
    let layer = layer_from(r#"{"id": "l", "type": "fill", "source": "s"}"#);
    let paint = resolve_paint(&layer).expect("resolves");

    assert_eq!(
        paint["fill-antialias"].as_constant(),
        Some(Value::Bool(true))
    );
    assert_eq!(
        paint["fill-opacity"].as_constant(),
        Some(Value::Number(1.0))
    );
    assert_eq!(
        paint["fill-translate"].as_constant(),
        Some(Value::Array(vec![Value::Number(0.0), Value::Number(0.0)]))
    );
    assert_eq!(
        paint["fill-translate-anchor"].as_constant(),
        Some(Value::String("map".into()))
    );

    let layout = resolve_layout(&layer).expect("resolves");
    assert_eq!(
        layout["fill-sort-key"].as_constant(),
        Some(Value::Number(0.0))
    );
}

// --- rejections ---

/// Data-driven-capable is per property, not per type. A data-driven expression on a property
/// with no attribute slot has nowhere to go, and evaluating it once as a uniform would give
/// every feature the first feature's value.
#[test]
fn a_property_that_cannot_be_data_driven_is_rejected() {
    let layer = layer_from(
        r#"{
            "id": "l", "type": "fill", "source": "s",
            "paint": { "fill-translate": ["get", "offset"] }
        }"#,
    );
    assert!(matches!(
        resolve_paint(&layer),
        Err(PropertyError::NotDataDriven { .. })
    ));

    // The same expression on a capable property is fine.
    let layer = layer_from(
        r#"{
            "id": "l", "type": "fill", "source": "s",
            "paint": { "fill-color": ["get", "color"] }
        }"#,
    );
    assert!(resolve_paint(&layer).is_ok());
}

#[test]
fn a_literal_of_the_wrong_type_is_rejected() {
    for (paint, what) in [
        (r#"{ "fill-opacity": "half" }"#, "a string for a number"),
        (r#"{ "fill-antialias": 1 }"#, "a number for a boolean"),
        (r#"{ "fill-color": 5 }"#, "a number for a color"),
        (r#"{ "fill-color": "not-a-color" }"#, "an invalid color"),
        (r#"{ "fill-translate": [1] }"#, "a one-element pair"),
        (r#"{ "fill-translate": [1, "x"] }"#, "a non-numeric pair"),
    ] {
        let layer = layer_from(&format!(
            r#"{{"id": "l", "type": "fill", "source": "s", "paint": {paint}}}"#
        ));
        assert!(resolve_paint(&layer).is_err(), "should reject {what}");
    }
}

/// A layer type R0 does not implement resolves to nothing rather than failing. The style has
/// line and circle layers, and they must not stop the fill layers from drawing (§1).
#[test]
fn an_unimplemented_layer_type_resolves_empty() {
    let style = hermetic();
    let line = style.layer("line-datadriven").expect("line-datadriven");
    let paint = resolve_paint(line).expect("resolves");
    assert!(paint.is_empty());
    assert!(is_all_uniform(&paint), "vacuously");
}
