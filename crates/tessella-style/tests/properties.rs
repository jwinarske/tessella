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
        ["fill-color", "fill-opacity", "fill-outline-color"],
        "in spec order — the outline inherits fill-color's binding, so it is data-driven too"
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

    // The style sets the color and nothing else. It arrives resolved rather than as the
    // string the style wrote: a colour-typed property is coerced at parse, so `"#101418"` and
    // a function returning `"#101418"` reach a binder in the same form.
    let colour = paint["background-color"]
        .as_constant()
        .expect("a constant colour");
    let Value::Color(colour) = colour else {
        panic!("a colour, got {colour:?}");
    };
    assert!((colour.r - 16.0 / 255.0).abs() < 1e-6, "{colour:?}");
    assert!((colour.g - 20.0 / 255.0).abs() < 1e-6, "{colour:?}");
    assert!((colour.b - 24.0 / 255.0).abs() < 1e-6, "{colour:?}");
    assert_eq!(colour.a, 1.0);
    assert_eq!(
        paint["background-opacity"].as_constant(),
        Some(Value::Number(1.0))
    );
    assert_eq!(paint["background-pattern"].as_constant(), Some(Value::Null));
}

/// `fill-outline-color` inherits `fill-color` — value *and* binding — when a style does not
/// set it.
///
/// The property table gives it transparent, which is mbgl's own default-constructed Color, but
/// that default is never what a layer draws with: the spec says it defaults to `fill-color` and
/// mbgl resolves that in the fill layer. So a bare fill layer outlines in black because
/// `fill-color` is black, not because the table says so.
///
/// An earlier version of this test asserted the table default was what came out, and an earlier
/// commit message claimed that getting it "backwards" would outline every fill in black. That
/// was wrong twice over: black is correct here, and the real consequence of getting it backwards
/// is the binding, not the colour. The oracle settled it — its data-driven fill drawable carries
/// the outline as a vertex attribute even though the style never mentions the property, which
/// only happens if the binding was inherited too.
#[test]
fn fill_outline_color_inherits_fill_color() {
    use tessella_style::property::{Binding, DefaultValue};

    // The table's own default is still transparent, and still what mbgl declares.
    let spec = tessella_style::property::paint_specs(&tessella_style::LayerKind::Fill)
        .expect("fill specs")
        .iter()
        .find(|spec| spec.name == "fill-outline-color")
        .expect("the spec");
    assert_eq!(spec.default, DefaultValue::Color(Color::transparent()));

    // But resolution inherits fill-color, so a bare layer outlines in black.
    let layer = layer_from(r#"{"id": "l", "type": "fill", "source": "s"}"#);
    let paint = resolve_paint(&layer).expect("resolves");
    let outline = paint["fill-outline-color"]
        .as_constant()
        .expect("a constant");
    assert_eq!(
        tessella_style::property::as_color(&outline).expect("a colour"),
        Color::black(),
        "inherited from fill-color's default"
    );

    // And the binding is inherited with it, which is the part that matters: a data-driven
    // fill-color makes the outline data-driven, needing a vertex attribute rather than a
    // uniform.
    let driven = layer_from(
        r#"{"id": "l", "type": "fill", "source": "s",
            "paint": {"fill-color": ["get", "c"]}}"#,
    );
    let paint = resolve_paint(&driven).expect("resolves");
    assert_eq!(
        paint["fill-outline-color"].binding,
        Binding::Attribute {
            interpolated: false
        }
    );

    // An explicit value is not overridden.
    let explicit = layer_from(
        r##"{"id": "l", "type": "fill", "source": "s",
            "paint": {"fill-color": "#ff0000", "fill-outline-color": "#00ff00"}}"##,
    );
    let paint = resolve_paint(&explicit).expect("resolves");
    let outline = paint["fill-outline-color"]
        .as_constant()
        .expect("a constant");
    assert_eq!(
        tessella_style::property::as_color(&outline).expect("a colour"),
        Color::parse("#00ff00").expect("green")
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
    // The hermetic style no longer contains one: background, fill, line, circle, symbol,
    // raster and fill-extrusion are all implemented, which is why this reaches for types
    // outside it. A layer type with no spec table resolves to nothing rather than to guessed
    // defaults — the difference between "this build does not know what a heatmap layer's
    // properties are" and "it thinks they are empty".
    let style = Style::parse(
        r#"{"version": 8, "sources": {}, "layers": [
             {"id": "h", "type": "heatmap", "source": "s", "paint": {"heatmap-opacity": 0.5}},
             {"id": "e", "type": "hillshade", "source": "s"}]}"#,
    )
    .expect("style parses");
    for id in ["h", "e"] {
        let paint = resolve_paint(style.layer(id).expect(id)).expect("resolves");
        assert!(paint.is_empty(), "{id}");
        assert!(is_all_uniform(&paint), "vacuously, {id}");
    }

    // And an implemented one does not resolve empty, which is what stops this passing for the
    // wrong reason once every type in the style spec has a table.
    let symbols = Style::parse(
        r#"{"version": 8, "sources": {}, "layers": [
             {"id": "s", "type": "symbol", "source": "s"}]}"#,
    )
    .expect("style parses");
    let paint = resolve_paint(symbols.layer("s").expect("s")).expect("resolves");
    assert!(!paint.is_empty(), "a symbol layer has paint properties now");
}

