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

use tessella_style::expression::Feature;
use tessella_style::{Layer, Value};

use alloc::collections::{BTreeMap, BTreeSet};

use tessella_glyph::fonts::Fonts;
use tessella_glyph::text::ONE_EM;

use crate::anchors::EXTENT;

/// The pixels a tile is drawn across at its own zoom.
///
/// With [`EXTENT`] this is mbgl's `tilePixelRatio`: how many tile units make a pixel, and so the
/// factor between anything a style states in pixels and the space a tile's geometry lives in.
const TILE_SIZE: f32 = 512.0;
use crate::size::SizeBinding;
use crate::symbol::{self, GlyphDependencies};
use crate::symbol_bucket::{
    IconLabel, IconOptions, Label, LaidOut, LineLabel, LineOptions, SizeRange, SymbolBuffers,
    SymbolOptions, build_icons, build_line_symbols, build_symbols,
};

/// One layout property, evaluated at a zoom with no feature.
///
/// A layout property may be a plain value or an expression, and an expression over zoom is the
/// common case — `text-size` interpolated across a range is in most styles. Evaluating it at
/// build time is what makes the size the one this tile draws at.
///
/// Shared with the frame, which needs the *same* `text-size` this layout shaped at: the shader
/// scales a glyph's corners by `size / 24`, so a size derived twice by two routes is a label
/// whose quads and whose spacing disagree.
use tessella_style::property::layout_value;

/// How a layer draws its icons, at a zoom and optionally for one feature.
///
/// `icon-size` is a *multiplier* and defaults to one, unlike `text-size` which names a size in
/// pixels and defaults to sixteen. Reading one as the other draws every marker sixteen times too
/// large, which is why they do not share this function.
fn icon_options(
    layer: &Layer,
    zoom: f64,
    feature: Option<&dyn Feature>,
    binding: &SizeBinding,
) -> IconOptions {
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

    #[allow(clippy::cast_possible_truncation)]
    let quad = |key: &str| -> Option<[f32; 4]> {
        let value = layout_value(layer, key, zoom, feature)?;
        let array = value.as_array()?;
        if array.len() != 4 {
            return None;
        }
        Some([
            array[0].as_number()? as f32,
            array[1].as_number()? as f32,
            array[2].as_number()? as f32,
            array[3].as_number()? as f32,
        ])
    };

    IconOptions {
        size: number("icon-size").unwrap_or(1.0),
        vertex_size: feature.map_or(SizeRange { min: 0.0, max: 0.0 }, |feature| {
            binding.vertex_size(feature, 1.0)
        }),
        text_fit: match layout_value(layer, "icon-text-fit", zoom, feature)
            .as_ref()
            .and_then(Value::as_str)
        {
            Some("width") => tessella_glyph::quads::IconTextFit::Width,
            Some("height") => tessella_glyph::quads::IconTextFit::Height,
            Some("both") => tessella_glyph::quads::IconTextFit::Both,
            _ => tessella_glyph::quads::IconTextFit::None,
        },
        // Top, right, bottom, left, as the spec writes it — the CSS order, not the extent order
        // everything else here uses. Reading it as the other rotates the padding a quarter turn.
        text_fit_padding: quad("icon-text-fit-padding").unwrap_or([0.0; 4]),
        offset: pair("icon-offset").unwrap_or([0.0, 0.0]),
        // On the wire in degrees, like `text-rotate`.
        rotate: number("icon-rotate").unwrap_or(0.0).to_radians(),
        anchor: anchor_of(layout_value(layer, "icon-anchor", zoom, feature).as_ref()),
    }
}

/// Whether a symbol layer draws a halo pass and a fill pass, for one of its two halves.
///
/// mbgl's `textPropertyValues` and `iconPropertyValues`, transcribed including their fallbacks:
///
/// ```text
/// hasHalo = haloColor.constantOr(black).a > 0 && haloWidth.constantOr(1) != 0
/// hasFill = color.constantOr(black).a > 0
/// ```
///
/// The fallbacks are the point and they are not the spec's defaults. `constantOr` answers with
/// the value it is given when the property varies per feature, and mbgl passes *opaque black*
/// and a width of *one* -- so a data-driven halo always haloes, where the spec's own defaults
/// (transparent black, width zero) would say it never does. A property that varies with zoom
/// alone has been evaluated by then and answers for itself.
///
/// Read from the layer rather than from resolved paint so [`SymbolLayout`] can settle it once:
/// how many drawables a symbol layer becomes is asked in two places, and the two must not be
/// able to disagree.
///
/// `prefix` is `"text"` or `"icon"`.
#[must_use]
pub fn passes(layer: &Layer, prefix: &str, zoom: f64) -> Passes {
    // A paint property's value at this zoom, or `None` when it varies per feature -- which is
    // exactly the case `constantOr` answers with its argument for.
    let constant = |key: &str| -> Option<Option<Value>> {
        match layer.paint.get(key)? {
            tessella_style::PropertyValue::Literal(literal) => Some(Some(literal.clone())),
            tessella_style::PropertyValue::Expression(raw) => {
                let Ok(expression) = tessella_style::Expression::parse(raw.value()) else {
                    return Some(None);
                };
                if expression.dependency().needs_feature() {
                    return Some(None);
                }
                Some(expression.evaluate(Some(zoom), None).ok())
            }
        }
    };
    // `absent` is what the *spec* default's alpha says, which is where the two rules meet: mbgl
    // evaluates the property first and only then reaches for `constantOr`, so a layer that never
    // set the property gets the spec's value and not the fallback.
    let opaque = |key: &str, absent: bool| -> bool {
        match constant(key) {
            None => absent,
            // Not constant, so mbgl's `constantOr(black)` answers black, whose alpha is one.
            Some(None) => true,
            Some(Some(value)) => {
                tessella_style::property::as_color(&value).is_ok_and(|color| color.a > 0.0)
            }
        }
    };
    let wide = match constant(&alloc::format!("{prefix}-halo-width")) {
        None => false,
        Some(None) => true,
        Some(Some(value)) => value.as_number().is_some_and(|width| width != 0.0),
    };
    Passes {
        halo: opaque(&alloc::format!("{prefix}-halo-color"), false) && wide,
        fill: opaque(&alloc::format!("{prefix}-color"), true),
    }
}

