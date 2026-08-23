//! Turns style values into expression trees.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    ArithmeticOp, AssertKind, CastKind, CompareOp, Expr, Interpolation, LegacyFunction, LegacyKind,
};
use crate::value::Value;

/// A style value that is not a well-formed expression.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The operator name is not implemented.
    ///
    /// Named rather than ignored: a style using an operator this build lacks would otherwise
    /// silently evaluate to something plausible, and the difference would surface as wrong
    /// output rather than as a diagnostic.
    #[error("unknown expression operator `{0}`")]
    UnknownOperator(String),
    /// The operator was given the wrong number of arguments.
    #[error("`{operator}` expects {expected}, got {got}")]
    Arity {
        /// Operator name.
        operator: String,
        /// What it wanted.
        expected: String,
        /// What it got.
        got: usize,
    },
    /// An argument was the wrong shape.
    #[error("`{operator}`: {detail}")]
    Malformed {
        /// Operator name.
        operator: String,
        /// What was wrong.
        detail: String,
    },
    /// The value is an array with a non-string head, so it is not an expression at all.
    #[error("not an expression: an expression is an array whose first element names an operator")]
    NotAnExpression,
}

/// Parses a style value into an expression tree.
pub(super) fn parse(value: &Value) -> Result<Expr, ParseError> {
    parse_with_default(value, None)
}

/// Parses a value, carrying the property spec's default for pre-expression functions.
pub(super) fn parse_with_default(
    value: &Value,
    property_default: Option<Value>,
) -> Result<Expr, ParseError> {
    // A pre-expression function is an object, which `looks_like_expression` does not recognize,
    // so without this check it falls through to `Expr::Literal` and a style that varies a
    // property by zoom silently gets the raw JSON object as the value. That is worse than an
    // error: it renders as a broken colour rather than as a message.
    if let Some(function) = parse_legacy_function(value, property_default)? {
        return Ok(function);
    }

    // A value that is not a call is itself. This is what makes `["match", x, "a", 1, 2]`
    // work: the outputs are bare values, not nested calls.
    if !value.looks_like_expression() {
        return Ok(Expr::Literal(value.clone()));
    }

    let items = value.as_array().ok_or(ParseError::NotAnExpression)?;
    let operator = items[0].as_str().ok_or(ParseError::NotAnExpression)?;
    let args = &items[1..];

    match operator {
        // `literal` is how a style writes an array or object that would otherwise be read as a
        // call. Its argument is data by definition and is never parsed further.
        "literal" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Literal(args[0].clone()))
        }
        "number" | "string" | "boolean" | "object" => {
            let kind = match operator {
                "number" => AssertKind::Number,
                "string" => AssertKind::String,
                "boolean" => AssertKind::Boolean,
                _ => AssertKind::Object,
            };
            // At least the value; any further arguments are fallbacks tried in order.
            expect_arity(operator, args, 1, usize::MAX)?;
            Ok(Expr::Assert {
                kind,
                args: parse_all(args)?,
            })
        }
        "array" => parse_array_assertion(operator, args),
        "zoom" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::Zoom)
        }
        "geometry-type" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::GeometryType)
        }
        "id" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::Id)
        }
        "properties" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::Properties)
        }
        "get" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Get {
                key: Box::new(parse(&args[0])?),
            })
        }
        "has" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Has {
                key: Box::new(parse(&args[0])?),
            })
        }
        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
            expect_arity(operator, args, 2, 2)?;
            let op = match operator {
                "==" => CompareOp::Eq,
                "!=" => CompareOp::Ne,
                "<" => CompareOp::Lt,
                "<=" => CompareOp::Le,
                ">" => CompareOp::Gt,
                _ => CompareOp::Ge,
            };
            let lhs = parse(&args[0])?;
            let rhs = parse(&args[1])?;
            check_comparable(operator, op, &lhs, &rhs)?;
            Ok(Expr::Compare {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "!" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Not(Box::new(parse(&args[0])?)))
        }
        "all" => Ok(Expr::All(parse_all(args)?)),
        "any" => Ok(Expr::Any(parse_all(args)?)),
        "coalesce" => Ok(Expr::Coalesce(parse_all(args)?)),
        "match" => parse_match(operator, args),
        "case" => parse_case(operator, args),
        "step" => parse_step(operator, args),
        "interpolate" => parse_interpolate(operator, args),
        "to-number" => Ok(Expr::Cast {
            to: CastKind::Number,
            args: parse_all(args)?,
        }),
        "to-string" => Ok(Expr::Cast {
            to: CastKind::String,
            args: parse_all(args)?,
        }),
        "to-boolean" => Ok(Expr::Cast {
            to: CastKind::Boolean,
            args: parse_all(args)?,
        }),
        "+" | "-" | "*" | "/" | "%" | "^" | "min" | "max" | "abs" | "floor" | "ceil" | "round" => {
            let op = match operator {
                "+" => ArithmeticOp::Add,
                "-" => ArithmeticOp::Subtract,
                "*" => ArithmeticOp::Multiply,
                "/" => ArithmeticOp::Divide,
                "%" => ArithmeticOp::Modulo,
                "^" => ArithmeticOp::Power,
                "min" => ArithmeticOp::Min,
                "max" => ArithmeticOp::Max,
                "abs" => ArithmeticOp::Abs,
                "floor" => ArithmeticOp::Floor,
                "ceil" => ArithmeticOp::Ceil,
                _ => ArithmeticOp::Round,
            };
            if args.is_empty() {
                return Err(ParseError::Arity {
                    operator: operator.to_string(),
                    expected: "at least 1 argument".to_string(),
                    got: 0,
                });
            }
            Ok(Expr::Arithmetic {
                op,
                args: parse_all(args)?,
            })
        }
        other => Err(ParseError::UnknownOperator(other.to_string())),
    }
}

