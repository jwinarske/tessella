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
    // Written as a curve, because the spec allows zoom only as a curve's input: `["*",
    // ["zoom"], 2]` is rejected, which §12.1 explains — a camera-only expression is cached as
    // interpolation endpoints, and zoom buried in arithmetic has no endpoints to cache.
    assert_eq!(
        dependency(r#"["step", ["zoom"], 0, 10, 20]"#),
        Dependency::Zoom
    );
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
        dependency(r#"["interpolate", ["linear"], ["zoom"], 0, ["get", "scale"], 1, 2]"#),
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

    // And the same for the fallback rather than the arms. Wrapped in a zoom curve because the
    // spec allows zoom only there, so a both-dependencies expression has to be written this
    // way — which is itself the shape §12.1 is built around.
    let fallback =
        r#"["step", ["zoom"], ["match", ["get", "n"], 1, "a", ["get", "kind"]], 10, "x"]"#;
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

/// An unknown operator is named rather than ignored. Silently evaluating it to something
/// plausible would surface as wrong output rather than as a diagnostic.
///
/// The operator here is deliberately fictional. This test used to name a real one that was not
/// implemented yet, and it broke the day that operator landed — which is a test measuring the
/// wrong thing: what is asserted is that unknown names are reported, not which names happen to
/// be unknown today.
#[test]
fn an_unknown_operator_is_reported() {
    let value: Value = serde_json::from_str(r#"["not-an-operator", "a", "b"]"#).unwrap();
    let error = Expression::parse(&value).expect_err("no such operator");
    assert!(format!("{error}").contains("not-an-operator"), "{error}");
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

/// An interpolation the spec does not name is refused, rather than approximated with linear.
///
/// The spec names three — `linear`, `exponential` and `cubic-bezier` — and all three are
/// implemented. What is refused is a fourth, which a style can only get by inventing one or by
/// being written against something that is not this spec. Falling back to linear there would
/// draw a curve nobody asked for and say nothing about it.
#[test]
fn an_unknown_interpolation_is_refused() {
    let value: Value =
        serde_json::from_str(r#"["interpolate", ["ease-in-quint"], ["zoom"], 0, 0, 1, 1]"#)
            .unwrap();
    let error = Expression::parse(&value).expect_err("no such interpolation");
    assert!(format!("{error}").contains("ease-in-quint"), "{error}");
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
        dependency(r#"["let", "a", 1, ["step", ["zoom"], ["var", "a"], 10, 2]]"#),
        Dependency::Zoom
    );
    assert_eq!(
        dependency(r#"["let", "a", ["get", "x"], ["step", ["zoom"], ["var", "a"], 10, 2]]"#),
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
    let red = Value::Color(tessella_style::property::Color {
        r: 1.0,
        g: 0.0,
        b: 0.0,
        a: 1.0,
    });
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
        // Four numbers, which is what the literal holds — the shape is inferred now rather
        // than flattened to "an array".
        Type::Array(tessella_style::expression::ArrayType {
            element: Some(tessella_style::expression::Scalar::Number),
            length: Some(4),
        })
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
        Value::Color(tessella_style::property::Color {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0
        })
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

/// `in`, `index-of`, `slice` and `length` work on both strings and arrays.
#[test]
fn the_membership_family_handles_strings_and_arrays() {
    assert_eq!(
        eval(r#"["in", "low", "helloworld"]"#, None, None),
        Value::Bool(true)
    );
    assert_eq!(
        eval(r#"["in", "foo", "helloworld"]"#, None, None),
        Value::Bool(false)
    );
    assert_eq!(
        eval(r#"["in", 9, ["literal", [9, 8, 7]]]"#, None, None),
        Value::Bool(true)
    );
    assert_eq!(
        eval(r#"["index-of", "low", "helloworld"]"#, None, None),
        Value::Number(3.0)
    );
    assert_eq!(
        eval(r#"["index-of", "zzz", "helloworld"]"#, None, None),
        Value::Number(-1.0),
        "not found is -1, not an error"
    );
    assert_eq!(eval(r#"["length", "abc"]"#, None, None), Value::Number(3.0));
    assert_eq!(
        eval(r#"["length", ["literal", [1, 2]]]"#, None, None),
        Value::Number(2.0)
    );
}

/// Negative indices count from the end, and out-of-range ones clamp.
///
/// Clamping is what makes `["slice", s, -100]` the whole string rather than an error, and an
/// inverted range empty rather than a panic — both are styles asking for something odd, and
/// neither is worth failing a tile over.
#[test]
fn indices_count_from_the_end_and_clamp() {
    assert_eq!(
        eval(r#"["slice", "0123456789", 7]"#, None, None),
        Value::String("789".to_string())
    );
    assert_eq!(
        eval(r#"["slice", "0123456789", -3]"#, None, None),
        Value::String("789".to_string())
    );
    assert_eq!(
        eval(r#"["slice", "0123456789", -100]"#, None, None),
        Value::String("0123456789".to_string())
    );
    assert_eq!(
        eval(r#"["slice", "0123456789", 5, 2]"#, None, None),
        Value::String(String::new()),
        "an inverted range is empty"
    );
    assert_eq!(
        eval(r#"["slice", ["literal", [1, 2, 3, 4, 5]], 2]"#, None, None),
        eval(r#"["literal", [3, 4, 5]]"#, None, None)
    );
}

/// A needle that is not there, or is null, finds nothing rather than erroring.
///
/// Styles write `["in", ["get", "x"], …]` on features that may not carry `x`, so this is the
/// common path rather than the edge case.
#[test]
fn a_missing_needle_finds_nothing() {
    assert_eq!(
        eval(
            r#"["in", ["get", "nope"], "helloworld"]"#,
            None,
            Some(&TestFeature::polygon(vec![]) as &dyn Feature)
        ),
        Value::Bool(false)
    );
    assert_eq!(
        eval(
            r#"["index-of", ["get", "nope"], ["literal", [1, 2]]]"#,
            None,
            Some(&TestFeature::polygon(vec![]) as &dyn Feature)
        ),
        Value::Number(-1.0)
    );
}

/// A legacy function with nothing to fall back to errors rather than yielding null.
///
/// The chain is the function's own default, then the property spec's, then a failure. Null would
/// render as absent, which looks like the feature having no value rather than like the style and
/// the data disagreeing about its type.
#[test]
fn a_legacy_function_with_no_default_errors() {
    use tessella_style::expression::{PropertySpec, Type};

    let spec = PropertySpec {
        default: None,
        expected: Some(Type::Number),
    };
    let value: Value =
        serde_json::from_str(r#"{"type": "identity", "property": "p"}"#).expect("json");
    let parsed = Expression::parse_for(&value, &spec).expect("parses");

    let empty = TestFeature::polygon(vec![]);
    assert!(
        parsed.evaluate(None, Some(&empty as &dyn Feature)).is_err(),
        "no property, no default, so no value"
    );

    let present = TestFeature::polygon(vec![("p", Value::Number(7.0))]);
    assert_eq!(
        parsed
            .evaluate(None, Some(&present as &dyn Feature))
            .expect("evaluates"),
        Value::Number(7.0)
    );
}

/// A default, from either source, is used instead of erroring.
#[test]
fn a_legacy_function_prefers_its_own_default_then_the_propertys() {
    use tessella_style::expression::{PropertySpec, Type};

    let spec = PropertySpec {
        default: Some(Value::Number(-1.0)),
        expected: Some(Type::Number),
    };
    let empty = TestFeature::polygon(vec![]);

    // The property spec's default.
    let from_spec: Value =
        serde_json::from_str(r#"{"type": "identity", "property": "p"}"#).expect("json");
    assert_eq!(
        Expression::parse_for(&from_spec, &spec)
            .expect("parses")
            .evaluate(None, Some(&empty as &dyn Feature))
            .expect("evaluates"),
        Value::Number(-1.0)
    );

    // The function's own default wins over it.
    let from_function: Value =
        serde_json::from_str(r#"{"type": "identity", "property": "p", "default": -2}"#)
            .expect("json");
    assert_eq!(
        Expression::parse_for(&from_function, &spec)
            .expect("parses")
            .evaluate(None, Some(&empty as &dyn Feature))
            .expect("evaluates"),
        Value::Number(-2.0)
    );
}

/// Only scalars can be searched for. An aggregate needle is a question with no answer.
///
/// Reporting "not found" instead would be indistinguishable from a genuine miss, which is the
/// distinction that makes this an error rather than a false.
///
/// Both paths are checked. A constant needle fails at parse, because constant folding evaluates
/// it there and no input could rescue it; a needle read from a feature fails at evaluation,
/// because whether it is an aggregate depends on the data.
#[test]
fn an_aggregate_needle_is_an_error() {
    for text in [
        r#"["in", ["literal", {}], "hello"]"#,
        r#"["in", ["literal", [1]], ["literal", [1, 2]]]"#,
        r#"["index-of", ["literal", {}], "hello"]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} folds to an error at parse"
        );
    }

    let feature = TestFeature::polygon(vec![("needle", Value::Array(vec![Value::Number(1.0)]))]);
    let data_driven = expr(r#"["in", ["get", "needle"], "hello"]"#);
    assert!(
        data_driven
            .evaluate(None, Some(&feature as &dyn Feature))
            .is_err(),
        "and from a feature it fails at evaluation"
    );
}

/// `format` builds sections, each carrying its own font, scale and colour.
///
/// That per-section state is why formatted text is a type rather than a string with markup in
/// it: one label can mix a place name with a smaller elevation in a different face, and R2's
/// shaper needs those as separate runs.
#[test]
fn format_builds_sections() {
    let value = eval(r#"["format", "a", {}, "b", {"font-scale": 2}]"#, None, None);
    let sections = value
        .get("sections")
        .and_then(Value::as_array)
        .expect("sections");
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].get("text"), Some(&Value::String("a".into())));
    assert_eq!(sections[0].get("scale"), Some(&Value::Null));
    assert_eq!(sections[1].get("text"), Some(&Value::String("b".into())));
    assert_eq!(sections[1].get("scale"), Some(&Value::Number(2.0)));

    // Every slot is present on every section, so a consumer never has to distinguish "absent"
    // from "not applicable".
    for section in sections {
        for key in ["text", "image", "scale", "fontStack", "textColor"] {
            assert!(section.get(key).is_some(), "{key} missing");
        }
    }
}

/// Content is coerced to text, so `format` accepts what a style actually writes.
#[test]
fn format_coerces_its_content() {
    let value = eval(r#"["format", 1, {}, true, {}]"#, None, None);
    let sections = value
        .get("sections")
        .and_then(Value::as_array)
        .expect("sections");
    assert_eq!(sections[0].get("text"), Some(&Value::String("1".into())));
    assert_eq!(sections[1].get("text"), Some(&Value::String("true".into())));
}

/// A property the spec types as formatted wraps whatever it got in a single section.
///
/// The same shape as the colour coercion, for the same reason: the style writes a string and the
/// shaper needs sections, so the conversion belongs at that boundary rather than in every
/// operator that might produce text.
#[test]
fn a_formatted_property_wraps_its_result() {
    use tessella_style::expression::{PropertySpec, Type};

    let spec = PropertySpec {
        default: None,
        expected: Some(Type::Formatted),
    };
    let value: Value = serde_json::from_str(r#""a label""#).expect("json");
    let parsed = Expression::parse_for(&value, &spec).expect("parses");
    let result = parsed.evaluate(None, None).expect("evaluates");

    let sections = result
        .get("sections")
        .and_then(Value::as_array)
        .expect("sections");
    assert_eq!(sections.len(), 1);
    assert_eq!(
        sections[0].get("text"),
        Some(&Value::String("a label".into()))
    );

    // And an expression already producing formatted text is left alone rather than nested.
    let already: Value = serde_json::from_str(r#"["format", "x", {}]"#).expect("json");
    let wrapped = Expression::parse_for(&already, &spec)
        .expect("parses")
        .evaluate(None, None)
        .expect("evaluates");
    assert_eq!(
        wrapped
            .get("sections")
            .and_then(Value::as_array)
            .expect("sections")
            .len(),
        1,
        "one section, not a section containing a formatted value"
    );
}

/// `concat` coerces its arguments; `join` does not.
///
/// The asymmetry is the spec's and it is deliberate. `concat` is how a style builds a label out
/// of whatever a feature carries, so coercing is the point. `join` over an array of numbers is a
/// style that has not decided how those numbers should read — one decimal place or three, comma
/// or point — and the spec would rather ask than pick.
#[test]
fn concat_coerces_and_join_does_not() {
    assert_eq!(
        eval(r#"["concat", "a", 1, true]"#, None, None),
        Value::String("a1true".to_string())
    );
    assert_eq!(
        eval(r#"["concat"]"#, None, None),
        Value::String(String::new()),
        "no arguments is the empty string, the identity for concatenation"
    );

    assert_eq!(
        eval(r#"["join", ["literal", ["1", "2", "3"]], "+"]"#, None, None),
        Value::String("1+2+3".to_string())
    );
    assert_eq!(
        eval(r#"["join", ["literal", []], ","]"#, None, None),
        Value::String(String::new())
    );

    // Numbers in the array, and a non-array, are both errors. Constant here, so they are caught
    // at parse by folding.
    for text in [
        r#"["join", ["literal", [1, 2]], "+"]"#,
        r#"["join", "1+2", "+"]"#,
    ] {
        let value: Value = serde_json::from_str(text).expect("json");
        assert!(
            Expression::parse(&value).is_err(),
            "{text} should not parse"
        );
    }
}

/// A colour reports itself as a colour, which is what the spec says and what the static side
/// already believed.
///
/// `Type::Color` has existed since colour-typed properties were coerced, but at runtime a
/// colour was four numbers in a `Value::Array` — indistinguishable from a plain array of the
/// same four numbers, and every evaluation allocated a `Vec` for sixteen bytes of channel.
/// (`typeof` would be the spec's way to ask; it is not implemented yet, so this asks the value.)
#[test]
fn a_colour_is_typed_as_one() {
    for source in [r#"["to-color", "red"]"#, r#"["rgb", 255, 0, 0]"#] {
        assert_eq!(eval(source, None, None).type_name(), "color", "{source}");
    }
    // And is not the array of its channels, which is what it used to be indistinguishable from.
    assert_ne!(
        eval(r#"["rgb", 255, 0, 0]"#, None, None),
        eval(r#"["literal", [1, 0, 0, 1]]"#, None, None)
    );
}

/// Interpolating between colours blends channel-wise, unchanged by the representation.
///
/// The arithmetic goes through `f64` exactly as the four-element array did: blending in `f32`
/// would be a diff on every interpolated colour, and the golden oracle would find it.
#[test]
fn colours_still_blend_channel_wise() {
    let midpoint = eval(
        r#"["interpolate", ["linear"], ["zoom"], 0, ["rgb", 0, 0, 0], 10, ["rgb", 255, 0, 0]]"#,
        Some(5.0),
        None,
    );
    let Value::Color(colour) = midpoint else {
        panic!("a colour, got {midpoint:?}");
    };
    assert!((colour.r - 0.5).abs() < 1e-6, "{colour:?}");
    assert_eq!(colour.g, 0.0);
    assert_eq!(colour.b, 0.0);
    assert_eq!(colour.a, 1.0);
}

/// `cubic-bezier` interpolation, which is CSS easing and mbgl's `util::UnitBezier`.
///
/// The curve is parametric — `x` and `y` are both functions of `t` — and what an interpolation
/// needs is `y` given `x`. A cubic has no closed-form inverse, so mbgl solves numerically:
/// Newton first, then bisection. The bisection is not decoration. Newton stalls where the
/// derivative approaches zero, which is exactly what a curve with a flat start is, and the case
/// below with a control point at the origin exercises it.
mod cubic_bezier {
    use super::*;

    fn eased(spec: &str, at: f64) -> f64 {
        let json = format!(r#"["interpolate", {spec}, ["zoom"], 0, 0, 100, 100]"#);
        let value: Value = serde_json::from_str(&json).expect("valid json");
        let expression = Expression::parse(&value).expect("parses");
        expression
            .evaluate(Some(at), None)
            .expect("evaluates")
            .as_number()
            .expect("a number")
    }

    /// The identity curve is the diagonal, so it must agree with linear everywhere.
    ///
    /// A useful check because it is the one case where the numerical solve has an exact answer
    /// to be measured against: if Newton or the bisection bracket were wrong, this is where the
    /// error would be plainest.
    #[test]
    fn the_identity_curve_is_linear() {
        for at in [0.0, 12.5, 25.0, 50.0, 75.0, 99.0, 100.0] {
            let bezier = eased(r#"["cubic-bezier", 0.0, 0.0, 1.0, 1.0]"#, at);
            assert!(
                (bezier - at).abs() < 1e-4,
                "at {at}: identity bezier gave {bezier}"
            );
        }
    }

    /// CSS `ease-in-out`, against values the curve is defined by rather than values it produced.
    ///
    /// Symmetric about the midpoint, which is the property the control points state, and slower
    /// than linear in the first half.
    #[test]
    fn ease_in_out_is_symmetric_and_slow_at_the_ends() {
        let spec = r#"["cubic-bezier", 0.42, 0.0, 0.58, 1.0]"#;
        assert!(
            (eased(spec, 50.0) - 50.0).abs() < 1e-4,
            "the midpoint is fixed"
        );

        let quarter = eased(spec, 25.0);
        let three_quarters = eased(spec, 75.0);
        assert!(quarter < 25.0, "eased in: {quarter} at a quarter");
        assert!(three_quarters > 75.0, "eased out: {three_quarters}");
        assert!(
            ((100.0 - three_quarters) - quarter).abs() < 1e-4,
            "symmetric: {quarter} vs {}",
            100.0 - three_quarters
        );
    }

    /// The curve is monotonic in x, so the output never goes backwards.
    #[test]
    fn the_output_never_goes_backwards() {
        let spec = r#"["cubic-bezier", 0.0, 0.7, 1.0, 0.3]"#;
        let mut previous = f64::NEG_INFINITY;
        for step in 0..=100 {
            let value = eased(spec, f64::from(step));
            assert!(
                value >= previous - 1e-9,
                "at {step}: {value} after {previous}"
            );
            previous = value;
        }
    }

    /// Four numbers in the unit square, and mbgl checks every one.
    ///
    /// A control point outside it makes a curve that is not a function of x, so solving has no
    /// single answer — which is why this is a parse error rather than a clamp.
    #[test]
    fn a_control_point_outside_the_unit_square_is_refused() {
        for spec in [
            r#"["cubic-bezier", 0.5, 0.0, 1.5, 1.0]"#,
            r#"["cubic-bezier", -0.1, 0.0, 1.0, 1.0]"#,
            r#"["cubic-bezier", 0.5, 0.0, 1.0]"#,
            r#"["cubic-bezier", 0.5, 0.0, 1.0, "x"]"#,
        ] {
            let json = format!(r#"["interpolate", {spec}, ["zoom"], 0, 0, 1, 1]"#);
            let value: Value = serde_json::from_str(&json).expect("valid json");
            assert!(
                Expression::parse(&value).is_err(),
                "{spec} should not parse"
            );
        }
    }
}
