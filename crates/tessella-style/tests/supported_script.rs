//! `["is-supported-script", s]`, and the script ranges behind it.
//!
//! # This is the one case where the spec suite is not the authority
//!
//! `tests/expression-suite/is-supported-script/default` expects `true` for Devanagari, and says
//! why in its own description: the JS harness runs without the `isSupportedScript` global, so the
//! operator degrades to a constant `true` there and the real behavior is exercised by render
//! tests instead. mbgl has no such stub -- `util::i18n::isStringInSupportedScript` is compiled in
//! -- and returns `false`. Parity is with mbgl, so that case stays outside the baseline and this
//! file is what pins the behavior.

use tessella_style::expression::{Feature, PropertySpec, Type};
use tessella_style::{Expression, Value, script};

struct Named(&'static str);
impl Feature for Named {
    fn property(&self, key: &str) -> Option<Value> {
        (key == "name").then(|| Value::String(self.0.to_string()))
    }
    fn geometry_type(&self) -> &str {
        "Point"
    }
}

fn ask(text: &'static str) -> Value {
    let value: Value =
        serde_json::from_str(r#"["is-supported-script", ["get", "name"]]"#).expect("json");
    Expression::parse(&value)
        .expect("parses")
        .evaluate(Some(0.0), Some(&Named(text)))
        .expect("evaluates")
}

/// The scripts an SDF atlas can lay out without reordering or contextual forms.
///
/// Arabic and Hebrew are here deliberately: both need *some* shaping, and mbgl admits them
/// anyway. The heuristic is about the scripts it will not attempt, not about the ones it renders
/// perfectly -- getting that backwards would drop half the labels on a world map.
#[test]
fn scripts_this_build_lays_out() {
    for text in [
        "Greifswalder Straße",
        "東京",
        "Ελληνικά",
        "Русский",
        "العربية",
        "שָׁלוֹם",
        "한국어",
        "ไทย",
    ] {
        assert_eq!(ask(text), Value::Bool(true), "{text}");
    }
}

/// And the ones it will not: the complex-shaping blocks mbgl names.
#[test]
fn scripts_that_need_shaping_this_build_does_not_do() {
    for text in ["देवनागरी", "বাংলা", "සිංහල", "བོད་སྐད་", "မြန်မာ", "ខ្មែរ"]
    {
        assert_eq!(ask(text), Value::Bool(false), "{text}");
    }
}

/// One unsupported character is enough, wherever it sits.
///
/// mbgl rejects the whole string on the first failing code unit, so a Latin name with a single
/// Devanagari character in it is not supported. A per-character answer would let a style draw a
/// label that is half-shaped.
#[test]
fn a_string_is_only_as_supported_as_its_worst_character() {
    assert_eq!(ask("Straße"), Value::Bool(true));
    assert_eq!(ask("Straßeद"), Value::Bool(false));
    assert_eq!(ask("दStraße"), Value::Bool(false));
    // Nothing to reject, which is mbgl's answer for an empty string too.
    assert!(script::is_supported(""));
}

/// Every boundary of the three ranges, since they are written down rather than generated.
///
/// One-off errors here are invisible: they move a handful of labels between the two branches of
/// a `case` and nothing fails.
#[test]
fn the_range_boundaries_are_mbgls() {
    for (last_ok, first_bad, last_bad, first_ok) in [
        // Devanagari through Sinhala.
        (0x08FF, 0x0900, 0x0DFF, 0x0E00),
        // Tibetan through Myanmar.
        (0x0EFF, 0x0F00, 0x109F, 0x10A0),
        // Khmer.
        (0x177F, 0x1780, 0x17FF, 0x1800),
    ] {
        assert!(script::char_is_supported(last_ok), "{last_ok:#06X}");
        assert!(!script::char_is_supported(first_bad), "{first_bad:#06X}");
        assert!(!script::char_is_supported(last_bad), "{last_bad:#06X}");
        assert!(script::char_is_supported(first_ok), "{first_ok:#06X}");
    }
}

/// Astral characters are supported, and for the reason mbgl's loop makes them so.
///
/// mbgl walks UTF-16 code units, so it sees a surrogate pair as two code units in 0xD800..0xDFFF,
/// which is in none of the ranges. This walks code points and sees one character above 0xFFFF,
/// which is also in none of them. The two agree, and this is what says so.
#[test]
fn characters_outside_the_basic_plane_are_supported() {
    assert!(script::char_is_supported(0x1_0000));
    assert!(script::char_is_supported(0x1_F600));
    assert_eq!(ask("😀"), Value::Bool(true));
    // A surrogate on its own is not a `char`, so the halves are checked as bare code points.
    assert!(script::char_is_supported(0xD800));
    assert!(script::char_is_supported(0xDFFF));
}

/// It is a boolean, and it depends on the feature when its argument does.
///
/// The classification is what decides whether a layer's `text-field` is evaluated per feature or
/// once; getting it wrong would give every label in a tile the first feature's name.
#[test]
fn it_is_a_feature_dependent_boolean() {
    let value: Value =
        serde_json::from_str(r#"["is-supported-script", ["get", "name"]]"#).expect("json");
    let parsed = Expression::parse(&value).expect("parses");
    assert_eq!(parsed.result_type(), Type::Boolean);
    assert!(
        parsed.dependency().needs_feature(),
        "reads a feature property"
    );

    let constant: Value = serde_json::from_str(r#"["is-supported-script", "abc"]"#).expect("json");
    let parsed = Expression::parse(&constant).expect("parses");
    assert!(parsed.dependency().is_constant(), "reads nothing");
    assert_eq!(
        parsed.evaluate(None, None).expect("evaluates"),
        Value::Bool(true)
    );
}

/// A boolean-typed property takes it directly, which is how a style actually uses it.
#[test]
fn it_satisfies_a_boolean_property() {
    let value: Value = serde_json::from_str(r#"["is-supported-script", "abc"]"#).expect("json");
    let spec = PropertySpec {
        default: None,
        expected: Some(Type::Boolean),
    };
    assert!(Expression::parse_for(&value, &spec).is_ok());
}

/// Arity is one, and a non-string argument is an error rather than a silent `false`.
#[test]
fn it_takes_exactly_one_string() {
    for src in [
        r#"["is-supported-script"]"#,
        r#"["is-supported-script", "a", "b"]"#,
    ] {
        let value: Value = serde_json::from_str(src).expect("json");
        assert!(Expression::parse(&value).is_err(), "{src}");
    }
    // A constant argument is folded at parse, so the type error surfaces there.
    let numeric: Value = serde_json::from_str(r#"["is-supported-script", 5]"#).expect("json");
    assert!(
        Expression::parse(&numeric).is_err(),
        "a number is not a script"
    );

    // A feature-dependent one cannot be folded, so it surfaces at evaluation instead. Both are
    // errors rather than a silent `false`: a style branching on this would take the wrong arm.
    struct Numeric;
    impl Feature for Numeric {
        fn property(&self, key: &str) -> Option<Value> {
            (key == "name").then_some(Value::Number(5.0))
        }
        fn geometry_type(&self) -> &str {
            "Point"
        }
    }
    let from_feature: Value =
        serde_json::from_str(r#"["is-supported-script", ["get", "name"]]"#).expect("json");
    let parsed = Expression::parse(&from_feature).expect("parses");
    assert!(parsed.evaluate(Some(0.0), Some(&Numeric)).is_err());
}
