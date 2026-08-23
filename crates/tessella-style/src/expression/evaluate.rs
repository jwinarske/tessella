//! Evaluates expression trees.
//!
//! A direct tree walk, which is what mbgl does and what DR-11 replaces with flat bytecode at
//! R1 (§12.1). It is here first because the VM has to reproduce these values, and the golden
//! oracle is what will confirm it does — optimizing something not yet known to be correct is
//! the wrong order.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    ArithmeticOp, AssertKind, CastKind, CompareOp, Expr, FilterTarget, Interpolation,
    LegacyFunction, LegacyKind,
};
use crate::value::Value;

/// The feature an expression is being evaluated against.
///
/// A trait rather than a concrete type because the source of features differs by pipeline
/// stage: a GeoJSON feature at R0, a decoded MVT feature at R1 read in place out of the fetch
/// buffer with no intermediate materialization (§12.2). Both answer these three questions.
pub trait Feature {
    /// A property by name, or `None` when the feature does not have it.
    fn property(&self, key: &str) -> Option<Value>;

    /// `Point`, `LineString` or `Polygon`, as the spec spells them.
    fn geometry_type(&self) -> &str;

    /// The feature id, if it has one.
    fn id(&self) -> Option<Value> {
        None
    }

    /// Every property, as an object.
    fn properties(&self) -> Value {
        Value::Null
    }
}

/// Something an expression could not do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvaluationError {
    /// A value had the wrong type for what was applied to it.
    #[error("expected {expected}, got {got}")]
    Type {
        /// What the operator needed.
        expected: &'static str,
        /// What it was handed.
        got: &'static str,
    },
    /// The expression reads the zoom and none was supplied.
    #[error("expression needs a zoom")]
    MissingZoom,
    /// The expression reads the feature and none was supplied.
    ///
    /// Distinct from a feature that merely lacks the property, which yields null. This is the
    /// error that fires when a data-driven expression was misclassified as camera-only and
    /// evaluated without a feature (DR-11).
    #[error("expression needs a feature")]
    MissingFeature,
    /// Interpolation was asked for between values it cannot interpolate.
    #[error("cannot interpolate between {0}")]
    NotInterpolatable(&'static str),
    /// A coercion failed.
    #[error("cannot convert {got} to {target}")]
    Cast {
        /// What was given.
        got: &'static str,
        /// What was wanted.
        target: &'static str,
    },
}

/// What an evaluation can see.
pub(super) struct Context<'a> {
    /// Current zoom, when there is one.
    pub(super) zoom: Option<f64>,
    /// Current feature, when there is one.
    pub(super) feature: Option<&'a dyn Feature>,
    /// Names bound by enclosing `let`s, innermost first.
    ///
    /// A borrowed chain rather than an owned map: a `let` pushes one frame onto the stack and
    /// hands a reference down, so entering a binding costs nothing and leaving it is a return.
    /// The innermost-first order is what makes shadowing a lookup rule rather than a rewrite.
    pub(super) scope: Option<&'a Binding<'a>>,
}

/// One name bound by a `let`, and the scope it was bound in.
pub(super) struct Binding<'a> {
    name: &'a str,
    value: Value,
    parent: Option<&'a Binding<'a>>,
}

impl Binding<'_> {
    /// The value bound to a name, searching inward-out so the innermost wins.
    fn lookup(&self, name: &str) -> Option<&Value> {
        let mut frame = Some(self);
        while let Some(current) = frame {
            if current.name == name {
                return Some(&current.value);
            }
            frame = current.parent;
        }
        None
    }
}

impl Context<'_> {
    /// A context with neither zoom nor feature, for constant folding.
    pub(super) fn empty() -> Self {
        Self {
            zoom: None,
            feature: None,
            scope: None,
        }
    }

    fn zoom(&self) -> Result<f64, EvaluationError> {
        self.zoom.ok_or(EvaluationError::MissingZoom)
    }

    fn feature(&self) -> Result<&dyn Feature, EvaluationError> {
        self.feature.ok_or(EvaluationError::MissingFeature)
    }
}

