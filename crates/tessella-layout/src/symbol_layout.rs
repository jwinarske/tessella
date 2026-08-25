//! A symbol layer's labels, resolved but not yet shaped.
//!
//! mbgl's `SymbolLayout`, and the reason it exists is timing. Every other layer type turns
//! features into vertices in one pass: the geometry is in the tile and nothing else is needed.
//! A symbol layer cannot, because shaping needs glyph metrics and the glyphs are a *network
//! resource* whose URL is not known until the text has been resolved. So the work splits in two,
//! and mbgl splits it the same way — construct the layout at parse time, and `prepareSymbols`
//! once the ranges have arrived.
//!
//! # What the first phase produces
//!
//! Text and geometry, per feature, plus the set of codepoints per font stack. That set is the
//! whole point: it is what the glyph manager fetches, and it cannot be known without evaluating
//! `text-field` against every feature of every symbol layer that reads this source.
//!
//! # Why the phases are types rather than a flag
//!
//! A half-built bucket that is sometimes shaped and sometimes not is the kind of state that
//! draws blank tiles when a font is slow. [`SymbolLayout`] holds no vertices at all; the only
//! way to get them is [`SymbolLayout::lay_out`], which takes the glyphs it needs as an argument.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tessella_style::document::PropertyValue;
use tessella_style::expression::{Expression, Feature};
use tessella_style::{Layer, Value};

use crate::symbol::{self, GlyphDependencies};
use crate::symbol_bucket::{
    Glyphs, Label, LaidOut, LineLabel, LineOptions, SymbolBuffers, SymbolOptions,
    build_line_symbols, build_symbols,
};

/// One layout property, evaluated at a zoom with no feature.
///
/// A layout property may be a plain value or an expression, and an expression over zoom is the
/// common case — `text-size` interpolated across a range is in most styles. Evaluating it at
/// build time is what makes the size the one this tile draws at.
fn layout_value(layer: &Layer, key: &str, zoom: f64) -> Option<Value> {
    match layer.layout.get(key)? {
        PropertyValue::Literal(literal) => Some(literal.clone()),
        PropertyValue::Expression(expression) => Expression::parse(expression.value())
            .ok()?
            .evaluate(Some(zoom), None)
            .ok(),
    }
}

/// `symbol-placement`: where a layer's labels sit relative to their features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Placement {
    /// At the feature's own point. The style default.
    #[default]
    Point,
    /// Repeated along the feature's line.
    Line,
    /// Once, at the middle of the feature's line.
    LineCenter,
}

impl Placement {
    /// Reads `symbol-placement`, defaulting the way the spec does.
    ///
    /// An unrecognized value is the default rather than an error: a style written against a
    /// newer spec must still draw, and mbgl's enum conversion does the same.
    #[must_use]
    pub fn of(layer: &Layer, zoom: f64) -> Self {
        match layout_value(layer, "symbol-placement", zoom)
            .as_ref()
            .and_then(Value::as_str)
        {
            Some("line") => Self::Line,
            Some("line-center") => Self::LineCenter,
            _ => Self::Point,
        }
    }

    /// Whether labels follow the feature's geometry rather than sitting at a point.
    #[must_use]
    pub const fn along_line(self) -> bool {
        matches!(self, Self::Line | Self::LineCenter)
    }
}

/// Where one label goes, in tile units.
#[derive(Debug, Clone, PartialEq)]
pub enum Anchoring {
    /// At a point.
    Point((f32, f32)),
    /// Along a line.
    Line(Vec<(f32, f32)>),
}

/// One feature's label, resolved but not shaped.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// What it says, after tokens and expressions.
    pub text: String,
    /// The font stack it is set in.
    pub fonts: Vec<String>,
    /// Where it goes.
    pub anchoring: Anchoring,
}

/// A symbol layer's contribution to one tile, before glyphs.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolLayout {
    /// One entry per label this layer draws on this tile.
    pub pending: Vec<Pending>,
    /// How the text is set.
    pub symbol: SymbolOptions,
    /// How it follows a line, when it does.
    pub line: LineOptions,
    /// Where the labels sit.
    pub placement: Placement,
}

