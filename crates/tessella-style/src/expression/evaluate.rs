//! Evaluates expression trees.
//!
//! A direct tree walk, which is what mbgl does and what DR-11 replaces with flat bytecode at
//! R1 (§12.1). It is here first because the VM has to reproduce these values, and the golden
//! oracle is what will confirm it does — optimizing something not yet known to be correct is
//! the wrong order.

use alloc::borrow::Cow;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use alloc::boxed::Box;

use super::{
    ArithmeticOp, AssertKind, CastKind, CompareOp, Expr, FilterTarget, Interpolation,
    LegacyFunction, LegacyKind, Type,
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
    /// The style asked for a failure, or one that only the operator can phrase.
    ///
    /// `["error", message]` is the deliberate case: a style saying a branch should not be
    /// reachable. The rest are messages with a value in them — an index and a bound — which no
    /// fixed variant can carry.
    #[error("{0}")]
    Custom(alloc::string::String),
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
        Expr::Rgba { args } => {
            let mut channels = [0.0, 0.0, 0.0, 1.0];
            for (slot, arg) in channels.iter_mut().zip(args) {
                *slot = expect_number(&evaluate(arg, context)?)?;
            }
            // Red, green and blue arrive 0..255 and alpha 0..1, which is how CSS spells it and
            // what the spec inherits.
            Ok(colour_value([
                channels[0] / 255.0,
                channels[1] / 255.0,
                channels[2] / 255.0,
                channels[3],
            ]))
        }
        Expr::Concat(args) => {
            // Everything coerces, which is what makes `["concat", ["get", "name"], " (", …]`
            // work on a property that might be a number. No arguments is the empty string, the
            // identity for concatenation.
            let mut out = String::new();
            for arg in args {
                out.push_str(&to_string(&evaluate(arg, context)?));
            }
            Ok(Value::String(out))
        }
        Expr::Join { items, separator } => {
            let items = evaluate(items, context)?;
            let separator = expect_str(separator, context)?;
            let Value::Array(items) = items else {
                return Err(EvaluationError::Type {
                    expected: "array",
                    got: items.type_name(),
                });
            };

            // Elements must already be strings. Unlike `concat`, `join` does not coerce: an
            // array of numbers is a style that has not decided how they should read, and the
            // spec would rather ask than pick a format.
            let mut parts = Vec::with_capacity(items.len());
            for item in &items {
                let Value::String(text) = item else {
                    return Err(EvaluationError::Type {
                        expected: "string",
                        got: item.type_name(),
                    });
                };
                parts.push(text.as_str());
            }
            Ok(Value::String(parts.join(&separator)))
        }
        // Rust's `to_uppercase` is the full Unicode mapping, which is the one that can change a
        // string's *length* — `ß` upcases to `SS`. mbgl walks codepoints and maps each singly,
        // so the two disagree on exactly those characters. The spec says "the input string
        // converted to upper case" and names no algorithm, so the difference is real and
        // unresolvable from the spec; the full mapping is chosen because it is the correct
        // answer for a reader, and it is written down here so the disagreement is not a
        // surprise if a golden ever covers it.
        Expr::CaseFold { upper, arg } => {
            let value = evaluate(arg, context)?;
            let Some(text) = value.as_str() else {
                return Err(EvaluationError::Type {
                    expected: "string",
                    got: value.type_name(),
                });
            };
            Ok(Value::String(if *upper {
                text.to_uppercase()
            } else {
                text.to_lowercase()
            }))
        }
        // `["at", index, array]`. Out of range is an error rather than null: the spec says the
        // index must be within the array, and a null would be indistinguishable from an element
        // that is one.
        Expr::At { index, array } => {
            let position = number_of(evaluate(index, context)?)?;
            let value = evaluate(array, context)?;
            let Some(items) = value.as_array() else {
                return Err(EvaluationError::Type {
                    expected: "array",
                    got: value.type_name(),
                });
            };
            if position.fract() != 0.0 || position < 0.0 {
                return Err(EvaluationError::Custom(alloc::format!(
                    "Array index must be a non-negative integer, but found {position} instead."
                )));
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let at = position as usize;
            items.get(at).cloned().ok_or_else(|| {
                EvaluationError::Custom(alloc::format!(
                    "Array index out of bounds: {at} > {}.",
                    items.len().saturating_sub(1)
                ))
            })
        }
        // An empty delimiter splits into *characters*, and mbgl means Unicode characters rather
        // than bytes -- it steps by `getUnicodeCharacterOffset`. So this splits on `char`
        // boundaries, which is the same thing for anything a style will hold.
        Expr::Split { input, delimiter } => {
            let text = string_of(evaluate(input, context)?)?;
            let separator = string_of(evaluate(delimiter, context)?)?;
            let parts: Vec<Value> = if separator.is_empty() {
                text.chars().map(|c| Value::String(c.to_string())).collect()
            } else {
                text.split(separator.as_str())
                    .map(|part| Value::String(part.to_string()))
                    .collect()
            };
            Ok(Value::Array(parts))
        }
        // Un-premultiplied on the way out, and the alpha rounded to two places. Both are mbgl's
        // `Color::toArray`, and both matter: colours are *stored* premultiplied here, so
        // returning the channels as held would give a translucent red as a dark one.
        Expr::ToRgba(inner) => {
            let value = evaluate(inner, context)?;
            let color = crate::property::as_color(&value).map_err(|_| EvaluationError::Type {
                expected: "color",
                got: value.type_name(),
            })?;
            if color.a == 0.0 {
                return Ok(Value::Array(alloc::vec![Value::Number(0.0); 4]));
            }
            let channel = |c: f32| f64::from(c) * 255.0 / f64::from(color.a);
            Ok(Value::Array(alloc::vec![
                Value::Number(channel(color.r)),
                Value::Number(channel(color.g)),
                Value::Number(channel(color.b)),
                Value::Number((f64::from(color.a) * 100.0 + 0.5).floor() / 100.0),
            ]))
        }
        Expr::TypeOf(inner) => Ok(Value::String(spec_type_name(&evaluate(inner, context)?))),
        // Always fails, which is the point: it is how a style says a branch should not be
        // reachable. The message is the style's, so it is evaluated rather than quoted.
        Expr::Error(inner) => Err(EvaluationError::Custom(string_of(evaluate(
            inner, context,
        )?)?)),
        Expr::Length(inner) => match evaluate(inner, context)? {
            Value::String(text) => Ok(Value::Number(text.chars().count() as f64)),
            Value::Array(items) => Ok(Value::Number(items.len() as f64)),
            other => Err(EvaluationError::Type {
                expected: "string or array",
                got: other.type_name(),
            }),
        },
        Expr::In { needle, haystack } => {
            let needle = evaluate(needle, context)?;
            let haystack = evaluate(haystack, context)?;
            Ok(Value::Bool(find_in(&needle, &haystack, 0)?.is_some()))
        }
        Expr::IndexOf {
            needle,
            haystack,
            from,
        } => {
            let needle = evaluate(needle, context)?;
            let haystack = evaluate(haystack, context)?;
            let length = sequence_length(&haystack)?;
            let start = match from {
                Some(from) => relative_index(expect_number(&evaluate(from, context)?)?, length),
                None => 0,
            };
            #[allow(clippy::cast_precision_loss)]
            Ok(Value::Number(
                find_in(&needle, &haystack, start)?.map_or(-1.0, |index| index as f64),
            ))
        }
        Expr::Slice { value, start, end } => {
            let value = evaluate(value, context)?;
            let length = sequence_length(&value)?;
            let first = relative_index(expect_number(&evaluate(start, context)?)?, length);
            let last = match end {
                Some(end) => relative_index(expect_number(&evaluate(end, context)?)?, length),
                None => length,
            };
            // An inverted or empty range is empty, not an error: `["slice", s, 5, 2]` is a
            // style asking for nothing, and returning nothing is the answer.
            let last = last.max(first);
            Ok(match value {
                Value::String(text) => {
                    Value::String(text.chars().skip(first).take(last - first).collect())
                }
                Value::Array(items) => {
                    Value::Array(items.into_iter().skip(first).take(last - first).collect())
                }
                // `sequence_length` already rejected anything else.
                _ => unreachable!("checked by sequence_length"),
            })
        }
        Expr::Format { sections } => {
            let mut out = Vec::with_capacity(sections.len());
            for section in sections {
                let content = evaluate(&section.content, context)?;
                let mut entry = alloc::collections::BTreeMap::new();

                // An image section carries no text and a text section carries no image; the
                // shaper needs both slots present so it can tell which it has.
                let (text, image) = match &content {
                    Value::Object(members) if members.contains_key("name") => {
                        (String::new(), content.clone())
                    }
                    other => (to_string(other), Value::Null),
                };
                entry.insert("text".to_string(), Value::String(text));
                entry.insert("image".to_string(), image);

                let optional = |expr: &Option<Box<Expr>>| -> Result<Value, EvaluationError> {
                    match expr {
                        Some(expr) => evaluate(expr, context),
                        None => Ok(Value::Null),
                    }
                };
                entry.insert("scale".to_string(), optional(&section.scale)?);
                entry.insert("fontStack".to_string(), optional(&section.font)?);
                entry.insert("textColor".to_string(), optional(&section.color)?);
                out.push(Value::Object(entry));
            }
            let mut formatted = alloc::collections::BTreeMap::new();
            formatted.insert("sections".to_string(), Value::Array(out));
            Ok(Value::Object(formatted))
        }
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
            let key = expect_str(key, context)?;
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
            let key = expect_str(key, context)?;
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

/// The length of a string or array, in the units the spec indexes by.
///
/// Characters rather than UTF-16 code units, which is where this diverges from the reference
/// implementation for text outside the basic multilingual plane. Recorded rather than papered
/// over: a style slicing an emoji would disagree, and the fix is a different index space rather
/// than a different bound.
fn sequence_length(value: &Value) -> Result<usize, EvaluationError> {
    match value {
        Value::String(text) => Ok(text.chars().count()),
        Value::Array(items) => Ok(items.len()),
        other => Err(EvaluationError::Type {
            expected: "string or array",
            got: other.type_name(),
        }),
    }
}

/// Resolves an index that may be negative, counting from the end.
///
/// Clamped rather than wrapped: an index past either end lands at that end, which is what makes
/// `["slice", s, -100]` the whole string rather than an error.
fn relative_index(index: f64, length: usize) -> usize {
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    if index < 0.0 {
        let from_end = length as f64 + index;
        if from_end < 0.0 { 0 } else { from_end as usize }
    } else if index > length as f64 {
        length
    } else {
        index as usize
    }
}

/// Finds a needle in a string or array, starting at `from`.
///
/// A null needle finds nothing rather than erroring, which the suite pins: styles use `["in",
/// ["get", "x"], …]` on features that may not carry `x`.
fn find_in(
    needle: &Value,
    haystack: &Value,
    from: usize,
) -> Result<Option<usize>, EvaluationError> {
    // Only scalars can be searched for. An object or array needle is a style asking a question
    // with no answer — the spec rejects it rather than reporting "not found", which would be
    // indistinguishable from a genuine miss.
    if matches!(needle, Value::Object(_) | Value::Array(_)) {
        return Err(EvaluationError::Type {
            expected: "boolean, string, number or null",
            got: needle.type_name(),
        });
    }

    match haystack {
        Value::String(text) => {
            let Value::String(needle) = needle else {
                return Ok(None);
            };
            let chars: Vec<char> = text.chars().collect();
            let target: Vec<char> = needle.chars().collect();
            if target.is_empty() {
                return Ok(Some(from.min(chars.len())));
            }
            Ok((from..chars.len().saturating_sub(target.len()) + 1)
                .find(|start| chars[*start..*start + target.len()] == target[..]))
        }
        Value::Array(items) => Ok(items
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, item)| *item == needle)
            .map(|(index, _)| index)),
        other => Err(EvaluationError::Type {
            expected: "string or array",
            got: other.type_name(),
        }),
    }
}

