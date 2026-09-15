//! Reading a `color-relief-color` expression as a ramp.
//!
//! A heatmap's ramp is baked into 256 texels because its parameter is a density in `0..1` -- a
//! fixed domain, so a fixed sampling loses nothing. An elevation has no fixed domain, so the
//! shader is handed the stops themselves and searches them. These are the rules that decides
//! which stops it gets.

use tessella_style::expression::{Expression, PropertySpec, Type};
use tessella_style::{Value, ramp};

/// Parsed the way a paint property is, which is the only way these parse at all.
///
/// `Expression::parse` constant-folds, and folding `["elevation"]` asks for a parameter nothing
/// has supplied yet -- so a relief ramp parsed bare fails with the error the *evaluator* raises
/// outside a ramp. It also decides that an `interpolate` over string outputs is not
/// interpolatable, because without an expected type it has no reason to read `"#ff0000"` as a
/// color. Both go away when the expected type is given, which is what `resolve_paint` does from
/// the property's own spec.
fn ramp_expression(json: &str, expected: Type) -> Expression {
    let value: Value = serde_json::from_str(json).expect("json");
    Expression::parse_for(
        &value,
        &PropertySpec {
            default: None,
            expected: Some(expected),
        },
    )
    .expect("parses")
}

/// A `color-relief-color` written as an interpolate gives up its own stops, one texel each.
#[test]
fn an_interpolate_relief_ramp_is_its_own_stops() {
    let expression = ramp_expression(
        r##"["interpolate",["linear"],["elevation"],
             0,"#000000", 150,"#ff0000", 750,"#ffffff"]"##,
        Type::Color,
    );
    let ramp = ramp::relief_ramp(&expression).expect("bakes");

    assert_eq!(ramp.len(), 3);
    assert_eq!(ramp.elevations, [0.0, 150.0, 750.0]);
    // The color at each stop is the expression evaluated there, which for a stop whose output is
    // a literal is that literal.
    assert!(
        (ramp.colors[1].r - 1.0).abs() < 1e-6,
        "{:?}",
        ramp.colors[1]
    );
    assert!(ramp.colors[1].g.abs() < 1e-6);
    assert!((ramp.colors[2].b - 1.0).abs() < 1e-6);
}

/// Anything that is not an interpolate is sampled over mbgl's fixed window instead.
///
/// The window matters: a relief ramp has no bounded domain the way a heatmap's density does, so
/// there is nothing to normalize against and mbgl picks a range rather than deriving one.
#[test]
fn a_non_interpolate_relief_ramp_falls_back_to_the_fixed_window() {
    let expression = ramp_expression(
        r##"["step",["elevation"],"#000000", 400,"#ffffff"]"##,
        Type::Color,
    );
    let ramp = ramp::relief_ramp(&expression).expect("bakes");

    assert_eq!(ramp.len(), ramp::RELIEF_FALLBACK_SAMPLES);
    assert_eq!(ramp.elevations[0], ramp::RELIEF_FALLBACK_RANGE.0);
    assert_eq!(
        ramp.elevations[ramp::RELIEF_FALLBACK_SAMPLES - 1],
        ramp::RELIEF_FALLBACK_RANGE.1
    );
    // The step is at 400 m, so the low samples are black and the high ones white.
    assert!(ramp.colors[0].r.abs() < 1e-6);
    assert!((ramp.colors[ramp::RELIEF_FALLBACK_SAMPLES - 1].r - 1.0).abs() < 1e-6);
}

/// `["elevation"]` reads the same slot `["heatmap-density"]` does, which is mbgl's own choice:
/// `elevationCompoundExpression` returns `*(params.colorRampParameter)`.
#[test]
fn elevation_reads_the_ramp_parameter() {
    let expression = ramp_expression(r#"["elevation"]"#, Type::Number);
    let value = expression
        .evaluate_at(None, None, None, None, None, Some(1234.5))
        .expect("evaluates");
    assert_eq!(value, tessella_style::Value::Number(1234.5));

    // And outside a ramp there is nothing to read, which is an error rather than a zero.
    assert!(
        expression
            .evaluate_at(None, None, None, None, None, None)
            .is_err()
    );
}

/// A ramp whose stops are expressions is evaluated at each elevation rather than read off the
/// stop, which is the case where the two differ and mbgl takes the evaluation.
#[test]
fn a_stops_color_is_the_expression_evaluated_there() {
    let expression = ramp_expression(
        r##"["interpolate",["linear"],["elevation"],
             0, ["to-color","#102030"],
             100, ["to-color","#405060"]]"##,
        Type::Color,
    );
    let ramp = ramp::relief_ramp(&expression).expect("bakes");
    assert_eq!(ramp.len(), 2);
    assert!(
        (ramp.colors[0].r - 16.0 / 255.0).abs() < 1e-5,
        "{:?}",
        ramp.colors[0]
    );
    assert!(
        (ramp.colors[1].b - 96.0 / 255.0).abs() < 1e-5,
        "{:?}",
        ramp.colors[1]
    );
}
