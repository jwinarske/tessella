//! Feature filters, in both of the spec's syntaxes.
//!
//! A filter is either a modern expression or the legacy form the spec carried before
//! expressions existed. They are not distinguishable by shape — `["==", "$type", "Polygon"]` is
//! legacy and `["==", ["geometry-type"], "Polygon"]` is modern, and both are three-element
//! arrays headed by `"=="`. So the document parser keeps a filter raw and the decision is made
//! here.
//!
//! # The rules are mbgl's, deliberately
//!
//! Both the discriminator and the conversion are ported from
//! `src/mbgl/style/conversion/filter.cpp` in the pinned tree, which is itself a port of
//! mapbox-gl-js. That is not deference for its own sake: the C++ implementation is the golden
//! oracle (§9.1), and a filter that admits a different set of features produces a different
//! bucket, which produces a different vertex buffer, which fails the diff for a reason two
//! removes from the actual disagreement. Matching the oracle's semantics exactly is what keeps
//! a filter difference legible as a filter difference.
//!
//! # Legacy semantics are not expression semantics
//!
//! The important difference is what happens when a feature does not fit the question. An
//! expression comparison on a missing property is an error; a legacy filter yields `false`. An
//! expression `<` between a string and a number is an error; a legacy filter yields `false`.
//! Legacy filters are total, and they were designed that way because they run over every
//! feature in a tile, where one odd feature must not fail the tile.
//!
//! That is why legacy comparisons convert to their own operators rather than to `["get", k]`
//! wrapped in guards: the guards would have to reproduce the type rule anyway, and the version
//! that looks obviously equivalent is the one that quietly differs.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::expression::{CompareOp, Expr, Expression, FilterTarget};
use crate::value::Value;

/// A filter that could not be understood.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// The filter is not an array, or is empty.
    #[error("a filter must be a non-empty array")]
    NotAnArray,
    /// The head of the array is not an operator name.
    #[error("a filter's first element must name an operator")]
    OperatorNotAString,
    /// A legacy filter named something other than a string property.
    #[error("a legacy filter's property must be a string")]
    PropertyNotAString,
    /// The filter uses an operator this build does not implement.
    #[error("filter operator `{0}` is not implemented")]
    Unsupported(String),
    /// The filter is a modern expression that failed to parse.
    #[error("filter expression: {0}")]
    Expression(#[from] crate::expression::ParseError),
}

/// A compiled feature filter.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    expression: Expression,
}

impl Filter {
    /// Compiles a filter, in either syntax.
    ///
    /// # Errors
    ///
    /// [`FilterError`] when the value is not a well-formed filter in either syntax.
    pub fn parse(value: &Value) -> Result<Self, FilterError> {
        let expr = if is_expression_filter(value) {
            return Ok(Self {
                expression: Expression::parse_filter(value)?,
            });
        } else {
            convert_legacy(value)?
        };
        Ok(Self {
            expression: Expression::from_expr(expr),
        })
    }

    /// A filter that admits everything, for a layer that declares none.
    #[must_use]
    pub fn always() -> Self {
        Self {
            expression: Expression::from_expr(Expr::Literal(Value::Bool(true))),
        }
    }

    /// The compiled expression.
    #[must_use]
    pub fn expression(&self) -> &Expression {
        &self.expression
    }

    /// Whether a feature passes.
    ///
    /// A filter that fails to evaluate rejects the feature rather than propagating an error.
    /// Legacy filters are total by construction, and a modern one that errors on a particular
    /// feature is describing a feature it cannot classify — which is a feature it should not
    /// admit.
    #[must_use]
    pub fn matches(&self, feature: &dyn crate::expression::Feature, zoom: Option<f64>) -> bool {
        matches!(
            self.expression.evaluate(zoom, Some(feature)),
            Ok(Value::Bool(true))
        )
    }
}

/// Decides whether a filter is written in the modern expression syntax.
///
/// Ported from `isExpression` in mbgl's `filter.cpp`. The shape of the test is unavoidably
/// case-by-case, because the two syntaxes genuinely overlap and what separates them is where
/// an array or a string appears rather than anything structural.
fn is_expression_filter(value: &Value) -> bool {
    let Some(items) = value.as_array() else {
        return false;
    };
    let Some(operator) = items.first().and_then(Value::as_str) else {
        return false;
    };

    match operator {
        // Modern and legacy `has` share a syntax, so it only matters that the pseudo-properties
        // are legacy: `["has", "$id"]` has no expression equivalent.
        "has" => {
            items.len() >= 2
                && items[1]
                    .as_str()
                    .is_some_and(|operand| operand != "$id" && operand != "$type")
        }
        // No expression spells these, so they are legacy whenever they appear.
        "!in" | "!has" | "none" => false,
        // Modern `in` takes a haystack array; legacy `in` takes a property name and loose
        // values.
        "in" => items.len() >= 3 && (items[1].as_str().is_none() || items[2].as_array().is_some()),
        // Legacy comparison is exactly three elements with no nesting. Anything else — a
        // nested call on either side, or a different length — is modern.
        "==" | "!=" | ">" | ">=" | "<" | "<=" => {
            items.len() != 3 || items[1].as_array().is_some() || items[2].as_array().is_some()
        }
        // Modern when every child is itself modern or a bare boolean.
        "any" | "all" => items[1..]
            .iter()
            .all(|child| is_expression_filter(child) || child.as_bool().is_some()),
        _ => true,
    }
}

