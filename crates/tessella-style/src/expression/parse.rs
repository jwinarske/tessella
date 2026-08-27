//! Turns style values into expression trees.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    ArithmeticOp, AssertKind, CastKind, CompareOp, Expr, FormatSection, Interpolation,
    LegacyFunction, LegacyKind, PropertySpec, Type,
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

    /// A constant expression that cannot be evaluated.
    ///
    /// Reported at parse because a constant has one value: if computing it fails, no input
    /// could have helped, and the alternative is the same failure once per feature per tile.
    #[error("constant expression cannot be evaluated: {source}")]
    ConstantFolds {
        /// What went wrong evaluating it.
        source: super::EvaluationError,
    },
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

/// Parses a value in a scope.
fn parse_in(value: &Value, scope: &[String]) -> Result<Expr, ParseError> {
    parse_rooted(value, &PropertySpec::default(), scope, false)
}

/// Parses a value, carrying the property spec's default for pre-expression functions.
pub(super) fn parse_with_default(value: &Value, spec: &PropertySpec) -> Result<Expr, ParseError> {
    parse_with_default_in(value, spec, &[])
}

/// The parser proper.
///
/// `scope` carries the names an enclosing `let` has bound. It is threaded through every operator
/// rather than resolved in a separate pass, because a `var` can appear anywhere an expression
/// can and a separate walker would have to reimplement each operator's argument shape to know
/// where that is.
fn parse_with_default_in(
    value: &Value,
    spec: &PropertySpec,
    scope: &[String],
) -> Result<Expr, ParseError> {
    parse_rooted(value, spec, scope, true)
}

