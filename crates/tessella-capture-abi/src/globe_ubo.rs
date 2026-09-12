// SPDX-License-Identifier: Apache-2.0

//! The anchored bend's per-drawable block — plan.md §17, an extension beyond the oracle.
//!
//! # Why this is not in `generated`
//!
//! Everything under [`crate::generated`] mirrors mbgl's own uniform blocks, header comment for
//! header comment, and is stamped "do not edit by hand" because a drifted layout there is a silent
//! mispack. mbgl has no globe, so it has no block for one. This is tessella's, and it lives beside
//! the mirror rather than inside it so the generator can keep overwriting the mirror.
//!
//! # Why it needs no ABI revision
//!
//! [`crate::envelope::UboUpdate`] already carries `(layer, slot, bytes)` for an arbitrary slot, so
//! a new block is new *content* on a channel that exists rather than a new shape on the wire. The
//! slot is the only thing that has to be agreed, and it is [`ID_GLOBE_BEND_UBO`](crate::globe_ubo::ID_GLOBE_BEND_UBO).
//!
//! # What it carries
//!
//! `globe::anchored_bend`'s coefficients, already in clip space:
//!
//! ```text
//! clip(du, dv) = anchor + d_u du + d_v dv + (d_uu du^2 + d_vv dv^2) / 2 + d_uv du dv
//! ```
//!
//! with `du`, `dv` in tile units from the tile's center. The point of sending clip space rather
//! than a sphere position is that `anchor` is a difference of large numbers -- the clip matrix at
//! z14 scales the unit sphere to 1,663,008 pixels -- which `f64` takes here and `f32` would lose on
//! the GPU. Every other coefficient is small, so the shader only ever adds small to small.

/// The slot this block binds at.
///
/// Eleven, because mbgl spans nought to ten and this has to be past *all* of it. Five looked free
/// from `FILL_LAYER_SSBO_COUNT` and is not: every family keeps its evaluated paint there --
/// `ID_FILL_EVALUATED_PROPS_UBO`, `ID_BACKGROUND_PROPS_UBO` and four more are all five, and the
/// consumer reads it as `kPropsSlot`. A block that landed there would have overwritten the layer's
/// colour with a matrix.
pub const ID_GLOBE_BEND_UBO: u32 = 11;

/// One drawable's coefficients, in the order the shader reads them.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct GlobeBendUbo {
    /// Clip position of the tile's center, with the layer's depth bias already in `z`.
    pub anchor: [f32; 4],
    /// First derivatives with respect to tile-local x and y.
    pub d_u: [f32; 4],
    /// See [`Self::d_u`].
    pub d_v: [f32; 4],
    /// Second derivatives. The mixed term is not zero: longitude and latitude share
    /// `cos(latitude)` in the sphere point.
    pub d_uu: [f32; 4],
    /// See [`Self::d_uu`].
    pub d_vv: [f32; 4],
    /// See [`Self::d_uu`].
    pub d_uv: [f32; 4],
    /// Clip displacement per metre of height above the surface -- `globe::AnchoredBend::d_h`.
    ///
    /// Zero for every family but the extrusions, which are the only geometry that leaves the
    /// surface. Sent for all of them rather than only for those, because the block is one shape
    /// and a stride that varied by family would be a second thing to agree on.
    pub d_h: [f32; 4],
}

impl GlobeBendUbo {
    /// Bytes on the wire, and the stride a buffer of these packs at.
    pub const STRIDE: u32 = 112;

    /// The block as little-endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::STRIDE as usize] {
        let mut out = [0u8; Self::STRIDE as usize];
        let mut at = 0;
        for row in [
            self.anchor,
            self.d_u,
            self.d_v,
            self.d_uu,
            self.d_vv,
            self.d_uv,
            self.d_h,
        ] {
            for value in row {
                out[at..at + 4].copy_from_slice(&value.to_le_bytes());
                at += 4;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{GlobeBendUbo, ID_GLOBE_BEND_UBO};

    /// Seven `vec4` and nothing else, at the stride the packer uses.
    #[test]
    fn the_block_is_one_hundred_and_twelve_bytes() {
        assert_eq!(core::mem::size_of::<GlobeBendUbo>(), 112);
        assert_eq!(
            GlobeBendUbo::STRIDE as usize,
            core::mem::size_of::<GlobeBendUbo>()
        );
        assert_eq!(core::mem::align_of::<GlobeBendUbo>(), 4);
    }

    /// The slot is past every block mbgl declares, not merely past a fill's.
    ///
    /// `FILL_LAYER_SSBO_COUNT` says five and means "a fill binds five", not "five is free". Slot
    /// five is where every family keeps its evaluated paint, so a block there would have replaced
    /// the layer's colour. This checks the whole table rather than one family's count.
    #[test]
    fn the_slot_is_past_every_block_mbgl_declares() {
        use crate::generated::ubo_slots::{
            ID_BACKGROUND_PROPS_UBO, ID_COLLISION_TILE_PROPS_UBO, ID_FILL_DRAWABLE_UBO,
            ID_FILL_EVALUATED_PROPS_UBO, ID_FILL_EXTRUSION_INSTANCED, ID_FILL_TILE_PROPS_UBO,
            ID_LINE_EXPRESSION_UBO, ID_SYMBOL_TILE_PROPS_UBO,
        };
        for taken in [
            ID_FILL_DRAWABLE_UBO,
            ID_SYMBOL_TILE_PROPS_UBO,
            ID_FILL_TILE_PROPS_UBO,
            ID_FILL_EVALUATED_PROPS_UBO,
            ID_BACKGROUND_PROPS_UBO,
            ID_LINE_EXPRESSION_UBO,
            ID_FILL_EXTRUSION_INSTANCED,
            ID_COLLISION_TILE_PROPS_UBO,
        ] {
            assert!(
                ID_GLOBE_BEND_UBO > taken,
                "slot {ID_GLOBE_BEND_UBO} is not past {taken}, which mbgl already binds"
            );
        }
    }

    /// Written in declaration order, little-endian, with nothing between the rows.
    #[test]
    fn the_bytes_are_the_six_rows_in_order() {
        let block = GlobeBendUbo {
            anchor: [1.0, 2.0, 3.0, 4.0],
            d_uv: [0.0, 0.0, 0.0, 24.0],
            ..GlobeBendUbo::default()
        };
        let bytes = block.to_bytes();
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[12..16], &4.0f32.to_le_bytes());
        assert_eq!(&bytes[92..96], &24.0f32.to_le_bytes());
    }
}