/// Evaluates an expression tree.
pub(super) fn evaluate(expr: &Expr, context: &Context<'_>) -> Result<Value, EvaluationError> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Zoom => Ok(Value::Number(context.zoom()?)),
        Expr::GeometryType => Ok(Value::String(
            context.feature()?.geometry_type().to_string(),
        )),
        Expr::Id => Ok(context.feature()?.id().unwrap_or(Value::Null)),
        Expr::Properties => Ok(context.feature()?.properties()),
        Expr::LegacyFunction(function) => evaluate_legacy(function, context),
        Expr::Let { bindings, body } => {
            // Bindings are evaluated once, in order, each seeing the ones before it. Evaluating
            // once is the whole reason a style writes `let` — the alternative, substituting the
            // bound expression at each `var`, would recompute it per use.
            evaluate_let(bindings, body, context)
        }
        Expr::Var(name) => context
            .scope
            .and_then(|scope| scope.lookup(name))
            .cloned()
            // Parsing rejects an unbound name, so reaching here means the tree was built by
            // hand rather than parsed.
            .ok_or(EvaluationError::Type {
                expected: "a bound variable",
                got: "an unbound one",
            }),
        Expr::Assert { kind, args } => {
            // Each argument in turn; the first of the required type wins. Only running out is
            // an error, which is what makes `["number", ["get", "x"], 0]` a fallback rather than
            // a check.
            let mut last = Value::Null;
            for arg in args {
                let value = evaluate(arg, context)?;
                if kind.matches(&value) {
                    return Ok(value);
                }
                last = value;
            }
            Err(EvaluationError::Type {
                expected: kind.type_name(),
                got: last.type_name(),
            })
        }
        Expr::AssertArray {
            item,
            length,
            value,
            fallback,
        } => {
            // With a fallback the assertion is a filter rather than a check: a value that does
            // not satisfy it becomes the fallback instead of an error.
            match (
                check_array(item.as_ref(), *length, value, context),
                fallback,
            ) {
                (Ok(value), _) => Ok(value),
                (Err(_), Some(fallback)) => evaluate(fallback, context),
                (Err(err), None) => Err(err),
            }
        }
        Expr::Get { key, object } => {
            let key = expect_string(&evaluate(key, context)?)?;
            // A property that is not there is null, not an error. Styles rely on this:
            // `["coalesce", ["get", "name_en"], ["get", "name"]]` is idiomatic.
            match object {
                Some(object) => {
                    let target = evaluate(object, context)?;
                    Ok(lookup_in_object(&target, &key)?.unwrap_or(Value::Null))
                }
                None => Ok(context.feature()?.property(&key).unwrap_or(Value::Null)),
            }
        }
        Expr::Has { key, object } => {
            let key = expect_string(&evaluate(key, context)?)?;
            match object {
                Some(object) => {
                    let target = evaluate(object, context)?;
                    Ok(Value::Bool(lookup_in_object(&target, &key)?.is_some()))
                }
                None => Ok(Value::Bool(context.feature()?.property(&key).is_some())),
            }
        }
        Expr::Compare { op, lhs, rhs } => {
            let lhs = evaluate(lhs, context)?;
            let rhs = evaluate(rhs, context)?;
            compare(*op, &lhs, &rhs)
        }
        Expr::Not(inner) => Ok(Value::Bool(!truthy(&evaluate(inner, context)?))),
        Expr::All(args) => {
            for arg in args {
                if !truthy(&evaluate(arg, context)?) {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        }
        Expr::Any(args) => {
            for arg in args {
                if truthy(&evaluate(arg, context)?) {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        }
        Expr::Coalesce(args) => {
            for arg in args {
                let value = evaluate(arg, context)?;
                if value != Value::Null {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        Expr::Match {
            input,
            arms,
            fallback,
        } => {
            let input = evaluate(input, context)?;
            for (labels, output) in arms {
                if labels.iter().any(|label| label == &input) {
                    return evaluate(output, context);
                }
            }
            evaluate(fallback, context)
        }
        Expr::Case { branches, fallback } => {
            for (condition, output) in branches {
                if truthy(&evaluate(condition, context)?) {
                    return evaluate(output, context);
                }
            }
            evaluate(fallback, context)
        }
        Expr::Arithmetic { op, args } => arithmetic(*op, args, context),
        Expr::Cast { to, args } => cast(*to, args, context),
        Expr::Step { input, base, stops } => {
            let position = expect_number(&evaluate(input, context)?)?;
            match locate(stops, position) {
                None => evaluate(base, context),
                Some(index) => evaluate(&stops[index].1, context),
            }
        }
        Expr::Interpolate {
            interpolation,
            input,
            stops,
        } => interpolate(*interpolation, input, stops, context),
        Expr::FilterCompare {
            target,
            op,
            literal,
        } => Ok(Value::Bool(filter_compare(target, *op, literal, context)?)),
        Expr::FilterHas { target } => {
            let feature = context.feature()?;
            Ok(Value::Bool(match target {
                // Every feature has a geometry type, so `["has", "$type"]` is a tautology.
                // mbgl folds it to a literal true, and so does this.
                FilterTarget::Type => true,
                FilterTarget::Id => feature.id().is_some(),
                FilterTarget::Property(key) => feature.property(key).is_some(),
            }))
        }
        Expr::FilterIn { target, values } => {
            let Some(actual) = filter_read(target, context)? else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(values.contains(&actual)))
        }
    }
}

/// Reads what a legacy filter targets, or `None` when the feature does not have it.
fn filter_read(
    target: &FilterTarget,
    context: &Context<'_>,
) -> Result<Option<Value>, EvaluationError> {
    let feature = context.feature()?;
    Ok(match target {
        FilterTarget::Property(key) => feature.property(key),
        FilterTarget::Id => feature.id(),
        FilterTarget::Type => Some(Value::String(feature.geometry_type().to_string())),
    })
}

/// Legacy comparison semantics: absent or mismatched is `false`, never an error.
///
/// The type rule is mbgl's, and it is stricter than it looks. Ordering compares a number
/// against a number or a string against a string, and anything else is `false` rather than a
/// coercion — so `["<", "height", 5]` skips a feature whose `height` is the string `"5"`
/// instead of quietly admitting it.
fn filter_compare(
    target: &FilterTarget,
    op: CompareOp,
    literal: &Value,
    context: &Context<'_>,
) -> Result<bool, EvaluationError> {
    let Some(actual) = filter_read(target, context)? else {
        return Ok(false);
    };

    Ok(match op {
        CompareOp::Eq => actual == *literal,
        // `!=` is `!(==)` in mbgl, which means a feature *missing* the property passes a
        // `!=` filter. That reads as surprising until you notice it is the only choice that
        // keeps `["!=", k, v]` the exact complement of `["==", k, v]`.
        CompareOp::Ne => actual != *literal,
        _ => match (&actual, literal) {
            (Value::Number(a), Value::Number(b)) => order(op, a, b),
            (Value::String(a), Value::String(b)) => order(op, a, b),
            _ => false,
        },
    })
}

fn order<T: PartialOrd>(op: CompareOp, actual: &T, literal: &T) -> bool {
    match op {
        CompareOp::Lt => actual < literal,
        CompareOp::Le => actual <= literal,
        CompareOp::Gt => actual > literal,
        CompareOp::Ge => actual >= literal,
        // Equality is handled before this is reached.
        CompareOp::Eq | CompareOp::Ne => false,
    }
}

/// The index of the last stop at or below `position`, or `None` when it precedes them all.
fn locate(stops: &[(f64, Expr)], position: f64) -> Option<usize> {
    if stops.is_empty() || position < stops[0].0 {
        return None;
    }
    // Stops ascend — the parser rejects any list that does not — so a partition point is
    // exact rather than approximate.
    Some(stops.partition_point(|(stop, _)| *stop <= position) - 1)
}

/// Evaluates a `let` by extending the scope one binding at a time.
///
/// Written recursively because each frame has to outlive the next: the borrowed chain points at
/// stack slots, so a loop would have to own the frames and lose the cheap entry that makes the
/// chain worth having.
fn evaluate_let(
    bindings: &[(String, Expr)],
    body: &Expr,
    context: &Context<'_>,
) -> Result<Value, EvaluationError> {
    let Some(((name, value), rest)) = bindings.split_first() else {
        return evaluate(body, context);
    };
    let bound = Binding {
        name,
        value: evaluate(value, context)?,
        parent: context.scope,
    };
    let inner = Context {
        zoom: context.zoom,
        feature: context.feature,
        scope: Some(&bound),
    };
    evaluate_let(rest, body, &inner)
}

/// Checks an array assertion, returning the value when it holds.
///
/// Separated from the dispatch because the fallback form has to catch the failure rather than
/// propagate it, and matching on a result reads better than writing the checks twice.
fn check_array(
    item: Option<&AssertKind>,
    length: Option<usize>,
    value: &Expr,
    context: &Context<'_>,
) -> Result<Value, EvaluationError> {
    let evaluated = evaluate(value, context)?;
    let Value::Array(items) = &evaluated else {
        return Err(EvaluationError::Type {
            expected: "array",
            got: evaluated.type_name(),
        });
    };
    if let Some(required) = length
        && items.len() != required
    {
        return Err(EvaluationError::Type {
            expected: "array",
            got: "array of the wrong length",
        });
    }
    if let Some(required) = item {
        // Every element, not just the first: the suite has `[1, "b"]` against `array<string>`
        // and expects it to fail on the element that is wrong rather than pass on the one that
        // is right.
        for element in items {
            if !required.matches(element) {
                return Err(EvaluationError::Type {
                    expected: required.type_name(),
                    got: element.type_name(),
                });
            }
        }
    }
    Ok(evaluated)
}

/// Reads a key out of a value that must be an object.
///
/// Unlike a missing key, a non-object *is* an error: `["get", "x", 5]` is a style asking for a
/// property of a number, which no data can make sensible.
fn lookup_in_object(target: &Value, key: &str) -> Result<Option<Value>, EvaluationError> {
    match target {
        Value::Object(members) => Ok(members.get(key).cloned()),
        other => Err(EvaluationError::Type {
            expected: "object",
            got: other.type_name(),
        }),
    }
}

/// Evaluates a pre-expression function.
///
/// # Where the fallback comes from
///
/// A legacy function does not error when nothing matches: it falls back, first to its own
/// `default` and then to the *property spec's*. That is the difference from `match`, which
/// requires a fallback branch and errors without one, and it is why the spec's default has to be
/// carried down here from parse.
fn evaluate_legacy(
    function: &LegacyFunction,
    context: &Context<'_>,
) -> Result<Value, EvaluationError> {
    // The input: a named property, or the zoom for a function without one.
    let input = match &function.property {
        Some(name) => context.feature()?.property(name).unwrap_or(Value::Null),
        None => Value::Number(context.zoom()?),
    };

    let fallback = || function.fallback().cloned().unwrap_or(Value::Null);

    match function.kind {
        // Identity passes the property through. A missing property falls back rather than
        // yielding null, which is what makes `{"type": "identity", "property": "x"}` usable on
        // features that do not all carry `x`.
        LegacyKind::Identity => Ok(if input == Value::Null {
            fallback()
        } else {
            input
        }),

        // Exact equality against each stop input. Types are not coerced: the spec's own suite
        // has a case where the property is the number 0 and the stop is the string "0", and it
        // expects the default.
        LegacyKind::Categorical => Ok(function
            .stops
            .iter()
            .find(|(stop, _)| *stop == input)
            .map_or_else(fallback, |(_, output)| output.clone())),

        // The output of the last stop at or below the input.
        LegacyKind::Interval => {
            let Some(position) = input.as_number() else {
                return Ok(fallback());
            };
            let matched = function
                .stops
                .iter()
                .rfind(|(stop, _)| stop.as_number().is_some_and(|s| s <= position));
            Ok(matched.map_or_else(fallback, |(_, output)| output.clone()))
        }

        LegacyKind::Exponential => {
            let Some(position) = input.as_number() else {
                return Ok(fallback());
            };
            let numeric: Vec<(f64, &Value)> = function
                .stops
                .iter()
                .filter_map(|(stop, output)| stop.as_number().map(|s| (s, output)))
                .collect();
            let Some(first) = numeric.first() else {
                return Ok(fallback());
            };

            // Outside the range the value is clamped, matching `interpolate`.
            if position <= first.0 {
                return Ok(first.1.clone());
            }
            let last = numeric.last().expect("non-empty");
            if position >= last.0 {
                return Ok(last.1.clone());
            }

            let index = numeric
                .iter()
                .rposition(|(stop, _)| *stop <= position)
                .unwrap_or(0);
            let (lower_stop, lower) = numeric[index];
            let (upper_stop, upper) = numeric[index + 1];
            let t = factor(
                Interpolation::Exponential {
                    base: function.base,
                },
                position,
                lower_stop,
                upper_stop,
            );
            // A non-interpolatable output steps rather than blends, which is what the spec does
            // for strings and booleans in an exponential function.
            mix(lower, upper, t).or_else(|_| Ok(lower.clone()))
        }
    }
}

fn interpolate(
    interpolation: Interpolation,
    input: &Expr,
    stops: &[(f64, Expr)],
    context: &Context<'_>,
) -> Result<Value, EvaluationError> {
    let position = expect_number(&evaluate(input, context)?)?;
    let Some(index) = locate(stops, position) else {
        // Below the first stop the value is clamped, not extrapolated.
        return evaluate(&stops[0].1, context);
    };
    if index + 1 >= stops.len() {
        // Above the last stop, likewise.
        return evaluate(&stops[index].1, context);
    }

    let (lower_stop, lower_expr) = &stops[index];
    let (upper_stop, upper_expr) = &stops[index + 1];
    let t = factor(interpolation, position, *lower_stop, *upper_stop);
    let lower = evaluate(lower_expr, context)?;
    let upper = evaluate(upper_expr, context)?;
    mix(&lower, &upper, t)
}

/// The 0..1 position of `position` between two stops, under an interpolation curve.
fn factor(interpolation: Interpolation, position: f64, lower: f64, upper: f64) -> f64 {
    let span = upper - lower;
    if span <= 0.0 {
        return 0.0;
    }
    match interpolation {
        Interpolation::Linear => (position - lower) / span,
        // A base of one is linear, and computing it through the exponential formula would
        // divide by zero rather than degrade gracefully.
        Interpolation::Exponential { base } if (base - 1.0).abs() < f64::EPSILON => {
            (position - lower) / span
        }
        Interpolation::Exponential { base } => {
            let numerator = base.powf(position - lower) - 1.0;
            let denominator = base.powf(span) - 1.0;
            if denominator == 0.0 {
                0.0
            } else {
                numerator / denominator
            }
        }
    }
}

/// Blends two values.
///
/// Numbers and equal-length numeric arrays only. Colors are deliberately absent: a color
/// arrives here as the string the style wrote, and blending in string space is meaningless
/// while blending in the wrong color space is subtly wrong everywhere. Color interpolation
/// belongs with the typed property view, which knows a property is a color, and R0 does not
/// need it — the hermetic style selects colors with `match` rather than interpolating them.
fn mix(lower: &Value, upper: &Value, t: f64) -> Result<Value, EvaluationError> {
    match (lower, upper) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + (b - a) * t)),
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            let mut out = Vec::with_capacity(a.len());
            for (a, b) in a.iter().zip(b) {
                out.push(mix(a, b, t)?);
            }
            Ok(Value::Array(out))
        }
        (Value::String(_), Value::String(_)) => Err(EvaluationError::NotInterpolatable("strings")),
        _ => Err(EvaluationError::NotInterpolatable("values of these types")),
    }
}

fn arithmetic(
    op: ArithmeticOp,
    args: &[Expr],
    context: &Context<'_>,
) -> Result<Value, EvaluationError> {
    let mut numbers = Vec::with_capacity(args.len());
    for arg in args {
        numbers.push(expect_number(&evaluate(arg, context)?)?);
    }

    let value = match op {
        ArithmeticOp::Add => numbers.iter().sum(),
        // One argument is negation, which is how the spec spells unary minus.
        ArithmeticOp::Subtract if numbers.len() == 1 => -numbers[0],
        ArithmeticOp::Subtract => numbers[1..].iter().fold(numbers[0], |acc, n| acc - n),
        ArithmeticOp::Multiply => numbers.iter().product(),
        ArithmeticOp::Divide => numbers[1..].iter().fold(numbers[0], |acc, n| acc / n),
        ArithmeticOp::Modulo => numbers[1..].iter().fold(numbers[0], |acc, n| acc % n),
        ArithmeticOp::Power => numbers[1..].iter().fold(numbers[0], |acc, n| acc.powf(*n)),
        ArithmeticOp::Min => numbers.iter().copied().fold(f64::INFINITY, f64::min),
        ArithmeticOp::Max => numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ArithmeticOp::Abs => numbers[0].abs(),
        ArithmeticOp::Floor => numbers[0].floor(),
        ArithmeticOp::Ceil => numbers[0].ceil(),
        ArithmeticOp::Round => round_half_away(numbers[0]),
    };
    Ok(Value::Number(value))
}

/// Rounds halves away from zero.
///
/// Not `f64::round`'s job description by accident: the style spec rounds -1.5 to -2, and Rust's
/// `round` agrees, but `round_ties_even` does not. Spelling it out keeps the intent visible if
/// anyone reaches for the "obviously equivalent" alternative.
fn round_half_away(value: f64) -> f64 {
    value.round()
}

fn cast(to: CastKind, args: &[Expr], context: &Context<'_>) -> Result<Value, EvaluationError> {
    // The spec tries each argument in turn and takes the first that converts, which is what
    // makes `["to-number", ["get", "x"], 0]` a defaulting idiom rather than a failure.
    let mut last = None;
    for arg in args {
        let value = evaluate(arg, context)?;
        match to {
            CastKind::Boolean => return Ok(Value::Bool(truthy(&value))),
            CastKind::String => return Ok(Value::String(to_string(&value))),
            CastKind::Number => match to_number(&value) {
                Some(number) => return Ok(Value::Number(number)),
                None => last = Some(value.type_name()),
            },
        }
    }
    Err(EvaluationError::Cast {
        got: last.unwrap_or("nothing"),
        target: "number",
    })
}

fn to_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => Some(*number),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        Value::Null => Some(0.0),
        // A string converts only if it is entirely a number, which is the spec's rule and not
        // the same as a leading-numeric-prefix parse.
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => {
            // A whole number renders without a trailing `.0`, as the spec's JSON-ish
            // conversion does; `["concat", ["to-string", 2]]` is "2", never "2.0".
            if number.fract() == 0.0 && number.is_finite() && number.abs() < 1e21 {
                alloc::format!("{}", *number as i64)
            } else {
                alloc::format!("{number}")
            }
        }
        other => alloc::format!("{other:?}"),
    }
}