/// Rejects a comparison that no input could satisfy.
///
/// # Rejecting the impossible, not proving the possible
///
/// The check only fires when both sides have types it can see. `["==", ["get", "x"], ["get",
/// "y"]]` compares two unknowns and is accepted, because a feature might carry anything and the
/// comparison could well succeed — if it does not, evaluation says so, which is the right place.
/// `["==", ["string", x], ["number", y]]` names both types itself and can never be true, so it
/// is a mistake in the style rather than a fact about the data.
///
/// Ordering is stricter than equality. Numbers and strings have an order; booleans do not have
/// one the spec is willing to invent, and null has nothing to order. Equality additionally
/// rejects arrays and objects, which the spec compares by neither identity nor structure and so
/// declines to compare at all.
fn check_comparable(
    operator: &str,
    op: CompareOp,
    lhs: &Expr,
    rhs: &Expr,
) -> Result<(), ParseError> {
    let (left, right) = (lhs.result_type(), rhs.result_type());
    let ordering = matches!(
        op,
        CompareOp::Lt | CompareOp::Le | CompareOp::Gt | CompareOp::Ge
    );

    if ordering && !(left.is_ordered() && right.is_ordered()) {
        let unordered = if left.is_ordered() { right } else { left };
        return Err(ParseError::Malformed {
            operator: operator.to_string(),
            detail: format!("{} has no ordering", unordered.name()),
        });
    }

    if !left.could_equal(right) {
        return Err(ParseError::Malformed {
            operator: operator.to_string(),
            detail: format!("{} and {} can never be equal", left.name(), right.name()),
        });
    }
    Ok(())
}

/// The spec's rules for `match` labels, which it enforces at compile time.
///
/// A label is a string or an integer, every label in one expression is the same kind, and no
/// label repeats. All three are checkable without knowing any other type, which is why they are
/// here rather than waiting on a type checker.
///
/// The rules earn their keep at different times. A non-integer or out-of-range label is a typo
/// the style author wants told about. A mixed-kind label set is a `match` whose input cannot be
/// both, so some branch is dead. A duplicate label is a branch that can never run — and unlike
/// the others it looks completely reasonable, which is why the spec calls it out rather than
/// letting the first match win.
fn check_match_labels(operator: &str, arms: &[(Vec<Value>, Expr)]) -> Result<(), ParseError> {
    // JavaScript's safe-integer range, which is what the spec's labels are bounded by.
    const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

    let malformed = |detail: String| ParseError::Malformed {
        operator: operator.to_string(),
        detail,
    };

    let mut expecting_strings: Option<bool> = None;
    let mut seen: Vec<&Value> = Vec::new();

    for (labels, _) in arms {
        for label in labels {
            let is_string = match label {
                Value::String(_) => true,
                Value::Number(number) => {
                    if number.fract() != 0.0 {
                        return Err(malformed(format!("label {number} is not an integer")));
                    }
                    if number.abs() > SAFE_INTEGER {
                        return Err(malformed(format!("label {number} is out of range")));
                    }
                    false
                }
                other => {
                    return Err(malformed(format!(
                        "a label must be a string or an integer, got {}",
                        other.type_name()
                    )));
                }
            };

            match expecting_strings {
                None => expecting_strings = Some(is_string),
                Some(previous) if previous != is_string => {
                    return Err(malformed(
                        "labels must all be strings or all be integers".to_string(),
                    ));
                }
                Some(_) => {}
            }

            if seen.contains(&label) {
                return Err(malformed(format!(
                    "label {label:?} appears more than once, so its second branch is unreachable"
                )));
            }
            seen.push(label);
        }
    }
    Ok(())
}

