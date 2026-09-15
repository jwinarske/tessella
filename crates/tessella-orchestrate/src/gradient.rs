// SPDX-License-Identifier: BSD-2-Clause
//! The color ramps a frame's gradient lines sample, and where each one sits.
//!
//! mbgl's `RenderLineLayer` owns one `colorRamp` image, 256 by 1, baked from `line-gradient` over
//! `["line-progress"]` and shared by every tile of the layer. This is that, one texture per layer
//! with an id that stays the layer's across frames, for the reason [`crate::dash`] gives.
//!
//! # Which layers draw one
//!
//! mbgl picks a line's shader in order: a dasharray first, then a pattern, then a gradient. A
//! layer that sets a gradient beside either of the others draws dashed or patterned and never
//! samples its ramp, so it gets none here.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::TextureId;
use tessella_style::Style;
use tessella_style::expression::Type;
use tessella_style::ramp::{RampParameter, bake};

use crate::tile::{Content, LayerBucket};

/// One gradient layer's ramp.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    /// The texture the ramp is uploaded as.
    pub texture: TextureId,
    /// The ramp, 256 texels of RGBA.
    pub pixels: Vec<u8>,
}

/// Every gradient line layer of a frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gradients {
    by_layer: BTreeMap<usize, Gradient>,
}

impl Gradients {
    /// The ramps a frame's buckets need.
    ///
    /// From the buckets, as [`crate::dash::Dashes::for_buckets`] is: they carry the resolved
    /// paint, and a layer appears once however many tiles of the cover drew it. `dashes` is the
    /// frame's, because a dashed layer draws its dashes instead.
    ///
    /// `base` is the first texture id to hand out; the caller owns the id space.
    #[must_use]
    pub fn for_buckets(
        style: &Style,
        buckets: &[(crate::tile::TileId, alloc::sync::Arc<Vec<LayerBucket>>)],
        dashes: &crate::dash::Dashes,
        base: u64,
    ) -> Self {
        let mut by_layer: BTreeMap<usize, Gradient> = BTreeMap::new();
        for (_, layer_buckets) in buckets {
            for bucket in layer_buckets.iter() {
                let index = bucket.layer_index;
                if !matches!(bucket.content, Content::Line(_))
                    || by_layer.contains_key(&index)
                    || dashes.get(index).is_some()
                {
                    continue;
                }
                // A pattern wins whether or not its sprite has arrived: mbgl tests the property's
                // presence, not the image's.
                if style
                    .layers
                    .get(index)
                    .is_none_or(|layer| layer.paint.contains_key("line-pattern"))
                {
                    continue;
                }
                // Unset resolves to a null rather than a color, which is the layer with no ramp.
                let Some(property) = bucket.paint.get("line-gradient") else {
                    continue;
                };
                if property.expression.result_type() != Type::Color {
                    continue;
                }
                let Ok(pixels) = bake(&property.expression, RampParameter::LineProgress) else {
                    continue;
                };
                #[allow(clippy::cast_possible_truncation)]
                by_layer.insert(
                    index,
                    Gradient {
                        texture: TextureId(base | index as u64),
                        pixels,
                    },
                );
            }
        }
        Self { by_layer }
    }

    /// The ramp a layer samples, if it draws a gradient.
    #[must_use]
    pub fn get(&self, layer_index: usize) -> Option<&Gradient> {
        self.by_layer.get(&layer_index)
    }

    /// Every ramp, in layer order, for the upload pass.
    pub fn iter(&self) -> impl Iterator<Item = &Gradient> {
        self.by_layer.values()
    }

    /// Whether any layer of the frame draws a gradient.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_layer.is_empty()
    }
}
