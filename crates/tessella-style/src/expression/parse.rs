//! Turns style values into expression trees.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::{
    ArithmeticOp, ArrayType, AssertKind, CastKind, CompareOp, Expr, FormatSection, Interpolation,
    LegacyFunction, LegacyKind, PropertySpec, Scalar, Type,
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
    /// A collator was written where a value goes.
    ///
    /// The spec's type system has a collator type, so `["let", "c", ["collator", …], …]` is
    /// legal by it. This build takes a collator only where one is written directly — a
    /// comparison's third argument, or `resolved-locale`'s only one — because that is where the
    /// expression is in hand without a value to carry it in. Named rather than accepted, since
    /// the alternative is a comparison that quietly ignores the collator it was given.
    #[cfg(feature = "collator")]
    #[error("a collator may only be written where one is expected, not bound and passed")]
    CollatorNotAValue,
    /// A comparison's third argument, or `resolved-locale`'s, was not a collator.
    ///
    /// Checked whether or not this build can collate. The shape of a collator argument is the
    /// spec's, not the table's, and a build that refuses the comparison should refuse it for the
    /// reason the style got wrong.
    #[error("expected a `[\"collator\", {{…}}]`")]
    NotACollator,
    /// A collator was given a comparison that is not between strings.
    #[error("cannot use collator to compare non-string type `{0:?}`")]
    CollatorOnNonString(Type),
    /// A comparison asked for a collator this build does not carry.
    ///
    /// The DUCET table is behind the `collator` feature for its size (DR-12), and without it
    /// there is no comparison to make — so the style is refused here rather than compared by
    /// codepoint, which is the wrong answer wearing the right shape.
    #[cfg(not(feature = "collator"))]
    #[error("this build has no collator: enable the `collator` feature")]
    CollatorUnavailable,

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
    /// An argument was a type the operator cannot take.
    ///
    /// mbgl's `checkSubtype`, applied where the expected type is known before the argument is
    /// parsed -- which is what makes `["coalesce", ["get", "a"], 5]` in a string property an
    /// error at compile time rather than a wrong value per feature.
    #[error("expected {expected} but found {found} instead")]
    TypeMismatch {
        /// What the position wanted.
        expected: String,
        /// What the argument produces.
        found: String,
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

/// Parses an argument whose type the position already decided, and checks it.
///
/// The expectation is carried into the parse rather than applied to the result, because an
/// operator that passes it through -- `coalesce`, `case`, `match`'s outputs, a `let` body -- has
/// to hand it to *its* arguments in turn. That is mbgl's `ParsingContext::expected`.
fn parse_expecting(value: &Value, scope: &[String], expected: Type) -> Result<Expr, ParseError> {
    let spec = PropertySpec {
        expected: Some(expected),
        ..PropertySpec::default()
    };
    let parsed = parse_rooted(value, &spec, scope, false)?;
    coerce_to(expected, parsed)
}

/// The argument as the position needs it, converted where the spec converts implicitly.
///
/// A colour property written `"red"` is a string that has to become a colour, and mbgl does that
/// by inserting the coercion rather than by widening what a colour position accepts. Doing the
/// same here keeps the check strict and keeps `["match", …, "red", "blue"]` in `fill-color`
/// working -- and it is what turns an unparseable literal into an error at parse, since the cast
/// over a constant folds immediately.
fn coerce_to(expected: Type, parsed: Expr) -> Result<Expr, ParseError> {
    let found = parsed.result_type();
    if found == Type::Value || expected.accepts(found) {
        return Ok(parsed);
    }
    match (expected, found) {
        // A literal is converted now rather than at evaluation: the style wrote the colour down,
        // so whether it is a colour is knowable here, and mbgl answers it here. Deferring it to a
        // cast would leave an unparseable colour inside a branch that never folds -- a `step`
        // over a feature property is not constant -- and the style would load with a stop that
        // fails per feature per tile instead.
        (Type::Color, Type::String) if matches!(parsed, Expr::Literal(Value::String(_))) => {
            let Expr::Literal(Value::String(text)) = &parsed else {
                unreachable!("just matched a string literal")
            };
            if crate::property::Color::parse(text).is_err() {
                return Err(ParseError::Malformed {
                    operator: "color".to_string(),
                    detail: format!("could not parse color from value '{text}'"),
                });
            }
            Ok(Expr::Cast {
                to: CastKind::Color,
                args: alloc::vec![parsed],
            })
        }
        (Type::Color, Type::String) => Ok(Expr::Cast {
            to: CastKind::Color,
            args: alloc::vec![parsed],
        }),
        _ => Err(ParseError::TypeMismatch {
            expected: expected.name().to_string(),
            found: found.name().to_string(),
        }),
    }
}

/// Whether two values of this type have something to walk between.
const fn interpolatable(found: Type) -> bool {
    match found {
        Type::Number | Type::Color | Type::Value => true,
        Type::Array(array) => {
            matches!(array.element, Some(Scalar::Number)) && array.length.is_some()
        }
        _ => false,
    }
}

/// [`parse_expecting`] when the position knows its type, and a plain parse when it does not.
fn parse_maybe_expecting(
    value: &Value,
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
    match expected {
        Some(wanted) => parse_expecting(value, scope, wanted),
        None => parse_in(value, scope),
    }
}

/// The array element type a scalar expectation names, or `None` for one an array cannot hold.
const fn scalar_of(expected: Type) -> Option<Scalar> {
    match expected {
        Type::Number => Some(Scalar::Number),
        Type::String => Some(Scalar::String),
        Type::Boolean => Some(Scalar::Boolean),
        _ => None,
    }
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
            // What comes out of the array is what this position wanted, so the array itself is
            // expected to hold that -- `["at", 1, …]` in a string property wants array<string>.
            // Only a scalar narrows an array: there is no array<color> in the spec's grammar.
            let array_of = spec.expected.and_then(scalar_of).map(|element| {
                Type::Array(ArrayType {
                    element: Some(element),
                    length: None,
                })
            });
            Ok(Expr::At {
                index: Box::new(parse_expecting(&args[0], scope, Type::Number)?),
                array: Box::new(match array_of {
                    Some(wanted) => parse_expecting(&args[1], scope, wanted)?,
                    None => parse_in(&args[1], scope)?,
                }),
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
        "is-supported-script" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::IsSupportedScript(Box::new(parse_in(
                &args[0], scope,
            )?)))
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
        // `["within", geojson]`. The argument is a literal rather than an expression: mbgl reads
        // the polygon once here and keeps it, and there is nothing a style could compute it from.
        //
        // Only a `Polygon` or a `MultiPolygon` is admitted, which is mbgl's own check -- a
        // `LineString` there is a parse error rather than a test nothing can pass. A *feature*
        // that is a polygon is a different question and is answered false at evaluation, where
        // mbgl answers it.
        // `["number-format", value, options]`. The options object is a literal, but each of its
        // values is an expression -- the spec's own case reads all three off the feature.
        "number-format" => {
            expect_arity(operator, args, 2, 2)?;
            let options = args[1].as_object().ok_or_else(|| ParseError::Malformed {
                operator: operator.to_string(),
                detail: "the second argument is an options object".to_string(),
            })?;
            let option = |name: &str| -> Result<Option<Box<Expr>>, ParseError> {
                match options.get(name) {
                    Some(value) => Ok(Some(Box::new(parse_in(value, scope)?))),
                    None => Ok(None),
                }
            };
            Ok(Expr::NumberFormat {
                value: Box::new(parse_in(&args[0], scope)?),
                locale: option("locale")?,
                currency: option("currency")?,
                min_digits: option("min-fraction-digits")?,
                max_digits: option("max-fraction-digits")?,
            })
        }
        "within" => {
            expect_arity(operator, args, 1, 1)?;
            let rings = within_rings(&args[0]).ok_or_else(|| ParseError::Malformed {
                operator: operator.to_string(),
                detail: "expected a GeoJSON Polygon or MultiPolygon".to_string(),
            })?;
            Ok(Expr::Within(rings))
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
                    // Each option has a type the spec pins down, and a style that gets one wrong
                    // is wrong about something the shaper needs: a font stack that is not
                    // `array<string>` names no font, and a scale that is not a number has no size.
                    for (key, wanted, target) in [
                        ("font-scale", Type::Number, &mut section.scale),
                        (
                            "text-font",
                            Type::Array(ArrayType {
                                element: Some(Scalar::String),
                                length: None,
                            }),
                            &mut section.font,
                        ),
                        ("text-color", Type::Color, &mut section.color),
                    ] {
                        if let Some(value) = options.get(key) {
                            *target = Some(Box::new(parse_expecting(value, scope, wanted)?));
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
        "image" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::Image(Box::new(parse_in(&args[0], scope)?)))
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
        #[cfg(feature = "collator")]
        "collator" => {
            expect_arity(operator, args, 1, 1)?;
            Err(ParseError::CollatorNotAValue)
        }
        #[cfg(feature = "collator")]
        "resolved-locale" => {
            expect_arity(operator, args, 1, 1)?;
            Ok(Expr::ResolvedLocale(alloc::boxed::Box::new(
                parse_collator(&args[0], scope)?,
            )))
        }
        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
            // Two, or three with a collator. The spec allows the third only on these six, and
            // only as a collator — so it is parsed as one here rather than as an expression that
            // might turn out to be one.
            //
            // The third argument is accepted, and checked, by a build that cannot collate. It
            // then refuses the comparison by name. Refusing the *arity* instead would reject the
            // same styles while calling three arguments the mistake, which is not what the
            // author wrote down and not what the spec says is wrong with it.
            expect_arity(operator, args, 2, 3)?;
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
            if let Some(third) = args.get(2) {
                // Both sides must be text. A collator orders letters, so ordering numbers with
                // one is a category error rather than a comparison that happens to ignore it —
                // and the spec catches it at compile time, which is where a style author sees
                // it. `Type::Value` passes: an expression whose type is not yet known may still
                // turn out to be a string, and refusing it would reject `["get", "name"]`.
                for side in [&lhs, &rhs] {
                    let kind = side.result_type();
                    if !matches!(kind, Type::String | Type::Value) {
                        return Err(ParseError::CollatorOnNonString(kind));
                    }
                }
                #[cfg(feature = "collator")]
                return Ok(Expr::CompareWith {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    collator: Box::new(parse_collator(third, scope)?),
                });
                #[cfg(not(feature = "collator"))]
                {
                    // The shape first, so the style author hears about a malformed collator
                    // before hearing that this build has none.
                    collator_options(third)?;
                    return Err(ParseError::CollatorUnavailable);
                }
            }
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
        // Every branch stands in the same position, so each takes the type that position wants.
        "coalesce" => Ok(Expr::Coalesce(match spec.expected {
            Some(expected) => args
                .iter()
                .map(|arg| parse_expecting(arg, scope, expected))
                .collect::<Result<Vec<_>, _>>()?,
            None => parse_all(args, scope)?,
        })),
        "let" => parse_let(operator, args, scope, spec.expected),
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
        "match" => parse_match(operator, args, scope, spec.expected),
        "case" => parse_case(operator, args, scope, spec.expected),
        "step" => parse_step(operator, args, scope, spec.expected),
        "interpolate" => parse_interpolate(operator, args, scope, spec.expected),
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
fn parse_let(
    operator: &str,
    args: &[Value],
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
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

    // The body is what the `let` evaluates to, so it stands where the whole expression does. A
    // binding does not: it is whatever it is, and each use decides what it has to be there.
    let body = Box::new(parse_maybe_expecting(
        &args[args.len() - 1],
        &scope,
        expected,
    )?);
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
/// The type a match label pins the input to.
const fn label_kind(label: &Value) -> Option<Type> {
    match label {
        Value::String(_) => Some(Type::String),
        Value::Number(_) => Some(Type::Number),
        _ => None,
    }
}

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

/// The options object of a `["collator", {…}]`, or why it is not one.
///
/// Split from [`parse_collator`] because the shape is checked by every build and the members are
/// parsed only by one that can collate: a build without the table still owes a style author the
/// spec's own diagnostic, and `["==", "a", "b", ["collator", ["subexpression"]]]` is wrong in a
/// way that has nothing to do with which table is compiled in.
fn collator_options(
    value: &Value,
) -> Result<&alloc::collections::BTreeMap<String, Value>, ParseError> {
    let items = value.as_array().ok_or(ParseError::NotACollator)?;
    if items.first().and_then(Value::as_str) != Some("collator") {
        return Err(ParseError::NotACollator);
    }
    items
        .get(1)
        .and_then(Value::as_object)
        .ok_or(ParseError::NotACollator)
}

/// Parses a `["collator", {…}]` in the one position the spec allows one.
///
/// Not through [`parse_in`], because a collator is not a value here: it may be written where a
/// comparison takes its third argument and where `resolved-locale` takes its only one, and both
/// are places the expression itself is in hand. A style that bound one with `let` and passed it
/// by `var` would be legal by the spec's type system and is refused, with the message saying so.
#[cfg(feature = "collator")]
fn parse_collator(value: &Value, scope: &[String]) -> Result<super::CollatorSpec, ParseError> {
    let options = collator_options(value)?;

    let member = |name: &str| -> Result<Option<Expr>, ParseError> {
        options
            .get(name)
            .map(|value| parse_in(value, scope))
            .transpose()
    };
    Ok(super::CollatorSpec {
        case_sensitive: member("case-sensitive")?,
        diacritic_sensitive: member("diacritic-sensitive")?,
        locale: member("locale")?,
    })
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
fn parse_match(
    operator: &str,
    args: &[Value],
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
    // One input, at least one label/output pair, and a fallback: 2 + 2k, so always even and
    // never fewer than four. (`case` is the odd-length one, having no separate input.)
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, one or more label/output pairs, and a fallback".to_string(),
            got: args.len(),
        });
    }

    // The labels decide what the input has to be: they are literals, so their type is known
    // before anything is parsed, and an input of another type can never match one of them.
    let label_type = match args[1..args.len() - 1].as_chunks::<2>().0.first() {
        Some(pair) => match &pair[0] {
            Value::Array(values) => values.first().map(label_kind),
            single => Some(label_kind(single)),
        }
        .flatten(),
        None => None,
    };
    let input = Box::new(match label_type {
        Some(wanted) => parse_expecting(&args[0], scope, wanted)?,
        None => parse_in(&args[0], scope)?,
    });

    // Every output stands in the same position. When the property named a type they all take it;
    // when nothing did, the first one sets it and the rest have to agree -- which is how
    // `["match", …, "a string", false]` is an error with no property spec in sight.
    let fallback = Box::new(parse_in(&args[args.len() - 1], scope)?);
    let mut output_type = expected.or_else(|| {
        let found = fallback.result_type();
        (found != Type::Value).then_some(found)
    });
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
        let output = match output_type {
            Some(wanted) => parse_expecting(&pair[1], scope, wanted)?,
            None => parse_in(&pair[1], scope)?,
        };
        if output_type.is_none() {
            let found = output.result_type();
            if found != Type::Value {
                output_type = Some(found);
            }
        }
        arms.push((labels, output));
    }

    check_match_labels(operator, &arms)?;

    Ok(Expr::Match {
        input,
        arms,
        fallback,
    })
}

/// `["case", condition, output, ..., fallback]`
fn parse_case(
    operator: &str,
    args: &[Value],
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
    if args.len() < 3 || args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "one or more condition/output pairs and a fallback".to_string(),
            got: args.len(),
        });
    }
    let fallback = Box::new(parse_maybe_expecting(
        &args[args.len() - 1],
        scope,
        expected,
    )?);
    let mut branches = Vec::new();
    for pair in args[..args.len() - 1].as_chunks::<2>().0 {
        branches.push((
            parse_expecting(&pair[0], scope, Type::Boolean)?,
            parse_maybe_expecting(&pair[1], scope, expected)?,
        ));
    }
    Ok(Expr::Case { branches, fallback })
}