/// `["array", v]`, `["array", item, v]`, `["array", item, n, v]`.
///
/// The leading arguments are a type name and a length, not expressions, so they are read
/// literally rather than parsed. That is why this cannot be folded into the assertion arm: the
/// arity decides which arguments are data and which is the value.
fn parse_array_assertion(operator: &str, args: &[Value]) -> Result<Expr, ParseError> {
    expect_arity(operator, args, 1, 3)?;

    let item_of = |value: &Value| -> Result<AssertKind, ParseError> {
        match value.as_str() {
            Some("number") => Ok(AssertKind::Number),
            Some("string") => Ok(AssertKind::String),
            Some("boolean") => Ok(AssertKind::Boolean),
            other => Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: format!(
                    "element type must be number, string or boolean, got {}",
                    other.unwrap_or("a non-string")
                ),
            }),
        }
    };

    match args {
        [value] => Ok(Expr::AssertArray {
            item: None,
            length: None,
            value: Box::new(parse(value)?),
        }),
        [item, value] => Ok(Expr::AssertArray {
            item: Some(item_of(item)?),
            length: None,
            value: Box::new(parse(value)?),
        }),
        [item, length, value] => {
            let length = length
                .as_number()
                .filter(|n| *n >= 0.0 && n.fract() == 0.0)
                .ok_or_else(|| ParseError::Malformed {
                    operator: operator.to_string(),
                    detail: "length must be a non-negative integer".to_string(),
                })?;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Ok(Expr::AssertArray {
                item: Some(item_of(item)?),
                length: Some(length as usize),
                value: Box::new(parse(value)?),
            })
        }
        _ => unreachable!("arity is checked above"),
    }
}

/// Recognizes and parses a pre-expression function.
///
/// Returns `Ok(None)` when the value is not one, so an ordinary object literal still reaches the
/// literal path. The shape is what identifies it: an object carrying `stops`, or an `identity`
/// function, which is the one form with no stops at all.
fn parse_legacy_function(
    value: &Value,
    property_default: Option<Value>,
) -> Result<Option<Expr>, ParseError> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let declared_type = object.get("type").and_then(Value::as_str);
    let has_stops = object.contains_key("stops");
    if !has_stops && declared_type != Some("identity") {
        return Ok(None);
    }

    let kind = match declared_type {
        // No `type` means exponential, which is the spec's default and the reason a bare
        // `{"stops": …}` interpolates rather than steps.
        None | Some("exponential") => LegacyKind::Exponential,
        Some("identity") => LegacyKind::Identity,
        Some("categorical") => LegacyKind::Categorical,
        Some("interval") => LegacyKind::Interval,
        Some(other) => {
            return Err(ParseError::Malformed {
                operator: "function".into(),
                detail: alloc::format!("unknown function type `{other}`"),
            });
        }
    };

    let mut stops = Vec::new();
    if let Some(list) = object.get("stops") {
        let entries = list.as_array().ok_or_else(|| ParseError::Malformed {
            operator: "function".into(),
            detail: "stops must be an array".into(),
        })?;
        for entry in entries {
            let pair = entry.as_array().ok_or_else(|| ParseError::Malformed {
                operator: "function".into(),
                detail: "each stop must be a two-element array".into(),
            })?;
            if pair.len() != 2 {
                return Err(ParseError::Malformed {
                    operator: "function".into(),
                    detail: "each stop must be a two-element array".into(),
                });
            }
            stops.push((pair[0].clone(), pair[1].clone()));
        }
    }

    Ok(Some(Expr::LegacyFunction(Box::new(LegacyFunction {
        kind,
        property: object
            .get("property")
            .and_then(Value::as_str)
            .map(alloc::string::ToString::to_string),
        stops,
        base: object.get("base").and_then(Value::as_number).unwrap_or(1.0),
        function_default: object.get("default").cloned(),
        property_default,
    }))))
}

fn parse_all(args: &[Value]) -> Result<Vec<Expr>, ParseError> {
    args.iter().map(parse).collect()
}

fn expect_arity(operator: &str, args: &[Value], min: usize, max: usize) -> Result<(), ParseError> {
    if args.len() >= min && args.len() <= max {
        return Ok(());
    }
    let expected = if min == max {
        format!("{min} argument(s)")
    } else {
        format!("{min} to {max} arguments")
    };
    Err(ParseError::Arity {
        operator: operator.to_string(),
        expected,
        got: args.len(),
    })
}

