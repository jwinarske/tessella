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

use alloc::collections::{BTreeMap, BTreeSet};

use tessella_glyph::fonts::Fonts;

use crate::anchors::EXTENT;
use crate::symbol::{self, GlyphDependencies};
use crate::symbol_bucket::{
    IconLabel, IconOptions, Label, LaidOut, LineLabel, LineOptions, SymbolBuffers, SymbolOptions,
    build_icons, build_line_symbols, build_symbols,
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

/// How a layer draws its icons, at a zoom and optionally for one feature.
///
/// `icon-size` is a *multiplier* and defaults to one, unlike `text-size` which names a size in
/// pixels and defaults to sixteen. Reading one as the other draws every marker sixteen times too
/// large, which is why they do not share this function.
fn icon_options(layer: &Layer, zoom: f64, feature: Option<&dyn Feature>) -> IconOptions {
    #[allow(clippy::cast_possible_truncation)]
    let number = |key: &str| {
        layout_value(layer, key, zoom, feature)
            .as_ref()
            .and_then(Value::as_number)
            .map(|value| value as f32)
    };
    #[allow(clippy::cast_possible_truncation)]
    let pair = |key: &str| -> Option<[f32; 2]> {
        let value = layout_value(layer, key, zoom, feature)?;
        let array = value.as_array()?;
        if array.len() != 2 {
            return None;
        }
        Some([array[0].as_number()? as f32, array[1].as_number()? as f32])
    };

    IconOptions {
        size: number("icon-size").unwrap_or(1.0),
        offset: pair("icon-offset").unwrap_or([0.0, 0.0]),
        // On the wire in degrees, like `text-rotate`.
        rotate: number("icon-rotate").unwrap_or(0.0).to_radians(),
        anchor: anchor_of(layout_value(layer, "icon-anchor", zoom, feature).as_ref()),
    }
}

/// Reads a `*-anchor` value, defaulting the way the spec does.
fn anchor_of(value: Option<&Value>) -> tessella_glyph::shaping::Anchor {
    use tessella_glyph::shaping::Anchor;
    match value.and_then(Value::as_str) {
        Some("left") => Anchor::Left,
        Some("right") => Anchor::Right,
        Some("top") => Anchor::Top,
        Some("bottom") => Anchor::Bottom,
        Some("top-left") => Anchor::TopLeft,
        Some("top-right") => Anchor::TopRight,
        Some("bottom-left") => Anchor::BottomLeft,
        Some("bottom-right") => Anchor::BottomRight,
        _ => Anchor::Center,
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

/// One feature's symbol, resolved but not shaped.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// What it says, after tokens and expressions. Empty for an icon with no label.
    pub text: String,
    /// The sprite its `icon-image` names, if it has one.
    ///
    /// A symbol is a label, an icon, or both. Most markers on a map are the middle case, which is
    /// why this is not a field of the text: a builder that resolved the icon only where there was
    /// text would draw none of them.
    pub icon: Option<String>,
    /// The font stack it is set in.
    pub fonts: Vec<String>,
    /// Where it goes.
    pub anchoring: Anchoring,
    /// How *this feature's* icon is drawn.
    pub icon_options: IconOptions,
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
        let label = symbol::label(layer, zoom, feature);
        let icon = symbol::icon_image(layer, zoom, feature);

        // A symbol needs one half or the other. Neither is the common case — most features of a
        // symbol source have no name and no icon — and it is why this is a filter rather than an
        // error.
        if label.as_ref().is_none_or(|label| label.text.is_empty()) && icon.is_none() {
            return;
        }
        let fonts = label
            .as_ref()
            .map(|label| label.fonts.clone())
            .unwrap_or_default();
        let text = label.map(|label| label.text).unwrap_or_default();

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
                text: text.clone(),
                icon: icon.clone(),
                fonts: fonts.clone(),
                anchoring,
                symbol: text_options(layer, zoom, Some(feature)),
                icon_options: icon_options(layer, zoom, Some(feature)),
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
            if pending.fonts.is_empty() || pending.text.is_empty() {
                continue;
            }
            out.entry(pending.fonts.clone())
                .or_default()
                .extend(pending.text.chars().map(|character| character as u32));
        }
        out
    }

    /// Joins line features that share an endpoint and say the same thing.
    ///
    /// A port of mbgl's `util::mergeLines`, which it runs on a symbol layer's features whenever
    /// `symbol-placement` is `line` and before any anchor is chosen.
    ///
    /// A road is rarely one feature. A tile cuts it at its edges and a source cuts it wherever an
    /// attribute changes — a speed limit, a surface, a bridge — so "Main Street" arrives as a
    /// dozen stubs laid end to end. Labelling them separately puts a dozen copies of the name
    /// along one street, and *drops* most of them instead: a stub shorter than its own label
    /// cannot hold one at all, which is why the street fixture produced far fewer labels than it
    /// has roads. Joining first is what turns a run of stubs into a road long enough to name.
    ///
    /// Two features join when one's last point is the other's first *and* their text matches.
    /// Text, not feature id — the point is to label the street rather than to reassemble the
    /// source — and a stub whose name differs stays its own line even where it touches.
    ///
    /// Merged-away features are dropped rather than left empty. mbgl clears their geometry and
    /// skips them later; here the layout would otherwise carry a pending symbol with no line in
    /// it, which every stage downstream would have to know to ignore.
    ///
    /// One greedy pass, and **not** run to a fixed point. The index holds one entry per text and
    /// endpoint, so where two roads of the same name start at the same place only one of them is
    /// reachable — a Y junction, of which a street tile has dozens. Running again joins more.
    /// mbgl's index is an `unordered_map` assigned into and overwrites identically, so a second
    /// pass would be a divergence: a silent one, because the extra joins look like better
    /// labelling rather than like a difference from the oracle.
    pub fn merge_lines(&mut self) {
        if !self.placement.along_line() {
            return;
        }

        /// Where a line ends, keyed exactly rather than by hash.
        ///
        /// mbgl hashes the text with the coordinate and indexes on that, which can collide and
        /// join two different streets that happen to touch. The tuple cannot, and is otherwise
        /// the same lookup — tile coordinates are integral, so the comparison is exact.
        type End = (String, i32, i32);

        let key = |text: &str, point: (f32, f32)| -> End {
            #[allow(clippy::cast_possible_truncation)]
            (text.to_string(), point.0 as i32, point.1 as i32)
        };

        // Which feature ends at a point, and which begins at one.
        let mut ends_at: BTreeMap<End, usize> = BTreeMap::new();
        let mut starts_at: BTreeMap<End, usize> = BTreeMap::new();

        for index in 0..self.pending.len() {
            let Anchoring::Line(line) = &self.pending[index].anchoring else {
                continue;
            };
            if line.is_empty() || self.pending[index].text.is_empty() {
                continue;
            }
            let text = self.pending[index].text.clone();
            let left = key(&text, line[0]);
            let right = key(&text, line[line.len() - 1]);

            let before = ends_at.get(&left).copied();
            let after = starts_at.get(&right).copied();

            match (before, after) {
                // A line on each side: join all three. Never a line with itself, which is what
                // keeps a closed ring from being merged into nothing.
                (Some(before), Some(after)) if before != after => {
                    starts_at.remove(&right);
                    self.join(after, index, true);
                    ends_at.remove(&left);
                    // The *merged* line, not the original. This line's points moved into
                    // `after` a moment ago, so joining `index` again appends nothing and leaves
                    // the road in two pieces — which looks like a correct merge on any fixture
                    // where only one end touches.
                    self.join(before, after, false);

                    starts_at.remove(&left);
                    ends_at.remove(&right);
                    if let Anchoring::Line(line) = &self.pending[before].anchoring {
                        let far = key(&text, line[line.len() - 1]);
                        ends_at.insert(far, before);
                    }
                }
                // A line ending where this one starts: append this to it.
                (Some(before), _) => {
                    ends_at.remove(&left);
                    ends_at.insert(right, before);
                    self.join(before, index, false);
                }
                // A line starting where this one ends: prepend this to it.
                (None, Some(after)) => {
                    starts_at.remove(&right);
                    starts_at.insert(left, after);
                    self.join(after, index, true);
                }
                (None, None) => {
                    starts_at.insert(left, index);
                    ends_at.insert(right, index);
                }
            }
        }

        // What was merged away has no line left; a pending symbol with no geometry is not one.
        self.pending.retain(|pending| match &pending.anchoring {
            Anchoring::Line(line) => !line.is_empty(),
            Anchoring::Point(_) => true,
        });
    }

    /// Moves `from`'s line onto `into`, leaving `from` empty.
    ///
    /// `prepend` puts it in front. Either way the shared point appears once: the joint is the
    /// last point of one and the first of the other, and keeping both would put a zero-length
    /// segment in the middle of the road for the anchor walk to divide by.
    fn join(&mut self, into: usize, from: usize, prepend: bool) {
        let Anchoring::Line(moving) = &mut self.pending[from].anchoring else {
            return;
        };
        let mut moving = core::mem::take(moving);
        if moving.is_empty() {
            return;
        }

        let Anchoring::Line(target) = &mut self.pending[into].anchoring else {
            return;
        };
        if prepend {
            moving.pop();
            moving.append(target);
            *target = moving;
        } else {
            target.pop();
            target.append(&mut moving);
        }
    }

    /// The sprites this layout needs, which is what the sprite sheet is looked up by.
    ///
    /// The icon counterpart of [`Self::dependencies`]. A name is not checked against the index
    /// here — the index may not have arrived — so this is what the layer *asked for* rather than
    /// what exists, and an icon the sheet does not have is a layout-time miss rather than a
    /// resolution failure.
    #[must_use]
    pub fn icons(&self) -> BTreeSet<String> {
        self.pending
            .iter()
            .filter_map(|pending| pending.icon.clone())
            .collect()
    }

    /// Lays out this layer's icons against a sprite index.
    ///
    /// The icon counterpart of [`Self::lay_out`], and a separate buffer for a real reason: text
    /// draws through `SymbolSDFShader` and an icon through `SymbolIconShader`, so the two halves
    /// of one symbol are two *drawables* and cannot share a vertex buffer.
    ///
    /// An icon naming a sprite the sheet does not have is skipped, so a style with one missing
    /// icon still draws the rest. Order is the layer's, as it is for text.
    #[must_use]
    pub fn lay_out_icons(
        &self,
        positions: &tessella_glyph::sprite::Positions,
    ) -> (SymbolBuffers, Vec<LaidOut>) {
        let labels: Vec<IconLabel> = self
            .pending
            .iter()
            .filter_map(|pending| {
                let image = pending.icon.clone()?;
                let anchor = match pending.anchoring {
                    Anchoring::Point(anchor) => anchor,
                    // A line-placed icon repeats along the line the way a label does, which
                    // needs the anchors `get_anchors` produces rather than a point. Not built:
                    // it would place every icon of a road at its first vertex, which draws and
                    // is wrong.
                    Anchoring::Line(_) => return None,
                };
                Some(IconLabel {
                    image,
                    anchor,
                    options: pending.icon_options,
                })
            })
            .collect();

        build_icons(&labels, positions)
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
                    .filter(|pending| !pending.text.is_empty())
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
                    .filter(|pending| !pending.text.is_empty())
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
