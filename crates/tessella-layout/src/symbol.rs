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
//! # A token is a `get`, not a substitution
//!
//! mbgl converts `"{name}"` at style-parse time into `toString(get("name"))`, so a feature
//! without the property yields an *empty* label and therefore no symbol at all.
//!
//! This is where `text-field` and a tile URL part company, and the difference is not cosmetic.
//! `util::replaceTokens` leaves an unrecognised token in place, braces and all, because a URL
//! may legitimately contain braces and a request that 404s with `{nmae}` in it says why. A label
//! cannot do that: most features in a symbol source have no name, so leaving the token would
//! write a literal `{name}` across the map on every unnamed feature. Which is what this did
//! until an end-to-end test asked a water layer for its labels and got seventy-five of them.
//!
//! A brace with no closing brace is not a token and stays literal, which is mbgl's rule too —
//! its scan stops at the next `{` or `}` and treats anything unterminated as text.
//!
//! # A feature with no text is not a symbol
//!
//! Most features in a symbol layer's source have no name. They produce no label rather than an
//! empty one — an empty label still has an anchor, a collision box and a place in the sort
//! order, and would push real labels off the map to draw nothing.

use alloc::collections::BTreeSet;
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

/// Resolves `{token}` against the feature's properties.
///
/// Each token is a `get`: present becomes its value, absent becomes nothing. That is what mbgl's
/// conversion to `toString(get(...))` does, and it is *not* the tile URL rule — see the module
/// note for why the two differ.
///
/// A brace that is never closed, or one closed by another `{`, is not a token and stays literal.
#[must_use]
pub fn replace_tokens(template: &str, feature: &dyn Feature) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];

        // mbgl scans to the next reserved character rather than to the next `}`, so `{a{b}` is
        // a literal `{a` followed by the token `{b}` rather than a token named `a{b`.
        let end = rest[1..].find(['{', '}']).map(|index| index + 1);
        match end {
            Some(close) if rest.as_bytes()[close] == b'}' => {
                if let Some(value) = feature.property(&rest[1..close]) {
                    out.push_str(&stringify(&value));
                }
                rest = &rest[close + 1..];
            }
            // Unterminated, or terminated by another brace: literal up to that point.
            Some(close) => {
                out.push_str(&rest[..close]);
                rest = &rest[close..];
            }
            None => break,
        }
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

/// Which glyphs a tile's symbol layers need, per font stack.
///
/// mbgl's `GlyphDependencies`. The manager cannot fetch until it knows what to fetch, and what
/// to fetch is not a property of the style — it is a property of the *data*: which characters
/// appear in this tile's labels. A style naming one font stack over a world of tiles needs a
/// handful of ranges in Iceland and hundreds in Japan.
///
/// # It is collected before anything is shaped
///
/// Shaping needs advances, advances come from glyphs, and glyphs arrive over the network. So a
/// tile is walked once to find out what it will need, the ranges are fetched, and only then is
/// anything laid out. Doing it the other way — shaping and discovering a missing glyph — turns
/// one round trip per tile into one per label.
pub type GlyphDependencies = alloc::collections::BTreeMap<Vec<String>, BTreeSet<u32>>;

/// Collects the glyphs every symbol layer of `style` needs for these features.
///
/// `draws_from` decides which layers read this source, so a caller passes the same predicate the
/// tile builder uses rather than this crate guessing at source matching.
///
/// Codepoints above the Basic Multilingual Plane are collected like any other. The manager
/// declines to request them — there is no range file up there — and that decision belongs to the
/// manager rather than being anticipated here, since a local rasterizer will want them.
pub fn glyph_dependencies<'a, F, I>(
    layers: I,
    zoom: f64,
    features: &[&dyn Feature],
    mut wanted: F,
) -> GlyphDependencies
where
    I: IntoIterator<Item = &'a Layer>,
    F: FnMut(&Layer) -> bool,
{
    let mut out = GlyphDependencies::new();
    for layer in layers {
        if !wanted(layer) {
            continue;
        }
        for feature in features {
            let Some(label) = label(layer, zoom, *feature) else {
                continue;
            };
            // A stack the layer names but that resolves to nothing has no glyphs to ask for,
            // and an entry under an empty key would build a URL of `//0-255.pbf`.
            if label.fonts.is_empty() {
                continue;
            }
            out.entry(label.fonts)
                .or_default()
                .extend(label.text.chars().map(|character| character as u32));
        }
    }
    out
}
