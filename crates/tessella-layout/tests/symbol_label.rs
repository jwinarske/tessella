//! Resolving a feature into the text a label says.
//!
//! Both `text-field` syntaxes are in use in the wild, often in one document, so both are checked
//! — a frontend that read only expressions would render half the basemaps on the internet with
//! no labels at all.

use std::collections::BTreeMap;

use tessella_layout::symbol::{label, replace_tokens};
use tessella_style::expression::Feature;
use tessella_style::{Layer, Value};

/// A feature with the properties given.
struct Props(BTreeMap<String, Value>);

impl Props {
    fn new(entries: &[(&str, Value)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        )
    }
}

impl Feature for Props {
    fn property(&self, key: &str) -> Option<Value> {
        self.0.get(key).cloned()
    }
    fn geometry_type(&self) -> &str {
        "Point"
    }
}

/// A symbol layer with the layout properties given.
fn layer(layout: &str) -> Layer {
    let text =
        format!(r#"{{"id": "labels", "type": "symbol", "source": "s", "layout": {layout}}}"#);
    serde_json::from_str(&text).expect("a layer")
}

fn named(name: &str) -> Props {
    Props::new(&[("name", Value::String(name.to_string()))])
}

/// The legacy token syntax resolves from the feature's properties.
#[test]
fn a_token_template_reads_the_feature() {
    let layer = layer(r#"{"text-field": "{name}"}"#);
    let resolved = label(&layer, 10.0, &named("Königsberg")).expect("a label");
    assert_eq!(resolved.text, "Königsberg");
}

/// Text around a token survives it.
#[test]
fn text_around_a_token_is_kept() {
    let layer = layer(r#"{"text-field": "★ {name} ★"}"#);
    let resolved = label(&layer, 10.0, &named("Oslo")).expect("a label");
    assert_eq!(resolved.text, "★ Oslo ★");
}

/// A token naming a property the feature does not have resolves to nothing.
///
/// mbgl converts `"{name}"` into `toString(get("name"))` at style-parse time, so an absent
/// property is an empty string and the label disappears. This is deliberately *not* the tile URL
/// rule, where an unrecognised token survives verbatim so a 404 can say why: most features in a
/// symbol source have no name, and leaving the token would write a literal `{name}` across the
/// map on every one of them.
#[test]
fn an_unknown_token_resolves_to_nothing() {
    let missing = layer(r#"{"text-field": "{nmae}"}"#);
    assert!(
        label(&missing, 10.0, &named("Oslo")).is_none(),
        "a label of only an absent token is no label"
    );

    // And it leaves the text around it alone.
    let decorated = layer(r#"{"text-field": "★{nmae}★"}"#);
    let resolved = label(&decorated, 10.0, &named("Oslo")).expect("a label");
    assert_eq!(resolved.text, "★★");
}

/// The modern expression syntax resolves too.
#[test]
fn an_expression_field_reads_the_feature() {
    let layer = layer(r#"{"text-field": ["get", "name"]}"#);
    let resolved = label(&layer, 10.0, &named("Bergen")).expect("a label");
    assert_eq!(resolved.text, "Bergen");
}

/// A feature with no name produces no label.
///
/// Most features in a symbol source have none. An empty label still has an anchor, a collision
/// box and a place in the sort order, and would push real labels off the map to draw nothing.
#[test]
fn a_feature_without_the_property_makes_no_label() {
    let layer = layer(r#"{"text-field": ["get", "name"]}"#);
    assert!(label(&layer, 10.0, &Props::new(&[])).is_none());
}

/// Whitespace is nothing to draw.
#[test]
fn a_whitespace_label_is_no_label() {
    let layer = layer(r#"{"text-field": "   "}"#);
    assert!(label(&layer, 10.0, &named("Oslo")).is_none());
}

/// A layer with no `text-field` has no labels at all.
#[test]
fn a_layer_without_a_text_field_makes_no_labels() {
    let layer = layer(r#"{"text-size": 12}"#);
    assert!(label(&layer, 10.0, &named("Oslo")).is_none());
}

/// A number shows without a trailing decimal.
///
/// The style spec has one number type and it is a double, so an elevation of 1200 arrives as
/// 1200.0, and a label reading "1200.0" would be wrong on a map people read. Rust's float
/// `Display` happens to do the right thing here where C++'s does not — this asserts the
/// behaviour rather than the mechanism, so it keeps holding if the mechanism changes.
#[test]
fn a_number_property_reads_as_a_number() {
    let layer = layer(r#"{"text-field": "{ele}"}"#);
    let feature = Props::new(&[("ele", Value::Number(1200.0))]);
    assert_eq!(label(&layer, 10.0, &feature).expect("a label").text, "1200");

    // And a real fraction keeps its point.
    let feature = Props::new(&[("ele", Value::Number(1200.5))]);
    assert_eq!(
        label(&layer, 10.0, &feature).expect("a label").text,
        "1200.5"
    );
}

/// The font stack comes from `text-font`, and defaults when the layer is silent.
#[test]
fn the_font_stack_is_read_or_defaulted() {
    let bold = layer(r#"{"text-field": "{name}", "text-font": ["Noto Sans Bold"]}"#);
    let resolved = label(&bold, 10.0, &named("Oslo")).expect("a label");
    assert_eq!(resolved.fonts, ["Noto Sans Bold"]);

    // `["Noto Sans Regular"]` must reach the literal arm rather than reading as a call to an
    // operator of that name — which is what the generated operator registry is for.
    let silent = layer(r#"{"text-field": "{name}"}"#);
    let resolved = label(&silent, 10.0, &named("Oslo")).expect("a label");
    assert_eq!(resolved.fonts.len(), 2, "the spec's default stack");
    assert!(resolved.fonts[0].contains("Open Sans"));
}

/// An expression that yields a token template has its tokens resolved too.
///
/// Styles written against the legacy syntax and later wrapped in an expression rely on this, and
/// mbgl resolves tokens after evaluating for the same reason.
#[test]
fn tokens_inside_an_expression_result_are_resolved() {
    let layer = layer(r#"{"text-field": ["concat", "{name}", " (", "{ele}", ")"]}"#);
    let feature = Props::new(&[
        ("name", Value::String("Peak".to_string())),
        ("ele", Value::Number(2400.0)),
    ]);
    assert_eq!(
        label(&layer, 10.0, &feature).expect("a label").text,
        "Peak (2400)"
    );
}

/// Token replacement on its own, including the shapes that are not tokens.
#[test]
fn token_replacement_handles_awkward_strings() {
    let feature = named("Oslo");

    assert_eq!(replace_tokens("", &feature), "");
    assert_eq!(replace_tokens("no tokens", &feature), "no tokens");
    assert_eq!(replace_tokens("{name} {name}", &feature), "Oslo Oslo");

    // An unclosed brace is not a token, and the rest of the string is literal.
    assert_eq!(replace_tokens("{name", &feature), "{name");

    // mbgl's scan stops at the next reserved character rather than the next `}`, so a brace
    // closed by another brace is literal up to it.
    assert_eq!(replace_tokens("{a{name}", &feature), "{aOslo");

    // An empty token names a property nothing has, so it resolves to nothing.
    assert_eq!(replace_tokens("{}", &feature), "");
}
