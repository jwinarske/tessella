//! Capture-stream envelope ABI and SPSC ring — plan.md §7, descends from mbgl `capture/`.
//!
//! This crate is the port boundary. The Rust deliverable is "a frontend that produces this
//! stream" (§2), and this is the single source of truth for its shape: envelope structs, the
//! ring, the coalescing table, and the DR-10 reverse channel. The flat C header shared with a
//! consumer mirror is generated from here, never hand-maintained alongside it.
//!
//! # Ownership (§2.1)
//!
//! Rev 1's aliasing model is gone. It leaned on co-residency — `AttributeDesc::sharedVector`
//! was a non-owning view into a bucket's vertex vector, and UBO and texture bytes were
//! borrowed for the duration of a callback. Rev 2 makes ownership explicit: bucket vertex and
//! index data live in refcounted slabs, the geometry envelope carries a slab handle plus
//! offset and stride, and UBO and texture bytes are copied into the ring at emit. The lifetime
//! footnotes disappear from the protocol. Envelopes carry no in-process pointers — slab
//! handles are offsets — which is also what keeps the §3.5 process-isolation option open.
//!
//! # Consumer neutrality (DR-13)
//!
//! The stream must contain nothing accidentally Filament-shaped. Two mirrors prove it: the
//! Fluorite/Filament mirror and the impeller-rs mirror (§3.6), the latter consuming at the
//! entity/HAL level. Consumer-specific needs are met by the §11.7 obligations, never by
//! changing envelope shape.
//!
//! # Stability
//!
//! Nothing here is frozen yet. The ABI freezes at R0 exit (§10) and covers envelope and ring
//! struct shape, atomics, mode-bit positions, and conventions; field additions to existing
//! envelopes stay open through R2. Discriminants below are provisional until that freeze.
//!
//! Layout asserts compile on every target, which is how R-6 (riscv64 atomics and alignment)
//! is caught at build time rather than in a soak.
//!
//! # Ingress validation
//!
//! Every `#[repr(uN)]` enum here crosses a C boundary, and a value arriving from the far
//! side is untrusted input regardless of which mirror wrote it: a producer/consumer version
//! skew, a torn ring read, or a mirror built against a different header all produce
//! discriminants this crate has never heard of. Holding one in an enum is undefined
//! behavior, not a wrong answer — so ingress goes through the `from_repr` constructors,
//! which return `None` for anything unrecognized. Never `transmute` a discriminant, and
//! never `as`-cast one into an enum.
//!
//! Status: envelope records, the mbgl mirrors, the ring, coalescing, and the reverse channel
//! are in. The generated C header is not.

// Not `forbid(unsafe_code)`: the ring and the `#[repr(C)]` envelope mirrors need it. Every
// other crate in the workspace forbids it outright.
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(test), no_std)]

/// Producer-side coalescing for the state envelopes (§4).
pub mod coalesce;
pub mod envelope;

/// SPSC transport for the lossless envelope stream (§4).
pub mod reverse;
pub mod ring;

/// Mirrors of mbgl scalar enums, generated from the pinned C++ tree.
///
/// DR-6: never hand-maintained. Regenerate with
/// `cargo run -p mbgl-codegen -- --mbgl <maplibre-native>` when the pin moves, and
/// `--check` to confirm the committed file is current.
pub mod generated;

pub use generated::mbgl_enums::{AttributeDataType, BuiltIn, RenderPass, TexturePixelType};

/// Revision of the capture-stream ABI this crate implements.
///
/// Rev 1 is the C++ `FrameDiff` stream in `include/mbgl/capture/frame_diff.hpp`. Rev 2 is
/// this one: explicit ownership (§2.1), the geometry/view namespace split (§5.3), and
/// `FrameOrder` split into [`EnvelopeKind::CameraUpdate`] and [`EnvelopeKind::OrderUpdate`]
/// (§6.3). See DR-4.
pub const ABI_REV: u32 = 2;

