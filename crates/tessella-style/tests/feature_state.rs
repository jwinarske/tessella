// SPDX-License-Identifier: BSD-2-Clause
//! `["feature-state", key]`: per-feature state a host set, read by a paint property.
//!
//! It was in the generated operator registry and nothing evaluated it (tessella#337), so a style
//! highlighting a feature through it drew every feature in its default state -- silently, because
//! an unknown operator would have been refused and this one parsed and returned nothing.
//!
//! # What state is, and is not
//!
//! Not a property: it is not in the tile and not in the style. A host sets it to mark a feature --
//! hovered, selected, part of a route -- and it changes while the tile it belongs to stays as it is.
//! That is the whole reason the specification allows it in a *paint* property only, and both of the
//! refusals below are that rule: a filter decides which features exist and a layout property decides
//! their geometry, and each is answered once when the tile is cut.

use std::collections::BTreeMap;

use tessella_style::expression::{Expression, Feature};
use tessella_style::{Value, filter::Filter};

/// A feature with properties and state, which is what the two lookups are told apart by.
struct Marked {
    properties: BTreeMap<String, Value>,
    state: BTreeMap<String, Value>,
}

impl Feature for Marked {
    fn property(&self, key: &str) -> Option<Value> {
        self.properties.get(key).cloned()
    }

    fn geometry_type(&self) -> &str {
        "Point"
    }

    fn state(&self, key: &str) -> Option<Value> {
        self.state.get(key).cloned()
    }
}

/// A feature that answers nothing, which is every implementation that predates the operator.
struct Plain;

impl Feature for Plain {
    fn property(&self, _key: &str) -> Option<Value> {
        None
    }

    fn geometry_type(&self) -> &str {
        "Point"
    }
}

fn parse(json: &str) -> Expression {
    Expression::parse(&serde_json::from_str::<Value>(json).expect("the json parses"))
        .expect("the expression parses")
}

fn marked(state: &[(&str, Value)]) -> Marked {
    Marked {
        properties: [("kind".to_string(), Value::String("road".into()))]
            .into_iter()
            .collect(),
        state: state
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect(),
    }
}

