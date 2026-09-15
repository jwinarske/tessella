// SPDX-License-Identifier: BSD-2-Clause

//! The terrain surface's family and its per-drawable block -- an extension beyond the oracle.
//!
//! # Why this is not in `generated`
//!
//! Everything under [`crate::generated`] mirrors mbgl's own shader families and uniform blocks and
//! is stamped "do not edit by hand". mbgl has no terrain: no `style/terrain.hpp`, no
//! `render_terrain*`, no `setTerrain`, at the pinned revision or upstream. So there is nothing to
//! mirror, and this lives beside the mirror the way [`crate::globe_ubo`] does, for the same reason
//! -- the generator keeps overwriting the mirror.
//!
//! # Why it needs no ABI revision
//!
//! Neither of the two things agreed here is a new shape on the wire. A family is an `i32` on
//! [`GeometryAdd::builtin_shader`](crate::envelope::GeometryAdd::builtin_shader) and a block is
//! `(layer, slot, bytes)` on [`UboUpdate`](crate::envelope::UboUpdate); both channels exist and
//! both take a value. What has to be agreed is which value, and that is all this module is.
//!
//! # Why the numbers are where they are
//!
//! mbgl's `BuiltIn` runs from `None` at zero to `WideVectorShader` at 35, and its uniform slots
//! span nought to ten with the globe's bend already claiming eleven. Both of these sit at 128 and
//! above: far enough that a future mbgl family or slot cannot reach them, and far enough that a
//! number in a dump is obviously not mbgl's to anyone reading one.
//!
//! # What the surface is
//!
//! One mesh for every tile -- `tessella_layout::terrain::mesh`, 129 vertices a side over a tile's
//! own coordinates -- drawn once per tile through a matrix, with the height read from the tile's
//! DEM per vertex. So the geometry is process-scoped and shared (§5.3) and everything that differs
//! between two tiles is in this block and in the texture the drawable names.

/// The shader family a terrain surface names.
///
/// Not an mbgl `BuiltIn`. See the module note for why 128.
pub const BUILTIN_TERRAIN_SHADER: i32 = 128;

/// The slot the terrain surface's per-drawable block binds at.
///
/// Past mbgl's nought-to-ten and past the globe bend's eleven.
pub const ID_TERRAIN_DRAWABLE_UBO: u32 = 128;

/// One terrain tile's block.
///
/// # Why the sampling is pre-scaled
///
/// `uv_scale` and `uv_offset` turn a tile coordinate straight into a texture coordinate:
/// `uv = pos * uv_scale + uv_offset`, one multiply-add per axis per vertex over a 17,415-vertex
/// mesh. They carry three facts that would otherwise each be a uniform and an operation -- the
/// DEM's own width, the pixel of border around it, and the half-texel that puts a DEM cell at its
/// own center. See `tessella_source::terrain` for that last one, which is the number in the chain
/// that is not obviously there.
///
/// Written out: a tile coordinate `x` lands on DEM pixel `x / EXTENT * dim - 0.5`, the stored
/// image adds one for the border, and a bilinear fetch at `uv` reads around texel `uv * stride -
/// 0.5`. Equating the two gives `uv = (x / EXTENT * dim + 1) / stride`, which is this scale and
/// this offset.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct TerrainDrawableUbo {
    /// Tile-local to clip. Its `z` takes meters, because `world_to_camera` already post-multiplies
    /// the z row by pixels-per-meter -- the same reason a fill extrusion's height is in meters.
    pub matrix: [f32; 16],
    /// The DEM's unpack vector: `red`, `green`, `blue`, `base_shift`, as
    /// `tessella_source::dem::Encoding::unpack` gives it.
    pub unpack: [f32; 4],
    /// `uv_scale`, `uv_offset`, `exaggeration`, `skirt`.
    ///
    /// `skirt` is how far a flagged vertex drops below the surface, in meters -- the curtain that
    /// hides the crack between two tiles at different zooms. The flag is the mesh vertex's third
    /// component; see `tessella_layout::terrain`.
    pub params: [f32; 4],
}

impl TerrainDrawableUbo {
    /// Bytes on the wire, and the stride a buffer of these packs at.
    pub const STRIDE: u32 = 96;

