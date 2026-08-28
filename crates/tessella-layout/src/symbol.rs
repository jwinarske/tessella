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
    ///
    /// The whole label, with every section run together. Everything that asks *what a label
    /// says* rather than how it is set reads this: the glyphs it needs, the key a line label is
    /// joined across tile seams by, whether it is blank.
    pub text: String,
    /// Its sections, which concatenate to [`Self::text`].
    ///
    /// One section for an ordinary label. A `["format", …]` gives several, and they can differ
    /// in scale — which is not a property of the text but of how it is drawn, so it is carried
    /// beside the text rather than inside it.
    pub sections: Vec<Section>,
    /// The font stack it is set in, as the style names it.
    pub fonts: Vec<String>,
}

/// One run of a label set the same way.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// The text of this run.
    pub text: String,
    /// Its `font-scale`, defaulting to one.
    pub scale: f32,
    /// The sprite this section draws, if it is an `["image", …]` rather than text.
    ///
    /// A section is one or the other. mbgl carries both slots on its `SectionOptions` and
    /// branches on which is set, and the same branch runs all the way down: an image is measured
    /// from the sprite, drawn from the icon atlas, and sized in pixels where a glyph is sized in
    /// ems.
    pub image: Option<String>,
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

/// The sprite a feature's `icon-image` names, if any.
///
/// The icon half of [`label`], and it has to be separate rather than a field on it: a symbol may
/// be an icon with no text at all — every POI marker on a map is one — so a builder that resolved
/// the icon only when there was text would draw none of them.
///
/// Tokens are resolved the same way, which is what makes `{maki}-15` work: the sprite name is
/// built from the feature's own properties, and a feature missing that property names no sprite
/// rather than one called `-15`.
#[must_use]
pub fn icon_image(layer: &Layer, zoom: f64, feature: &dyn Feature) -> Option<String> {
    let field = layer.layout.get("icon-image")?;

    let name = match field {
        PropertyValue::Literal(Value::String(template)) => replace_tokens(template, feature),
        PropertyValue::Literal(other) => stringify(other),
        PropertyValue::Expression(expression) => {
            let value = evaluate(expression.value(), zoom, feature)?;
            match value {
                Value::String(name) => replace_tokens(&name, feature),
                other => stringify(&other),
            }
        }
    };

    // An empty name is no icon. Unlike a label it is not trimmed: a sprite name is a key into the
    // index and leading space in one is a name that simply is not there, which is a missing icon
    // rather than a malformed one.
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// The label a feature produces in this layer, if any.
///
/// `None` when the layer has no `text-field`, when the field resolves to nothing, or when the
/// feature simply has no name — which is most features in most symbol sources.
#[must_use]
pub fn label(layer: &Layer, zoom: f64, feature: &dyn Feature) -> Option<Label> {
    let field = layer.layout.get("text-field")?;

    // A `["format", …]` evaluates to `{"sections": [...]}`, and the sections carry what the flat
    // text cannot: each run's own scale. Read here rather than reconstructed later, because by
    // the time anything downstream has the string the structure is gone.
    let mut sections: Vec<Section> = Vec::new();

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
                Value::Object(ref members) if members.contains_key("sections") => {
                    sections = read_sections(members.get("sections"), feature);
                    sections
                        .iter()
                        .map(|section| section.text.as_str())
                        .collect()
                }
                other => stringify(&other),
            }
        }
    };

    // Whitespace-only is nothing to draw. A label of spaces has a width and a collision box, and
    // would push real labels off the map to show nothing.
    if text.trim().is_empty() {
        return None;
    }

    // An ordinary label is one section at scale one, so nothing downstream needs to ask which
    // kind of label it has.
    if sections.is_empty() {
        sections.push(Section {
            text: text.clone(),
            scale: 1.0,
            image: None,
        });
    }

    Some(Label {
        text,
        sections,
        fonts: font_stack(layer, zoom, feature),
    })
}

/// Reads the sections a `["format", …]` evaluated to.
///
/// Each is `{text, image, scale, fontStack, textColor}`. Only the text and the scale are read:
/// an image section draws a sprite in the line and a per-section font stack sets it in another
/// face, and neither is built — a section carrying an image contributes no text, which is what
/// it would contribute anyway until the shaper can place one.
fn read_sections(sections: Option<&Value>, feature: &dyn Feature) -> Vec<Section> {
    let Some(Value::Array(entries)) = sections else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| {
            let members = entry.as_object()?;
            let text = match members.get("text") {
                Some(Value::String(text)) => replace_tokens(text, feature),
                _ => String::new(),
            };
            #[allow(clippy::cast_possible_truncation)]
            let scale = match members.get("scale") {
                Some(Value::Number(scale)) if *scale > 0.0 => *scale as f32,
                _ => 1.0,
            };
            let image = match members.get("image") {
                Some(Value::Object(members)) => match members.get("name") {
                    Some(Value::String(name)) => Some(name.clone()),
                    _ => None,
                },
                _ => None,
            };
            Some(Section { text, scale, image })
        })
        .collect()
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