/// `["step", input, base, stop, output, ...]`
fn parse_step(
    operator: &str,
    args: &[Value],
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(ParseError::Arity {
            operator: operator.to_string(),
            expected: "an input, a base, and one or more stop/output pairs".to_string(),
            got: args.len(),
        });
    }
    // The input is what the stops are compared against, so it is a number; the base and every
    // output stand where the whole expression does.
    let input = Box::new(parse_expecting(&args[0], scope, Type::Number)?);
    let base = Box::new(parse_maybe_expecting(&args[1], scope, expected)?);
    let stops = parse_stops(operator, &args[2..], scope, expected)?;
    Ok(Expr::Step { input, base, stops })
}

/// `["interpolate", interpolation, input, stop, output, ...]`
fn parse_interpolate(
    operator: &str,
    args: &[Value],
    scope: &[String],
    expected: Option<Type>,
) -> Result<Expr, ParseError> {
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

    // What is interpolated between is a number, and every stop output stands where the whole
    // expression does.
    let input = Box::new(parse_expecting(&args[1], scope, Type::Number)?);
    let stops = parse_stops(operator, &args[2..], scope, expected)?;

    // Not everything can be interpolated. Numbers and colours can, and so can an array of numbers
    // whose length is known -- without a length there is no telling that two stops have the same
    // number of components to walk between, which is why `array<number>` is refused where
    // `array<number, 2>` is taken.
    let output = expected.or_else(|| stops.first().map(|(_, stop)| stop.result_type()));
    if let Some(found) = output
        && !interpolatable(found)
    {
        return Err(ParseError::Malformed {
            operator: operator.to_string(),
            detail: format!("type {} is not interpolatable", found.name()),
        });
    }
    Ok(Expr::Interpolate {
        interpolation,
        input,
        stops,
    })
}

