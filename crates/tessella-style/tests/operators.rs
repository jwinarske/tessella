//! The expression operator registry, and the classification it decides.

use tessella_style::generated::operators::OPERATORS;
use tessella_style::{PropertyValue, Value, is_operator};

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("a value")
}

fn property(json: &str) -> PropertyValue {
    serde_json::from_str(json).expect("a property")
}

/// The table is sorted and unique, which `is_operator`'s binary search relies on.
#[test]
fn the_registry_is_sorted_and_unique() {
    let mut sorted = OPERATORS.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted.as_slice(), OPERATORS.as_slice(), "sorted");
    sorted.dedup();
    assert_eq!(sorted.len(), OPERATORS.len(), "unique");
    assert!(OPERATORS.len() > 60, "{} operators", OPERATORS.len());
}

/// Every operator in the table answers to `is_operator`, and nothing else does.
#[test]
fn is_operator_agrees_with_the_table() {
    for name in OPERATORS {
        assert!(is_operator(name), "{name}");
    }
    for name in ["Noto Sans Regular", "gett", "", "Zoom", "filter-in", "GET"] {
        assert!(!is_operator(name), "{name}");
    }
}

/// mbgl's internal filter spelling is not a style operator.
///
/// `filter-in` and its siblings exist only inside mbgl, generated when a legacy filter is
/// converted. A style document never contains one, and admitting them would let
/// `["filter-in", ...]` — a perfectly legal literal array of strings — parse as a call.
#[test]
fn converted_filter_operators_are_excluded() {
    assert!(OPERATORS.iter().all(|name| !name.starts_with("filter-")));
}

/// The operators a real style actually uses are all present.
///
/// A spot check that the extraction did not silently take one registry and miss the other:
/// `get` and `zoom` come from the compound registry, `interpolate` and `match` from the special
/// forms.
#[test]
fn the_registry_covers_both_halves() {
    for name in [
        "get", "zoom", "has", "concat", "+", "min", "rgba", "to-rgba", "typeof",
    ] {
        assert!(is_operator(name), "compound: {name}");
    }
    for name in [
        "interpolate",
        "match",
        "step",
        "case",
        "coalesce",
        "let",
        "var",
        "literal",
        "==",
        "all",
    ] {
        assert!(is_operator(name), "special form: {name}");
    }
}

/// A font stack is a literal, not a call to an operator named after its first font.
///
/// The case the registry exists for. `text-font` is an `array<string>`, and
/// `["Noto Sans Regular"]` is how essentially every style writes one — indistinguishable by
/// shape from a call. Classified as an expression, its fonts cannot be read, and a style's
/// labels lose their glyphs.
#[test]
fn a_font_stack_is_a_literal() {
    let stack = property(r#"["Noto Sans Regular", "Arial Unicode MS Regular"]"#);
    assert!(
        stack.as_literal().is_some(),
        "classified as {stack:?}, not a literal"
    );
    assert_eq!(
        stack
            .as_literal()
            .and_then(Value::as_array)
            .map(<[Value]>::len),
        Some(2)
    );
}

/// A real call is still a call.
#[test]
fn an_operator_headed_array_is_an_expression() {
    for json in [
        r#"["get", "kind"]"#,
        r#"["interpolate", ["linear"], ["zoom"], 10, 1, 16, 4]"#,
        r#"["match", ["get", "x"], "a", 1, 2]"#,
        r#"["literal", ["Noto Sans Regular"]]"#,
        r#"["==", ["get", "a"], 1]"#,
    ] {
        assert!(property(json).as_expression().is_some(), "{json}");
        assert!(parse(json).looks_like_expression(), "{json}");
    }
}

/// An array of numbers was never a call and still is not.
///
/// A `line-dasharray` or a colour triple. This worked before the registry — the head is not a
/// string — and the test stays because it is the other half of the classification.
#[test]
fn an_array_of_numbers_is_a_literal() {
    for json in [r"[2, 4]", r"[255, 0, 0, 1]", r"[]"] {
        assert!(!parse(json).looks_like_expression(), "{json}");
    }
}

/// A misspelled operator is still reported, rather than becoming an array of strings.
///
/// The registry makes `["gett", "x"]` a legal literal *as a value*, which is right. But a
/// caller that asked to parse an expression has said what it expects, and the only way an
/// unrecognized head reaches that call is a typo. The spec catches these by type-checking
/// against the property the value was written for; nothing here knows the property.
#[test]
fn a_misspelled_operator_is_still_reported() {
    let value = parse(r#"["gett", "kind"]"#);
    assert!(!value.looks_like_expression(), "as a value it is a literal");

    let error = tessella_style::expression::Expression::parse(&value)
        .expect_err("as an expression it is a typo");
    assert!(format!("{error}").contains("gett"), "{error}");
}