/// `["match", input, label|labels, output, ..., fallback]`
fn parse_match(operator: &str, args: &[Value]) -> Result<Expr, ParseError> {
    // One input, at least one label/output pair, and a fallback: 2 + 2k, so always even and
    // never fewer than four. (`case` is the odd-length one, having no separate input.)
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, one or more label/output pairs, and a fallback".to_string(),
            got: args.len(),
        });
    }

    let input = Box::new(parse(&args[0])?);
    let fallback = Box::new(parse(&args[args.len() - 1])?);
    let mut arms = Vec::new();
    for pair in args[1..args.len() - 1].as_chunks::<2>().0 {
        // A label is one value or an array of them. An array here is a label set rather than
        // a nested expression, which is why it is not parsed.
        let labels = match &pair[0] {
            Value::Array(values) => values.clone(),
            single => alloc::vec![single.clone()],
        };
        if labels.is_empty() {
            return Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: "a label set must not be empty".to_string(),
            });
        }
        arms.push((labels, parse(&pair[1])?));
    }

    check_match_labels(operator, &arms)?;

    Ok(Expr::Match {
        input,
        arms,
        fallback,
    })
}

/// `["case", condition, output, ..., fallback]`
fn parse_case(operator: &str, args: &[Value]) -> Result<Expr, ParseError> {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "one or more condition/output pairs and a fallback".to_string(),
            got: args.len(),
        });
    }
    let fallback = Box::new(parse(&args[args.len() - 1])?);
    let mut branches = Vec::new();
    for pair in args[..args.len() - 1].as_chunks::<2>().0 {
        branches.push((parse(&pair[0])?, parse(&pair[1])?));
    }
    Ok(Expr::Case { branches, fallback })
}

/// `["step", input, base, stop, output, ...]`
fn parse_step(operator: &str, args: &[Value]) -> Result<Expr, ParseError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, a base, and one or more stop/output pairs".to_string(),
            got: args.len(),
        });
    }
    let input = Box::new(parse(&args[0])?);
    let base = Box::new(parse(&args[1])?);
    let stops = parse_stops(operator, &args[2..])?;
    Ok(Expr::Step { input, base, stops })
}

/// `["interpolate", interpolation, input, stop, output, ...]`
fn parse_interpolate(operator: &str, args: &[Value]) -> Result<Expr, ParseError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an interpolation, an input, and one or more stop/output pairs".to_string(),
            got: args.len(),
        });
    }

    let interpolation = match &args[0] {
        Value::Array(spec) => match spec.first().and_then(Value::as_str) {
            Some("linear") => Interpolation::Linear,
            Some("exponential") => {
                let base = spec.get(1).and_then(Value::as_number).ok_or_else(|| {
                    ParseError::Malformed {
                        operator: operator.to_string(),
                        detail: "exponential interpolation needs a numeric base".to_string(),
                    }
                })?;
                Interpolation::Exponential { base }
            }
            // cubic-bezier is in the spec and not implemented. Rejecting it names the gap;
            // approximating it with linear would be a silently wrong curve.
            Some(other) => {
                return Err(ParseError::Malformed {
                    operator: operator.to_string(),
                    detail: format!("interpolation `{other}` is not implemented"),
                });
            }
            None => {
                return Err(ParseError::Malformed {
                    operator: operator.to_string(),
                    detail: "interpolation must name a type".to_string(),
                });
            }
        },
        _ => {
            return Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: "interpolation must be an array".to_string(),
            });
        }
    };

    let input = Box::new(parse(&args[1])?);
    let stops = parse_stops(operator, &args[2..])?;
    Ok(Expr::Interpolate {
        interpolation,
        input,
        stops,
    })
}

/// Parses stop/output pairs, requiring ascending stops.
///
/// Ascending order is checked rather than assumed. Both `interpolate` and `step` locate a stop
/// by binary search, and an out-of-order stop list would make that search return an arbitrary
/// neighbour — a wrong value from a style that looks perfectly reasonable.
fn parse_stops(operator: &str, args: &[Value]) -> Result<Vec<(f64, Expr)>, ParseError> {
    let mut stops = Vec::with_capacity(args.len() / 2);
    let mut previous: Option<f64> = None;
    for pair in args.as_chunks::<2>().0 {
        let position = pair[0].as_number().ok_or_else(|| ParseError::Malformed {
            operator: operator.to_string(),
            detail: "a stop position must be a number".to_string(),
        })?;
        if let Some(previous) = previous
            && position <= previous
        {
            return Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: format!("stops must ascend; {position} follows {previous}"),
            });
        }
        previous = Some(position);
        stops.push((position, parse(&pair[1])?));
    }
    Ok(stops)
}