/// `line-floorwidth` is not a style-spec property and is a real one to mbgl.
///
/// `setLineWidth` assigns both, so the mirror is unconditional — unlike `fill-outline-color`,
/// which only falls back when the style is silent. It is what puts the line paint buffer's
/// stride at 16 rather than 12, and the golden dump shows it at offset 8 of a layer whose style
/// never mentions it.
#[test]
fn line_floorwidth_mirrors_line_width() {
    let style = hermetic();
    let line = style.layer("line-datadriven").expect("line-datadriven");
    let paint = resolve_paint(line).expect("resolves");

    let width = paint.get("line-width").expect("line-width");
    let floor = paint.get("line-floorwidth").expect("line-floorwidth");
    assert_eq!(floor.expression, width.expression, "same expression");
    assert_eq!(floor.binding, width.binding, "and the same binding");
    assert!(
        matches!(width.binding, Binding::Attribute { .. }),
        "the style's width is a match on a feature property"
    );

    // The mirror is unconditional, so a constant width mirrors too — and stays a uniform.
    let plain = Style::parse(
        r#"{"version": 8, "sources": {}, "layers": [
             {"id": "l", "type": "line", "source": "s", "paint": {"line-width": 3.0}}]}"#,
    )
    .expect("style parses");
    let paint = resolve_paint(plain.layer("l").expect("l")).expect("resolves");
    assert_eq!(
        paint
            .get("line-floorwidth")
            .expect("line-floorwidth")
            .binding,
        Binding::Uniform
    );
}

/// Colors are stored premultiplied, as mbgl stores them.
///
/// The golden dump's colors are all opaque, so this is a difference the oracle cannot show and
/// the spec has to decide. A half-transparent red is `(0.5, 0, 0, 0.5)`, not `(1, 0, 0, 0.5)`,
/// and a consumer blending the second as though it were the first draws it at twice the
/// intensity.
#[test]
fn colors_are_stored_premultiplied() {
    let half = Color::parse("rgba(255, 0, 0, 0.5)").expect("a color");
    assert_eq!(half.a, 0.5);
    assert!((half.r - 0.5).abs() < 1e-6, "{}", half.r);
    assert_eq!(half.g, 0.0);

    let transparent = Color::parse("rgba(255, 255, 255, 0)").expect("a color");
    assert_eq!([transparent.r, transparent.g, transparent.b], [0.0; 3]);
    assert_eq!(transparent.a, 0.0);
}

/// An opaque color is its own premultiple, which is why the oracle's colors matched before this
/// was right.
#[test]
fn an_opaque_color_is_its_own_premultiple() {
    let color = Color::parse("#2f6f4f").expect("a color");
    assert_eq!(color.a, 1.0);
    assert!((color.r - 47.0 / 255.0).abs() < 1e-6);
}

/// The channel conversion is a multiply by `alpha / 255`, not a divide by 255.
///
/// The same real number and a different `f32`: `#2f6f4f`'s green comes out one ULP low the other
/// way, while its blue is identical either way — so a check on a single channel concludes the two
/// spellings are interchangeable. Both cases are asserted here for that reason.
#[test]
fn the_channel_conversion_is_mbgls_expression() {
    let color = Color::parse("#2f6f4f").expect("a color");

    let mbgl_way = 111.0_f32 * (1.0_f32 / 255.0);
    assert_eq!(color.g.to_bits(), mbgl_way.to_bits());
    assert_ne!(
        (111.0_f32 / 255.0).to_bits(),
        mbgl_way.to_bits(),
        "the two spellings differ on this channel, which is why one was chosen"
    );

    assert_eq!(color.b.to_bits(), (79.0_f32 * (1.0_f32 / 255.0)).to_bits());
    assert_eq!(
        (79.0_f32 / 255.0).to_bits(),
        (79.0_f32 * (1.0_f32 / 255.0)).to_bits(),
        "and agree on this one, which is how the difference stays hidden"
    );
}