/// The envelopes carried on the ring, one variant per row of the §4 coalescing table.
///
/// Discriminants are provisional until the R0 ABI freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum EnvelopeKind {
    /// Process-scoped, refcounted geometry: shared geometry id, attributes, indexes,
    /// segments, texture refs, shader identity, vertex count. Removed when the last view
    /// releases it (§5.3).
    GeometryAdd = 1,
    /// Drops a shared geometry (§5.3).
    GeometryRemove = 2,
    /// Per-view use of a shared geometry: `(viewId, geometryId, layerIndex, subLayerIndex,
    /// renderPass flags, tileID)`. Declares the view's [`CameraMode`] (DR-9).
    ViewUse = 3,
    /// Drops a per-view use (§5.3).
    ViewRelease = 4,
    /// Absolute uniform write, keyed `(viewId, layerIndex, slot)`. Latest-wins is exact
    /// because the writes are absolute (§4). Rev 1's per-drawable `ownerId` key is gone with
    /// the per-drawable buffer path (DR-16).
    UboUpdate = 5,
    /// Texture bytes plus a dirty-rect list, ordered within a texture (§6.4).
    TextureUpdate = 6,
    /// Per-view camera block: `projMatrix`, `centerZoom0`, bearing, pitch, `pixelsPerMeter`,
    /// light, `frameNo`, `opaquePassCutoff`, `depthRangeSize`, `orderEpoch` (§6.3).
    ///
    /// In consumer-camera mode this degrades to the non-matrix fields; the consumer's camera
    /// is authoritative and the producer reads it back over the reverse channel (§11.1).
    CameraUpdate = 7,
    /// Per-view ordered draw list plus a new order epoch, emitted only when the list differs
    /// from the last one emitted (§6.3).
    OrderUpdate = 8,
    /// Per-`(viewId, layerIndex)` stencil tile set, emitted on change only. The consumer
    /// synthesizes masks from these; reference values are never carried (§2.2).
    StencilTiles = 9,
}

/// How the ring treats an envelope under consumer stall (§4).
///
/// Coalescing is a ring property, not just an emission property: it is what bounds occupancy
/// when the consumer stalls (R-4), and latest-wins is only correct because every coalescable
/// envelope is an absolute state write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CoalescePolicy {
    /// Delivered in order, never dropped or merged. Backpressure blocks the producer by
    /// design; the ring is sized for worst-case tile turnover.
    Lossless = 0,
    /// Superseded by the next envelope with the same key.
    LatestWins = 1,
    /// Dirty rects merge into the existing list, spilling to their union past the cap (§6.4).
    RectListMerge = 2,
}

impl EnvelopeKind {
    /// Every declared envelope kind. Adding a variant without adding it here fails the
    /// round-trip test rather than silently escaping coverage.
    pub const ALL: [Self; 9] = [
        Self::GeometryAdd,
        Self::GeometryRemove,
        Self::ViewUse,
        Self::ViewRelease,
        Self::UboUpdate,
        Self::TextureUpdate,
        Self::CameraUpdate,
        Self::OrderUpdate,
        Self::StencilTiles,
    ];

    /// Converts a wire discriminant into an [`EnvelopeKind`], rejecting unknown values.
    ///
    /// See the crate-level note on ingress validation: this is the only supported way to
    /// turn a number off the ring into an envelope kind.
    #[must_use]
    pub const fn from_repr(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::GeometryAdd),
            2 => Some(Self::GeometryRemove),
            3 => Some(Self::ViewUse),
            4 => Some(Self::ViewRelease),
            5 => Some(Self::UboUpdate),
            6 => Some(Self::TextureUpdate),
            7 => Some(Self::CameraUpdate),
            8 => Some(Self::OrderUpdate),
            9 => Some(Self::StencilTiles),
            _ => None,
        }
    }

    /// The §4 coalescing policy for this envelope kind.
    #[must_use]
    pub const fn coalesce_policy(self) -> CoalescePolicy {
        match self {
            Self::GeometryAdd | Self::GeometryRemove | Self::ViewUse | Self::ViewRelease => {
                CoalescePolicy::Lossless
            }
            Self::UboUpdate | Self::CameraUpdate | Self::OrderUpdate | Self::StencilTiles => {
                CoalescePolicy::LatestWins
            }
            Self::TextureUpdate => CoalescePolicy::RectListMerge,
        }
    }
}

