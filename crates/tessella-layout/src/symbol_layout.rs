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

use tessella_glyph::fonts::Fonts;

use crate::anchors::EXTENT;
use crate::symbol::{self, GlyphDependencies};
use crate::symbol_bucket::{
    Label, LaidOut, LineLabel, LineOptions, SymbolBuffers, SymbolOptions, build_line_symbols,
    build_symbols,
};

/// One layout property, evaluated at a zoom with no feature.
///
/// A layout property may be a plain value or an expression, and an expression over zoom is the
/// common case — `text-size` interpolated across a range is in most styles. Evaluating it at
/// build time is what makes the size the one this tile draws at.
fn layout_value(
    layer: &Layer,
    key: &str,
    zoom: f64,
    feature: Option<&dyn Feature>,
) -> Option<Value> {
    match layer.layout.get(key)? {
        PropertyValue::Literal(literal) => Some(literal.clone()),
        PropertyValue::Expression(expression) => Expression::parse(expression.value())
            .ok()?
            .evaluate(Some(zoom), feature)
            .ok(),
    }
}

/// How a layer sets its text, at a zoom and optionally for one feature.
///
/// The spec allows `text-size`, `text-max-width` and `text-letter-spacing` to be data-driven, so
/// two features of one layer can be set differently. Evaluating without a feature gives the
/// layer's own values, which is what a layout is constructed with and what a layer with no
/// data-driven property resolves to for every feature.
fn text_options(layer: &Layer, zoom: f64, feature: Option<&dyn Feature>) -> SymbolOptions {
    #[allow(clippy::cast_possible_truncation)]
    let number = |key: &str| {
        layout_value(layer, key, zoom, feature)
            .as_ref()
            .and_then(Value::as_number)
            .map(|value| value as f32)
    };
    SymbolOptions {
        size: number("text-size").unwrap_or(16.0),
        max_width_ems: number("text-max-width").unwrap_or(10.0),
        letter_spacing: number("text-letter-spacing").unwrap_or(0.0),
        ..SymbolOptions::default()
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
        match layout_value(layer, "symbol-placement", zoom, None)
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
    /// How *this feature's* text is set.
    ///
    /// The layer's, unless a layout property is data-driven — `text-size` is the one styles
    /// actually use that way, to make a capital larger than a town on the same layer. Held per
    /// label rather than per layer because that is the granularity the spec gives it, and the
    /// vertex already carries a size per quad.
    pub symbol: SymbolOptions,
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
    /// The layer's own values are held here; each label carries whatever its own feature
    /// evaluated to, which is the same thing unless a property is data-driven.
    #[must_use]
    pub fn new(layer: &Layer, zoom: f64, overscaling: f32) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let number = |key: &str| {
            layout_value(layer, key, zoom, None)
                .as_ref()
                .and_then(Value::as_number)
                .map(|value| value as f32)
        };

        let symbol = text_options(layer, zoom, None);
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
                // Not clipped: `get_anchors` tests each candidate position against the tile, so
                // a road crossing a seam gets anchors on the near side from each tile and the
                // two interleave rather than doubling up. Cutting the line here would instead
                // give each side its own ends, and a name would appear at every seam.
                Anchoring::Line(ring.clone())
            } else {
                let Some(first) = ring.first() else { continue };
                // A point label belongs to the tile it is in, and to no other. The features
                // reaching this builder are the whole source rather than one tile's share, so
                // without the test every tile of the cover draws every label — which looks
                // right on the tile that owns it and wrong on its neighbours. Half-open, so a
                // point on a boundary lands in exactly one tile.
                if !(0.0..EXTENT).contains(&first.0) || !(0.0..EXTENT).contains(&first.1) {
                    continue;
                }
                Anchoring::Point(*first)
            };

            self.pending.push(Pending {
                text: label.text.clone(),
                fonts: label.fonts.clone(),
                anchoring,
                symbol: text_options(layer, zoom, Some(feature)),
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

    /// The distinct font stacks this layout's labels are set in.
    ///
    /// Usually one. `text-font` is evaluated per feature, so a data-driven one gives a layer
    /// several — which is why laying out takes the whole store rather than one stack's glyphs.
    #[must_use]
    pub fn stacks(&self) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = Vec::new();
        for pending in &self.pending {
            if !out.contains(&pending.fonts) {
                out.push(pending.fonts.clone());
            }
        }
        out
    }

    /// The second phase: shape the labels and build the vertex buffers.
    ///
    /// A label whose glyphs are not all packed draws the ones that are and still measures the
    /// whole for collision, so a pan into new text draws what it has rather than nothing.
    ///
    /// Labels are laid out in *runs* — the longest stretch of consecutive labels sharing a font
    /// stack and a set of text options — and the runs joined. mbgl reaches the same place from
    /// the other end, handing `prepareSymbols` the whole `GlyphMap` and evaluating layout
    /// properties per feature.
    ///
    /// Consecutive, not grouped. A layer's labels sit in its buffer in the order the layer
    /// offers them — the golden pins that, since a tile's per-frame state is written into the
    /// slice layout recorded — so gathering every label of one font stack together would
    /// reorder the buffer against the oracle the moment a second stack appeared. With one stack
    /// and one size, which is the common case, there is one run and no join.
    ///
    /// # Panics
    ///
    /// When the joined buffers would exceed what a `u16` index reaches. See
    /// [`SymbolBuffers::append`].
    #[must_use]
    pub fn lay_out(&self, fonts: &Fonts) -> (SymbolBuffers, Vec<LaidOut>) {
        let mut buffers = SymbolBuffers::default();
        let mut laid = Vec::new();

        let mut start = 0usize;
        while start < self.pending.len() {
            let head = &self.pending[start];
            let end = self.pending[start..]
                .iter()
                .position(|pending| pending.fonts != head.fonts || pending.symbol != head.symbol)
                .map_or(self.pending.len(), |offset| start + offset);
            let run = &self.pending[start..end];
            start = end;

            let glyphs = fonts.stack(&head.fonts);
            let (built, entries) = if self.placement.along_line() {
                let labels: Vec<LineLabel> = run
                    .iter()
                    .filter_map(|pending| match &pending.anchoring {
                        Anchoring::Line(line) => Some(LineLabel {
                            text: pending.text.to_string(),
                            line: line.clone(),
                        }),
                        Anchoring::Point(_) => None,
                    })
                    .collect();
                let options = LineOptions {
                    symbol: head.symbol,
                    ..self.line
                };
                build_line_symbols(&labels, &glyphs, &options)
            } else {
                let labels: Vec<Label> = run
                    .iter()
                    .filter_map(|pending| match pending.anchoring {
                        Anchoring::Point(anchor) => Some(Label {
                            text: pending.text.to_string(),
                            anchor,
                        }),
                        Anchoring::Line(_) => None,
                    })
                    .collect();
                build_symbols(&labels, &glyphs, &head.symbol)
            };

            // Each run's ranges address its own buffer, so they shift by what was already here.
            // Getting this wrong writes one label's per-frame state over another's, which draws
            // as a label that will not fade and errors nowhere.
            let base = buffers.append(&built);
            laid.extend(entries.into_iter().map(|mut entry| {
                entry.vertices = entry.vertices.start + base..entry.vertices.end + base;
                entry
            }));
        }

        (buffers, laid)
    }
}