impl SymbolLayout {
    /// An empty layout reading `layer`'s layout properties at `zoom`.
    ///
    /// `overscaling` is the tile's, which line placement needs so a child tile's anchors stay
    /// aligned with its parent's — without it every label jumps at a zoom crossing.
    ///
    /// Layout properties are evaluated at the zoom and *not* per feature. The spec allows
    /// `text-size` and several others to be data-driven, and a layer that uses that draws its
    /// labels at the layer's size rather than each feature's. It is a real gap and not a
    /// simplification of the model: the vertex already carries a size per quad, so what is
    /// missing is a size per label rather than anything about the encoding.
    #[must_use]
    pub fn new(layer: &Layer, zoom: f64, overscaling: f32) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let number = |key: &str| {
            layout_value(layer, key, zoom)
                .as_ref()
                .and_then(Value::as_number)
                .map(|value| value as f32)
        };

        let symbol = SymbolOptions {
            size: number("text-size").unwrap_or(16.0),
            max_width_ems: number("text-max-width").unwrap_or(10.0),
            letter_spacing: number("text-letter-spacing").unwrap_or(0.0),
            ..SymbolOptions::default()
        };
        let placement = Placement::of(layer, zoom);

        Self {
            pending: Vec::new(),
            symbol,
            line: LineOptions {
                symbol,
                spacing: number("symbol-spacing").unwrap_or(250.0),
                // The spec's default is 45 degrees, and it is in degrees on the wire.
                max_angle: number("text-max-angle").unwrap_or(45.0).to_radians(),
                overscaling,
                centred: placement == Placement::LineCenter,
            },
            placement,
        }
    }

    /// Resolves one feature's label and records it, if it has one.
    ///
    /// `rings` is the feature's geometry in tile units, already projected and clipped — the
    /// caller owns that because it is the tile builder that knows the tile.
    ///
    /// A feature whose `text-field` resolves to nothing is not recorded, which is what makes an
    /// unnamed road produce no symbol rather than a label reading `{name}`.
    pub fn push(
        &mut self,
        layer: &Layer,
        zoom: f64,
        feature: &dyn Feature,
        rings: &[Vec<(f32, f32)>],
    ) {
        let Some(label) = symbol::label(layer, zoom, feature) else {
            return;
        };
        if label.text.is_empty() {
            return;
        }

        for ring in rings {
            let anchoring = if self.placement.along_line() {
                // A line needs two points to have a direction; one point is not a short line.
                if ring.len() < 2 {
                    continue;
                }
                Anchoring::Line(ring.clone())
            } else {
                let Some(first) = ring.first() else { continue };
                Anchoring::Point(*first)
            };

            self.pending.push(Pending {
                text: label.text.clone(),
                fonts: label.fonts.clone(),
                anchoring,
            });
        }
    }

    /// The glyphs this layout needs before it can be shaped.
    ///
    /// What the manager fetches. A stack the layer names but that resolves to nothing is left
    /// out, because an entry under an empty key builds a URL of `//0-255.pbf`.
    #[must_use]
    pub fn dependencies(&self) -> GlyphDependencies {
        let mut out = GlyphDependencies::new();
        for pending in &self.pending {
            if pending.fonts.is_empty() {
                continue;
            }
            out.entry(pending.fonts.clone())
                .or_default()
                .extend(pending.text.chars().map(|character| character as u32));
        }
        out
    }

    /// Whether this layer draws anything on this tile.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The second phase: shape the labels and build the vertex buffers.
    ///
    /// A label whose glyphs are not all packed draws the ones that are and still measures the
    /// whole for collision, so a pan into new text draws what it has rather than nothing.
    ///
    /// Point-placed and line-placed labels go into *separate* buffers even though a layer has
    /// one of each at most, because the two builders assign vertex ranges against their own
    /// buffer and a range from one does not address the other. A layer mixing them would be a
    /// layer with two `symbol-placement` values, which the spec does not have.
    #[must_use]
    pub fn lay_out<G: Glyphs + ?Sized>(&self, glyphs: &G) -> (SymbolBuffers, Vec<LaidOut>) {
        if self.placement.along_line() {
            let labels: Vec<LineLabel> = self
                .pending
                .iter()
                .filter_map(|pending| match &pending.anchoring {
                    Anchoring::Line(line) => Some(LineLabel {
                        text: pending.text.to_string(),
                        line: line.clone(),
                    }),
                    Anchoring::Point(_) => None,
                })
                .collect();
            build_line_symbols(&labels, glyphs, &self.line)
        } else {
            let labels: Vec<Label> = self
                .pending
                .iter()
                .filter_map(|pending| match pending.anchoring {
                    Anchoring::Point(anchor) => Some(Label {
                        text: pending.text.to_string(),
                        anchor,
                    }),
                    Anchoring::Line(_) => None,
                })
                .collect();
            build_symbols(&labels, glyphs, &self.symbol)
        }
    }
}