/// Which side owns the camera for a view. Declared per view at [`EnvelopeKind::ViewUse`]
/// (DR-9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CameraMode {
    /// The producer's transform is authoritative and ships a fused `projMatrix` (§6.3). Used
    /// for non-interactive views: cluster insets, fixed tracks.
    Producer = 0,
    /// The consumer's camera is authoritative. The producer emits tile-local transforms in
    /// the shared world space plus `pixelsPerMeter` and reads the camera back one frame stale
    /// over the reverse channel (§11.1). Pan-to-photon drops to the consumer's own render
    /// latency; the ring leaves the interactive path. See R-8 for the staleness artifacts
    /// this trades against.
    Consumer = 1,
}

/// Uniform transport mode (DR-16, resolves R-12).
///
/// There is one path. The per-drawable-buffer variant of rev 1 is gone, and no fallback
/// exists: maps require an SSBO-capable backend — Vulkan today, GLES 3.1+ if a consumer ever
/// implements one. impeller-rs's GLES HAL floors at 3.0 and composites only; it does not draw
/// maps. The bit is reserved so that a future GLES-3.0-only SKU could add a fallback without
/// a flag day, not because one is planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UniformTransport {
    /// One consolidated buffer per `(view, layer)`, drawables indexing via `uboIndex`, no
    /// length ceiling (§11.2).
    ConsolidatedSsbo = 0,
}

impl CameraMode {
    /// Converts a wire discriminant into a [`CameraMode`], rejecting unknown values.
    ///
    /// See the crate-level note on ingress validation.
    #[must_use]
    pub const fn from_repr(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Producer),
            1 => Some(Self::Consumer),
            _ => None,
        }
    }
}

impl UniformTransport {
    /// Converts a wire discriminant into a [`UniformTransport`], rejecting unknown values.
    ///
    /// DR-16 leaves exactly one valid value. A second one arriving means the far side was
    /// built against a header this crate does not know, which is a fault to report rather
    /// than a mode to guess at.
    #[must_use]
    pub const fn from_repr(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::ConsolidatedSsbo),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every declared kind survives a discriminant round trip, and nothing outside the set
    /// is accepted. This is the test that catches a discriminant renumbering that forgets to
    /// update `from_repr` — which would otherwise be a silent misparse of the ring.
    #[test]
    fn envelope_kind_round_trips_and_rejects_unknown() {
        for kind in EnvelopeKind::ALL {
            assert_eq!(EnvelopeKind::from_repr(kind as u16), Some(kind));
        }
        assert_eq!(EnvelopeKind::from_repr(0), None);
        assert_eq!(EnvelopeKind::from_repr(10), None);
        assert_eq!(EnvelopeKind::from_repr(u16::MAX), None);
    }

    /// Discriminants must be distinct — two kinds sharing one would make the ring ambiguous.
    #[test]
    fn envelope_kind_discriminants_are_distinct() {
        for (i, a) in EnvelopeKind::ALL.iter().enumerate() {
            for b in &EnvelopeKind::ALL[i + 1..] {
                assert_ne!(*a as u16, *b as u16, "{a:?} and {b:?} share a discriminant");
            }
        }
    }

    /// The §4 coalescing table, transcribed. Geometry and view lifecycle are lossless
    /// because dropping one loses content; the state envelopes coalesce because every one
    /// of them is an absolute write.
    #[test]
    fn coalescing_matches_the_normative_table() {
        use CoalescePolicy::{LatestWins, Lossless, RectListMerge};
        let expected = [
            (EnvelopeKind::GeometryAdd, Lossless),
            (EnvelopeKind::GeometryRemove, Lossless),
            (EnvelopeKind::ViewUse, Lossless),
            (EnvelopeKind::ViewRelease, Lossless),
            (EnvelopeKind::UboUpdate, LatestWins),
            (EnvelopeKind::TextureUpdate, RectListMerge),
            (EnvelopeKind::CameraUpdate, LatestWins),
            (EnvelopeKind::OrderUpdate, LatestWins),
            (EnvelopeKind::StencilTiles, LatestWins),
        ];
        assert_eq!(expected.len(), EnvelopeKind::ALL.len());
        for (kind, policy) in expected {
            assert_eq!(kind.coalesce_policy(), policy, "{kind:?}");
        }
    }