/// The state of the feature being evaluated, by key.
#[test]
fn it_reads_the_state_of_the_feature() {
    let expression = parse(r#"["feature-state", "hover"]"#);
    let hovered = marked(&[("hover", Value::Bool(true))]);
    assert_eq!(
        expression
            .evaluate(None, Some(&hovered))
            .expect("evaluates"),
        Value::Bool(true)
    );
}

/// A key the feature has no state for is null, as an absent property is.
///
/// Erroring instead would make every unhighlighted feature a failed evaluation, where what a style
/// writes is `["case", ["feature-state", "hover"], …, …]` and expects the default arm.
#[test]
fn absent_state_is_null_rather_than_an_error() {
    let expression = parse(r#"["feature-state", "hover"]"#);
    assert_eq!(
        expression
            .evaluate(None, Some(&marked(&[])))
            .expect("evaluates"),
        Value::Null
    );
    // And a feature that does not implement the method at all answers the same, which is what the
    // trait's default is for: every `Feature` written before this operator keeps working.
    assert_eq!(
        expression.evaluate(None, Some(&Plain)).expect("evaluates"),
        Value::Null
    );
}

/// State and properties are separate namespaces, which is the point of the operator existing.
///
/// A key in both answers differently through each, and that is not a curiosity: a host marking
/// `selected` on a feature whose tags also carry `selected` must not have the tag win.
#[test]
fn state_is_not_the_properties() {
    let feature = Marked {
        properties: [("tone".to_string(), Value::String("tag".into()))]
            .into_iter()
            .collect(),
        state: [("tone".to_string(), Value::String("state".into()))]
            .into_iter()
            .collect(),
    };
    assert_eq!(
        parse(r#"["get", "tone"]"#)
            .evaluate(None, Some(&feature))
            .expect("evaluates"),
        Value::String("tag".into())
    );
    assert_eq!(
        parse(r#"["feature-state", "tone"]"#)
            .evaluate(None, Some(&feature))
            .expect("evaluates"),
        Value::String("state".into())
    );
}

/// The idiom a style actually writes: a highlight through `case`, with a default arm.
#[test]
fn a_highlight_picks_the_state_arm_and_falls_back() {
    // Two hashes: a color is a `#` straight after a quote, which ends an `r#"..."#` string there.
    let expression =
        parse(r##"["case", ["==", ["feature-state", "hover"], true], "#ff0000", "#204060"]"##);
    let hovered = expression
        .evaluate(None, Some(&marked(&[("hover", Value::Bool(true))])))
        .expect("evaluates");
    let plain = expression
        .evaluate(None, Some(&marked(&[])))
        .expect("evaluates");
    assert_ne!(hovered, plain, "the two arms drew the same color");
}

/// It depends on the feature *and* on the state, which are different occasions.
///
/// Both bits, because the two are needed for different decisions: the feature bit is what makes the
/// property a per-vertex attribute rather than a uniform, and the state bit is what a caller
/// deciding how much to redo for a change would read.
#[test]
fn it_depends_on_the_feature_and_the_state() {
    let dependency = parse(r#"["feature-state", "hover"]"#).dependency();
    assert!(dependency.needs_feature(), "{dependency:?}");
    assert!(dependency.needs_state(), "{dependency:?}");
    assert!(!dependency.needs_zoom(), "{dependency:?}");
    assert!(!dependency.is_constant(), "{dependency:?}");

    // And an expression that does not read it says so, which is what makes the bit worth having.
    let ordinary = parse(r#"["get", "kind"]"#).dependency();
    assert!(ordinary.needs_feature());
    assert!(!ordinary.needs_state());
}

/// Nested, which is the shape a real style has: the state is read inside an interpolation.
#[test]
fn the_bit_survives_a_nesting() {
    let dependency =
        parse(r#"["interpolate", ["linear"], ["zoom"], 0, ["feature-state", "width"], 16, 4]"#)
            .dependency();
    assert!(dependency.needs_state(), "{dependency:?}");
    assert!(dependency.needs_zoom(), "{dependency:?}");
    assert!(dependency.needs_feature(), "{dependency:?}");
}

/// A filter cannot read it.
///
/// A filter decides which features exist at all and runs once when the tile is built, so a filter
/// over state would answer for whatever the state was then and never change -- which reads as a
/// highlight that works until the first pan.
#[test]
fn a_filter_cannot_read_feature_state() {
    let value: Value =
        serde_json::from_str(r#"["==", ["feature-state", "hover"], true]"#).expect("json");
    let refused = Filter::parse(&value).expect_err("a filter over state is refused");
    let said = format!("{refused}");
    assert!(said.contains("feature state"), "{said}");
    assert!(said.contains("paint property"), "{said}");

    // The same filter over a property is fine, which is what says the refusal is about the
    // operator rather than about the shape.
    let ordinary: Value = serde_json::from_str(r#"["==", ["get", "kind"], "road"]"#).expect("json");
    assert!(Filter::parse(&ordinary).is_ok());
}

/// Nor a layout property, for the same reason one level along: layout is cut once.
///
/// A line layer, because that is a kind `layout_specs` has a table for. The kinds it does not --
/// symbol above all -- are read through `layout_value`, which has no error channel at all; the test
/// below covers what happens there.
#[test]
fn a_layout_property_cannot_read_feature_state() {
    let layer = r#"{
        "id": "l", "type": "line", "source": "s", "source-layer": "roads",
        "layout": {"line-sort-key": ["feature-state", "rank"]}
    }"#;
    let layer: tessella_style::Layer = serde_json::from_str(layer).expect("the layer parses");
    let refused =
        tessella_style::property::resolve_layout(&layer).expect_err("layout over state is refused");
    let said = format!("{refused}");
    assert!(said.contains("feature state"), "{said}");

    // And a paint property on the same layer takes it, which is the allowed half of the rule.
    let painted = r##"{
        "id": "l", "type": "fill", "source": "s", "source-layer": "roads",
        "paint": {"fill-color": ["case", ["feature-state", "hover"], "#ff0000", "#204060"]}
    }"##;
    let painted: tessella_style::Layer = serde_json::from_str(painted).expect("the layer parses");
    tessella_style::property::resolve_paint(&painted).expect("a paint property may read state");
}

/// A symbol layout property reading state answers nothing, which is the caller's default.
///
/// `layout_specs` has no table for a symbol layer, so `resolve_layout` answers an empty map for one
/// and the refusal above never sees it. `layout_value` is what reads those properties, one at a
/// time, and it has no error channel: its contract is already "no value, so the caller's own
/// default applies". So this is where the rule lands for the layer kind a host is most likely to
/// try it on -- a label whose text came from state would be baked in when the tile was cut.
#[test]
fn a_symbol_layout_property_answers_nothing_for_state() {
    let layer = r#"{
        "id": "l", "type": "symbol", "source": "s", "source-layer": "roads",
        "layout": {"text-field": ["feature-state", "label"], "text-size": 18}
    }"#;
    let layer: tessella_style::Layer = serde_json::from_str(layer).expect("the layer parses");
    // Empty rather than refused, which is the hazard `layout_value` documents.
    assert!(
        tessella_style::property::resolve_layout(&layer)
            .expect("a symbol layer resolves to nothing")
            .is_empty()
    );

    let marked = marked(&[("label", Value::String("Main Street".into()))]);
    assert_eq!(
        tessella_style::property::layout_value(&layer, "text-field", 14.0, Some(&marked)),
        None,
        "a label read its text from state, which is cut into the tile and cannot change"
    );
    // A property that does not read state is unaffected, which is what says the check is about the
    // operator rather than about this function.
    assert_eq!(
        tessella_style::property::layout_value(&layer, "text-size", 14.0, Some(&marked)),
        Some(Value::Number(18.0))
    );
}

/// One argument, and it must be there.
#[test]
fn the_arity_is_one() {
    for json in [r#"["feature-state"]"#, r#"["feature-state", "a", "b"]"#] {
        let value: Value = serde_json::from_str(json).expect("json");
        assert!(Expression::parse(&value).is_err(), "{json}");
    }
}

/// `["within", …]` is the one operator that reads a feature's coordinates rather than its tags.
///
/// Asked by a caller that holds a feature's properties and not its geometry -- which is what
/// re-evaluating a paint property from a recorded index is. It cannot answer `within` at all, and
/// the honest move is to not re-evaluate rather than to answer "outside" and be quietly wrong.
#[test]
fn reads_geometry_names_the_one_operator_that_does() {
    let polygon = r#"["within", {"type": "Polygon",
        "coordinates": [[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]]}]"#;
    assert!(parse(polygon).reads_geometry());

    // Nested, which is the shape it would really take: a highlight whose default arm is a
    // geometry test.
    let nested = format!(r#"["case", ["==", ["feature-state", "hover"], true], true, {polygon}]"#);
    let value: Value = serde_json::from_str(&nested).expect("json");
    assert!(Expression::parse(&value).expect("parses").reads_geometry());

    // And the ordinary expressions do not.
    for json in [
        r#"["get", "kind"]"#,
        r#"["feature-state", "hover"]"#,
        r#"["geometry-type"]"#,
        r#"["interpolate", ["linear"], ["zoom"], 0, 1, 16, 4]"#,
    ] {
        assert!(!parse(json).reads_geometry(), "{json}");
    }
}
