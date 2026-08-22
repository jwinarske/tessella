//! Turns style values into expression trees.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{ArithmeticOp, CastKind, CompareOp, Expr, Interpolation};
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
            Ok(Expr::Compare {
                op,
                lhs: Box::new(parse(&args[0])?),
                rhs: Box::new(parse(&args[1])?),
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
    for pair in args[1..args.len() - 1].chunks_exact(2) {
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
    for pair in args[..args.len() - 1].chunks_exact(2) {
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
    for pair in args.chunks_exact(2) {
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
