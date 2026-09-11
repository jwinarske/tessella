//! The dash atlases a frame's line layers need, and where each one sits.
//!
//! [`tessella_glyph::dash`] turns one `line-dasharray` into a distance field. This decides which
//! of them a frame needs, builds each once for the whole cover rather than once per tile, and
//! gives it a texture id that stays the layer's across frames.
//!
//! # One texture per layer, not one atlas for the style
//!
//! mbgl's `LineAtlas` is a map from (`from`, `to`, cap) to a `DashPatternTexture`, and each of
//! those owns a whole 256-wide image of its own -- so "atlas" names the packing within one
//! pattern's rows and not a sheet shared between patterns. This keeps that shape, keyed by the
//! layer instead of by the pattern.
//!
//! Keying by the layer is what makes the id stable. A dasharray may step with zoom, so the
//! *pattern* a layer needs changes under a moving camera; numbering the textures by the patterns
//! a frame happened to want would hand the same id to different pixels as the set shifted, and
//! the drawables still holding it would start sampling somebody else's dashes. Keyed by the
//! layer, a changed pattern re-uploads to the same id -- which is right, because every drawable
//! naming it wants the new one.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::TextureId;
use tessella_glyph::dash::{Atlas, Cap, Position};
use tessella_style::crossfade::{Crossfade, ZoomHistory, crossfade, faded};
use tessella_style::{LayerKind, Style};

use crate::tile::{Content, LayerBucket};

/// One dashed layer's atlas and its placement.
#[derive(Debug, Clone, PartialEq)]
pub struct Dash {
    /// The texture the atlas is uploaded as.
    pub texture: TextureId,
    /// The distance field itself.
    pub atlas: Atlas,
    /// Where the pattern being faded from sits in it.
    pub from: Position,
    /// And the one being faded to.
    pub to: Position,
    /// How far between them the frame is, and how each is scaled.
    pub crossfade: Crossfade,
}

/// Every dashed line layer of a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dashes {
    by_layer: BTreeMap<usize, Dash>,
}

impl Dashes {
    /// The atlases a frame's buckets need.
    ///
    /// Built from the buckets rather than from the style because they carry the *resolved* paint
    /// -- a dasharray is an expression, and resolving one per layer per frame off the style
    /// document would redo work the tile build already did. A layer appears once however many
    /// tiles of the cover drew it.
    ///
    /// `base` is the first texture id to hand out; the caller owns the id space.
    #[must_use]
    pub fn for_buckets(
        style: &Style,
        buckets: &[(crate::tile::TileId, Vec<LayerBucket>)],
        zoom: f64,
        history: ZoomHistory,
        base: u64,
    ) -> Self {
        let mut seeded = history;
        if seeded.first {
            seeded.update(zoom, None);
        }
        // No clock, as a pattern's fade has none: mbgl's "no time" sentinel leaves the time term
        // complete, so the mix is the zoom's fractional part alone.
        let fade = crossfade(zoom, &seeded, None, 0);

        let mut by_layer: BTreeMap<usize, Dash> = BTreeMap::new();
        for (_, layer_buckets) in buckets {
            for bucket in layer_buckets {
                if !matches!(bucket.content, Content::Line(_))
                    || by_layer.contains_key(&bucket.layer_index)
                {
                    continue;
                }
                let Some(property) = bucket.paint.get("line-dasharray") else {
                    continue;
                };
                let pattern = faded(|z| property.numbers_at(z), zoom, &seeded);
                let (Some(from), Some(to)) = (pattern.from, pattern.to) else {
                    continue;
                };
                let cap = style
                    .layers
                    .get(bucket.layer_index)
                    .map_or(Cap::Butt, cap_of);
                let Some((atlas, from, to)) = Atlas::pair(&from, &to, cap) else {
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                by_layer.insert(
                    bucket.layer_index,
                    Dash {
                        texture: TextureId(base + bucket.layer_index as u64),
                        atlas,
                        from,
                        to,
                        crossfade: fade,
                    },
                );
            }
        }
        Self { by_layer }
    }

    /// The atlas a layer draws with, if it has one.
    #[must_use]
    pub fn get(&self, layer_index: usize) -> Option<&Dash> {
        self.by_layer.get(&layer_index)
    }

    /// Every atlas, in layer order, for the upload pass.
    pub fn iter(&self) -> impl Iterator<Item = &Dash> {
        self.by_layer.values()
    }

    /// Whether any layer of the frame is dashed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_layer.is_empty()
    }
}

/// Which cap a line layer's dashes are drawn with.
///
/// `line-cap` is a layout property with three values and the atlas has two shapes: mbgl maps
/// `round` to its round pattern and both `butt` and `square` to the square one, because a square
/// cap extends the dash rather than rounding it and the distance field is the same either way.
fn cap_of(layer: &tessella_style::Layer) -> Cap {
    if layer.kind != LayerKind::Line {
        return Cap::Butt;
    }
    let literal = match layer.layout.get("line-cap") {
        Some(tessella_style::PropertyValue::Literal(value)) => value.as_str(),
        _ => None,
    };
    match literal {
        Some("round") => Cap::Round,
        _ => Cap::Butt,
    }
}
