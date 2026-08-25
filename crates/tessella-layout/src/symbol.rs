//! Turning a feature into a label.
//!
//! The step between the decoder and placement: a feature's properties become the text a label
//! says, and the layout properties around it become how that text is set. Shaping, the atlas and
//! the collision box all sit on the far side of this; what happens here is resolving the style.
//!
//! # `text-field` has two syntaxes and both are still in use
//!
//! The modern form is an expression — `["get", "name"]` — and the legacy form is a template
//! string, `"{name}"`, with braces naming properties. mbgl supports both because styles in the
//! wild use both, often in the same document, and a frontend that read only expressions would
//! render half the basemaps on the internet with no labels at all.
//!
//! An unknown token is left in place, braces and all, which is `util::replaceTokens`' rule and
//! the same one the tile URL templates follow. A label reading `{nmae}` on the map is a typo
//! somebody can see and fix; a label silently reduced to nothing is one they cannot.
//!
//! # A feature with no text is not a symbol
//!
//! Most features in a symbol layer's source have no name. They produce no label rather than an
//! empty one — an empty label still has an anchor, a collision box and a place in the sort
//! order, and would push real labels off the map to draw nothing.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tessella_style::expression::{Expression, Feature};
use tessella_style::{Layer, PropertyValue, Value};

/// Parses and evaluates a style expression against a feature.
///
/// Parsing per call rather than once is deliberate for now: a symbol layer's `text-field` is
/// evaluated once per feature, and §12.1's compiled-expression cache is the place that stops
/// being cheap. Doing it here would put a cache in the wrong crate.
fn evaluate(value: &Value, zoom: f64, feature: &dyn Feature) -> Option<Value> {
    Expression::parse(value)
        .ok()?
        .evaluate(Some(zoom), Some(feature))
        .ok()
}

/// A label resolved from one feature.
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    /// What it says, after tokens and expressions are resolved.
    pub text: String,
    /// The font stack it is set in, as the style names it.
    pub fonts: Vec<String>,
}

/// Replaces `{token}` with the feature's property of that name.
///
/// mbgl's `replaceTokens`. An unrecognised token survives verbatim, braces included — the same
/// rule the tile URL templates follow, and for the same reason: a visible `{nmae}` is a typo
/// somebody can fix, and a silently empty label is not.
#[must_use]
pub fn replace_tokens(template: &str, feature: &dyn Feature) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('}') else {
            // An unclosed brace is not a token; the rest of the string is literal.
            break;
        };
        let name = &rest[1..close];
        match feature.property(name) {
            Some(value) => out.push_str(&stringify(&value)),
            None => out.push_str(&rest[..=close]),
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// A property as a label would show it.
///
/// The style spec has one number type and it is a double, so an elevation of 1200 arrives as
/// `1200.0` and a label reading "1200.0" would be wrong on a map people read. Rust's float
/// `Display` already drops the trailing zero, so there is nothing to do here — mbgl formats
/// explicitly because C++'s default does not. Written out because the absence of a special case
/// is the sort of thing that gets "fixed" back in.
fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

/// The font stack a layer sets its text in.
///
/// The spec's default when the layer does not say, which is what mbgl uses and what an origin
/// serving `{fontstack}` expects to be asked for.
#[must_use]
pub fn font_stack(layer: &Layer, zoom: f64, feature: &dyn Feature) -> Vec<String> {
    let Some(value) = layer.layout.get("text-font") else {
        return alloc::vec![
            "Open Sans Regular".to_string(),
            "Arial Unicode MS Regular".to_string()
        ];
    };
    let resolved = match value {
        PropertyValue::Literal(literal) => literal.clone(),
        PropertyValue::Expression(expression) => {
            evaluate(expression.value(), zoom, feature).unwrap_or(Value::Null)
        }
    };
    resolved.as_array().map_or_else(Vec::new, |fonts| {
        fonts
            .iter()
            .filter_map(|font| font.as_str().map(ToString::to_string))
            .collect()
    })
}

/// The label a feature produces in this layer, if any.
///
/// `None` when the layer has no `text-field`, when the field resolves to nothing, or when the
/// feature simply has no name — which is most features in most symbol sources.
#[must_use]
pub fn label(layer: &Layer, zoom: f64, feature: &dyn Feature) -> Option<Label> {
    let field = layer.layout.get("text-field")?;

    let text = match field {
        // A literal string is a token template; a literal anything else is taken as written.
        PropertyValue::Literal(Value::String(template)) => replace_tokens(template, feature),
        PropertyValue::Literal(other) => stringify(other),
        PropertyValue::Expression(expression) => {
            let value = evaluate(expression.value(), zoom, feature)?;
            match value {
                // An expression that produced a string may still contain tokens: mbgl resolves
                // them afterwards, and styles written against the legacy syntax and then
                // wrapped in an expression rely on it.
                Value::String(text) => replace_tokens(&text, feature),
                other => stringify(&other),
            }
        }
    };

    // Whitespace-only is nothing to draw. A label of spaces has a width and a collision box, and
    // would push real labels off the map to show nothing.
    if text.trim().is_empty() {
        return None;
    }

    Some(Label {
        text,
        fonts: font_stack(layer, zoom, feature),
    })
}
