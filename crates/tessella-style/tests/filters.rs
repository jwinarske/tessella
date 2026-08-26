//! Feature filters, both syntaxes.
//!
//! The rules under test are mbgl's, because mbgl is the golden oracle: a filter that admits a
//! different set of features produces a different bucket, which produces a different vertex
//! buffer, which fails the diff two removes from the actual disagreement.

use tessella_style::expression::Feature;
use tessella_style::{Dependency, Filter, Value};

fn filter(json: &str) -> Filter {
    let value: Value = serde_json::from_str(json).expect("valid json");
    Filter::parse(&value).expect("parses")
}

struct TestFeature {
    kind: &'static str,
    id: Option<Value>,
    properties: Vec<(&'static str, Value)>,
}

impl TestFeature {
    fn new(kind: &'static str, properties: Vec<(&'static str, Value)>) -> Self {
        Self {
            kind,
            id: None,
            properties,
        }
    }

    fn with_id(mut self, id: Value) -> Self {
        self.id = Some(id);
        self
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

    fn id(&self) -> Option<Value> {
        self.id.clone()
    }
}

fn polygon(properties: Vec<(&'static str, Value)>) -> TestFeature {
    TestFeature::new("Polygon", properties)
}

fn num(n: f64) -> Value {
    Value::Number(n)
}

fn text(s: &str) -> Value {
    Value::String(s.into())
}

// --- the hermetic style's filter ---

/// Exactly what the probe style uses on its fill and line layers. If this is wrong, no bucket
/// the Rust side builds can match the oracle.
#[test]
fn the_hermetic_styles_type_filter_works() {
    let f = filter(r#"["==", "$type", "Polygon"]"#);
    assert!(f.matches(&polygon(vec![]), None));
    assert!(!f.matches(&TestFeature::new("LineString", vec![]), None));
    assert!(!f.matches(&TestFeature::new("Point", vec![]), None));
}

// --- discriminating the two syntaxes ---

/// The two syntaxes overlap, and what separates them is where an array or a string appears
/// rather than anything structural.
#[test]
fn legacy_and_modern_filters_are_told_apart() {
    // Legacy: three elements, no nesting.
    let legacy = filter(r#"["==", "kind", "a"]"#);
    // Modern: a nested call on the left.
    let modern = filter(r#"["==", ["get", "kind"], "a"]"#);

    let a = polygon(vec![("kind", text("a"))]);
    assert!(legacy.matches(&a, None));
    assert!(modern.matches(&a, None));

    // And they diverge exactly where they should: on a feature without the property, the
    // legacy form is false and the modern form errors, which `matches` also reports as false.
    let missing = polygon(vec![]);
    assert!(!legacy.matches(&missing, None));
    assert!(!modern.matches(&missing, None));
}

/// `["has", "$id"]` has no expression equivalent, so it is legacy even though modern `has`
/// shares the syntax.
#[test]
fn has_on_a_pseudo_property_is_legacy() {
    let with_id = polygon(vec![]).with_id(num(7.0));
    let without = polygon(vec![]);

    let f = filter(r#"["has", "$id"]"#);
    assert!(f.matches(&with_id, None));
    assert!(!f.matches(&without, None));

    // Every feature has a geometry type, so this is a tautology.
    let f = filter(r#"["has", "$type"]"#);
    assert!(f.matches(&without, None));
}

// --- legacy semantics ---

/// Legacy filters are total. They run over every feature in a tile, and one odd feature must
/// not fail the tile, so a missing property is `false` rather than an error.
#[test]
fn a_missing_property_is_false_not_an_error() {
    let missing = polygon(vec![("other", num(1.0))]);
    assert!(!filter(r#"["==", "kind", "a"]"#).matches(&missing, None));
    assert!(!filter(r#"["<", "height", 5]"#).matches(&missing, None));
    assert!(!filter(r#"["in", "kind", "a", "b"]"#).matches(&missing, None));
    assert!(!filter(r#"["has", "kind"]"#).matches(&missing, None));
}

/// `!=` is the exact complement of `==`, which means a feature *missing* the property passes
/// it. Surprising until you notice it is the only reading that keeps the two complementary.
#[test]
fn not_equal_is_the_complement_of_equal() {
    let missing = polygon(vec![]);
    let matching = polygon(vec![("kind", text("a"))]);
    let other = polygon(vec![("kind", text("b"))]);

    for feature in [&missing, &matching, &other] {
        let eq = filter(r#"["==", "kind", "a"]"#).matches(feature, None);
        let ne = filter(r#"["!=", "kind", "a"]"#).matches(feature, None);
        assert_ne!(eq, ne, "== and != must partition every feature");
    }

    assert!(filter(r#"["!=", "kind", "a"]"#).matches(&missing, None));
}

/// Ordering compares a number with a number or a string with a string. Anything else is
/// false rather than a coercion, so a `height` of `"5"` does not quietly pass `["<", ..., 10]`.
#[test]
fn ordering_requires_matching_types() {
    let numeric = polygon(vec![("height", num(5.0))]);
    let stringy = polygon(vec![("height", text("5"))]);

    assert!(filter(r#"["<", "height", 10]"#).matches(&numeric, None));
    assert!(!filter(r#"["<", "height", 10]"#).matches(&stringy, None));
    assert!(!filter(r#"[">", "height", 10]"#).matches(&numeric, None));
    assert!(filter(r#"[">=", "height", 5]"#).matches(&numeric, None));
    assert!(filter(r#"["<=", "height", 5]"#).matches(&numeric, None));

    // Strings order against strings.
    let named = polygon(vec![("name", text("b"))]);
    assert!(filter(r#"["<", "name", "c"]"#).matches(&named, None));
    assert!(!filter(r#"["<", "name", "a"]"#).matches(&named, None));
}

#[test]
fn membership_and_its_negation() {
    let a = polygon(vec![("kind", text("a"))]);
    let z = polygon(vec![("kind", text("z"))]);

    assert!(filter(r#"["in", "kind", "a", "b"]"#).matches(&a, None));
    assert!(!filter(r#"["in", "kind", "a", "b"]"#).matches(&z, None));
    assert!(!filter(r#"["!in", "kind", "a", "b"]"#).matches(&a, None));
    assert!(filter(r#"["!in", "kind", "a", "b"]"#).matches(&z, None));

    // `$type` membership.
    let f = filter(r#"["in", "$type", "Polygon", "LineString"]"#);
    assert!(f.matches(&polygon(vec![]), None));
    assert!(!f.matches(&TestFeature::new("Point", vec![]), None));
}

#[test]
fn combinators_compose() {
    let a = polygon(vec![("kind", text("a")), ("height", num(9.0))]);

    assert!(filter(r#"["all", ["==", "kind", "a"], ["<", "height", 10]]"#).matches(&a, None));
    assert!(!filter(r#"["all", ["==", "kind", "a"], ["<", "height", 5]]"#).matches(&a, None));
    assert!(filter(r#"["any", ["==", "kind", "z"], ["<", "height", 10]]"#).matches(&a, None));
    assert!(!filter(r#"["none", ["==", "kind", "a"]]"#).matches(&a, None));
    assert!(filter(r#"["none", ["==", "kind", "z"]]"#).matches(&a, None));
}

/// An operator with no operands. `["any"]` is false because nothing satisfies it; `["all"]`
/// and `["none"]` are true because nothing contradicts them.
#[test]
fn empty_combinators_have_the_identity_value() {
    let feature = polygon(vec![]);
    assert!(!filter(r#"["any"]"#).matches(&feature, None));
    assert!(filter(r#"["all"]"#).matches(&feature, None));
    assert!(filter(r#"["none"]"#).matches(&feature, None));
}

#[test]
fn id_filters_read_the_feature_id() {
    let seven = polygon(vec![]).with_id(num(7.0));
    let eight = polygon(vec![]).with_id(num(8.0));
    let anonymous = polygon(vec![]);

    assert!(filter(r#"["==", "$id", 7]"#).matches(&seven, None));
    assert!(!filter(r#"["==", "$id", 7]"#).matches(&eight, None));
    assert!(!filter(r#"["==", "$id", 7]"#).matches(&anonymous, None));
    assert!(filter(r#"["<", "$id", 8]"#).matches(&seven, None));
}

/// An unrecognized operator is reported, not admitted.
///
/// mbgl's legacy converter has a permissive fallback that returns a literal true, which looks
/// like a misspelled filter would quietly admit everything. It does not, and the reason is the
/// dispatch above it: `isExpression` returns true for any operator it does not specifically
/// know, so an unrecognized name goes to the expression parser and fails there. Every operator
/// that reaches the legacy converter is one `isExpression` deliberately rejected, and all of
/// those are handled by name. The fallback is unreachable.
///
/// Worth a test because the permissive branch is right there in the source and reads like the
/// observable behavior.
#[test]
fn an_unrecognized_operator_is_reported() {
    let value: Value = serde_json::from_str(r#"["equals", "kind", "a"]"#).unwrap();
    let error = Filter::parse(&value).expect_err("`equals` is not an operator");
    assert!(format!("{error}").contains("equals"), "{error}");
}

/// Ordering a geometry type has no operator in mbgl and therefore no defined meaning.
/// Refused rather than invented.
#[test]
fn ordering_a_geometry_type_is_refused() {
    let value: Value = serde_json::from_str(r#"["<", "$type", "Polygon"]"#).unwrap();
    assert!(Filter::parse(&value).is_err());
}

#[test]
fn within_is_refused_by_name() {
    let value: Value = serde_json::from_str(r#"["within", {"type": "Polygon"}]"#).unwrap();
    let error = Filter::parse(&value).expect_err("within is not implemented");
    assert!(format!("{error}").contains("within"), "{error}");
}

// --- modern filters ---

#[test]
fn modern_expression_filters_evaluate() {
    let a = polygon(vec![("kind", text("a")), ("height", num(9.0))]);

    assert!(filter(r#"["==", ["geometry-type"], "Polygon"]"#).matches(&a, None));
    assert!(filter(r#"["all", ["has", "kind"], [">", ["get", "height"], 5]]"#).matches(&a, None));
    assert!(!filter(r#"["all", ["has", "kind"], [">", ["get", "height"], 50]]"#).matches(&a, None));
}

/// A modern filter that errors on a particular feature is describing a feature it cannot
/// classify, which is a feature it should not admit.
#[test]
fn a_modern_filter_that_errors_rejects_the_feature() {
    let stringy = polygon(vec![("height", text("tall"))]);
    // `>` between a string and a number is an expression error, not false.
    assert!(!filter(r#"[">", ["get", "height"], 5]"#).matches(&stringy, None));
}

/// Every filter reads the feature, so every filter classifies as data-driven. A filter that
/// classified as constant would be folded away and stop filtering.
#[test]
fn filters_classify_as_data_driven() {
    for source in [
        r#"["==", "$type", "Polygon"]"#,
        r#"["==", "kind", "a"]"#,
        r#"["in", "kind", "a"]"#,
        r#"["has", "kind"]"#,
        r#"["all", ["==", "kind", "a"]]"#,
        r#"["==", ["get", "kind"], "a"]"#,
    ] {
        assert_eq!(
            filter(source).expression().dependency(),
            Dependency::Feature,
            "{source}"
        );
    }
}

#[test]
fn a_layer_without_a_filter_admits_everything() {
    assert!(Filter::always().matches(&polygon(vec![]), None));
    assert!(Filter::always().matches(&TestFeature::new("Point", vec![]), None));
}

/// mbgl's `Filter.ID`, assertion for assertion.
///
/// `$id` is not a property and the comparisons on it are type-strict, and both halves are easy
/// to get subtly wrong. A style filtering `["==", "$id", 1234]` against a source whose ids are
/// strings must match nothing rather than everything, and a feature carrying a *property* named
/// `id` must not answer to `$id` — the two live in different namespaces, so conflating them
/// makes a filter that works on one source silently select the wrong features on another.
mod mbgl_id {
    use super::{TestFeature, filter, num};
    use tessella_style::Value;

    fn with_number_id() -> TestFeature {
        TestFeature::new("Point", vec![]).with_id(num(1234.0))
    }

    fn with_string_id() -> TestFeature {
        TestFeature::new("Point", vec![]).with_id(Value::String("1".to_string()))
    }

    /// Equality on `$id` compares the type as well as the value.
    #[test]
    fn equality_on_an_id_is_type_strict() {
        assert!(filter(r#"["==", "$id", 1234]"#).matches(&with_number_id(), None));
        assert!(
            !filter(r#"["==", "$id", "1234"]"#).matches(&with_number_id(), None),
            "a numeric id matched a string"
        );
    }

    /// A property called `id` is not `$id`.
    #[test]
    fn a_property_named_id_is_not_the_feature_id() {
        let by_property = TestFeature::new("Point", vec![("id", num(1234.0))]);
        assert!(
            !filter(r#"["==", "$id", 1234]"#).matches(&by_property, None),
            "a property named id answered to $id"
        );
        assert!(!filter(r#"["==", "$id", "1234"]"#).matches(&by_property, None));
    }

    /// Ordering a numeric id, at and around the boundary.
    #[test]
    fn ordering_a_numeric_id() {
        let feature = with_number_id();
        let cases: [(&str, bool); 10] = [
            (r#"["<", "$id", 0]"#, false),
            (r#"["<", "$id", 1234]"#, false),
            (r#"["<=", "$id", 1234]"#, true),
            (r#"["<", "$id", 123]"#, false),
            (r#"["<=", "$id", 123]"#, false),
            (r#"[">", "$id", 0]"#, true),
            (r#"[">", "$id", 123]"#, true),
            (r#"[">=", "$id", 123]"#, true),
            (r#"[">", "$id", 1234]"#, false),
            (r#"[">=", "$id", 1234]"#, true),
        ];
        for (json, expected) in cases {
            assert_eq!(
                filter(json).matches(&feature, None),
                expected,
                "{json} against id 1234"
            );
        }
    }

    /// Ordering across types is false in every direction, rather than coercing.
    ///
    /// The case that separates a type-strict comparison from a permissive one: a coercing
    /// implementation answers *true* to one of the four and false to the others, so any single
    /// assertion here can pass by luck.
    #[test]
    fn ordering_across_types_is_always_false() {
        for json in [
            r#"[">", "$id", "1"]"#,
            r#"["<", "$id", "1"]"#,
            r#"[">=", "$id", "1"]"#,
            r#"["<=", "$id", "1"]"#,
        ] {
            assert!(
                !filter(json).matches(&with_number_id(), None),
                "{json} coerced a number against a string"
            );
        }

        for json in [
            r#"[">", "$id", 1]"#,
            r#"["<", "$id", 1]"#,
            r#"[">=", "$id", 1]"#,
            r#"["<=", "$id", 1]"#,
        ] {
            assert!(
                !filter(json).matches(&with_string_id(), None),
                "{json} coerced a string against a number"
            );
        }
    }

    /// A string id orders lexicographically, not numerically.
    ///
    /// `"1" < "012"` is false and `"1" < "1234"` is true, which is only so under string
    /// ordering — a numeric reading gives the opposite for the first.
    #[test]
    fn a_string_id_orders_as_a_string() {
        let feature = with_string_id();
        let cases: [(&str, bool); 10] = [
            (r#"["<", "$id", "0"]"#, false),
            (r#"["<", "$id", "1234"]"#, true),
            (r#"["<=", "$id", "1234"]"#, true),
            (r#"["<", "$id", "012"]"#, false),
            (r#"["<=", "$id", "012"]"#, false),
            (r#"[">", "$id", "0"]"#, true),
            (r#"[">", "$id", "234"]"#, false),
            (r#"[">=", "$id", "012"]"#, true),
            (r#"[">", "$id", "1234"]"#, false),
            (r#"[">=", "$id", "1234"]"#, false),
        ];
        for (json, expected) in cases {
            assert_eq!(
                filter(json).matches(&feature, None),
                expected,
                "{json} against id \"1\""
            );
        }
    }

    /// A feature with no id matches nothing that asks about one.
    #[test]
    fn a_feature_with_no_id_matches_nothing() {
        let anonymous = TestFeature::new("Point", vec![]);
        for json in [
            r#"["==", "$id", 1234]"#,
            r#"["!=", "$id", 1234]"#,
            r#"[">", "$id", 0]"#,
            r#"["has", "$id"]"#,
        ] {
            let matched = filter(json).matches(&anonymous, None);
            assert!(
                !matched || json.starts_with(r#"["!="#),
                "{json} matched a feature with no id"
            );
        }
    }
}

/// A filter may put a zoom curve anywhere. A property may not.
///
/// The rule is mbgl's `parseLayerPropertyExpression`, and mbgl's `Converter<Filter>` calls
/// `parseExpression`, which does not apply it. What the rule is *for* says why: a property is
/// evaluated once per zoom interval and interpolated between the endpoints, which needs one
/// identifiable curve to take endpoints from; a filter is evaluated per feature at the tile's
/// own zoom and never interpolated.
///
/// `["all", …, ["step", ["zoom"], …]]` is the ordinary way a road layer drops footways above a
/// zoom, and twenty-three layers of one real vendor style are written that way — so applying
/// the property rule here refused the style over its own house style.
#[test]
fn a_filter_may_bury_a_zoom_curve() {
    let json = r#"["all",
        ["==", ["get", "class"], "path"],
        ["step", ["zoom"], false, 16, true]]"#;
    let compiled = filter(json);
    assert_eq!(
        compiled.expression().dependency(),
        Dependency::ZoomAndFeature
    );

    let path = TestFeature::new("LineString", vec![("class", Value::String("path".into()))]);
    assert!(!compiled.matches(&path, Some(14.0)), "below the step");
    assert!(compiled.matches(&path, Some(17.0)), "above the step");
}

/// And the same expression as a *property* is still refused.
///
/// Relaxing the filter path must not relax the property path with it: the endpoint machinery
/// has no way to find a curve buried under an `all`, so accepting one there would silently
/// evaluate the property at a single zoom and never interpolate it.
#[test]
fn a_property_may_not() {
    let json = r#"["all", ["step", ["zoom"], false, 16, true]]"#;
    let value: Value = serde_json::from_str(json).expect("valid json");
    assert!(tessella_style::Expression::parse(&value).is_err());
}

/// A zoom filter given no zoom admits nothing, which is why the builders pass the tile's.
///
/// Not a rule so much as the consequence of one: `["zoom"]` outside a zoom context is an
/// evaluation error, and a filter that errors matches no feature. A builder that forgot the
/// zoom would therefore draw the layer at no zoom at all rather than at the wrong ones — a
/// silent blank rather than a visible mistake, which is why it is asserted.
#[test]
fn a_zoom_filter_without_a_zoom_admits_nothing() {
    let compiled = filter(r#"["step", ["zoom"], false, 16, true]"#);
    let feature = TestFeature::new("LineString", vec![]);
    assert!(!compiled.matches(&feature, None));
    assert!(compiled.matches(&feature, Some(17.0)));
}