    /// Values transcribed from the C++ by hand, so a regeneration that silently renumbers
    /// something fails here rather than downstream. These are the ones with consequences:
    /// `Invalid` is a sentinel at the top of the range rather than the next value in
    /// sequence, and the component-count ordering is what a binding stride depends on.
    #[test]
    fn attribute_data_type_matches_mbgl() {
        assert_eq!(AttributeDataType::Byte as u8, 0);
        assert_eq!(AttributeDataType::Float as u8, 25);
        assert_eq!(AttributeDataType::Float4 as u8, 28);
        assert_eq!(AttributeDataType::Invalid as u8, 255);
        assert_eq!(AttributeDataType::ALL.len(), 30);

        assert_eq!(
            AttributeDataType::from_repr(255),
            Some(AttributeDataType::Invalid)
        );
        // The gap between the last real type (Float4 = 28) and the sentinel must not be
        // accepted: mbgl leaves 29..=254 undefined and a value landing there is corruption.
        assert_eq!(AttributeDataType::from_repr(29), None);
        assert_eq!(AttributeDataType::from_repr(254), None);
    }

    #[test]
    fn texture_pixel_type_matches_mbgl() {
        assert_eq!(TexturePixelType::RGBA as u8, 0);
        assert_eq!(TexturePixelType::Alpha as u8, 1);
        assert_eq!(TexturePixelType::ALL.len(), 5);
        assert_eq!(TexturePixelType::from_repr(5), None);
    }

    /// `BuiltIn` has no underlying type in the C++, so it is `int` and the mirror is `i32`.
    /// `None` at zero is load-bearing: it is the default in `DrawableAdd`.
    #[test]
    fn builtin_shader_matches_mbgl() {
        assert_eq!(BuiltIn::None as i32, 0);
        assert_eq!(BuiltIn::BackgroundShader as i32, 3);
        assert_eq!(BuiltIn::from_repr(-1), None);
        assert_eq!(
            BuiltIn::from_repr(BuiltIn::ALL.len() as i32),
            None,
            "one past the last shader must not resolve"
        );
        for shader in BuiltIn::ALL {
            assert_eq!(BuiltIn::from_repr(shader as i32), Some(shader));
        }
    }

    /// RenderPass is a mask, so the test that matters is that combinations round-trip and
    /// undefined bits do not. Masking an unknown bit off silently would let a consumer draw a
    /// pass it does not understand into a pass it does.
    #[test]
    fn render_pass_is_a_mask_that_rejects_undefined_bits() {
        assert_eq!(RenderPass::NONE.bits(), 0);
        assert_eq!(RenderPass::OPAQUE.bits(), 1);
        assert_eq!(RenderPass::TRANSLUCENT.bits(), 2);
        assert_eq!(RenderPass::PASS3D.bits(), 4);
        assert_eq!(RenderPass::VALID_BITS, 0b111);

        let both = RenderPass::OPAQUE | RenderPass::TRANSLUCENT;
        assert_eq!(RenderPass::from_bits(both.bits()), Some(both));
        assert!(both.contains(RenderPass::OPAQUE));
        assert!(!both.contains(RenderPass::PASS3D));
        assert!(
            both.contains(RenderPass::NONE),
            "the empty mask is a subset of everything"
        );

        assert_eq!(RenderPass::from_bits(0b1000), None);
        assert_eq!(RenderPass::from_bits(u8::MAX), None);
    }

    #[test]
    fn mode_enums_reject_unknown_discriminants() {
        assert_eq!(CameraMode::from_repr(0), Some(CameraMode::Producer));
        assert_eq!(CameraMode::from_repr(1), Some(CameraMode::Consumer));
        assert_eq!(CameraMode::from_repr(2), None);

        assert_eq!(
            UniformTransport::from_repr(0),
            Some(UniformTransport::ConsolidatedSsbo)
        );
        assert_eq!(UniformTransport::from_repr(1), None);
    }
}