/// The rings of a GeoJSON `Polygon` or `MultiPolygon`, flattened.
///
/// Flattened because containment does not care which polygon a ring belongs to: mbgl's own test
/// walks every ring and counts crossings, and a multi-polygon is the union of its parts. What is
/// refused is anything that is not one of the two -- the spec's suite hands it a `LineString` and
/// wants a compile error.
fn within_rings(value: &Value) -> Option<Vec<Vec<[f64; 2]>>> {
    let point = |value: &Value| -> Option<[f64; 2]> {
        let pair = value.as_array()?;
        Some([pair.first()?.as_number()?, pair.get(1)?.as_number()?])
    };
    let ring =
        |value: &Value| -> Option<Vec<[f64; 2]>> { value.as_array()?.iter().map(point).collect() };
    let polygon = |value: &Value| -> Option<Vec<Vec<[f64; 2]>>> {
        value.as_array()?.iter().map(ring).collect()
    };
    let coordinates = value.get("coordinates")?;
    match value.get("type")?.as_str()? {
        "Polygon" => polygon(coordinates),
        "MultiPolygon" => {
            let mut rings = Vec::new();
            for part in coordinates.as_array()? {
                rings.extend(polygon(part)?);
            }
            Some(rings)
        }
        _ => None,
    }
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
    expected: Option<Type>,
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
        stops.push((position, parse_maybe_expecting(&pair[1], scope, expected)?));
    }
    Ok(stops)
}
