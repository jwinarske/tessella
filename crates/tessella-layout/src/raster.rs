//! A raster tile's geometry: a quad, and where in the image each corner samples.
//!
//! A transcription of mbgl's `RasterBucket`. There is almost nothing here compared to a fill or a
//! line, and that is the point — a raster tile *is* an image, so the geometry is the rectangle it
//! is stretched over and the interesting work is the texture upload and the colour adjustment
//! beside it.
//!
//! # Why a quad per masked tile rather than one quad
//!
//! mbgl builds one quad for each entry of the tile's clip mask, which is the set of quadrants a
//! tile actually draws — a parent under one child draws the other three. With no mask the whole
//! tile is one quad, which is every case a settled frame at a fixed camera produces and every
//! case any capture contains.
//!
//! The mask comes from [`tessella_tile::mask::update_tile_masks`], which is mbgl's
//! `algorithm::updateTileMasks`. It is a *raster* mechanism and not a stencil one: `TileMask` is
//! consumed only by `RasterBucket::setMask` and `HillshadeBucket::setMask`, both of which turn it
//! into geometry, while `renderTileClippingMasks` draws a full-tile quad per render tile and
//! never sees a quadrant. So a mask needs nothing added to the capture stream — it is geometry,
//! and geometry already travels.

use alloc::vec::Vec;

/// The tile extent, which is also the texture's coordinate space.
pub const EXTENT: i32 = 8192;

/// One vertex: where it is in the tile, and where it samples the image.
///
/// The two are the same numbers for a whole-tile quad and diverge for a masked one, which is why
/// they are separate attributes rather than one. mbgl declares them as `Short2` and `UShort2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterVertex {
    /// Position in tile units.
    pub position: [i16; 2],
    /// Position in the image, in the same units.
    pub texture: [u16; 2],
}

/// One raster layer's geometry for one tile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RasterBucket {
    /// Four vertices per quad.
    pub vertices: Vec<RasterVertex>,
    /// Six indices per quad.
    pub indices: Vec<u16>,
}

impl RasterBucket {
    /// Whether anything was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// How many quads it holds.
    #[must_use]
    pub fn quads(&self) -> usize {
        self.vertices.len() / 4
    }

    /// Adds the quad for one entry of a tile's mask.
    ///
    /// `(z, x, y)` is the mask entry *relative to the tile*: `(0, 0, 0)` is the whole tile, and
    /// `(1, 0, 1)` is its bottom-left quarter. The extent halves with each level, which is what
    /// makes a mask's quads tile the parent exactly.
    ///
    /// # Panics
    ///
    /// When the quad's coordinates would not fit an `i16`. The extent is 8192 and a mask level
    /// only ever shrinks it, so this is unreachable for a mask a tile could have.
    pub fn add_quad(&mut self, z: u8, x: u32, y: u32) {
        let extent = EXTENT >> z.min(15);
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let (left, top) = ((x as i32 * extent) as i16, (y as i32 * extent) as i16);
        #[allow(clippy::cast_possible_truncation)]
        let (right, bottom) = (left + extent as i16, top + extent as i16);

        #[allow(clippy::cast_possible_truncation)]
        let base = self.vertices.len() as u16;
        assert!(
            self.vertices.len() + 4 <= usize::from(u16::MAX),
            "a raster buffer past what a u16 index reaches needs a new segment"
        );

        // Top-left, top-right, bottom-left, bottom-right — mbgl's order, and the one the two
        // triangles below index against.
        for (position, texture) in [
            ([left, top], [left, top]),
            ([right, top], [right, top]),
            ([left, bottom], [left, bottom]),
            ([right, bottom], [right, bottom]),
        ] {
            #[allow(clippy::cast_sign_loss)]
            self.vertices.push(RasterVertex {
                position,
                texture: [texture[0] as u16, texture[1] as u16],
            });
        }

        // Two triangles sharing the diagonal: 0,1,2 then 1,2,3.
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base + 1, base + 2, base + 3]);
    }

    /// The bucket for a whole tile.
    #[must_use]
    pub fn whole_tile() -> Self {
        let mut bucket = Self::default();
        bucket.add_quad(0, 0, 0);
        bucket
    }

    /// The bucket for a tile's clip mask.
    ///
    /// `mask` is what [`tessella_tile::mask::update_tile_masks`] produced for this tile: the
    /// sub-tiles it should still draw, relative to itself. mbgl's `RasterBucket::setMask` builds
    /// exactly this — a quad per entry at `EXTENT >> z` — and the correspondence is why the mask
    /// needs no place on the wire: it is geometry, and geometry already travels.
    ///
    /// An **empty** mask is an empty bucket, which draws nothing. That is the answer for a tile
    /// entirely covered by better ones, and it is the opposite of the whole-tile mask rather than
    /// a degenerate form of it.
    ///
    /// mbgl special-cases the whole-tile mask to keep using shared full-extent buffers; here it
    /// is one quad either way, so the case is not branched on. The saving mbgl makes is in buffer
    /// *sharing* rather than in vertex count, and this side shares by geometry identity instead —
    /// two tiles with the same mask produce the same bytes and therefore the same id (§5.3).
    #[must_use]
    pub fn masked(mask: &[tessella_tile::mask::MaskEntry]) -> Self {
        let mut bucket = Self::default();
        for entry in mask {
            bucket.add_quad(entry.z, entry.x, entry.y);
        }
        bucket
    }
}

/// The three colour adjustments a raster layer's shader is given, derived from its paint.
///
/// Each is a *factor* rather than the property's own value, because the shader wants the number
/// it multiplies by and the property is stated the way a person thinks about it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterColour {
    /// How the hue rotation redistributes the channels.
    pub spin_weights: [f32; 4],
    /// What saturation multiplies by.
    pub saturation_factor: f32,
    /// What contrast multiplies by.
    pub contrast_factor: f32,
}

/// `raster-hue-rotate`, in degrees, as the three channel weights the shader mixes with.
///
/// Rotating a hue is a rotation about the grey axis of the colour cube, and these are that
/// rotation's row — the same construction a YIQ hue shift uses. The fourth weight is zero and
/// exists because the shader wants a `vec4`.
#[must_use]
pub fn spin_weights(degrees: f32) -> [f32; 4] {
    let radians = degrees.to_radians();
    let (sin, cos) = radians.sin_cos();
    let root3 = libm::sqrtf(3.0);
    [
        (2.0 * cos + 1.0) / 3.0,
        (-root3 * sin - cos + 1.0) / 3.0,
        (root3 * sin - cos + 1.0) / 3.0,
        0.0,
    ]
}

/// `raster-saturation` as the factor the shader multiplies by.
///
/// Asymmetric, and mbgl's: desaturating is linear and saturating is a reciprocal that runs away
/// as it approaches one. The `1.001` is what stops it dividing by zero at the property's own
/// maximum — a bound in the arithmetic rather than in the property.
#[must_use]
pub fn saturation_factor(saturation: f32) -> f32 {
    if saturation > 0.0 {
        1.0 - 1.0 / (1.001 - saturation)
    } else {
        -saturation
    }
}

/// `raster-contrast` as the factor the shader multiplies by.
///
/// The same asymmetry for the same reason: reducing contrast is linear and raising it is a
/// reciprocal. At a contrast of one the divisor is zero, which the property's own range stops
/// short of rather than the arithmetic doing so — the spec bounds it at 1 exclusive.
#[must_use]
pub fn contrast_factor(contrast: f32) -> f32 {
    if contrast > 0.0 {
        1.0 / (1.0 - contrast)
    } else {
        1.0 + contrast
    }
}