/// Which of a symbol half's two passes a layer draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Passes {
    /// The halo, which draws underneath the letters.
    pub halo: bool,
    /// The letters themselves.
    pub fill: bool,
}

/// Clips a line to the tile box, the way mbgl clips one before placing anchors.
///
/// # Why a line is clipped at all
///
/// mbgl runs `util::clipLines(feature.geometry, 0, 0, EXTENT, EXTENT)` and then `getAnchors` once
/// per clipped run. Where a road leaves the tile and comes back, that is two runs and two
/// independent anchor walks; uncut it is one walk whose spacing carries straight across the gap.
/// So the anchors land in different places, and a label with them.
///
/// This is a *segment* clip and not a polyline clip, which is mbgl's and is the point: each
/// segment is cut against the box on its own and dropped when it lies wholly outside, and a new
/// run is started whenever a segment does not continue the last one. Clipping the polyline
/// properly would join runs mbgl keeps apart.
///
/// It is also what gives `continued_line` its meaning: the flag tests a run's first point against
/// 0 and `EXTENT` exactly, which is a coordinate only a cut produces.
pub fn clip_line(line: &[(f32, f32)], x1: f32, y1: f32, x2: f32, y2: f32) -> Vec<Vec<(f32, f32)>> {
    clip_lines(core::slice::from_ref(&line), x1, y1, x2, y2)
}

/// The same, over all of a feature's rings at once.
///
/// mbgl's `clipLines` takes the whole `GeometryCollection` and accumulates into one
/// `clippedLines`, so the "does this segment continue the run" test compares against the last
/// point pushed *whatever ring it came from*. Two rings that meet end to end therefore come back
/// as a single run. Clipping each ring on its own is a different answer, so the loop is here
/// rather than at the call site.
#[must_use]
pub fn clip_lines(
    lines: &[&[(f32, f32)]],
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) -> Vec<Vec<(f32, f32)>> {
    let mut out: Vec<Vec<(f32, f32)>> = Vec::new();
    for line in lines {
        if line.len() < 2 {
            continue;
        }
        clip_one(line, x1, y1, x2, y2, &mut out);
    }
    out
}