/// A colour, as the spec renders one: four channels in 0..1.
fn colour_value(channels: [f64; 4]) -> Value {
    // Inline, not a four-element `Value::Array`. The array spelling cost a heap allocation for
    // sixteen bytes of channel, paid once per feature for every colour property a layer
    // data-drives, to rebuild something the style fixed when it was parsed.
    #[allow(clippy::cast_possible_truncation)]
    Value::Color(crate::property::Color {
        r: channels[0] as f32,
        g: channels[1] as f32,
        b: channels[2] as f32,
        a: channels[3] as f32,
    })
}

/// Converts a value to colour channels, or reports that it is not one.
///
/// Strings go through the same CSS parser the rest of the crate uses, so a colour written in a
/// legacy function and a colour written as a paint value agree to the last bit. Arrays are
/// `[r, g, b]` or `[r, g, b, a]` with the channels 0..255 and the alpha 0..1 — CSS's convention,
/// not the normalized one, which is why an already-normalized colour must not be sent through
/// here a second time.
fn to_colour(value: &Value) -> Option<[f64; 4]> {
    match value {
        Value::Color(color) => Some([
            f64::from(color.r),
            f64::from(color.g),
            f64::from(color.b),
            f64::from(color.a),
        ]),
        Value::String(text) => {
            let parsed = crate::property::Color::parse(text).ok()?;
            Some([
                f64::from(parsed.r),
                f64::from(parsed.g),
                f64::from(parsed.b),
                f64::from(parsed.a),
            ])
        }
        Value::Array(items) if items.len() == 3 || items.len() == 4 => {
            let mut channels = [0.0, 0.0, 0.0, 1.0];
            for (slot, item) in channels.iter_mut().zip(items) {
                *slot = item.as_number()?;
            }
            Some([
                channels[0] / 255.0,
                channels[1] / 255.0,
                channels[2] / 255.0,
                channels[3],
            ])
        }
        _ => None,
    }
}