/// Parses, knowing whether this is the whole property value or a nested position.
fn parse_rooted(
    value: &Value,
    spec: &PropertySpec,
    scope: &[String],
    at_root: bool,
) -> Result<Expr, ParseError> {
    // A pre-expression function is an object, which `looks_like_expression` does not recognize,
    // so without this check it falls through to `Expr::Literal` and a style that varies a
    // property by zoom silently gets the raw JSON object as the value. That is worse than an
    // error: it renders as a broken colour rather than as a message.
    if let Some(function) = parse_legacy_function(value, spec)? {
        return Ok(function);
    }

    // An array in expression position is always a call: the spec has no bare array literal, and
    // data arrays are written `["literal", […]]`. Treating `[1, 2]` as data was too permissive,
    // and silently so — a style meaning to call something and misspelling the operator got a
    // constant array rather than a message.
    //
    // The exception is a property the spec *types* as an array, where the whole value may be a
    // constant: `fill-translate`'s default is `[0, 0]`, which is data and not a call to an
    // operator named `0`. So the check is skipped exactly where a bare array is a legal value,
    // which is at the root of an array-typed property and nowhere else.
    let array_valued_root = at_root && matches!(spec.expected, Some(Type::Array(_)));
    if let Some(items) = value.as_array()
        && !array_valued_root
    {
        let Some(first) = items.first() else {
            return Err(ParseError::Malformed {
                operator: "expression".to_string(),
                detail: "an empty array is not an expression".to_string(),
            });
        };
        if first.as_str().is_none() {
            return Err(ParseError::Malformed {
                operator: "expression".to_string(),
                detail: alloc::format!(
                    "an expression starts with an operator name, got {}",
                    first.type_name()
                ),
            });
        }
    }

    // A value that is not a call is itself. This is what makes `["match", x, "a", 1, 2]`
    // work: the outputs are bare values, not nested calls.
    if !value.looks_like_expression() {
        // Except an array headed by a string that names no operator. As a *value* that is a
        // literal array of strings and perfectly legal — `["Noto Sans Regular"]` is the
        // ordinary spelling of a font stack. But this function has been told the value is an
        // expression, and the only way an unrecognized head gets here is a misspelling. The
        // spec catches those a different way, by type-checking the array against the property
        // it was written for; nothing here knows the property, so the name is reported instead
        // of a `["gett", "x"]` quietly becoming a two-element array of strings.
        if let Some(head) = value
            .as_array()
            .and_then(<[Value]>::first)
            .and_then(Value::as_str)
        {
            return Err(ParseError::UnknownOperator(head.to_string()));
        }
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
                args: parse_all(args, scope)?,
            })
        }
        "array" => parse_array_assertion(operator, args, scope),
        "concat" => Ok(Expr::Concat(parse_all(args, scope)?)),
        "join" => {
            expect_arity(operator, args, 2, 2)?;
            Ok(Expr::Join {
                items: Box::new(parse_in(&args[0], scope)?),
                separator: Box::new(parse_in(&args[1], scope)?),
            })
        }
        "length" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Length(Box::new(parse_in(&args[0], scope)?)))
        }
        "at" => {
            expect_arity(operator, args, 2, 2)?;
            Ok(Expr::At {
                index: Box::new(parse_in(&args[0], scope)?),
                array: Box::new(parse_in(&args[1], scope)?),
            })
        }
        "split" => {
            expect_arity(operator, args, 2, 2)?;
            Ok(Expr::Split {
                input: Box::new(parse_in(&args[0], scope)?),
                delimiter: Box::new(parse_in(&args[1], scope)?),
            })
        }
        "to-rgba" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::ToRgba(Box::new(parse_in(&args[0], scope)?)))
        }
        "typeof" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::TypeOf(Box::new(parse_in(&args[0], scope)?)))
        }
        "error" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Error(Box::new(parse_in(&args[0], scope)?)))
        }
        "upcase" | "downcase" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::CaseFold {
                upper: operator == "upcase",
                arg: Box::new(parse_in(&args[0], scope)?),
            })
        }
        "in" => {
            expect_arity(operator, args, 2, 2)?;
            Ok(Expr::In {
                needle: Box::new(parse_in(&args[0], scope)?),
                haystack: Box::new(parse_in(&args[1], scope)?),
            })
        }
        "index-of" => {
            expect_arity(operator, args, 2, 3)?;
            Ok(Expr::IndexOf {
                needle: Box::new(parse_in(&args[0], scope)?),
                haystack: Box::new(parse_in(&args[1], scope)?),
                from: args
                    .get(2)
                    .map(|value| parse_in(value, scope))
                    .transpose()?
                    .map(Box::new),
            })
        }
        "slice" => {
            expect_arity(operator, args, 2, 3)?;
            Ok(Expr::Slice {
                value: Box::new(parse_in(&args[0], scope)?),
                start: Box::new(parse_in(&args[1], scope)?),
                end: args
                    .get(2)
                    .map(|value| parse_in(value, scope))
                    .transpose()?
                    .map(Box::new),
            })
        }
        "format" => {
            // Content and options alternate. The trailing options object may be omitted, which
            // is why the arity is not simply even.
            if args.is_empty() {
                return Err(ParseError::Arity {
                    operator: operator.to_string(),
                    expected: "at least one section".to_string(),
                    got: 0,
                });
            }
            let mut sections = Vec::new();
            let mut index = 0;
            while index < args.len() {
                let content = Box::new(parse_in(&args[index], scope)?);
                // An options object follows unless this is the last argument.
                let options = args.get(index + 1).and_then(Value::as_object);
                let mut section = FormatSection {
                    content,
                    scale: None,
                    font: None,
                    color: None,
                };
                if let Some(options) = options {
                    for (key, target) in [
                        ("font-scale", &mut section.scale),
                        ("text-font", &mut section.font),
                        ("text-color", &mut section.color),
                    ] {
                        if let Some(value) = options.get(key) {
                            *target = Some(Box::new(parse_in(value, scope)?));
                        }
                    }
                    index += 2;
                } else {
                    index += 1;
                }
                sections.push(section);
            }
            Ok(Expr::Format { sections })
        }
        "rgb" | "rgba" => {
            let arity = if operator == "rgb" { 3 } else { 4 };
            expect_arity(operator, args, arity, arity)?;
            Ok(Expr::Rgba {
                args: parse_all(args, scope)?,
            })
        }
        "to-string" | "to-boolean" => {
            // Unlike `to-number` and `to-color`, these always succeed, so a fallback could
            // never be reached and the spec treats one as a mistake rather than dead weight.
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Cast {
                to: if operator == "to-string" {
                    CastKind::String
                } else {
                    CastKind::Boolean
                },
                args: parse_all(args, scope)?,
            })
        }
        "to-color" => {
            expect_arity(operator, args, 1, usize::MAX)?;
            let parsed = parse_all(args, scope)?;
            // Converting something that is already a colour would read its normalized channels
            // as 0..255 and darken it by a factor of 255. The spec makes `["to-color", ["rgba",
            // …]]` a pass-through for exactly this reason, and the check is static because the
            // difference between a colour and the four numbers it looks like is a type.
            if let [only] = parsed.as_slice()
                && only.result_type() == Type::Color
            {
                return Ok(parsed.into_iter().next().expect("one argument"));
            }
            Ok(Expr::Cast {
                to: CastKind::Color,
                args: parsed,
            })
        }
        "zoom" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::Zoom)
        }
        // Mapbox Style Spec v3. mbgl's compound-expression registry has neither, so an mbgl
        // build rejects any layer using one — which is exactly what a vendor style does to
        // eleven of its layers, every one of them a label.
        "pitch" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::Pitch)
        }
        "distance-from-center" => {
            expect_arity(operator, args, 0, 0)?;
            Ok(Expr::DistanceFromCenter)
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
            // With a second argument the lookup is in *that* object rather than in the feature,
            // which also means the expression stops depending on the feature at all — the
            // classifier reads the same `object` field to decide.
            expect_arity(operator, args, 1, 2)?;
            Ok(Expr::Get {
                key: Box::new(parse_in(&args[0], scope)?),
                object: args
                    .get(1)
                    .map(|value| parse_in(value, scope))
                    .transpose()?
                    .map(Box::new),
            })
        }
        "has" => {
            expect_arity(operator, args, 1, 2)?;
            Ok(Expr::Has {
                key: Box::new(parse_in(&args[0], scope)?),
                object: args
                    .get(1)
                    .map(|value| parse_in(value, scope))
                    .transpose()?
                    .map(Box::new),
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
            let lhs = parse_in(&args[0], scope)?;
            let rhs = parse_in(&args[1], scope)?;
            check_comparable(operator, op, &lhs, &rhs)?;
            Ok(Expr::Compare {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "!" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Not(Box::new(parse_in(&args[0], scope)?)))
        }
        "all" => Ok(Expr::All(parse_all(args, scope)?)),
        "any" => Ok(Expr::Any(parse_all(args, scope)?)),
        "coalesce" => Ok(Expr::Coalesce(parse_all(args, scope)?)),
        "let" => parse_let(operator, args, scope),
        "var" => {
            expect_arity(operator, args, 1, 1)?;
            let name = args[0].as_str().ok_or_else(|| ParseError::Malformed {
                operator: operator.to_string(),
                detail: "a variable name must be a string".to_string(),
            })?;
            // Resolved against the scope threaded down from any enclosing `let`. A name that is
            // not there was never bound, and the spec rejects that at compile time rather than
            // yielding null at evaluation — where it would be one silent wrong value per
            // feature rather than one loud message at load.
            if scope.iter().any(|bound| bound == name) {
                Ok(Expr::Var(name.to_string()))
            } else {
                Err(ParseError::Malformed {
                    operator: operator.to_string(),
                    detail: format!("`{name}` is not bound by any enclosing let"),
                })
            }
        }
        "match" => parse_match(operator, args, scope),
        "case" => parse_case(operator, args, scope),
        "step" => parse_step(operator, args, scope),
        "interpolate" => parse_interpolate(operator, args, scope),
        "to-number" => Ok(Expr::Cast {
            to: CastKind::Number,
            args: parse_all(args, scope)?,
        }),
        // The three constants, which mbgl declares with a no-argument signature. Folded to their
        // value at parse rather than carried as operators: they depend on nothing, so an
        // expression node for them would be a node the classifier has to walk and the evaluator
        // has to visit to reach a number that was known here.
        "e" | "pi" | "ln2" => {
            if !args.is_empty() {
                return Err(ParseError::Arity {
                    operator: operator.to_string(),
                    expected: "no arguments".to_string(),
                    got: args.len(),
                });
            }
            let value = match operator {
                "e" => core::f64::consts::E,
                "pi" => core::f64::consts::PI,
                _ => core::f64::consts::LN_2,
            };
            Ok(Expr::Literal(Value::Number(value)))
        }
        "+" | "-" | "*" | "/" | "%" | "^" | "min" | "max" | "abs" | "floor" | "ceil" | "round"
        | "sqrt" | "ln" | "log2" | "log10" | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" => {
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
                "sqrt" => ArithmeticOp::Sqrt,
                "ln" => ArithmeticOp::Ln,
                "log2" => ArithmeticOp::Log2,
                "log10" => ArithmeticOp::Log10,
                "sin" => ArithmeticOp::Sin,
                "cos" => ArithmeticOp::Cos,
                "tan" => ArithmeticOp::Tan,
                "asin" => ArithmeticOp::Asin,
                "acos" => ArithmeticOp::Acos,
                "atan" => ArithmeticOp::Atan,
                _ => ArithmeticOp::Round,
            };
            // `+`, `*`, `min` and `max` are folds over identities, so no arguments is not an
            // error but the identity itself: zero, one, positive infinity, negative infinity.
            // The evaluator already folds from those, so this is only the gate. The others have
            // no identity to return — `["-"]` and `["floor"]` are missing an operand.
            if args.is_empty() && !op.is_variadic() {
                return Err(ParseError::Arity {
                    operator: operator.to_string(),
                    expected: "at least 1 argument".to_string(),
                    got: 0,
                });
            }
            // And a unary one takes exactly one. mbgl declares each with a single-`double`
            // signature, so a second argument is an arity error rather than something to fold
            // over -- `["sqrt", 4, 9]` names no function.
            if op.is_unary() && args.len() != 1 {
                return Err(ParseError::Arity {
                    operator: operator.to_string(),
                    expected: "1 argument".to_string(),
                    got: args.len(),
                });
            }
            Ok(Expr::Arithmetic {
                op,
                args: parse_all(args, scope)?,
            })
        }
        other => Err(ParseError::UnknownOperator(other.to_string())),
    }
}

