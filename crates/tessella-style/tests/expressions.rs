//! Expression parsing, classification and evaluation.
//!
//! Classification carries most of the weight here. A misclassification is not a wrong pixel,
//! which is what makes it dangerous: calling a data-driven expression camera-only gives every
//! feature in a layer the first feature's value, and calling a camera-only one data-driven
//! merely makes the port look inherently slow (DR-11).

use tessella_style::expression::Feature;
use tessella_style::{Dependency, Expression, Value};

fn expr(json: &str) -> Expression {
    let value: Value = serde_json::from_str(json).expect("valid json");
    Expression::parse(&value).expect("parses")
}

fn dependency(json: &str) -> Dependency {
    expr(json).dependency()
}

struct TestFeature {
    kind: &'static str,
    properties: Vec<(&'static str, Value)>,
}

impl TestFeature {
    fn polygon(properties: Vec<(&'static str, Value)>) -> Self {
        Self {
            kind: "Polygon",
            properties,
        }
    }
}

impl Feature for TestFeature {
    fn property(&self, key: &str) -> Option<Value> {
        self.properties
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.clone())
    }

    fn geometry_type(&self) -> &str {
        self.kind
    }
}

fn eval(json: &str, zoom: Option<f64>, feature: Option<&dyn Feature>) -> Value {
    expr(json).evaluate(zoom, feature).expect("evaluates")
}

// --- classification ---

#[test]
fn literals_are_constant() {
    assert_eq!(dependency("3"), Dependency::None);
    assert_eq!(dependency(r#""a""#), Dependency::None);
    assert_eq!(dependency(r#"["literal", [1, 2]]"#), Dependency::None);
    assert_eq!(dependency(r#"["+", 1, 2]"#), Dependency::None);
}

#[test]
fn zoom_makes_an_expression_camera_only() {
    assert_eq!(dependency(r#"["zoom"]"#), Dependency::Zoom);
    assert_eq!(
        dependency(r#"["interpolate", ["linear"], ["zoom"], 10, 1, 16, 4]"#),
        Dependency::Zoom
    );
    assert_eq!(dependency(r#"["*", ["zoom"], 2]"#), Dependency::Zoom);
}

#[test]
fn feature_access_makes_an_expression_data_driven() {
    assert_eq!(dependency(r#"["get", "kind"]"#), Dependency::Feature);
    assert_eq!(dependency(r#"["has", "kind"]"#), Dependency::Feature);
    assert_eq!(dependency(r#"["geometry-type"]"#), Dependency::Feature);
    assert_eq!(dependency(r#"["id"]"#), Dependency::Feature);
    assert_eq!(dependency(r#"["properties"]"#), Dependency::Feature);
}

#[test]
fn zoom_and_feature_together_join_to_both() {
    assert_eq!(
        dependency(r#"["*", ["zoom"], ["get", "scale"]]"#),
        Dependency::ZoomAndFeature
    );
    assert_eq!(
        dependency(r#"["interpolate", ["linear"], ["zoom"], 10, ["get", "a"], 16, 4]"#),
        Dependency::ZoomAndFeature
    );
}

/// The classification describes the expression, not one evaluation of it. An arm that happens
/// not to be taken for a given feature still makes the expression data-driven — otherwise the
/// property would be bound as a uniform and every feature would share one value.
#[test]
fn an_untaken_branch_still_counts() {
    let match_expr = r#"["match", 1, 1, "constant", ["get", "kind"]]"#;
    assert_eq!(dependency(match_expr), Dependency::Feature);

    let case_expr = r#"["case", false, ["get", "kind"], "fallback"]"#;
    assert_eq!(dependency(case_expr), Dependency::Feature);

    // And the same for the fallback rather than the arms.
    let fallback = r#"["match", ["zoom"], 1, "a", ["get", "kind"]]"#;
    assert_eq!(dependency(fallback), Dependency::ZoomAndFeature);
}

/// `get` reads the feature even when its key is a constant, which is the common case and the
/// easy one to get wrong by classifying on the arguments alone.
#[test]
fn get_depends_on_the_feature_even_with_a_constant_key() {
    assert_eq!(dependency(r#"["get", "kind"]"#), Dependency::Feature);
}

#[test]
fn the_dependency_lattice_joins_correctly() {
    use Dependency::{Feature, None, Zoom, ZoomAndFeature};
    assert_eq!(None.join(None), None);
    assert_eq!(None.join(Zoom), Zoom);
    assert_eq!(Zoom.join(None), Zoom);
    assert_eq!(Zoom.join(Zoom), Zoom);
    assert_eq!(Feature.join(Feature), Feature);
    assert_eq!(Zoom.join(Feature), ZoomAndFeature);
    assert_eq!(Feature.join(Zoom), ZoomAndFeature);
    assert_eq!(ZoomAndFeature.join(None), ZoomAndFeature);

    assert!(None.is_constant());
    assert!(!Zoom.is_constant());
    assert!(Zoom.needs_zoom() && !Zoom.needs_feature());
    assert!(Feature.needs_feature() && !Feature.needs_zoom());
    assert!(ZoomAndFeature.needs_zoom() && ZoomAndFeature.needs_feature());
}

/// Constant folding is DR-11's first tier: a property that depends on nothing is evaluated
/// once, at parse, and never reaches the evaluator again.
#[test]
fn constants_fold() {
    assert_eq!(
        expr(r#"["+", 2, 3]"#).as_constant(),
        Some(Value::Number(5.0))
    );
    assert_eq!(
        expr(r#"["match", "b", "a", 1, "b", 2, 0]"#).as_constant(),
        Some(Value::Number(2.0))
    );
    assert_eq!(expr(r#"["zoom"]"#).as_constant(), None);
    assert_eq!(expr(r#"["get", "x"]"#).as_constant(), None);
}

// --- evaluation ---

#[test]
fn evaluates_the_hermetic_styles_match() {
    // Straight from the probe style's fill-datadriven layer.
    let color = r##"["match", ["get", "kind"], "a", "#c04030", "#3050c0"]"##;
    let a = TestFeature::polygon(vec![("kind", Value::String("a".into()))]);
    let b = TestFeature::polygon(vec![("kind", Value::String("b".into()))]);
    let none = TestFeature::polygon(vec![]);

    assert_eq!(eval(color, None, Some(&a)), Value::String("#c04030".into()));
    assert_eq!(eval(color, None, Some(&b)), Value::String("#3050c0".into()));
    assert_eq!(
        eval(color, None, Some(&none)),
        Value::String("#3050c0".into()),
        "a missing property falls through to the fallback"
    );
}

/// A property the feature does not carry is null, not an error. `coalesce` over `get` is
/// idiomatic and would break otherwise.
#[test]
fn a_missing_property_is_null() {
    let feature = TestFeature::polygon(vec![("name", Value::String("x".into()))]);
    assert_eq!(
        eval(r#"["get", "absent"]"#, None, Some(&feature)),
        Value::Null
    );
    assert_eq!(
        eval(
            r#"["coalesce", ["get", "absent"], ["get", "name"]]"#,
            None,
            Some(&feature)
        ),
        Value::String("x".into())
    );
    assert_eq!(
        eval(r#"["has", "absent"]"#, None, Some(&feature)),
        Value::Bool(false)
    );
}

/// Evaluating a data-driven expression with no feature is an error rather than a default.
/// This is the failure a DR-11 misclassification produces, and it must be loud.
#[test]
fn a_data_driven_expression_without_a_feature_fails() {
    let result = expr(r#"["get", "kind"]"#).evaluate(Some(13.0), None);
    assert!(result.is_err(), "must not silently yield null");

    let result = expr(r#"["zoom"]"#).evaluate(None, None);
    assert!(result.is_err(), "must not silently yield zero");
}

#[test]
fn interpolates_linearly_and_clamps_outside_the_stops() {
    let e = r#"["interpolate", ["linear"], ["zoom"], 10, 0, 20, 100]"#;
    assert_eq!(eval(e, Some(10.0), None), Value::Number(0.0));
    assert_eq!(eval(e, Some(15.0), None), Value::Number(50.0));
    assert_eq!(eval(e, Some(20.0), None), Value::Number(100.0));
    // Clamped, not extrapolated.
    assert_eq!(eval(e, Some(0.0), None), Value::Number(0.0));
    assert_eq!(eval(e, Some(99.0), None), Value::Number(100.0));
}

/// An exponential base of one is linear. Computing it through the exponential formula divides
/// by zero, so it is special-cased rather than left to produce NaN.
#[test]
fn an_exponential_base_of_one_is_linear() {
    let exponential = r#"["interpolate", ["exponential", 1], ["zoom"], 0, 0, 10, 10]"#;
    let linear = r#"["interpolate", ["linear"], ["zoom"], 0, 0, 10, 10]"#;
    for zoom in [0.0, 2.5, 5.0, 7.5, 10.0] {
        assert_eq!(
            eval(exponential, Some(zoom), None),
            eval(linear, Some(zoom), None),
            "base 1 must match linear at zoom {zoom}"
        );
    }
}

#[test]
fn steps_hold_their_value_until_the_next_stop() {
    let e = r#"["step", ["zoom"], "small", 10, "medium", 15, "large"]"#;
    assert_eq!(eval(e, Some(0.0), None), Value::String("small".into()));
    assert_eq!(eval(e, Some(9.99), None), Value::String("small".into()));
    assert_eq!(eval(e, Some(10.0), None), Value::String("medium".into()));
    assert_eq!(eval(e, Some(14.9), None), Value::String("medium".into()));
    assert_eq!(eval(e, Some(15.0), None), Value::String("large".into()));
    assert_eq!(eval(e, Some(99.0), None), Value::String("large".into()));
}

/// Only false, null, zero, NaN and the empty string are false. An empty array is true, which
/// is where several scripting languages disagree with the spec.
#[test]
fn truthiness_follows_the_spec() {
    assert_eq!(eval(r#"["!", false]"#, None, None), Value::Bool(true));
    assert_eq!(eval(r#"["!", 0]"#, None, None), Value::Bool(true));
    assert_eq!(eval(r#"["!", ""]"#, None, None), Value::Bool(true));
    assert_eq!(eval(r#"["!", null]"#, None, None), Value::Bool(true));

    assert_eq!(eval(r#"["!", 1]"#, None, None), Value::Bool(false));
    assert_eq!(eval(r#"["!", "x"]"#, None, None), Value::Bool(false));
    assert_eq!(
        eval(r#"["!", ["literal", []]]"#, None, None),
        Value::Bool(false),
        "an empty array is true"
    );
}

/// Comparing across types is a *compile* error, not a coercion and not an evaluation error.
///
/// This test used to assert evaluation errors, and one of its assertions — that `["==", "10",
/// 10]` is simply false — was wrong against the spec, which rejects a comparison whose operand
/// types are both known and different. The style-spec suite says so directly, and catching it at
/// parse is the better place regardless: a style with this mistake in it is broken for every
/// feature, so reporting it once at load beats reporting it per feature per tile forever.
#[test]
fn comparing_across_known_types_is_a_compile_error() {
    for text in [
        r#"["<", "10", 9]"#,
        r#"["==", "10", 10]"#,
        r#"["<", true, false]"#,
        r#"["<", null, null]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("valid json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} should not parse"
        );
    }

    assert_eq!(eval(r#"["<", 9, 10]"#, None, None), Value::Bool(true));
    assert_eq!(eval(r#"["<", "a", "b"]"#, None, None), Value::Bool(true));
    assert_eq!(eval(r#"["==", 10, 10]"#, None, None), Value::Bool(true));
}

/// Comparing unknowns is allowed, because the unknown might match.
///
/// This is the other half of the rule and the half that keeps it usable: a feature property can
/// be anything, so `["==", ["get", "a"], ["get", "b"]]` could well be true and rejecting it
/// would reject most real styles. The checker rejects what cannot succeed, not what it cannot
/// prove.
#[test]
fn comparing_unknown_types_is_allowed() {
    for text in [
        r#"["==", ["get", "a"], ["get", "b"]]"#,
        r#"["<", ["get", "a"], 5]"#,
        r#"["==", ["get", "a"], "literal"]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("valid json");
        assert!(Expression::parse(&value).is_ok(), "{text} should parse");
    }
}

#[test]
fn arithmetic_matches_the_spec() {
    assert_eq!(eval(r#"["+", 1, 2, 3]"#, None, None), Value::Number(6.0));
    assert_eq!(eval(r#"["-", 5]"#, None, None), Value::Number(-5.0));
    assert_eq!(eval(r#"["-", 5, 2]"#, None, None), Value::Number(3.0));
    assert_eq!(eval(r#"["*", 2, 3, 4]"#, None, None), Value::Number(24.0));
    assert_eq!(eval(r#"["/", 10, 4]"#, None, None), Value::Number(2.5));
    assert_eq!(eval(r#"["%", 10, 3]"#, None, None), Value::Number(1.0));
    assert_eq!(eval(r#"["^", 2, 10]"#, None, None), Value::Number(1024.0));
    assert_eq!(eval(r#"["min", 3, 1, 2]"#, None, None), Value::Number(1.0));
    assert_eq!(eval(r#"["max", 3, 1, 2]"#, None, None), Value::Number(3.0));
    assert_eq!(eval(r#"["floor", 1.7]"#, None, None), Value::Number(1.0));
    assert_eq!(eval(r#"["ceil", 1.2]"#, None, None), Value::Number(2.0));
    // Halves round away from zero, not to even.
    assert_eq!(eval(r#"["round", 1.5]"#, None, None), Value::Number(2.0));
    assert_eq!(eval(r#"["round", 2.5]"#, None, None), Value::Number(3.0));
    assert_eq!(eval(r#"["round", -1.5]"#, None, None), Value::Number(-2.0));
}

/// A whole number stringifies without a trailing `.0`.
#[test]
fn number_to_string_drops_a_trailing_zero() {
    assert_eq!(
        eval(r#"["to-string", 2]"#, None, None),
        Value::String("2".into())
    );
    assert_eq!(
        eval(r#"["to-string", 2.5]"#, None, None),
        Value::String("2.5".into())
    );
}

/// `to-number` tries each argument in turn, which is what makes it a defaulting idiom.
#[test]
fn to_number_falls_through_to_a_default() {
    let feature = TestFeature::polygon(vec![("n", Value::String("nope".into()))]);
    assert_eq!(
        eval(r#"["to-number", ["get", "n"], 7]"#, None, Some(&feature)),
        Value::Number(7.0)
    );
    assert_eq!(
        eval(r#"["to-number", "42"]"#, None, None),
        Value::Number(42.0)
    );
}

// --- parse errors ---

/// An unimplemented operator is named rather than ignored. Silently evaluating it to something
/// plausible would surface as wrong output rather than as a diagnostic.
#[test]
fn an_unknown_operator_is_reported() {
    let value: Value = serde_json::from_str(r#"["concat", "a", "b"]"#).unwrap();
    let error = Expression::parse(&value).expect_err("concat is not implemented");
    assert!(format!("{error}").contains("concat"), "{error}");
}

#[test]
fn wrong_arity_is_reported() {
    for bad in [
        r#"["get"]"#,
        r#"["get", "a", "b"]"#,
        r#"["zoom", 1]"#,
        r#"["==", 1]"#,
        r#"["match", 1, 2]"#,
        r#"["case", true]"#,
    ] {
        let value: Value = serde_json::from_str(bad).unwrap();
        assert!(Expression::parse(&value).is_err(), "{bad} should not parse");
    }
}

/// Both `interpolate` and `step` locate a stop by binary search, so an out-of-order stop list
/// would return an arbitrary neighbour: a wrong value from a style that looks reasonable.
#[test]
fn out_of_order_stops_are_rejected() {
    for bad in [
        r#"["interpolate", ["linear"], ["zoom"], 16, 1, 10, 4]"#,
        r#"["step", ["zoom"], "a", 15, "b", 10, "c"]"#,
        r#"["interpolate", ["linear"], ["zoom"], 10, 1, 10, 4]"#,
    ] {
        let value: Value = serde_json::from_str(bad).unwrap();
        assert!(
            Expression::parse(&value).is_err(),
            "{bad} has non-ascending stops"
        );
    }
}

/// cubic-bezier is in the spec and not implemented. Approximating it with linear would be a
/// silently wrong curve, so it is refused by name.
#[test]
fn unimplemented_interpolation_is_refused() {
    let value: Value = serde_json::from_str(
        r#"["interpolate", ["cubic-bezier", 0, 0, 1, 1], ["zoom"], 0, 0, 1, 1]"#,
    )
    .unwrap();
    let error = Expression::parse(&value).expect_err("cubic-bezier is not implemented");
    assert!(format!("{error}").contains("cubic-bezier"), "{error}");
}

/// `literal` is how a style writes data that would otherwise read as a call.
#[test]
fn literal_protects_data_from_being_parsed() {
    let e = expr(r#"["literal", ["get", "not-a-call"]]"#);
    assert_eq!(e.dependency(), Dependency::None);
    assert_eq!(
        e.as_constant(),
        Some(Value::Array(vec![
            Value::String("get".into()),
            Value::String("not-a-call".into())
        ]))
    );
}

/// `let` binds names its body can read, and an inner binding shadows an outer one.
#[test]
fn let_binds_and_shadows() {
    assert_eq!(
        eval(r#"["let", "a", 2, ["*", ["var", "a"], 3]]"#, None, None),
        Value::Number(6.0)
    );
    assert_eq!(
        eval(
            r#"["let", "a", 1, ["let", "a", 2, ["var", "a"]]]"#,
            None,
            None
        ),
        Value::Number(2.0),
        "the inner binding wins"
    );
    assert_eq!(
        eval(
            r#"["let", "a", 1, ["+", ["let", "a", 2, ["var", "a"]], ["var", "a"]]]"#,
            None,
            None
        ),
        Value::Number(3.0),
        "and the outer one is intact once the inner scope ends"
    );
}

/// A later binding can read an earlier one; nothing can read itself.
#[test]
fn a_binding_sees_the_ones_before_it() {
    assert_eq!(
        eval(
            r#"["let", "a", 2, "b", ["*", ["var", "a"], 5], ["var", "b"]]"#,
            None,
            None
        ),
        Value::Number(10.0)
    );

    let value: Value = serde_json::from_str(r#"["let", "a", ["var", "a"], 1]"#).expect("json");
    assert!(
        Expression::parse(&value).is_err(),
        "a binding must not read itself"
    );
}

/// An unbound name is a compile error, not a null at evaluation.
///
/// The difference matters at the scale styles run at: a style with this mistake is broken for
/// every feature of every tile, so one message at load beats a wrong value per feature forever.
#[test]
fn an_unbound_variable_is_a_compile_error() {
    for text in [
        r#"["var", "nope"]"#,
        r#"["let", "a", 1, ["var", "b"]]"#,
        r#"["+", 1, ["var", "a"]]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} should not parse"
        );
    }
}

/// A variable name looks like an identifier. The suite rejects `$a`.
#[test]
fn binding_names_are_identifiers() {
    for name in ["$a", "1a", "a-b", "", "a b"] {
        let text = format!(r#"["let", "{name}", 1, ["var", "{name}"]]"#);
        let value: Value = serde_json::from_str(&text).expect("json");
        assert!(Expression::parse(&value).is_err(), "{name} should not bind");
    }
    for name in ["a", "_a", "a1", "snake_case"] {
        let text = format!(r#"["let", "{name}", 1, ["var", "{name}"]]"#);
        let value: Value = serde_json::from_str(&text).expect("json");
        assert!(Expression::parse(&value).is_ok(), "{name} should bind");
    }
}

/// A `let`'s dependency is its bindings' as well as its body's.
///
/// Taking only the body would classify `["let", "a", ["get", "x"], ["var", "a"]]` as constant,
/// which is the misclassification that gives every feature in a layer the first one's value —
/// the failure this file's header is about, arriving through a binding form.
#[test]
fn a_let_inherits_its_bindings_dependencies() {
    assert_eq!(
        dependency(r#"["let", "a", ["get", "x"], ["var", "a"]]"#),
        Dependency::Feature
    );
    assert_eq!(
        dependency(r#"["let", "a", ["zoom"], ["var", "a"]]"#),
        Dependency::Zoom
    );
    assert_eq!(
        dependency(
            r#"["let", "a", ["get", "x"], "b", ["zoom"], ["+", ["var", "a"], ["var", "b"]]]"#
        ),
        Dependency::ZoomAndFeature
    );
    assert_eq!(
        dependency(r#"["let", "a", 1, ["var", "a"]]"#),
        Dependency::None,
        "a constant binding stays constant"
    );
}

/// A constant expression that cannot be evaluated is rejected at parse.
///
/// A constant has exactly one value. If computing it fails, no input could have helped, so the
/// failure belongs to the style rather than to the data — and reporting it at load beats the
/// same failure once per feature per tile, forever, for a mistake visible on sight.
#[test]
fn a_constant_that_cannot_evaluate_is_a_parse_error() {
    for text in [
        r#"["number", ["get", "x", ["literal", {"y": 0}]]]"#,
        r#"["number", "not a number"]"#,
        r#"["array", "number", ["literal", ["a"]]]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} should not parse"
        );
    }
}

/// The same expression over data is *not* rejected, because data might make it work.
#[test]
fn a_data_driven_expression_is_not_folded() {
    for text in [
        r#"["number", ["get", "x"]]"#,
        r#"["array", "number", ["get", "xs"]]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(Expression::parse(&value).is_ok(), "{text} should parse");
    }
}

/// `get` with a second argument reads that object, and stops depending on the feature.
///
/// Classifying it feature-driven anyway would be safe for correctness and wrong for cost: a
/// lookup in a literal table would be re-evaluated per feature forever.
#[test]
fn get_from_an_object_does_not_read_the_feature() {
    assert_eq!(
        dependency(r#"["get", "a", ["literal", {"a": 1}]]"#),
        Dependency::None
    );
    assert_eq!(dependency(r#"["get", "a"]"#), Dependency::Feature);
    assert_eq!(
        eval(r#"["get", "a", ["literal", {"a": 7}]]"#, None, None),
        Value::Number(7.0)
    );
    assert_eq!(
        eval(r#"["get", "b", ["literal", {"a": 7}]]"#, None, None),
        Value::Null,
        "a missing key is null, as it is on a feature"
    );
}

/// Variadic arithmetic with no arguments is its identity, not an error.
#[test]
fn variadic_arithmetic_folds_to_its_identity() {
    assert_eq!(eval(r#"["+"]"#, None, None), Value::Number(0.0));
    assert_eq!(eval(r#"["*"]"#, None, None), Value::Number(1.0));
    assert_eq!(eval(r#"["min"]"#, None, None), Value::Number(f64::INFINITY));
    assert_eq!(
        eval(r#"["max"]"#, None, None),
        Value::Number(f64::NEG_INFINITY)
    );

    // The others have no identity to return: they are missing an operand.
    for text in [r#"["-"]"#, r#"["/"]"#, r#"["floor"]"#, r#"["abs"]"#] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} should not parse"
        );
    }
}

/// `to-string` on an array or object produces JSON, not Rust's debug form.
///
/// This was a real defect: the fallback arm was `format!("{value:?}")`, so a style doing
/// `["to-string", ["get", "tags"]]` rendered `Array([Number(1.0)])` onto the map. Wrong output
/// that looks like a crash report is still wrong output, and no type would have caught it.
#[test]
fn to_string_serializes_aggregates_as_json() {
    assert_eq!(
        eval(r#"["to-string", ["literal", [1, 2]]]"#, None, None),
        Value::String("[1,2]".to_string())
    );
    assert_eq!(
        eval(r#"["to-string", ["literal", {"y": 1}]]"#, None, None),
        Value::String(r#"{"y":1}"#.to_string())
    );
    assert_eq!(
        eval(r#"["to-string", ["literal", ["a\"b"]]]"#, None, None),
        Value::String(r#"["a\"b"]"#.to_string()),
        "and quotes are escaped"
    );
    assert_eq!(
        eval(r#"["to-string", ["literal", []]]"#, None, None),
        Value::String("[]".to_string())
    );
}

/// `to-color` reads CSS strings and 0..255 channel arrays, and errors on neither.
#[test]
fn to_color_converts_strings_and_channel_arrays() {
    let red = Value::Array(vec![
        Value::Number(1.0),
        Value::Number(0.0),
        Value::Number(0.0),
        Value::Number(1.0),
    ]);
    assert_eq!(eval(r#"["to-color", "red"]"#, None, None), red);
    assert_eq!(
        eval(r#"["to-color", ["literal", [255, 0, 0, 1]]]"#, None, None),
        red,
        "array channels are 0..255"
    );
    assert_eq!(eval(r#"["to-color", ["rgb", 255, 0, 0]]"#, None, None), red);

    // Fallbacks, as with the other casts.
    assert_eq!(
        eval(r#"["to-color", "not a colour", "red"]"#, None, None),
        red
    );
}

/// A colour is not the array it looks like, and the difference is static.
///
/// `["to-color", ["rgba", …]]` is a pass-through while `["to-color", [0, 255, 0, 1]]` rescales.
/// Getting this wrong is not subtle: converting an already-normalized colour reads its channels
/// as 0..255 and darkens it by a factor of 255.
#[test]
fn a_colour_is_not_an_array_of_numbers() {
    use tessella_style::expression::Type;

    let colour: Value = serde_json::from_str(r#"["rgba", 0, 255, 0, 1]"#).expect("json");
    assert_eq!(
        Expression::parse(&colour).expect("parses").result_type(),
        Type::Color
    );

    let array: Value = serde_json::from_str(r#"["literal", [0, 255, 0, 1]]"#).expect("json");
    assert_eq!(
        Expression::parse(&array).expect("parses").result_type(),
        Type::Array
    );

    // Converting a colour twice must not change it.
    assert_eq!(
        eval(r#"["to-color", ["to-color", "lime"]]"#, None, None),
        eval(r#"["to-color", "lime"]"#, None, None)
    );
}

/// A property the spec types as a colour has its result coerced, wherever the value came from.
#[test]
fn a_colour_property_coerces_its_result() {
    use tessella_style::expression::{PropertySpec, Type};

    let spec = PropertySpec {
        default: None,
        expected: Some(Type::Color),
    };
    let value: Value = serde_json::from_str(r#""red""#).expect("json");
    let parsed = Expression::parse_for(&value, &spec).expect("parses");
    assert_eq!(
        parsed.evaluate(None, None).expect("evaluates"),
        Value::Array(vec![
            Value::Number(1.0),
            Value::Number(0.0),
            Value::Number(0.0),
            Value::Number(1.0),
        ])
    );

    // And an expression already producing a colour is left alone rather than rescaled.
    let already: Value = serde_json::from_str(r#"["rgba", 255, 0, 0, 1]"#).expect("json");
    assert_eq!(
        Expression::parse_for(&already, &spec)
            .expect("parses")
            .evaluate(None, None)
            .expect("evaluates"),
        parsed.evaluate(None, None).expect("evaluates")
    );
}