/// Whether a value satisfies the type a property spec asks for.
///
/// A colour is a string in the style and an array once resolved, so both are accepted; the
/// coercion happens later, and rejecting the string here would make every colour function fall
/// back to its default.
fn matches_spec_type(expected: Type, value: &Value) -> bool {
    match expected {
        Type::Value => true,
        Type::Number => matches!(value, Value::Number(_)),
        Type::String => matches!(value, Value::String(_)),
        Type::Boolean => matches!(value, Value::Bool(_)),
        Type::Object => matches!(value, Value::Object(_)),
        Type::Array(_) => matches!(value, Value::Array(_)),
        Type::Color => matches!(value, Value::String(_) | Value::Array(_)),
        Type::Null => matches!(value, Value::Null),
        // A formatted property accepts anything, because anything can be wrapped in a section.
        Type::Formatted => true,
    }
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

    // Function default, then the property's, then an error. Running out is not a null: a legacy
    // function with nothing to fall back to has no value for this feature, and null would render
    // as absent rather than report that the style and the data disagree.
    let fallback = || -> Result<Value, EvaluationError> {
        function.fallback().cloned().ok_or_else(|| {
            EvaluationError::Custom(alloc::format!(
                "expected {}, got null",
                function
                    .property_type
                    .map_or_else(|| alloc::string::String::from("a value"), Type::name)
            ))
        })
    };

    match function.kind {
        // Identity passes the property through, but only when it is the type the property spec
        // asks for. A `number` property whose feature carries a string falls back rather than
        // handing a string to something expecting a number — the check is the whole difference
        // between identity and a bare `["get", …]`.
        LegacyKind::Identity => {
            if input == Value::Null {
                return fallback();
            }
            let acceptable = function
                .property_type
                .is_none_or(|expected| matches_spec_type(expected, &input));
            if acceptable { Ok(input) } else { fallback() }
        }

        // Exact equality against each stop input. Types are not coerced: the spec's own suite
        // has a case where the property is the number 0 and the stop is the string "0", and it
        // expects the default.
        LegacyKind::Categorical => function
            .stops
            .iter()
            .find(|(stop, _)| *stop == input)
            .map_or_else(fallback, |(_, output)| Ok(output.clone())),

        // The output of the last stop at or below the input, clamping below the range to the
        // first stop rather than falling back. The fallback is for a property that is missing or
        // the wrong type; a property that is simply small is still in the function's domain.
        LegacyKind::Interval => {
            let Some(position) = input.as_number() else {
                return fallback();
            };
            let mut chosen: Option<(f64, &Value)> = None;
            for (stop, output) in &function.stops {
                let Some(stop) = stop.as_number() else {
                    continue;
                };
                if stop > position {
                    break;
                }
                // Strictly greater, so duplicate stop inputs keep the *first* of them. The
                // suite pins this: stops at 1 for both "b" and "c" select "b".
                if chosen.is_none_or(|(previous, _)| stop > previous) {
                    chosen = Some((stop, output));
                }
            }
            match chosen {
                Some((_, output)) => Ok(output.clone()),
                // Below every stop: clamp to the first, if there is one.
                None => function
                    .stops
                    .first()
                    .map_or_else(fallback, |(_, output)| Ok(output.clone())),
            }
        }

        LegacyKind::Exponential => {
            let Some(position) = input.as_number() else {
                return fallback();
            };
            let numeric: Vec<(f64, &Value)> = function
                .stops
                .iter()
                .filter_map(|(stop, output)| stop.as_number().map(|s| (s, output)))
                .collect();
            let Some(first) = numeric.first() else {
                return fallback();
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
        // The curve eases the plain fraction rather than the input. mbgl's
        // `CubicBezierInterpolator` computes `interpolationFactor(1.0, …)` -- which for a base
        // of one is exactly `(position - lower) / span` -- and solves the Bézier for it.
        Interpolation::CubicBezier { x1, y1, x2, y2 } => {
            crate::expression::solve_unit_bezier(x1, y1, x2, y2, (position - lower) / span)
        }
    }
}

/// Blends two values.
///
/// Numbers, colours, and equal-length numeric arrays. A colour-typed property has its curve's
/// *stops* coerced rather than its result (see `coerce_to_color`), so both ends arrive as
/// colours. The channels are premultiplied sRGB in 0..1, which is the space mbgl blends in, and
/// they are blended channel-wise exactly as the four-element array they used to be — the
/// arithmetic is unchanged, only the container is.
///
/// The form is `a * (1 - t) + b * t`, not the algebraically equal `a + (b - a) * t`. Both the
/// spec's reference implementation and mbgl use the first, and in floating point they differ in
/// the last bits — which is a diff on every interpolated color and width.
fn mix(lower: &Value, upper: &Value, t: f64) -> Result<Value, EvaluationError> {
    match (lower, upper) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * (1.0 - t) + b * t)),
        (Value::Color(a), Value::Color(b)) => {
            // Through `f64` and back, so the rounding matches what the four-element array did
            // bit for bit. Blending in `f32` would be a diff on every interpolated colour.
            let blend = |a: f32, b: f32| {
                #[allow(clippy::cast_possible_truncation)]
                {
                    (f64::from(a) * (1.0 - t) + f64::from(b) * t) as f32
                }
            };
            Ok(Value::Color(crate::property::Color {
                r: blend(a.r, b.r),
                g: blend(a.g, b.g),
                b: blend(a.b, b.b),
                a: blend(a.a, b.a),
            }))
        }
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
        // The spec's unary maths, each `<cmath>`'s function of the same name. A domain error --
        // `sqrt` of a negative, `asin` past one, `ln` of zero -- produces NaN or an infinity
        // rather than an error, which is what mbgl's `Result<double>` returns too: the C
        // functions do not report, and the spec does not ask them to.
        ArithmeticOp::Sqrt => numbers[0].sqrt(),
        ArithmeticOp::Ln => numbers[0].ln(),
        ArithmeticOp::Log2 => numbers[0].log2(),
        ArithmeticOp::Log10 => numbers[0].log10(),
        ArithmeticOp::Sin => numbers[0].sin(),
        ArithmeticOp::Cos => numbers[0].cos(),
        ArithmeticOp::Tan => numbers[0].tan(),
        ArithmeticOp::Asin => numbers[0].asin(),
        ArithmeticOp::Acos => numbers[0].acos(),
        ArithmeticOp::Atan => numbers[0].atan(),
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
            CastKind::Color => match to_colour(&value) {
                Some(colour) => return Ok(colour_value(colour)),
                None => last = Some(value.type_name()),
            },
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
        // Arrays and objects serialize as compact JSON. This used to be `{other:?}`, which is
        // Rust's Debug form: a style doing `["to-string", ["get", "tags"]]` rendered
        // `Array([Number(1.0)])` onto the map. Wrong output that looks like a crash report is
        // still wrong output, and nothing in the type system was going to catch it.
        aggregate => {
            let mut out = String::new();
            write_json(&mut out, aggregate);
            out
        }
    }
}