/// `["let", name, value, …, body]`.
///
/// # Scope is resolved here, not at evaluation
///
/// Parsing walks the body with the bound names in hand, so an unbound `var` is a parse error and
/// shadowing is decided before anything runs. The alternative — carrying names to evaluation and
/// failing there — turns a style-authoring mistake into a per-feature per-tile error, which is
/// the same trade the comparison checker makes and for the same reason.
fn parse_let(operator: &str, args: &[Value], scope: &[String]) -> Result<Expr, ParseError> {
    // Pairs of name and value, then a body: odd, and at least three.
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "name/value pairs followed by a body".to_string(),
            got: args.len(),
        });
    }

    // The enclosing scope, extended as each binding is made. Extending rather than replacing is
    // what gives shadowing its meaning: an inner name is pushed after the outer one and found
    // first on lookup.
    let mut scope: Vec<String> = scope.to_vec();
    let mut bindings = Vec::new();
    for pair in args[..args.len() - 1].as_chunks::<2>().0 {
        let name = pair[0].as_str().ok_or_else(|| ParseError::Malformed {
            operator: operator.to_string(),
            detail: "a binding name must be a string".to_string(),
        })?;
        if !is_binding_name(name) {
            return Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: format!("`{name}` is not a valid variable name"),
            });
        }
        // Parsed in the scope built so far, so a later binding may read an earlier one and a
        // binding cannot read itself.
        let value = parse_in(&pair[1], &scope)?;
        bindings.push((name.to_string(), value));
        scope.push(name.to_string());
    }

    let body = Box::new(parse_in(&args[args.len() - 1], &scope)?);
    Ok(Expr::Let { bindings, body })
}

