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
//!
//! # Sharing one, without giving up the stable id
//!
//! Layers wanting byte-identical atlases share one texture, as mbgl's do. The id is still a
//! layer's: it is `base` plus the *lowest* index among the layers that want those bytes, and the
//! higher ones name it rather than uploading a copy.
//!
//! That keeps the invariant above exactly as it was, which is why it needs no counter over the
//! live set and no table outliving the frame. `base + L` still means "the atlas layer L wants",
//! because L is one of the layers wanting it -- so when L's dasharray steps, `base + L` re-uploads
//! to what L now wants, as before. A layer that shared it and did not step simply becomes the
//! lowest index for the old bytes and takes its own id, uploading them there. No id ever names
//! bytes that the layer it is numbered for does not want.
//!
//! Keyed on the atlas bytes rather than on the dasharray pair that produced them, because the
//! bytes are what a texture is. Two patterns that render the same field share the upload without
//! anything having to prove they are the same pattern.

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
    /// The layers whose id names a texture, ascending: one per distinct atlas.
    ///
    /// What [`Dashes::iter`] walks, so the upload pass sends each atlas once however many layers
    /// draw with it. `by_layer` still holds an entry per layer, because that is what a drawable
    /// asks for and a sharing layer needs its own `from`, `to` and crossfade.
    owners: Vec<usize>,
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
        buckets: &[(crate::tile::TileId, alloc::sync::Arc<Vec<LayerBucket>>)],
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

        // Resolved first and numbered second, because the id is the lowest layer index wanting
        // an atlas and the buckets arrive tile by tile -- a layer's first appearance in that
        // order says nothing about whether a lower-numbered layer wants the same bytes. A
        // `BTreeMap` hands the second pass the layers in index order, which is what makes
        // "lowest" well defined without sorting anything.
        let mut resolved: BTreeMap<usize, (Atlas, Position, Position)> = BTreeMap::new();
        for (_, layer_buckets) in buckets {
            for bucket in layer_buckets.iter() {
                if !matches!(bucket.content, Content::Line(_))
                    || resolved.contains_key(&bucket.layer_index)
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
                resolved.insert(bucket.layer_index, (atlas, from, to));
            }
        }

        let mut by_layer: BTreeMap<usize, Dash> = BTreeMap::new();
        let mut owners: Vec<usize> = Vec::new();
        let mut ids: BTreeMap<(Vec<u8>, u32), u64> = BTreeMap::new();
        for (layer_index, (atlas, from, to)) in resolved {
            #[allow(clippy::cast_possible_truncation)]
            let own = base + layer_index as u64;
            let texture = match ids.entry((atlas.data.clone(), atlas.height)) {
                alloc::collections::btree_map::Entry::Occupied(shared) => *shared.get(),
                alloc::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(own);
                    owners.push(layer_index);
                    own
                }
            };
            by_layer.insert(
                layer_index,
                Dash {
                    texture: TextureId(texture),
                    atlas,
                    from,
                    to,
                    crossfade: fade,
                },
            );
        }
        Self { by_layer, owners }
    }

    /// The atlas a layer draws with, if it has one.
    #[must_use]
    pub fn get(&self, layer_index: usize) -> Option<&Dash> {
        self.by_layer.get(&layer_index)
    }

    /// Every distinct atlas, in layer order, for the upload pass.
    ///
    /// One entry per texture and not per layer: layers sharing an atlas share the upload, which
    /// is the whole point of sharing it.
    pub fn iter(&self) -> impl Iterator<Item = &Dash> {
        self.owners
            .iter()
            .filter_map(|layer| self.by_layer.get(layer))
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