/// Writes a value as compact JSON, the form the spec's string conversion produces.
///
/// Object keys come out sorted because [`Value`]'s objects are ordered maps — the same choice
/// that makes a style's serialization reproducible everywhere else.
fn write_json(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(_) | Value::String(_) => {
            if let Value::String(text) = value {
                out.push('"');
                for ch in text.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        // Control characters have no literal form in JSON.
                        c if (c as u32) < 0x20 => {
                            out.push_str(&alloc::format!("\\u{:04x}", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            } else {
                out.push_str(&to_string(value));
            }
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        Value::Object(members) => {
            out.push('{');
            for (index, (key, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, &Value::String(key.clone()));
                out.push(':');
                write_json(out, member);
            }
            out.push('}');
        }
        // The spec's `to-string` on a colour gives its `rgba(...)` form, and JSON has no colour,
        // so the string conversion is the one that carries meaning here.
        Value::Color(color) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let byte = |channel: f32| (channel * 255.0).round() as u8;
            out.push_str(&alloc::format!(
                "\"rgba({},{},{},{})\"",
                byte(color.r),
                byte(color.g),
                byte(color.b),
                color.a
            ));
        }
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
        // An empty array and an empty object are true, and so is any colour.
        Value::Array(_) | Value::Object(_) | Value::Color(_) => true,
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

/// The spec's name for a value's type, as `typeof` reports it.
///
/// An array names its element type and its length — `array<number, 3>` — and an array whose
/// elements disagree is `array<value, N>`. mbgl builds it the same way, taking the first
/// element's type and widening to `value` at the first that differs; an empty array has no
/// element type to name, so it is `array` alone.
fn spec_type_name(value: &Value) -> alloc::string::String {
    use alloc::string::ToString as _;
    match value {
        Value::Array(items) => {
            let mut element: Option<&'static str> = None;
            for item in items {
                let this = scalar_type_name(item);
                match element {
                    None => element = Some(this),
                    Some(seen) if seen == this => {}
                    Some(_) => {
                        element = Some("value");
                        break;
                    }
                }
            }
            match element {
                Some(name) => alloc::format!("array<{name}, {}>", items.len()),
                None => "array".to_string(),
            }
        }
        other => scalar_type_name(other).to_string(),
    }
}

/// A value's own type name, without descending into an array.
fn scalar_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Color(_) => "color",
        Value::Array(_) => "array",
    }
}

/// A number argument, or the type error the operator would have raised.
fn number_of(value: Value) -> Result<f64, EvaluationError> {
    value.as_number().ok_or(EvaluationError::Type {
        expected: "number",
        got: value.type_name(),
    })
}

/// A string argument, or the type error the operator would have raised.
fn string_of(value: Value) -> Result<alloc::string::String, EvaluationError> {
    value
        .as_str()
        .map(alloc::string::ToString::to_string)
        .ok_or(EvaluationError::Type {
            expected: "string",
            got: value.type_name(),
        })
}

fn expect_number(value: &Value) -> Result<f64, EvaluationError> {
    value.as_number().ok_or(EvaluationError::Type {
        expected: "number",
        got: value.type_name(),
    })
}

/// The string an expression names, without copying it when it is already one.
///
/// # Why this is not `expect_string(&evaluate(..))`
///
/// That spelling allocates twice for the commonest expression in any style. `["get", "name"]`
/// holds its key as `Expr::Literal(Value::String)`; evaluating it clones the `Value`, and
/// `expect_string` then copies the text out of the clone. Both happen per feature, so a layer
/// of seventeen thousand features does thirty-four thousand allocations to read a key that was
/// known when the style was parsed.
///
/// A literal is borrowed straight from the tree. Anything else is evaluated as before, but the
/// text is moved out of the resulting `Value` rather than copied from it.
fn expect_str<'a>(expr: &'a Expr, context: &Context<'_>) -> Result<Cow<'a, str>, EvaluationError> {
    if let Expr::Literal(Value::String(text)) = expr {
        return Ok(Cow::Borrowed(text));
    }
    match evaluate(expr, context)? {
        Value::String(text) => Ok(Cow::Owned(text)),
        other => Err(EvaluationError::Type {
            expected: "string",
            got: other.type_name(),
        }),
    }
}