fn clip_one(
    line: &[(f32, f32)],
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    out: &mut Vec<Vec<(f32, f32)>>,
) {
    for pair in line.windows(2) {
        let (mut p0, mut p1) = (pair[0], pair[1]);

        // Each edge in turn, and `continue` on a segment wholly outside it. The order is mbgl's;
        // a segment crossing two edges is cut by both, in this sequence.
        if p0.0 < x1 && p1.0 < x1 {
            continue;
        } else if p0.0 < x1 {
            p0 = (
                x1,
                (p0.1 + (p1.1 - p0.1) * ((x1 - p0.0) / (p1.0 - p0.0))).round(),
            );
        } else if p1.0 < x1 {
            p1 = (
                x1,
                (p0.1 + (p1.1 - p0.1) * ((x1 - p0.0) / (p1.0 - p0.0))).round(),
            );
        }

        if p0.1 < y1 && p1.1 < y1 {
            continue;
        } else if p0.1 < y1 {
            p0 = (
                (p0.0 + (p1.0 - p0.0) * ((y1 - p0.1) / (p1.1 - p0.1))).round(),
                y1,
            );
        } else if p1.1 < y1 {
            p1 = (
                (p0.0 + (p1.0 - p0.0) * ((y1 - p0.1) / (p1.1 - p0.1))).round(),
                y1,
            );
        }

        if p0.0 >= x2 && p1.0 >= x2 {
            continue;
        } else if p0.0 >= x2 {
            p0 = (
                x2,
                (p0.1 + (p1.1 - p0.1) * ((x2 - p0.0) / (p1.0 - p0.0))).round(),
            );
        } else if p1.0 >= x2 {
            p1 = (
                x2,
                (p0.1 + (p1.1 - p0.1) * ((x2 - p0.0) / (p1.0 - p0.0))).round(),
            );
        }

        if p0.1 >= y2 && p1.1 >= y2 {
            continue;
        } else if p0.1 >= y2 {
            p0 = (
                (p0.0 + (p1.0 - p0.0) * ((y2 - p0.1) / (p1.1 - p0.1))).round(),
                y2,
            );
        } else if p1.1 >= y2 {
            p1 = (
                (p0.0 + (p1.0 - p0.0) * ((y2 - p0.1) / (p1.1 - p0.1))).round(),
                y2,
            );
        }

        let starts_a_run = match out.last() {
            None => true,
            Some(run) => !run.is_empty() && run.last() != Some(&p0),
        };
        if starts_a_run {
            out.push(alloc::vec![p0]);
        }
        if let Some(run) = out.last_mut() {
            run.push(p1);
        }
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

/// `text-justify`, against the anchor that `auto` would ask.
///
/// Absent is `center`, which is the spec's default, and *not* the same as `auto`: a layer with
/// `text-anchor: left` and no justify is centre-justified, and only one that writes `auto` takes
/// its justification from the anchor. mbgl draws the same line -- it evaluates the property, whose
/// default is `center`, and consults `getAnchorJustification` in the one branch where the value is
/// `auto`.
fn justify_of(
    value: Option<&Value>,
    anchor: tessella_glyph::shaping::Anchor,
) -> tessella_glyph::shaping::Justify {
    use tessella_glyph::shaping::Justify;
    match value.and_then(Value::as_str) {
        Some("left") => Justify::Left,
        Some("right") => Justify::Right,
        Some("auto") => anchor.justification(),
        _ => Justify::Center,
    }
}

/// Every position `text-variable-anchor` offers, reduced for placement.
///
/// # The anchor is not the layout's any more
///
/// A label with variable anchors has no one place to be shaped around: which anchor it takes is
/// decided per frame, against the collision index, and can change as the map moves. So the
/// shaping is centred and *both* shifts are applied at placement -- the radial offset, and the
/// `-(align - 0.5) * size` that moves the box off the point.
///
/// That is mbgl's arrangement too, and for the same reason: it shapes around `Center` and
/// `calculateVariableLayoutOffset` does the rest. This used to fold the first anchor into the
/// shaping, which is exact for a label that never moves and cannot express one that does.
///
/// `text-radial-offset` is data-driven and the anchor list is not, so the distance is read per
/// label in [`text_options`] and only the directions are here -- each one a unit-distance offset
/// the caller scales.
fn variable_anchors(layer: &Layer, zoom: f64) -> Vec<VariableAnchor> {
    let Some(value) = layout_value(layer, "text-variable-anchor", zoom, None) else {
        return Vec::new();
    };
    let Some(list) = value.as_array() else {
        return Vec::new();
    };
    list.iter()
        .map(|entry| {
            let anchor = anchor_of(Some(entry));
            VariableAnchor {
                alignment: anchor.alignment(),
                anchor,
            }
        })
        .collect()
}

/// The first anchor `text-variable-anchor` offers, for the justification `auto` asks it for.
fn first_variable_anchor(
    layer: &Layer,
    zoom: f64,
    feature: Option<&dyn Feature>,
) -> Option<tessella_glyph::shaping::Anchor> {
    let anchors = layout_value(layer, "text-variable-anchor", zoom, feature)?;
    Some(anchor_of(Some(anchors.as_array()?.first()?)))
}

/// How a layer sets its text, at a zoom and optionally for one feature.
///
/// The spec allows `text-size`, `text-max-width` and `text-letter-spacing` to be data-driven, so
/// two features of one layer can be set differently. Evaluating without a feature gives the
/// layer's own values, which is what a layout is constructed with and what a layer with no
/// data-driven property resolves to for every feature.
fn text_options(
    layer: &Layer,
    zoom: f64,
    feature: Option<&dyn Feature>,
    binding: &SizeBinding,
) -> SymbolOptions {
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
    // A variable anchor replaces both the anchor and the offset: the shaping is centred and
    // placement does the moving. The spec says not to write `text-offset` and
    // `text-radial-offset` together and does not say what happens if you do; mbgl takes the
    // radial one, and so does this.
    let variable = first_variable_anchor(layer, zoom, feature);
    let plain = anchor_of(layout_value(layer, "text-anchor", zoom, feature).as_ref());
    SymbolOptions {
        size: number("text-size").unwrap_or(16.0),
        // What the *vertex* carries, which is not the same question as what the layer's size is:
        // the shader reads one or the other and the binder decides which. Zero with no feature in
        // hand, because the layer-wide options are the ones a layer with no per-feature size
        // uses, and that layer's shader reads the uniform.
        vertex_size: feature.map_or(SizeRange { min: 0.0, max: 0.0 }, |feature| {
            binding.vertex_size(feature, 16.0)
        }),
        // Both of these were unread, and the pair of them is how a style puts a name under the
        // marker it names. Without them a POI label sat on top of its own icon.
        anchor: if variable.is_some() {
            tessella_glyph::shaping::Anchor::Center
        } else {
            plain
        },
        offset: if variable.is_some() {
            [0.0, 0.0]
        } else {
            pair("text-offset")
                .map(|offset| [offset[0] * ONE_EM, offset[1] * ONE_EM])
                .unwrap_or([0.0, 0.0])
        },
        // The offset a variable anchor points, in shaping units. mbgl branches on whether the
        // style *wrote* `text-radial-offset`, not on its value: a layer writing both takes the
        // radial one, and a layer writing only `text-offset` takes that. Reading the radial alone
        // left the second case with no offset at all, so its labels sat on their own points --
        // which against the oracle is four percent of a POI layer.
        variable_offset: if variable.is_none() {
            [0.0, 0.0]
        } else if layer.layout.contains_key("text-radial-offset") {
            [number("text-radial-offset").unwrap_or(0.0) * ONE_EM, 0.0]
        } else {
            pair("text-offset")
                .map(|offset| [offset[0] * ONE_EM, offset[1] * ONE_EM])
                .unwrap_or([0.0, 0.0])
        },
        variable_radial: variable.is_some() && layer.layout.contains_key("text-radial-offset"),
        // The anchor `auto` asks is the variable one where there is one, because that is the
        // anchor the label is actually placed at.
        justify: justify_of(
            layout_value(layer, "text-justify", zoom, feature).as_ref(),
            variable.unwrap_or(plain),
        ),
        max_width_ems: number("text-max-width").unwrap_or(10.0),
        // `text-letter-spacing` is in ems and everything downstream of it is in pixels, so it
        // is resolved here where the unit changes rather than carried in the spec's unit and
        // multiplied wherever it happens to be read. mbgl does the same, one line above its
        // line height, and the two were wrong here in the same way.
        letter_spacing: number("text-letter-spacing").unwrap_or(0.0) * ONE_EM,
        line_height_ems: number("text-line-height").unwrap_or(1.2),
        // `text-writing-mode` is a list, and only whether it *contains* `vertical` matters here:
        // it decides which characters a vertical shaping keeps upright, and whether one is made
        // at all. The order the list gives is a placement preference, and placement is where it
        // is read.
        allow_vertical_placement: layout_value(layer, "text-writing-mode", zoom, feature)
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|modes| modes.iter().any(|mode| mode.as_str() == Some("vertical"))),
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

/// `*-rotation-alignment` and `*-pitch-alignment`: what a symbol is oriented against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    /// Fixed to the screen. A label stays upright and the same size however the map is turned.
    #[default]
    Viewport,
    /// Fixed to the ground. A label turns and tilts with the map.
    Map,
}

/// The two alignments a symbol's halves resolve to.
///
/// `auto` is the spec's default for both and resolves in two steps, in this order. Rotation
/// alignment takes `map` for a line-placed symbol and `viewport` for a point-placed one — a road
/// name follows its road, a town name stays upright. Pitch alignment then *inherits whatever
/// rotation alignment became*, which is why the order matters: resolving pitch first would give
/// every line label a viewport pitch and lay none of them flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alignments {
    /// What the symbol turns with.
    pub rotation: Alignment,
    /// What it tilts with.
    pub pitch: Alignment,
}

impl Alignments {
    /// Resolves both from a layer, for a placement.
    ///
    /// `prefix` is `text` or `icon`: the two halves carry their own pair, and a style setting one
    /// and leaving the other `auto` is ordinary rather than exotic.
    #[must_use]
    pub fn of(layer: &Layer, zoom: f64, placement: Placement, prefix: &str) -> Self {
        let read = |key: &str| -> Option<Alignment> {
            match layout_value(layer, key, zoom, None)
                .as_ref()
                .and_then(Value::as_str)?
            {
                "map" => Some(Alignment::Map),
                "viewport" => Some(Alignment::Viewport),
                // `auto`, and anything a newer spec adds.
                _ => None,
            }
        };

        let rotation = read(&alloc::format!("{prefix}-rotation-alignment")).unwrap_or({
            if placement.along_line() {
                Alignment::Map
            } else {
                Alignment::Viewport
            }
        });
        // Inherited, not defaulted. A line label that rotates with the map also pitches with it
        // unless the style says otherwise.
        let pitch = read(&alloc::format!("{prefix}-pitch-alignment")).unwrap_or(rotation);

        Self { rotation, pitch }
    }

    /// Whether the symbol's glyphs are walked along a line rather than placed at a point.
    ///
    /// mbgl's `alongLine`, and it is *both* conditions: a line-placed symbol that does not rotate
    /// with the map is drawn upright at each anchor rather than following the road, so it is not
    /// walked. The label plane is the identity in that case, because the projection does the walk
    /// itself and a plane would bend it twice.
    #[must_use]
    pub const fn along_line(self, placement: Placement) -> bool {
        placement.along_line() && matches!(self.rotation, Alignment::Map)
    }

    /// Whether the shader turns the symbol, rather than the projection doing it.
    ///
    /// mbgl's `rotateInShader`. A symbol that turns with the map *and* lies flat is turned by the
    /// label-plane projection; one that is walked along a line is turned by the walk. What is
    /// left — turning with the map while standing up on screen — is the only case the shader has
    /// to do itself.
    #[must_use]
    pub const fn rotate_in_shader(self, placement: Placement) -> bool {
        matches!(self.rotation, Alignment::Map)
            && matches!(self.pitch, Alignment::Viewport)
            && !self.along_line(placement)
    }
}

/// Where one label goes, in tile units.
#[derive(Debug, Clone, PartialEq)]
pub enum Anchoring {
    /// At a point.
    Point((f32, f32)),
    /// Along the feature's lines, in tile units.
    ///
    /// All of them, because mbgl keeps a feature's `GeometryCollection` whole and only the first
    /// ring takes part in merging. Holding one ring per pending instead made every ring a
    /// merge candidate, and rings mbgl keeps apart were spliced into one line whose anchors then
    /// landed somewhere else entirely.
    Line(Vec<Vec<(f32, f32)>>),
}

impl Pending {
    /// The ring that takes part in merging, which is the first and only the first.
    fn first_line(&self) -> Option<&Vec<(f32, f32)>> {
        match &self.anchoring {
            Anchoring::Line(lines) => lines.first(),
            Anchoring::Point(_) => None,
        }
    }
}

/// One feature's symbol, resolved but not shaped.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    /// This feature's `symbol-sort-key`, which decides where it sits in the layout.
    ///
    /// Not a property of the label so much as of the list: the layout holds its features in
    /// sort-key order, and everything downstream -- which anchor the repeat filter keeps, which
    /// label is placed first, which glyphs are drawn over which -- follows that order.
    pub sort_key: f32,
    /// What it says, after tokens and expressions. Empty for an icon with no label.
    pub text: String,
    /// Its sections, which concatenate to [`Self::text`]. One for an ordinary label.
    pub sections: Vec<crate::symbol::Section>,
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
    /// This feature's data-driven paint, evaluated but not yet written.
    ///
    /// Held rather than written because a symbol's vertices do not exist yet: glyphs arrive after
    /// the tile is decoded, so the count `PaintBinder::push` wants cannot be known where the
    /// feature is in scope. `build_symbols` decides the vertex order and the orchestrator writes
    /// these against it. Empty for a layer whose paint is entirely the layer's.
    pub paint: crate::PaintValues,
    /// How *this feature's* text is set.
    ///
    /// The layer's, unless a layout property is data-driven — `text-size` is the one styles
    /// actually use that way, to make a capital larger than a town on the same layer. Held per
    /// label rather than per layer because that is the granularity the spec gives it, and the
    /// vertex already carries a size per quad.
    pub symbol: SymbolOptions,
}