    /// The block as little-endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::STRIDE as usize] {
        let mut out = [0u8; Self::STRIDE as usize];
        let mut at = 0;
        for value in self.matrix.iter().chain(&self.unpack).chain(&self.params) {
            out[at..at + 4].copy_from_slice(&value.to_le_bytes());
            at += 4;
        }
        out
    }

    /// The sampling pair for a DEM of `dim` pixels stored with a pixel of border on each side.
    ///
    /// `extent` is the tile's coordinate range, which is 8192 for everything this build produces.
    #[must_use]
    pub fn sampling(dim: u32, extent: u16) -> (f32, f32) {
        #[allow(clippy::cast_precision_loss)]
        let dim = dim as f32;
        let stride = dim + 2.0;
        if stride <= 0.0 || extent == 0 {
            return (0.0, 0.0);
        }
        (dim / (f32::from(extent) * stride), 1.0 / stride)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The block is the size it says, and its fields are in the order the shader reads them.
    #[test]
    fn the_block_packs_to_its_stride() {
        let block = TerrainDrawableUbo {
            matrix: core::array::from_fn(|index| index as f32),
            unpack: [6553.6, 25.6, 0.1, 10000.0],
            params: [1.0, 2.0, 3.0, 4.0],
        };
        let bytes = block.to_bytes();
        assert_eq!(bytes.len(), TerrainDrawableUbo::STRIDE as usize);
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0.0);
        assert_eq!(f32::from_le_bytes(bytes[60..64].try_into().unwrap()), 15.0);
        assert_eq!(
            f32::from_le_bytes(bytes[64..68].try_into().unwrap()),
            6553.6
        );
        assert_eq!(f32::from_le_bytes(bytes[80..84].try_into().unwrap()), 1.0);
        assert_eq!(f32::from_le_bytes(bytes[92..96].try_into().unwrap()), 4.0);
    }

    /// The sampling pair lands a tile coordinate on the DEM cell that owns it.
    ///
    /// Checked at the two ends and the middle against the chain the doc spells out, rather than
    /// against constants: the half texel and the border pixel both live in this arithmetic and a
    /// test carrying its own copy of the answer would agree with a wrong one.
    #[test]
    fn the_sampling_puts_a_cell_at_its_own_center() {
        const DIM: u32 = 256;
        const EXTENT: u16 = 8192;
        let (scale, offset) = TerrainDrawableUbo::sampling(DIM, EXTENT);
        #[allow(clippy::cast_precision_loss)]
        let stride = (DIM + 2) as f32;

        // What a bilinear fetch at `uv` reads around, as a stored-texel index.
        let texel = |x: f32| (x * scale + offset) * stride - 0.5;
        // What the DEM says that coordinate is, in stored-texel terms: the cell-center offset,
        // then the border.
        #[allow(clippy::cast_precision_loss)]
        let want = |x: f32| x / f32::from(EXTENT) * DIM as f32 - 0.5 + 1.0;

        for x in [0.0, 32.0, 4096.0, 8191.0, 8192.0] {
            assert!(
                (texel(x) - want(x)).abs() < 1e-3,
                "{x}: {} {}",
                texel(x),
                want(x)
            );
        }
        // The tile's first cell center sits on stored texel 1, which is the first real pixel
        // after the border.
        #[allow(clippy::cast_precision_loss)]
        let first_center = f32::from(EXTENT) / DIM as f32 * 0.5;
        assert!(
            (texel(first_center) - 1.0).abs() < 1e-3,
            "{}",
            texel(first_center)
        );
    }

    /// A degenerate DEM samples nothing rather than dividing by it.
    #[test]
    fn a_degenerate_dem_samples_nothing() {
        assert_eq!(TerrainDrawableUbo::sampling(256, 0), (0.0, 0.0));
        let (scale, offset) = TerrainDrawableUbo::sampling(0, 8192);
        assert_eq!(scale, 0.0);
        assert!((offset - 0.5).abs() < f32::EPSILON, "{offset}");
    }

    /// Neither number collides with anything mbgl has.
    #[test]
    fn the_numbers_are_past_mbgls() {
        use crate::BuiltIn;
        for family in BuiltIn::ALL {
            assert_ne!(family as i32, BUILTIN_TERRAIN_SHADER, "{family:?}");
        }
        // Past the highest mbgl declares, so a family it adds later cannot reach this one.
        const { assert!(BUILTIN_TERRAIN_SHADER > 35) };
        const { assert!(ID_TERRAIN_DRAWABLE_UBO > crate::globe_ubo::ID_GLOBE_BEND_UBO) };
    }
}
