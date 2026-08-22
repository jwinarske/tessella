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
//! Status: scaffold. No implementation yet.

// Not `forbid(unsafe_code)`: the ring and the `#[repr(C)]` envelope mirrors need it. Every
// other crate in the workspace forbids it outright.
#![deny(unsafe_op_in_unsafe_fn)]
#![no_std]

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Absolute uniform write, keyed `(viewId or shared, layerIndex/ownerId, slot)`.
    /// Latest-wins is exact because the writes are absolute (§4).
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
