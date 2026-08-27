//! `["config", …]`: the option a style declares, and the layer that reads it.
//!
//! # Why this is worth having
//!
//! `config` is where a style parameterizes itself, which in practice is labels. The vendor style
//! this build is measured against writes its `text-field` as
//!
//! ```text
//! ["coalesce", ["get", ["concat", "name_", ["config", "language"]]], ["get", "name"]]
//! ```
//!
//! — the localized name when the configured language has one, the plain name otherwise. An
//! operator the parser does not know makes that unparseable, and an unparseable `text-field`
//! drops the layer: a map with no labels rather than a map with unlocalized ones. Twelve layers
//! of one style, and two hundred and four calls across five.
//!
//! # Resolved in the document, not in the evaluator
//!
//! A config value cannot change while a style is loaded, so `Style::resolve_config` substitutes
//! it and nothing downstream knows it was ever there. That is also why `Expression::parse` alone
//! rejects a bare `["config", …]` — by the time anything parses, there are none left.

use tessella_style::config::{ConfigOption, ConfigType, ConfigValues};
use tessella_style::{Style, Value};

fn style_with(root: &str, layers: &str) -> Style {
    Style::parse(&format!(
        r#"{{"version": 8, {root} "sources": {{"s": {{"type": "vector", "tiles": []}}}},
            "layers": [{layers}]}}"#
    ))
    .expect("the document parses")
}

/// An option resolves to its default, and a layer reading it compiles.
#[test]
fn config_is_resolved_from_the_schema() {
    let mut style = style_with(
        r#""schema": {"language": {"default": "en", "type": "string"}},"#,
        r#"{"id": "labels", "type": "symbol", "source": "s", "source-layer": "l",
            "layout": {"text-field":
              ["coalesce", ["get", ["concat", "name_", ["config", "language"]]],
                           ["get", "name"]]}}"#,
    );

    let rejected = style.reject_uncompilable();
    assert!(
        rejected.is_empty(),
        "the layer should compile: {rejected:?}"
    );

    // And the call is gone, replaced by what it resolved to.
    let field = style.layers[0]
        .layout
        .get("text-field")
        .and_then(|value| value.as_expression())
        .expect("still an expression");
    let text = format!("{:?}", field.value());
    assert!(!text.contains("config"), "a config call survived: {text}");
    assert!(
        text.contains("en"),
        "the option's value is not in it: {text}"
    );
}

/// A missing option is null, which is what makes the fallback work.
///
/// The spec says `config` "returns null if the requested option is missing". That is not a
/// consolation prize: the styles that use it wrap it in a `coalesce`, so null is the branch the
/// author wrote for exactly this case. A style with no `schema` at all still gets its labels.
#[test]
fn a_missing_option_is_null_and_the_layer_survives() {
    let mut style = style_with(
        "",
        r#"{"id": "labels", "type": "symbol", "source": "s", "source-layer": "l",
            "layout": {"text-field": ["coalesce", ["config", "language"], ["get", "name"]]}}"#,
    );
    let rejected = style.reject_uncompilable();
    assert!(
        rejected.is_empty(),
        "a style with no schema still has labels: {rejected:?}"
    );
}

/// A property that is nothing but a config call stops being an expression.
///
/// Substitution can turn the whole value into a literal, and a literal left tagged as an
/// expression is read by the compile step as a call to an operator named by its first element.
#[test]
fn a_whole_property_of_config_becomes_a_literal() {
    let mut style = style_with(
        r#""schema": {"road": {"default": "red", "type": "color"}},"#,
        r#"{"id": "roads", "type": "line", "source": "s", "source-layer": "l",
            "paint": {"line-color": ["config", "road"]}}"#,
    );
    assert!(style.reject_uncompilable().is_empty(), "it compiles");
    assert_eq!(
        style.layers[0]
            .paint
            .get("line-color")
            .and_then(tessella_style::document::PropertyValue::as_literal),
        Some(&Value::String("red".into())),
        "the property is a literal now"
    );
}

/// The constraints the spec puts on an option's value, each applied.
#[test]
fn an_option_is_constrained_as_the_spec_says() {
    let clamped = ConfigOption {
        default: Value::Number(99.0),
        kind: Some(ConfigType::Number),
        array: false,
        min_value: Some(0.0),
        max_value: Some(10.0),
        step_value: None,
        values: None,
        metadata: None,
    };
    assert_eq!(clamped.resolve(None), Value::Number(10.0), "clamped down");
    assert_eq!(
        clamped.resolve(Some(&Value::Number(-5.0))),
        Value::Number(0.0),
        "clamped up"
    );

    let stepped = ConfigOption {
        step_value: Some(0.5),
        max_value: None,
        ..clamped.clone()
    };
    assert_eq!(
        stepped.resolve(Some(&Value::Number(1.3))),
        Value::Number(1.5),
        "rounded to the nearest step"
    );

    // "Permitted enumerated values; invalid input uses default."
    let enumerated = ConfigOption {
        default: Value::String("day".into()),
        kind: Some(ConfigType::String),
        array: false,
        min_value: None,
        max_value: None,
        step_value: None,
        values: Some(vec![
            Value::String("day".into()),
            Value::String("night".into()),
        ]),
        metadata: None,
    };
    assert_eq!(
        enumerated.resolve(Some(&Value::String("night".into()))),
        Value::String("night".into()),
        "a listed value stands"
    );
    assert_eq!(
        enumerated.resolve(Some(&Value::String("dusk".into()))),
        Value::String("day".into()),
        "an unlisted one falls back to the default"
    );
}

/// An import's values answer the three-argument form.
#[test]
fn an_import_supplies_its_own_values() {
    let style = style_with(
        r#""imports": [{"id": "basemap", "url": "mapbox://styles/mapbox/standard",
                       "config": {"lightPreset": "night"}}],"#,
        r#"{"id": "l", "type": "line", "source": "s", "source-layer": "l"}"#,
    );
    let values = ConfigValues::new(&style.schema, &style.imports);

    assert_eq!(
        values.get("lightPreset", Some("basemap")),
        Value::String("night".into())
    );
    // No fallback to the imported style's own default: that default is in a document nothing
    // here has fetched, and inventing one would be worse than saying nothing.
    assert_eq!(values.get("missing", Some("basemap")), Value::Null);
    assert_eq!(values.get("lightPreset", Some("nosuch")), Value::Null);
    // The two-argument form reads this style's own schema, which is empty here.
    assert_eq!(values.get("lightPreset", None), Value::Null);
}

/// Resolving twice changes nothing.
#[test]
fn resolution_is_idempotent() {
    let mut style = style_with(
        r#""schema": {"language": {"default": "en"}},"#,
        r#"{"id": "labels", "type": "symbol", "source": "s", "source-layer": "l",
            "layout": {"text-field": ["concat", "name_", ["config", "language"]]}}"#,
    );
    style.resolve_config();
    let once = format!("{:?}", style.layers[0].layout.get("text-field"));
    style.resolve_config();
    let twice = format!("{:?}", style.layers[0].layout.get("text-field"));
    assert_eq!(once, twice);
}