/// A variable name: a letter or underscore, then letters, digits or underscores.
///
/// The suite rejects `$a`, which is what this rule is for. Names that look like identifiers keep
/// `var` unambiguous and leave room for the spec to give punctuation a meaning later.
fn is_binding_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
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

    // Each operand on its own first, which is the check that was missing. An array, an object
    // or a colour cannot be compared *at all*, whatever it is compared against — and asking
    // only whether the two could be equal never finds out, because an unknown could equal
    // anything and `["get", …]` is always unknown.
    for (side, kind) in [(left, "left"), (right, "right")] {
        if !side.is_comparable(ordering) {
            return Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: format!(
                    "comparisons are not supported for type {} ({kind} operand)",
                    side.name()
                ),
            });
        }
    }

    if !left.could_equal(right) {
        return Err(ParseError::Malformed {
            operator: operator.to_string(),
            detail: format!("cannot compare {} and {}", left.name(), right.name()),
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
fn parse_array_assertion(
    operator: &str,
    args: &[Value],
    scope: &[String],
) -> Result<Expr, ParseError> {
    expect_arity(operator, args, 1, 4)?;

    let item_of = |value: &Value| -> Result<Option<AssertKind>, ParseError> {
        match value.as_str() {
            Some("number") => Ok(Some(AssertKind::Number)),
            Some("string") => Ok(Some(AssertKind::String)),
            Some("boolean") => Ok(Some(AssertKind::Boolean)),
            // `value` is the spec's way of saying "any element type", which is the same as not
            // constraining one.
            Some("value") => Ok(None),
            other => Err(ParseError::Malformed {
                operator: operator.to_string(),
                detail: format!(
                    "element type must be number, string, boolean or value, got {}",
                    other.unwrap_or("a non-string")
                ),
            }),
        }
    };

    // A length of `null` means unconstrained, which is how the four-argument form writes "any
    // length, but here is a fallback".
    let length_of = |value: &Value| -> Result<Option<usize>, ParseError> {
        if matches!(value, Value::Null) {
            return Ok(None);
        }
        let length = value
            .as_number()
            .filter(|n| *n >= 0.0 && n.fract() == 0.0)
            .ok_or_else(|| ParseError::Malformed {
                operator: operator.to_string(),
                detail: "length must be a non-negative integer or null".to_string(),
            })?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(Some(length as usize))
    };

    let (item, length, value, fallback) = match args {
        [value] => (None, None, value, None),
        [item, value] => (item_of(item)?, None, value, None),
        [item, length, value] => (item_of(item)?, length_of(length)?, value, None),
        [item, length, value, fallback] => {
            (item_of(item)?, length_of(length)?, value, Some(fallback))
        }
        _ => unreachable!("arity is checked above"),
    };

    Ok(Expr::AssertArray {
        item,
        length,
        value: Box::new(parse_in(value, scope)?),
        fallback: fallback
            .map(|value| parse_in(value, scope))
            .transpose()?
            .map(Box::new),
    })
}

/// Recognizes and parses a pre-expression function.
///
/// Returns `Ok(None)` when the value is not one, so an ordinary object literal still reaches the
/// literal path. The shape is what identifies it: an object carrying `stops`, or an `identity`
/// function, which is the one form with no stops at all.
fn parse_legacy_function(value: &Value, spec: &PropertySpec) -> Result<Option<Expr>, ParseError> {
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
        property_default: spec.default.clone(),
        property_type: spec.expected,
    }))))
}