/// One candidate position from `text-variable-anchor`, reduced to what placement needs.
///
/// The anchor itself does not survive the layout: what placement wants from it is where the box
/// sits relative to the point (`alignment`, each in 0..1) and which way the radial offset points
/// (`offset`, in shaping units). Both are the anchor's alone and neither depends on the label, so
/// they are computed once here rather than per label per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VariableAnchor {
    /// How far along the box's width and height the point sits, from `Anchor::alignment`.
    pub alignment: (f32, f32),
    /// The anchor itself, for `shaping::variable_offset` to point the label with.
    pub anchor: tessella_glyph::shaping::Anchor,
}

/// A symbol layer's contribution to one tile, before glyphs.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolLayout {
    /// The positions `text-variable-anchor` offers, in the order the style wrote them.
    ///
    /// Empty for a layer that does not use it, which is the ordinary case and the one where a
    /// label goes where its `text-anchor` says and stays there. Where it is not empty, placement
    /// tries each in turn and keeps the first that fits -- so the anchor is not a property of the
    /// layout at all, and the shaping is centred with the offset applied per frame.
    ///
    /// A layer's rather than a label's: the spec does not allow `text-variable-anchor` to be
    /// data-driven. `text-radial-offset` is, which is why the distance lives in `SymbolOptions`
    /// and only the directions are here.
    pub variable_anchors: Vec<VariableAnchor>,

    /// Whether this layer's icons have to be sampled with interpolation.
    ///
    /// mbgl's `iconsNeedLinear`, plus the `iconScaled` test that wraps it in
    /// `RenderSymbolLayer`. An icon drawn at exactly its own size wants *nearest* sampling: its
    /// texels land one to a pixel, and interpolating between them only smears the edges across
    /// two. Anything that rescales or rotates it wants linear instead, because then the texels do
    /// not line up and nearest would alias.
    ///
    /// Three of mbgl's four conditions live in the style and are answered here:
    ///
    /// - `icon-size` other than a constant `1`, which is `constantOr(1.0) != 1.0`;
    /// - an `icon-size` that is data-driven or varies with zoom, which is an expression here and
    ///   `isDataDriven() || !isZoomConstant()` there;
    /// - a non-zero `icon-rotate`.
    ///
    /// A literal is a constant and anything else is not, which is the same split mbgl makes: a
    /// property written as an expression may evaluate to one today and something else at the next
    /// zoom, and the sampler is chosen once for the bucket.
    ///
    /// The fourth -- a sprite whose pixel ratio differs from the map's -- needs the sheet, which
    /// this does not have. The caller with the sheet applies it.
    pub icons_need_linear: bool,
    /// One entry per label this layer draws on this tile.
    pub pending: Vec<Pending>,
    /// How the text is set.
    pub symbol: SymbolOptions,
    /// How `text-size` reaches the shader: as a uniform, or out of the vertex.
    ///
    /// Held on the layout rather than on each label because the *classification* is the layer's
    /// -- every label in it reads its size the same way -- while the size itself may not be.
    pub text_size: SizeBinding,
    /// The same for `icon-size`.
    pub icon_size: SizeBinding,
    /// Whether this layer's features are held in `symbol-sort-key` order.
    ///
    /// mbgl's `sortFeaturesByKey`: the key is set *and* `symbol-z-order` is not `viewport-y`,
    /// which asks for a different ordering entirely.
    pub sort_by_key: bool,
    /// Which passes this layer's text draws: the halo, the letters, or both.
    ///
    /// A halo is a *second drawable over the same geometry*, drawn underneath, so this is what
    /// decides how many drawables a symbol layer becomes. Settled here because that count is
    /// asked in two places -- the binding walk and the encoder -- and the two must not be able
    /// to disagree.
    pub text_passes: Passes,
    /// How it follows a line, when it does.
    pub line: LineOptions,
    /// Where the labels sit.
    pub placement: Placement,
    /// What the text is oriented against.
    pub text_alignments: Alignments,
    /// What the icons are.
    pub icon_alignments: Alignments,
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

        // `text-size`'s spec default is sixteen pixels; `icon-size`'s is a multiplier of one.
        let text_size = SizeBinding::of(layer, "text-size", zoom, 16.0);
        let icon_size = SizeBinding::of(layer, "icon-size", zoom, 1.0);
        let text_passes = passes(layer, "text", zoom);
        // mbgl's `sortFeaturesByKey`. `symbol-z-order: viewport-y` sorts by screen position at
        // placement instead, so the two are alternatives rather than a pair.
        let sort_by_key = layer.layout.contains_key("symbol-sort-key")
            && layout_value(layer, "symbol-z-order", zoom, None)
                .as_ref()
                .and_then(Value::as_str)
                != Some("viewport-y");
        let symbol = text_options(layer, zoom, None, &text_size);
        let placement = Placement::of(layer, zoom);

        // How many tile units a pixel is on this tile. mbgl's `tilePixelRatio`, and the factor
        // between everything a style states in pixels and the space a tile's geometry lives in.
        let tile_pixel_ratio = EXTENT / (TILE_SIZE * overscaling.max(1.0));
        // `text-size` at zoom 18, not at this tile's zoom, which is mbgl's own choice and its
        // own reason: anchors computed from one size for every zoom are in the same place in
        // every tile, so a label does not jump when the map crosses a zoom.
        #[allow(clippy::cast_possible_truncation)]
        let max_text_size = layout_value(layer, "text-size", 18.0, None)
            .as_ref()
            .and_then(Value::as_number)
            .map_or(16.0, |value| value as f32);

        // A literal is a constant; an expression is not. See `icons_need_linear`.
        let literal_number = |key: &str, default: f64| -> Option<f64> {
            match layer.layout.get(key) {
                None => Some(default),
                Some(property) => property.as_literal().and_then(Value::as_number),
            }
        };
        let icons_need_linear = literal_number("icon-size", 1.0) != Some(1.0)
            || literal_number("icon-rotate", 0.0) != Some(0.0);

        Self {
            pending: Vec::new(),
            icons_need_linear,
            variable_anchors: variable_anchors(layer, zoom),
            symbol,
            text_size,
            icon_size,
            sort_by_key,
            text_passes,
            line: LineOptions {
                symbol,
                // Into tile units, which is what this field holds and what `get_anchors` walks.
                //
                // `symbol-spacing` is stated in *pixels* -- 250 of them by default -- and a tile
                // is 8192 units across the 512 pixels it is drawn at. Storing the style's number
                // unconverted dropped an anchor every 250 tile units where the style asked for
                // every 4000: sixteen times the labels, which on a map is a road wearing a shield
                // every few pixels of its length. mbgl calls the factor `tilePixelRatio`.
                spacing: number("symbol-spacing").unwrap_or(250.0)
                    * (EXTENT / (TILE_SIZE * overscaling.max(1.0))),
                // The spec's default is 45 degrees, and it is in degrees on the wire.
                max_angle: number("text-max-angle").unwrap_or(45.0).to_radians(),
                overscaling,
                centred: placement == Placement::LineCenter,
                max_box_scale: tile_pixel_ratio * max_text_size / tessella_glyph::text::ONE_EM,
            },
            text_alignments: Alignments::of(layer, zoom, placement, "text"),
            icon_alignments: Alignments::of(layer, zoom, placement, "icon"),
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
    #[allow(clippy::needless_pass_by_value)]
    pub fn push(
        &mut self,
        layer: &Layer,
        zoom: f64,
        feature: &dyn Feature,
        rings: &[Vec<(f32, f32)>],
        paint: crate::PaintValues,
    ) {
        let label = symbol::label(layer, zoom, feature);
        let icon = symbol::icon_image(layer, zoom, feature);
        #[allow(clippy::cast_possible_truncation)]
        let sort_key = layout_value(layer, "symbol-sort-key", zoom, Some(feature))
            .as_ref()
            .and_then(Value::as_number)
            .map_or(0.0, |value| value as f32);

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
        let (text, sections) = label
            .map(|label| (label.text, label.sections))
            .unwrap_or_default();

        if self.placement.along_line() {
            // Every ring of the feature, in one pending, whole and clipped later.
            //
            // One pending per *feature*, not per ring, because that is mbgl's `SymbolFeature`:
            // `mergeLines` keys and splices `geometry[0]` alone and leaves the rest attached, so
            // a ring that is not the first can never be merged into another feature's line.
            //
            // And whole rather than clipped, because mbgl's order is merge, then clip, then
            // anchors: `mergeLines(features)` closes the constructor and `clipLines` runs per
            // feature in `finalizeSymbols`. Clipping here would hand `merge_lines` the runs
            // instead of the lines, and merging runs re-joins what the clip separated.
            //
            // A line needs two points to have a direction; one point is not a short line, and
            // `clipLines` walks `begin()..end() - 1`, which is empty for such a ring anyway.
            let lines: Vec<Vec<(f32, f32)>> = rings
                .iter()
                .filter(|ring| ring.len() >= 2)
                .cloned()
                .collect();
            if lines.is_empty() {
                return;
            }
            self.insert(Pending {
                sort_key,
                text,
                sections,
                icon,
                fonts,
                anchoring: Anchoring::Line(lines),
                symbol: text_options(layer, zoom, Some(feature), &self.text_size),
                icon_options: icon_options(layer, zoom, Some(feature), &self.icon_size),
                paint,
            });
            return;
        }

        for ring in rings {
            let anchorings = {
                let Some(first) = ring.first() else { continue };
                // A point label belongs to the tile it is in, and to no other. The features
                // reaching this builder are the whole source rather than one tile's share, so
                // without the test every tile of the cover draws every label — which looks
                // right on the tile that owns it and wrong on its neighbours. Half-open, so a
                // point on a boundary lands in exactly one tile.
                if !(0.0..EXTENT).contains(&first.0) || !(0.0..EXTENT).contains(&first.1) {
                    continue;
                }
                alloc::vec![Anchoring::Point(*first)]
            };

            for anchoring in anchorings {
                self.insert(Pending {
                    sort_key,
                    text: text.clone(),
                    sections: sections.clone(),
                    icon: icon.clone(),
                    fonts: fonts.clone(),
                    anchoring,
                    symbol: text_options(layer, zoom, Some(feature), &self.text_size),
                    icon_options: icon_options(layer, zoom, Some(feature), &self.icon_size),
                    // One feature can anchor several times -- a road named along its length is
                    // one feature and several pendings -- and each of them carries the paint.
                    paint: paint.clone(),
                });
            }
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

    /// Adds a pending symbol, in `symbol-sort-key` order where the layer asks for one.
    ///
    /// # Why the order is the whole of the feature
    ///
    /// Almost everything downstream of the layout is order-dependent, and none of it obviously
    /// so. The repeat-distance filter keeps the *first* anchor it sees carrying a given name and
    /// drops every later one within half a `symbol-spacing`; placement competes labels in list
    /// order; and the vertices are emitted in list order, which is what decides which label is
    /// drawn over which. A style that sets `symbol-sort-key` is asking for all three.
    ///
    /// Found by measuring rather than by reading: the Protomaps road-label layers set
    /// `["get", "min_zoom"]`, and every anchor this build generated for them matched mbgl's
    /// exactly -- all forty-seven of them -- while two labels still drew in a different place.
    /// The repeat filter had seen the same anchors in a different order and kept different ones.
    ///
    /// `lower_bound`, so a feature is inserted *before* the ones it ties with. That is mbgl's
    /// `std::lower_bound` and it reverses the relative order of equal keys, which is a property
    /// of the arrangement rather than an accident of it.
    fn insert(&mut self, pending: Pending) {
        if !self.sort_by_key {
            self.pending.push(pending);
            return;
        }
        let at = self
            .pending
            .partition_point(|held| held.sort_key < pending.sort_key);
        self.pending.insert(at, pending);
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
            // The first ring and no other, which is `mergeLines`: `getKey` reads
            // `geometry[0].front()` and `.back()`, and the splices are into `geometry[0]`. A
            // feature's later rings are never keyed, so they cannot be merged onto anything.
            let Some(line) = self.pending[index].first_line() else {
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
                    if let Some(&last) = self.pending[before]
                        .first_line()
                        .and_then(|line| line.last())
                    {
                        ends_at.insert(key(&text, last), before);
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
            // Empty *first* ring, which is what a merge leaves behind. mbgl clears
            // `geometry[0]` and leaves the feature in the list with its later rings, and those
            // still reach `clipLines`; dropping the whole pending here would lose them.
            Anchoring::Line(lines) => lines.iter().any(|line| !line.is_empty()),
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
        // The first ring only, taken out and left empty -- mbgl's `geom[0].clear()`. The later
        // rings stay where they are.
        let Some(moving) = moving.first_mut() else {
            return;
        };
        let mut moving = core::mem::take(moving);
        if moving.is_empty() {
            return;
        }

        let Anchoring::Line(target) = &mut self.pending[into].anchoring else {
            return;
        };
        let Some(target) = target.first_mut() else {
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

    /// Whether any of this layout's symbols resolved a sprite.
    ///
    /// Asked before shaping, because it decides how many *drawables* the bucket declares, and
    /// that has to be settled before any of them is encoded. A layer naming an `icon-image` that
    /// no feature resolves -- a shield expression over features with no `ref` -- declares one
    /// drawable rather than two, which is what keeps the count and the records in step.
    #[must_use]
    pub fn has_icons(&self) -> bool {
        self.pending.iter().any(|pending| pending.icon.is_some())
    }

    /// Whether any symbol here has text, and so needs glyphs before it can be encoded.
    ///
    /// The same test [`Self::dependencies`] uses to decide what to ask for: a pending symbol with
    /// no fonts or no text contributes nothing, and a layer made entirely of those asks for
    /// nothing. Such a layer must not then be held back waiting for glyphs that will never be
    /// fetched, which is what an unconditional "a symbol needs fonts" does to an icon-only layer.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.pending
            .iter()
            .any(|pending| !pending.fonts.is_empty() && !pending.text.is_empty())
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
        instances: &[LaidOut],
    ) -> (SymbolBuffers, Vec<LaidOut>) {
        // Driven by the *instances* rather than by the pending symbols, which is what makes a
        // line-placed icon expressible at all. A point-placed symbol is one pending and one
        // instance; a line-placed one is one pending and an instance per anchor, so a road named
        // three times along its length wants three icons and not one at its first vertex.
        //
        // Pairing on `LaidOut::pending` rather than on position, for the same reason. The old
        // pairing was by index into `self.pending`, which holds only where the two lists are the
        // same length — the point case, which is the only one that reached here.
        let labels: Vec<IconLabel> = instances
            .iter()
            .filter_map(|laid| {
                let pending = self.pending.get(laid.pending)?;
                let image = pending.icon.clone()?;
                Some(IconLabel {
                    pending: laid.pending,
                    image,
                    // The instance's own anchor. For a point symbol that is the feature's; for
                    // a line-placed one it is where `get_anchors` put this repetition.
                    anchor: laid.anchor,
                    options: pending.icon_options,
                    // The label this icon is drawn around, if it has one. An entry that shaped
                    // no glyphs is a placeholder for an icon-only symbol, and `icon-text-fit`
                    // has nothing to fit to.
                    text: (laid.glyphs > 0).then_some(laid.extent),
                })
            })
            .collect();

        build_icons(&labels, positions)
    }

    /// A pending symbol's icon extent around its anchor, as `(left, right)` in logical pixels.
    ///
    /// mbgl shapes the icon *before* it computes anchors and hands `getAnchors` both extents, so
    /// the anchors a feature gets depend on how wide its shield is as well as on its label. This
    /// is why laying out takes the sprite index: without it the icon can only be shaped in the
    /// second pass, which is after the anchors it should have contributed to.
    ///
    /// Zero for a symbol with no icon, or one whose sprite the sheet does not hold — the same
    /// zero mbgl passes when `shapedIcon` is absent.
    fn icon_extent(
        &self,
        pending: &Pending,
        icons: Option<&tessella_glyph::sprite::Positions>,
    ) -> (f32, f32) {
        let Some(image) = pending.icon.as_ref() else {
            return (0.0, 0.0);
        };
        let Some(position) = icons.and_then(|icons| icons.get(image)) else {
            return (0.0, 0.0);
        };
        let (width, height) = position.display_size();
        #[allow(clippy::cast_possible_truncation)]
        let placed = tessella_glyph::quads::shape_icon(
            (width as f32, height as f32),
            pending.icon_options.offset,
            pending.icon_options.anchor,
        );
        (placed.left, placed.right)
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
    /// **One entry per pending symbol**, including the icon-only ones that shape no text: those
    /// get an empty extent and an empty vertex range. Emitting only the text-bearing ones would
    /// be tidier and wrong — `lay_out_icons` needs to find each icon's label by index, and a
    /// list that skips entries silently pairs every icon after the first text-less symbol with
    /// the wrong one. An empty extent also places as *nothing*, which is what a symbol with no
    /// text should reserve.
    ///
    /// # Panics
    ///
    /// When the joined buffers would exceed what a `u16` index reaches. See
    /// [`SymbolBuffers::append`].
    #[must_use]
    pub fn lay_out(
        &self,
        fonts: &Fonts,
        icons: Option<&tessella_glyph::sprite::Positions>,
    ) -> (SymbolBuffers, Vec<LaidOut>) {
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
            let start_of_run = start;
            start = end;

            // Where this run starts in the output, so the empties can be interleaved back into
            // their own positions afterwards.
            let glyphs = fonts.stack(&head.fonts);
            let (built, entries) = if self.placement.along_line() {
                let labels: Vec<LineLabel> = run
                    .iter()
                    .enumerate()
                    .filter(|(_, pending)| {
                        // Text *or* an icon. A symbol with only an icon still has anchors — from
                        // the icon's own extent — and dropping it here is what made a layer of
                        // oneway arrows draw nothing.
                        !pending.text.is_empty() || pending.icon.is_some()
                    })
                    .filter_map(|(offset, pending)| match &pending.anchoring {
                        Anchoring::Line(lines) => Some(LineLabel {
                            pending: start_of_run + offset,
                            sections: pending.sections.clone(),
                            icon: self.icon_extent(pending, icons),
                            text: pending.text.to_string(),
                            lines: lines.clone(),
                        }),
                        Anchoring::Point(_) => None,
                    })
                    .collect();
                let options = LineOptions {
                    symbol: head.symbol,
                    ..self.line
                };
                build_line_symbols(&labels, &glyphs, icons, &options)
            } else {
                let labels: Vec<Label> = run
                    .iter()
                    .enumerate()
                    .filter(|(_, pending)| !pending.text.is_empty())
                    .filter_map(|(offset, pending)| match pending.anchoring {
                        Anchoring::Point(anchor) => Some(Label {
                            pending: start_of_run + offset,
                            sections: pending.sections.clone(),
                            text: pending.text.to_string(),
                            anchor,
                        }),
                        Anchoring::Line(_) => None,
                    })
                    .collect();
                build_symbols(&labels, &glyphs, icons, &head.symbol)
            };

            // Each run's ranges address its own buffer, so they shift by what was already here.
            // Getting this wrong writes one label's per-frame state over another's, which draws
            // as a label that will not fade and errors nowhere.
            let base = buffers.append(&built);
            let mut shifted = entries.into_iter().map(|mut entry| {
                entry.vertices = entry.vertices.start + base..entry.vertices.end + base;
                entry
            });

            if self.placement.along_line() {
                // A line label is laid out once per *repetition* along its road, so there is no
                // one-to-one to keep. Icons are point-placed only, so nothing needs one here.
                laid.extend(shifted);
            } else {
                // A point label is laid out exactly once, so the output can be kept one-to-one
                // with `pending` by putting a placeholder where each text-less symbol belongs.
                // `lay_out_icons` finds an icon's label by index, and a list that skipped
                // entries would silently pair every icon after the first text-less symbol with
                // the wrong one.
                for (offset, pending) in run.iter().enumerate() {
                    if pending.text.is_empty() {
                        laid.push(LaidOut {
                            pending: start_of_run + offset,
                            anchor: match pending.anchoring {
                                Anchoring::Point(anchor) => anchor,
                                Anchoring::Line(_) => (0.0, 0.0),
                            },
                            // An empty extent places as nothing, which is what a symbol with no
                            // text should reserve.
                            extent: (0.0, 0.0, 0.0, 0.0),
                            vertical: None,
                            glyphs: 0,
                            content_margins: None,
                            segment: 0,
                            line: alloc::sync::Arc::default(),
                            vertices: buffers.vertices.len()..buffers.vertices.len(),
                        });
                    } else if let Some(entry) = shifted.next() {
                        laid.push(entry);
                    }
                }
            }
        }

        (buffers, laid)
    }
}