/// The spec's notion of truth.
///
/// Only `false`, `null`, `0`, `NaN` and the empty string are false. Notably an empty array and
/// an empty object are true, which is the opposite of what several scripting languages do.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => *number != 0.0 && !number.is_nan(),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn compare(op: CompareOp, lhs: &Value, rhs: &Value) -> Result<Value, EvaluationError> {
    let result = match op {
        CompareOp::Eq => lhs == rhs,
        CompareOp::Ne => lhs != rhs,
        _ => {
            // Ordering compares numbers with numbers and strings with strings. Comparing
            // across types has no meaning the spec defines, so it is an error rather than a
            // silent coercion that would make `["<", "10", 9]` quietly true.
            match (lhs, rhs) {
                (Value::Number(a), Value::Number(b)) => match op {
                    CompareOp::Lt => a < b,
                    CompareOp::Le => a <= b,
                    CompareOp::Gt => a > b,
                    _ => a >= b,
                },
                (Value::String(a), Value::String(b)) => match op {
                    CompareOp::Lt => a < b,
                    CompareOp::Le => a <= b,
                    CompareOp::Gt => a > b,
                    _ => a >= b,
                },
                _ => {
                    return Err(EvaluationError::Type {
                        expected: "two numbers or two strings",
                        got: lhs.type_name(),
                    });
                }
            }
        }
    };
    Ok(Value::Bool(result))
}

fn expect_number(value: &Value) -> Result<f64, EvaluationError> {
    value.as_number().ok_or(EvaluationError::Type {
        expected: "number",
        got: value.type_name(),
    })
}

fn expect_string(value: &Value) -> Result<String, EvaluationError> {
    value
        .as_str()
        .map(ToString::to_string)
        .ok_or(EvaluationError::Type {
            expected: "string",
            got: value.type_name(),
        })
}