fn parse_all(args: &[Value], scope: &[String]) -> Result<Vec<Expr>, ParseError> {
    args.iter().map(|arg| parse_in(arg, scope)).collect()
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
fn parse_match(operator: &str, args: &[Value], scope: &[String]) -> Result<Expr, ParseError> {
    // One input, at least one label/output pair, and a fallback: 2 + 2k, so always even and
    // never fewer than four. (`case` is the odd-length one, having no separate input.)
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, one or more label/output pairs, and a fallback".to_string(),
            got: args.len(),
        });
    }

    let input = Box::new(parse_in(&args[0], scope)?);
    let fallback = Box::new(parse_in(&args[args.len() - 1], scope)?);
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
        arms.push((labels, parse_in(&pair[1], scope)?));
    }

    check_match_labels(operator, &arms)?;

    Ok(Expr::Match {
        input,
        arms,
        fallback,
    })
}

/// `["case", condition, output, ..., fallback]`
fn parse_case(operator: &str, args: &[Value], scope: &[String]) -> Result<Expr, ParseError> {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "one or more condition/output pairs and a fallback".to_string(),
            got: args.len(),
        });
    }
    let fallback = Box::new(parse_in(&args[args.len() - 1], scope)?);
    let mut branches = Vec::new();
    for pair in args[..args.len() - 1].as_chunks::<2>().0 {
        branches.push((parse_in(&pair[0], scope)?, parse_in(&pair[1], scope)?));
    }
    Ok(Expr::Case { branches, fallback })
}

