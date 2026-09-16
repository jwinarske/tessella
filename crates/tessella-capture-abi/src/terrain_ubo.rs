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
    /// What the ground is painted, where no layer covers it.
    ///
    /// The style's `background-color`, so bare terrain reads continuous with the flat map. The
    /// spec gives terrain no paint of its own, and a background is what a flat map shows where
    /// nothing else draws -- which makes it the nearest thing to a ground color the style has.
    ///
    /// Per layer rather than per drawable, strictly: every tile of the ground takes the same four
    /// numbers. It rides here anyway, because the alternative is a second uniform slot and a
    /// second thing for the two sides to agree on, against sixteen bytes a tile that nothing
    /// measures.
    pub color: [f32; 4],
    /// `uv_scale`, `uv_offset_x`, `uv_offset_y`, `exaggeration`.
    ///
    /// Two offsets and not one. A layer drawing *from* the DEM reads the whole tile and they are
    /// equal; a fill or a line is one of `2^dz` squares of a coarser DEM tile and they are not.
    pub params: [f32; 4],
    /// `skirt`, and three that are spare.
    ///
    /// How far a flagged vertex drops below the surface, in meters -- the curtain hiding the crack
    /// between two tiles at different zooms. The flag is the mesh vertex's own component; see
    /// `tessella_layout::terrain`.
    ///
    /// Its own row because the four before it are spoken for, and a row rather than a corner of
    /// one because std140 aligns a `vec4` and the next thing to need one will find it here. Only
    /// the ground reads it: a layer *on* the terrain has no skirt, being a surface on a surface.
    pub skirt: [f32; 4],
}

impl TerrainDrawableUbo {
    /// Bytes on the wire, and the stride a buffer of these packs at.
    pub const STRIDE: u32 = 128;

    /// The block as little-endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::STRIDE as usize] {
        let mut out = [0u8; Self::STRIDE as usize];
        let mut at = 0;
        for value in self
            .matrix
            .iter()
            .chain(&self.unpack)
            .chain(&self.color)
            .chain(&self.params)
            .chain(&self.skirt)
        {
            out[at..at + 4].copy_from_slice(&value.to_le_bytes());
            at += 4;
        }
        out
    }

    /// The sampling pair for a tile that is one of `2^dz` squares of the DEM tile covering it.
    ///
    /// [`Self::sampling`] is the `dz` of zero case -- a layer drawing *from* the DEM, whose tile is
    /// the DEM's own. Everything else is on a vector or raster tile at a coordinate the DEM source
    /// may not have, and reads the square of a coarser tile that covers its ground.
    ///
    /// `column` and `row` are which square, in `0 .. 2^dz`.
    #[must_use]
    pub fn sampling_within(
        dim: u32,
        extent: u16,
        dz: u8,
        column: u32,
        row: u32,
    ) -> (f32, f32, f32) {
        #[allow(clippy::cast_precision_loss)]
        let dim_f = dim as f32;
        let stride = dim_f + 2.0;
        if stride <= 0.0 || extent == 0 || dz >= 32 {
            return (0.0, 0.0, 0.0);
        }
        #[allow(clippy::cast_precision_loss)]
        let tiles = (1u32 << dz) as f32;
        // Pixels first, as `tessella_source::terrain::DemSampler` computes them: the tile's own
        // coordinates scaled into its share of the DEM, then moved to its square, then back half a
        // texel so a cell sits at its own center.
        let scale = dim_f / (f32::from(extent) * tiles);
        #[allow(clippy::cast_precision_loss)]
        let offset_x = column as f32 * dim_f / tiles - 0.5;
        #[allow(clippy::cast_precision_loss)]
        let offset_y = row as f32 * dim_f / tiles - 0.5;
        // Then into texture coordinates: one for the border, and the half texel a bilinear fetch
        // reads around. See [`Self::sampling`] for the chain written out.
        (
            scale / stride,
            (offset_x + 1.5) / stride,
            (offset_y + 1.5) / stride,
        )
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
        #[allow(clippy::cast_precision_loss)]
        let block = TerrainDrawableUbo {
            matrix: core::array::from_fn(|index| index as f32),
            unpack: [6553.6, 25.6, 0.1, 10000.0],
            color: [0.1, 0.2, 0.3, 1.0],
            params: [1.0, 2.0, 3.0, 4.0],
            skirt: [5.0, 0.0, 0.0, 0.0],
        };
        let bytes = block.to_bytes();
        let at = |offset: usize| f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(bytes.len(), TerrainDrawableUbo::STRIDE as usize);
        assert_eq!(at(0), 0.0);
        assert_eq!(at(60), 15.0);
        assert_eq!(at(64), 6553.6);
        // The color sits between the unpack vector and the scalars, which is the order the
        // shader declares them in -- a block whose fields agree in size and disagree in order is
        // a mispack nothing rejects.
        assert_eq!(at(80), 0.1);
        assert_eq!(at(92), 1.0);
        assert_eq!(at(96), 1.0);
        assert_eq!(at(108), 4.0);
        assert_eq!(at(112), 5.0);
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

    /// A tile inside a coarser DEM samples its own square of it, and the whole-tile case agrees
    /// with [`TerrainDrawableUbo::sampling`] exactly.
    ///
    /// Checked against the pixel arithmetic `tessella_source::terrain::DemSampler` uses rather than
    /// against constants: the two have to place a coordinate on the same cell, and a test carrying
    /// its own copy of the answer would agree with a wrong one.
    #[test]
    fn a_tile_samples_its_own_square_of_a_coarser_dem() {
        const DIM: u32 = 256;
        const EXTENT: u16 = 8192;
        let (whole_scale, whole_offset) = TerrainDrawableUbo::sampling(DIM, EXTENT);
        let (scale, x, y) = TerrainDrawableUbo::sampling_within(DIM, EXTENT, 0, 0, 0);
        assert!((scale - whole_scale).abs() < 1e-9);
        assert!((x - whole_offset).abs() < 1e-9 && (y - whole_offset).abs() < 1e-9);

        // One zoom deeper, the square at column one row zero: the tile's own center should land
        // where three quarters across and one quarter down the parent does.
        let (scale, x, y) = TerrainDrawableUbo::sampling_within(DIM, EXTENT, 1, 1, 0);
        let child = 4096.0 * scale + x;
        let parent = 6144.0 * whole_scale + whole_offset;
        assert!((child - parent).abs() < 1e-6, "{child} {parent}");
        let child_y = 4096.0 * scale + y;
        let parent_y = 2048.0 * whole_scale + whole_offset;
        assert!((child_y - parent_y).abs() < 1e-6, "{child_y} {parent_y}");
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