/// Converts a legacy filter into expression form.
fn convert_legacy(value: &Value) -> Result<Expr, FilterError> {
    if value == &Value::Null {
        return Ok(Expr::Literal(Value::Bool(true)));
    }
    let items = value.as_array().ok_or(FilterError::NotAnArray)?;
    if items.is_empty() {
        return Err(FilterError::NotAnArray);
    }
    let operator = items[0]
        .as_str()
        .ok_or(FilterError::OperatorNotAString)?
        .to_string();

    // An operator with no operands. `["any"]` is false because nothing satisfies it; `["all"]`
    // and `["none"]` are true because nothing contradicts them.
    if items.len() <= 1 {
        return Ok(Expr::Literal(Value::Bool(operator != "any")));
    }

    match operator.as_str() {
        "==" | "<" | "<=" | ">" | ">=" => comparison(&operator, items),
        // `!=` is the exact complement of `==`, including for a feature that lacks the
        // property. Building it as a negation rather than as its own operator is what
        // guarantees that.
        "!=" => Ok(Expr::Not(alloc::boxed::Box::new(comparison("==", items)?))),
        "any" => Ok(Expr::Any(convert_children(items)?)),
        "all" => Ok(Expr::All(convert_children(items)?)),
        "none" => Ok(Expr::Not(alloc::boxed::Box::new(Expr::Any(
            convert_children(items)?,
        )))),
        "in" => membership(items),
        "!in" => Ok(Expr::Not(alloc::boxed::Box::new(membership(items)?))),
        "has" => has(items),
        "!has" => Ok(Expr::Not(alloc::boxed::Box::new(has(items)?))),
        // `within` is a geometry predicate this build does not implement. mbgl parses it as a
        // modern expression; refusing it by name beats silently admitting every feature.
        "within" => Err(FilterError::Unsupported(operator)),
        // mbgl returns a literal true here, and this mirrors it — but the branch is
        // unreachable, in both implementations. `is_expression_filter` returns true for any
        // operator it does not specifically know, so an unrecognized name never arrives here;
        // it goes to the expression parser and is reported there. Everything that does arrive
        // is an operator the discriminator deliberately rejected, and all of those are handled
        // above by name. Kept so the port stays line-for-line comparable with the source.
        _ => Ok(Expr::Literal(Value::Bool(true))),
    }
}

fn convert_children(items: &[Value]) -> Result<Vec<Expr>, FilterError> {
    items[1..].iter().map(convert_legacy).collect()
}

/// The pseudo-properties `$type` and `$id` name the feature itself rather than its properties.
fn target(items: &[Value]) -> Result<FilterTarget, FilterError> {
    let property = items
        .get(1)
        .and_then(Value::as_str)
        .ok_or(FilterError::PropertyNotAString)?;
    Ok(match property {
        "$type" => FilterTarget::Type,
        "$id" => FilterTarget::Id,
        other => FilterTarget::Property(other.to_string()),
    })
}

fn comparison(operator: &str, items: &[Value]) -> Result<Expr, FilterError> {
    let target = target(items)?;
    let op = match operator {
        "==" => CompareOp::Eq,
        "<" => CompareOp::Lt,
        "<=" => CompareOp::Le,
        ">" => CompareOp::Gt,
        _ => CompareOp::Ge,
    };

    // mbgl registers `filter-type-==` and `filter-type-in` and nothing else for `$type`, so
    // ordering a geometry type has no meaning defined anywhere. Refused rather than invented.
    if target == FilterTarget::Type && op != CompareOp::Eq {
        return Err(FilterError::Unsupported(alloc::format!(
            "{operator} on $type"
        )));
    }

    Ok(Expr::FilterCompare {
        target,
        op,
        literal: items.get(2).cloned().unwrap_or(Value::Null),
    })
}

fn membership(items: &[Value]) -> Result<Expr, FilterError> {
    Ok(Expr::FilterIn {
        target: target(items)?,
        values: items[2.min(items.len())..].to_vec(),
    })
}

fn has(items: &[Value]) -> Result<Expr, FilterError> {
    Ok(Expr::FilterHas {
        target: target(items)?,
    })
}
