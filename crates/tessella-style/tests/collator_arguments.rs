// SPDX-License-Identifier: Apache-2.0

//! A comparison's collator argument, in a build with the table and in one without.
//!
//! The shape of the argument and the type of what it compares are the spec's rules, not the
//! table's, so both builds enforce them and both name the same fault. Only the last case parts:
//! with the weights the comparison compiles, and without them it is refused by name.
//!
//! This is what keeps the suite honest. The three cases the spec rejects at compile time
//! (`collator/comparison-number-error`, `equals-non-string-error`, `non-object-error`) used to
//! "pass" in a build without the feature because `==` took two arguments and the case handed it
//! three — two unrelated failures agreeing, which would have flipped to a failure the moment the
//! operator was implemented.

use tessella_style::expression::{ParseError, Type};
use tessella_style::{Expression, Value};

fn parse(json: &str) -> Result<Expression, ParseError> {
    let value: Value = serde_json::from_str(json).expect("valid json");
    Expression::parse(&value)
}

/// A collator orders letters, so ordering numbers with one is a category error.
#[test]
fn a_collator_may_not_compare_numbers() {
    for text in [
        r#"["==", 1, 2, ["collator", {"case-sensitive": false}]]"#,
        r#"["<", 1, 2, ["collator", {"case-sensitive": false}]]"#,
    ] {
        assert!(
            matches!(
                parse(text),
                Err(ParseError::CollatorOnNonString(Type::Number))
            ),
            "{text} should name the type it cannot compare, got {:?}",
            parse(text)
        );
    }
}

/// An expression whose type is not yet known may still turn out to be a string.
#[test]
fn a_collator_may_compare_what_is_not_yet_known_to_be_text() {
    let text = r#"["==", ["get", "name"], "Berlin", ["collator", {}]]"#;
    assert!(
        !matches!(parse(text), Err(ParseError::CollatorOnNonString(_))),
        "`get` is a value, not a non-string"
    );
}

/// The options are an object, whatever this build can do with them.
#[test]
fn the_options_must_be_an_object() {
    for text in [
        r#"["==", "a", "b", ["collator", ["subexpression"]]]"#,
        r#"["==", "a", "b", ["collator"]]"#,
        r#"["==", "a", "b", "not-a-collator"]"#,
    ] {
        assert!(
            matches!(parse(text), Err(ParseError::NotACollator)),
            "{text} should be refused for its shape, got {:?}",
            parse(text)
        );
    }
}

/// Six operators take one, and nothing else does.
#[test]
fn only_the_comparisons_take_a_third_argument() {
    for operator in ["==", "!=", "<", "<=", ">", ">="] {
        let text = format!(r#"["{operator}", "a", "b", ["collator", {{}}]]"#);
        assert!(
            !matches!(parse(&text), Err(ParseError::Arity { .. })),
            "{text} should not be an arity fault"
        );
    }
    assert!(
        parse(r#"["in", "a", "b", ["collator", {}]]"#).is_err(),
        "`in` takes no collator"
    );
}

/// A well-formed comparison compiles where the weights are, and is refused by name where they
/// are not. Refusing it is the honest answer: comparing by codepoint would order `a` after `A`
/// and call it a collation.
#[test]
fn a_well_formed_comparison_parts_on_the_feature() {
    let parsed = parse(r#"["==", "a", "A", ["collator", {"case-sensitive": false}]]"#);
    #[cfg(feature = "collator")]
    assert!(parsed.is_ok(), "got {parsed:?}");
    #[cfg(not(feature = "collator"))]
    assert!(
        matches!(parsed, Err(ParseError::CollatorUnavailable)),
        "got {parsed:?}"
    );
}