/// `["step", input, base, stop, output, ...]`
fn parse_step(operator: &str, args: &[Value], scope: &[String]) -> Result<Expr, ParseError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, a base, and one or more stop/output pairs".to_string(),
            got: args.len(),
        });
    }
    let input = Box::new(parse_in(&args[0], scope)?);
    let base = Box::new(parse_in(&args[1], scope)?);
    let stops = parse_stops(operator, &args[2..], scope)?;
    Ok(Expr::Step { input, base, stops })
}

/// `["interpolate", interpolation, input, stop, output, ...]`
fn parse_interpolate(operator: &str, args: &[Value], scope: &[String]) -> Result<Expr, ParseError> {
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
            // Four numbers, each in `0..=1`, and mbgl checks every one of them: the two
            // control points of a unit Bézier, the first and last being implicitly `(0, 0)`
            // and `(1, 1)`. A control point outside the unit square makes a curve that is not
            // a function of `x`, so solving it has no single answer -- which is why the range
            // check is a parse error rather than a clamp.
            Some("cubic-bezier") => {
                // Exactly five elements: the name and four numbers. mbgl tests
                // `arrayLength(interp) == 5` and does not read the arguments at all otherwise,
                // so a fifth number is an error rather than something to ignore — and the spec
                // suite has a case that says so.
                let control = |index: usize| -> Option<f64> {
                    if spec.len() != 5 {
                        return None;
                    }
                    spec.get(index)
                        .and_then(Value::as_number)
                        .filter(|value| (0.0..=1.0).contains(value))
                };
                let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                    (control(1), control(2), control(3), control(4))
                else {
                    return Err(ParseError::Malformed {
                        operator: operator.to_string(),
                        detail: "cubic-bezier interpolation requires four numeric arguments \
                                 with values between 0 and 1"
                            .to_string(),
                    });
                };
                Interpolation::CubicBezier { x1, y1, x2, y2 }
            }
            Some(other) => {
                return Err(ParseError::Malformed {
                    operator: operator.to_string(),
                    detail: format!("interpolation `{other}` is not one the spec names"),
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

    let input = Box::new(parse_in(&args[1], scope)?);
    let stops = parse_stops(operator, &args[2..], scope)?;
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
fn parse_stops(
    operator: &str,
    args: &[Value],
    scope: &[String],
) -> Result<Vec<(f64, Expr)>, ParseError> {
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
        stops.push((position, parse_in(&pair[1], scope)?));
    }
    Ok(stops)
}
