//! Rev 2 envelope records.
//!
//! Every record here is flat, `#[repr(C)]`, and free of pointers. Variable-length data is
//! addressed two ways, and which one applies is a property of the data rather than a choice:
//!
//! - **Bulk geometry** — vertex and index bytes — lives in refcounted slabs and is referenced
//!   by [`SlabRef`] (§2.1). These are the bytes the driver uploads, so they are touched
//!   exactly once after layout and never copied at the seam (§11.3).
//! - **Everything else** — attribute descriptors, segment tables, texture refs, order entries,
//!   stencil tiles, UBO bytes, texture pixels — rides inline in the ring, addressed by [`Span`]
//!   offsets into the payload region that follows the fixed record.
//!
//! The split is forced, not stylistic. Latest-wins coalescing (§4) means a superseded envelope
//! is dropped where it sits, and that is only sound if its payload is self-contained in the
//! ring slot — an envelope holding slab references would need its refcounts unwound on drop, in
//! the producer, for a record the consumer never saw. So every coalescable envelope is inline
//! by construction. The lossless geometry envelopes could have gone either way, and go to slabs
//! because that is where the zero-copy upload is; their metadata stays inline because a segment
//! table is parsed at drain and discarded, which refcounting would only make more expensive.
//!
//! Sizes and alignments are asserted at the bottom of this file. They are what the generated C
//! header mirrors, and what R-6 checks on every target.

use crate::{AttributeDataType, BuiltIn, CameraMode, RenderPass, TexturePixelType};

/// Identifies a view — one map instance's camera, cover, and draw order.
///
/// Rev 1 called this `MapID` and put it on everything, because every `Map` owned its own
/// everything. Under §5.3 it survives only on view-scoped envelopes; geometry, textures and
/// atlases are process-scoped and carry no view at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ViewId(pub u32);

/// Identifies a piece of process-scoped, refcounted geometry (§5.3).
///
/// Unlike rev 1's drawable id, this is unique process-wide rather than per-map, which is what
/// lets several views reference one vertex buffer instead of each building their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct GeometryId(pub u64);

/// Identifies a process-scoped texture: a glyph atlas, a sprite atlas, or a tile image.
///
/// One id per unique content, and one consumer-side GPU texture per id no matter how many
/// views draw with it (§5.5). Rev 1's `contentHash` existed so a consumer could dedup these
/// after the fact; shared ownership retires it (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct TextureId(pub u64);

/// Identifies a refcounted slab of geometry bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct SlabId(pub u32);

/// Generation counter for a view's draw order (§6.3).
///
/// A [`CameraUpdate`] names the order it was computed against, and a consumer must not apply
/// one whose epoch it does not yet hold. That rule is what keeps the split of rev 1's
/// `FrameOrder` from showing up as one-frame flicker under churn (R-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct OrderEpoch(pub u64);

/// A tile address: canonical z/x/y plus the overscale and wrap that place it.
///
/// Field order differs from mbgl's `OverscaledTileID`, which nests a `CanonicalTileID` and
/// pads. Rev 2 is our protocol rather than a mirror of theirs, so the fields are ordered to
/// pack without holes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct TileId {
    /// Canonical x.
    pub x: u32,
    /// Canonical y.
    pub y: u32,
    /// Canonical z.
    pub z: u8,
    /// Zoom the tile is displayed at, which exceeds `z` when a tile is overscaled.
    pub overscaled_z: u8,
    /// World copy, for a map wrapped past the antimeridian.
    pub wrap: i16,
}

/// A rectangle in texture space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Rect16 {
    /// Left edge.
    pub x: u16,
    /// Top edge.
    pub y: u16,
    /// Width.
    pub w: u16,
    /// Height.
    pub h: u16,
}

/// Pixel dimensions of a texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Extent {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A run of elements in the payload region following an envelope's fixed record.
///
/// `offset` is in bytes from the start of that region; `count` is in elements, not bytes, so
/// the element type is implied by the field the span sits in. Nothing outside the ring slot is
/// addressable this way, which is what keeps coalescing sound and what keeps the §3.5
/// process-isolation option open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Span {
    /// Byte offset into the envelope's payload region.
    pub offset: u32,
    /// Number of elements.
    pub count: u32,
}

/// Required alignment of an envelope's payload region.
///
/// Span-addressed element types are read in place out of that region, and the widest field any
/// of them contains is 8 bytes ([`GeometryId`] in [`OrderEntry`], [`TextureId`] in
/// [`TextureRef`]). The producer aligns the region to this and the assertion block below proves
/// no element type ever needs more, so an in-place read is always aligned — including on
/// riscv64, where an unaligned read is a fault rather than a slowdown (R-6).
pub const PAYLOAD_ALIGN: usize = 8;

impl Span {
    /// Resolves this span to a byte range within a payload region of `payload_len` bytes,
    /// returning `None` if it does not fit.
    ///
    /// Span offsets and counts arrive from the far side of the ABI and are untrusted for the
    /// same reasons discriminants are: version skew, a torn ring read, or a mirror built
    /// against a different header. Walking `offset + count * size_of::<T>()` without checking
    /// is an out-of-bounds read, so this does the arithmetic with overflow checks and the
    /// bound comparison in one place rather than leaving each call site to remember.
    ///
    /// Returns a half-open `(start, end)` byte range.
    #[must_use]
    pub fn extent<T>(self, payload_len: usize) -> Option<(usize, usize)> {
        let start = self.offset as usize;
        let len = (self.count as usize).checked_mul(size_of::<T>())?;
        let end = start.checked_add(len)?;
        (end <= payload_len).then_some((start, end))
    }
}

/// A reference into a refcounted geometry slab.
///
/// The consumer holds the slab alive until the driver's copy completes — for Filament, until
/// the `BufferDescriptor` release callback fires (§11.3). An in-process Rust consumer holds
/// the slab's `Arc` directly and the copy degenerates to a refcount bump (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct SlabRef {
    /// Slab this data lives in.
    pub slab: u32,
    /// Byte offset within the slab.
    pub offset: u32,
    /// Length in bytes.
    pub length: u32,
}

/// Why a piece of geometry was announced.
///
/// Carried from rev 1 unchanged, because §6.1 makes it a diagnostic rather than a hint: the
/// whole damage premise is that geometry churn tracks tile churn rather than frame rate, so a
/// steady stream of [`AddReason::AttributesModified`] on a static scene is a visible bug, and
/// §9.3 asserts it at zero in CI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AddReason {
    /// First announcement for this geometry id.
    Created = 0,
    /// Index data was replaced.
    IndexDataReplaced = 1,
    /// Vertex attributes were replaced.
    AttributesReplaced = 2,
    /// Same arrays, values rewritten in place. Should not appear on a static scene.
    AttributesModified = 3,
}

impl AddReason {
    /// Every reason, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Created,
        Self::IndexDataReplaced,
        Self::AttributesReplaced,
        Self::AttributesModified,
    ];

    /// Converts a wire value into an [`AddReason`], rejecting anything unrecognized.
    #[must_use]
    pub const fn from_repr(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Created),
            1 => Some(Self::IndexDataReplaced),
            2 => Some(Self::AttributesReplaced),
            3 => Some(Self::AttributesModified),
            _ => None,
        }
    }
}

/// Per-drawable render state, as a mask.
///
/// Rev 1 carried these as four separate bools on `DrawableAdd`. They travel with
/// [`ViewUse::render_pass`] because they are the same kind of fact — how this geometry draws in
/// this view's order — and splitting related draw state across two envelopes would cost a join
/// the consumer has no reason to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct DrawFlags(u8);

impl DrawFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);
    /// Geometry is 3D and participates in the 3D pass.
    pub const IS_3D: Self = Self(1 << 0);
    /// Draw is stencil-tested against the layer's clip set (§2.2).
    pub const ENABLE_STENCIL: Self = Self(1 << 1);
    /// Draw participates in depth testing.
    pub const ENABLE_DEPTH: Self = Self(1 << 2);
    /// Draw writes color. Cleared for depth- or stencil-only passes.
    pub const ENABLE_COLOR: Self = Self(1 << 3);

    /// Every bit this protocol defines.
    pub const VALID_BITS: u8 = 0b1111;

    /// Converts a wire value into [`DrawFlags`], rejecting undefined bits.
    #[must_use]
    pub const fn from_bits(value: u8) -> Option<Self> {
        if value & !Self::VALID_BITS == 0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for DrawFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// One vertex attribute, as a binding rather than as bytes.
///
/// Rev 1's `sharedVector`/`rawData` pair collapses to a single [`SlabRef`]: under §2.1 both the
/// bucket-vector path and the background layer's owned-bytes path allocate from slabs, so the
/// consumer no longer has two lifetimes to reason about.
///
/// The two data types are not redundant and must not be merged. `data_type` is what the buffer
/// supplies; `declared_data_type` is what the shader declares for the slot. A binding uses the
/// declared type with the supplied offset and stride — binding the supplied type hands the
/// shader a narrower attribute than it reads (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct AttributeDesc {
    /// Shader-side attribute id.
    pub attr_id: u32,
    /// Binding slot the shader declares for this permutation.
    ///
    /// `-1` means the geometry supplied an override the shader does not declare, and the
    /// consumer must drop it, exactly as the mbgl backends do in `buildAttributeBindings`.
    /// The LineShader floor-width override is the case that exists in practice (§2.2).
    pub binding: i32,
    /// Where the bytes are.
    pub source: SlabRef,
    /// Byte offset of this attribute within a vertex.
    pub offset: u32,
    /// First vertex, for a binding that does not start at the buffer's start.
    pub vertex_offset: u32,
    /// Bytes between consecutive vertices.
    pub stride: u32,
    /// The type the buffer supplies, as an [`AttributeDataType`] discriminant.
    ///
    /// Raw rather than the enum, because a record is reconstructed from untrusted bytes and an
    /// out-of-range discriminant in an enum field is undefined behavior. Read it through
    /// [`AttributeDesc::data_type`].
    pub data_type: u8,
    /// The type the shader declares, as an [`AttributeDataType`] discriminant. Bind this one.
    pub declared_data_type: u8,
    /// Padding to a 4-byte boundary. Must be zero.
    pub _pad: [u8; 2],
}

/// One draw segment: a contiguous index range with its own vertex base.
///
/// Rev 1 used `size_t`. These are tile-bounded counts, so 32 bits is not a narrowing that can
/// bite: §12.4 puts indexes at u16 with a u32 spill per segment, and a segment that overflowed
/// a u32 vertex base would have overflowed a tile long before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct Segment {
    /// First vertex.
    pub vertex_offset: u32,
    /// First index.
    pub index_offset: u32,
    /// Vertex count.
    pub vertex_length: u32,
    /// Index count.
    pub index_length: u32,
}

/// Binds a texture to a shader slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct TextureRef {
    /// Texture to bind.
    pub texture: TextureId,
    /// Shader-side slot.
    pub slot: u32,
    /// Padding. Must be zero.
    pub _pad: u32,
}

/// Announces process-scoped, refcounted geometry (§5.3).
///
/// Carries no view, no layer, and no tile: those are per-view facts and live on [`ViewUse`].
/// One of these plus N `ViewUse` records is what replaces rev 1's N copies of `DrawableAdd`,
/// and is why upload bandwidth scales with unique tiles rather than with view count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct GeometryAdd {
    /// Process-wide geometry id.
    pub geometry: GeometryId,
    /// Distinguishes the data-driven-attribute variants of one shader family.
    pub permutation_key: u64,
    /// Index buffer.
    pub indexes: SlabRef,
    /// Number of vertices.
    pub vertex_count: u32,
    /// [`AttributeDesc`] run in the payload region.
    pub attrs: Span,
    /// [`AttributeDesc`] run for instanced attributes.
    pub instance_attrs: Span,
    /// [`Segment`] run.
    pub segments: Span,
    /// [`TextureRef`] run.
    pub texture_refs: Span,
    /// Shader family, as a [`BuiltIn`] discriminant.
    pub builtin_shader: i32,
    /// Vertex type, as an [`AttributeDataType`] discriminant.
    pub vertex_type: u8,
    /// Why this was announced, as an [`AddReason`] discriminant. Watch
    /// [`AddReason::AttributesModified`] (§6.1).
    pub reason: u8,
    /// Padding. Must be zero.
    pub _pad: [u8; 2],
}

/// Drops a piece of shared geometry, once no view holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct GeometryRemove {
    /// Geometry to drop.
    pub geometry: GeometryId,
}

/// Declares a view and its configuration (§5.3, DR-18).
///
/// Ordered ahead of any [`ViewUse`] naming the view, and re-emitted when the configuration
/// changes rather than being repeated per use.
///
/// DR-9 originally hung `camera_mode` off `ViewUse`, which is per (view, geometry) while the
/// mode is per view. That meant the mode was repeated on every use and every copy had to
/// agree, with no principled response available to a consumer that saw disagreement: it cannot
/// know which copy is current, and treating a later one as a mode change would swap the
/// world-space convention mid-frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ViewDeclare {
    /// View being declared.
    pub view: ViewId,
    /// Which side owns this view's camera, as a [`CameraMode`] discriminant (DR-9).
    pub camera_mode: u8,
    /// Padding, reserved. Must be zero.
    ///
    /// This is where the §5.4 per-view `maxzoom` clamp and view class will go — a cluster
    /// inset capped at z14 never joins a z16 crossing burst, and the decode pool orders
    /// foreground views ahead of background ones. Reserving the space now is why neither
    /// needs an envelope added after the ABI freezes at R0 exit.
    pub _reserved: [u8; 3],
}

/// Drops a view and everything scoped to it (§5.3, DR-18).
///
/// The consumer releases the view's scene, uniform buffers, stencil sets and reverse-channel
/// slot. Geometry the view was using is not dropped with it — that is refcounted and
/// process-scoped, and other views may still hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ViewUndeclare {
    /// View being dropped.
    pub view: ViewId,
}

/// Binds shared geometry into one view's draw order (§5.3).
///
/// Carries nothing about the view itself — that is [`ViewDeclare`]'s job (DR-18). A `ViewUse`
/// naming a view the consumer has not seen declared is a protocol fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ViewUse {
    /// Geometry being used.
    pub geometry: GeometryId,
    /// View using it.
    pub view: ViewId,
    /// Layer group this draw belongs to. Pairs with [`UboUpdate::layer_index`].
    pub layer_index: i32,
    /// Order within the layer.
    pub sub_layer_index: i32,
    /// Tile this geometry covers, meaningful only when `has_tile` is set.
    pub tile: TileId,
    /// Pass or passes this draw participates in.
    pub render_pass: RenderPass,
    /// Render state.
    pub draw_flags: DrawFlags,
    /// Non-zero when `tile` is meaningful. Rev 1's `tileID` was optional.
    pub has_tile: u8,
    /// Padding. Must be zero.
    pub _pad: u8,
}

/// Releases one view's use of shared geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct ViewRelease {
    /// Geometry being released.
    pub geometry: GeometryId,
    /// View releasing it.
    pub view: ViewId,
    /// Padding. Must be zero.
    pub _pad: u32,
}

/// An absolute write to a uniform buffer.
///
/// DR-16 leaves one transport: a consolidated buffer per `(view, layer)` that drawables index
/// through [`OrderEntry::ubo_index`]. Rev 1's per-drawable buffers are gone, and with them
/// rev 1's `ownerId` — there is no per-drawable uniform path to name.
///
/// Writes are absolute, never deltas, which is the reason latest-wins coalescing is exact
/// rather than approximate (§4). The producer suppresses byte-identical rewrites at the source
/// (§6.1), so an envelope reaching the ring means something actually changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct UboUpdate {
    /// View this buffer belongs to.
    pub view: ViewId,
    /// Layer group, or `-1` for the frame-wide buffers the renderer owns rather than any
    /// layer — `GlobalPaintParamsUBO` and friends. Layers that size geometry in screen space
    /// rather than tile space cannot be reconstructed without those (R-2).
    pub layer_index: i32,
    /// Buffer slot.
    pub slot: u32,
    /// Padding. Must be zero.
    pub _pad: u32,
    /// Buffer bytes, inline in the payload region. `count` is a byte count here.
    pub data: Span,
}

/// Maximum dirty rects carried before spilling to their union (§6.4).
pub const TEXTURE_RECT_CAP: usize = 4;

/// New pixels for a process-scoped texture.
///
/// Carries no view: one texture serves every view that draws with it (§6.6).
///
/// The rect list replaces rev 1's single optional rect. A union over two small updates in
/// opposite atlas corners uploads the whole atlas; a list of four does not. The shelf allocator
/// on the producer side keeps insertions clustered so the list rarely spills (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct TextureUpdate {
    /// Texture being written.
    pub texture: TextureId,
    /// Full texture dimensions.
    pub size: Extent,
    /// Dirty regions. Only the first `rect_count` are meaningful.
    pub rects: [Rect16; TEXTURE_RECT_CAP],
    /// Pixel bytes for the dirty regions, inline in the payload region. `count` is a byte
    /// count here.
    pub pixels: Span,
    /// Pixel format, as a [`TexturePixelType`] discriminant.
    pub format: u8,
    /// Number of meaningful entries in `rects`. Zero means a whole-texture upload.
    pub rect_count: u8,
    /// Padding. Must be zero.
    pub _pad: [u8; 2],
}

/// One tile of a clip set: which tile, and the matrix that places its mask quad.
///
/// The matrix must travel with the tile because the consumer draws the quad itself and has
/// nothing else to derive it from — in particular not a content drawable's own matrix, which
/// carries the layer's translate on top of the tile transform and would put the mask where the
/// content is not (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct StencilTile {
    /// Column-major tile-to-clip.
    pub matrix: [f32; 16],
    /// Tile this mask covers.
    pub tile: TileId,
}

/// The tile set a layer group wants clipped, emitted on change only.
///
/// Stencil reference values are deliberately absent. mbgl assigns them from a running counter
/// it resets on overflow, which is bookkeeping about a stencil buffer the producer does not
/// own. The consumer assigns its own and keys them by tile (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct StencilTiles {
    /// View this clip set belongs to.
    pub view: ViewId,
    /// Layer group being clipped.
    pub layer_index: i32,
    /// [`StencilTile`] run in the payload region.
    pub tiles: Span,
}

/// One geometry's position in a view's painter order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct OrderEntry {
    /// Geometry being drawn.
    pub geometry: GeometryId,
    /// Sort key within the layer.
    pub draw_priority: i64,
    /// Layer group.
    pub layer_index: u32,
    /// Order within the layer.
    pub sub_layer_index: i32,
    /// Slot in the layer's consolidated buffer.
    ///
    /// Assigned per pass from the view's own draw order, so it belongs to the order rather
    /// than to [`GeometryAdd`] — which is also why sharing geometry across views does not
    /// share this.
    pub ubo_index: u32,
    /// Pass this entry draws in.
    pub pass: RenderPass,
    /// Padding. Must be zero.
    pub _pad: [u8; 3],
}

/// A view's draw order, emitted only when it differs from the last one emitted (§6.3).
///
/// Rev 1 emitted this every frame as part of `FrameOrder`, byte-identical or not, and it was
/// the largest per-frame payload on the protocol. Splitting it from the camera is what drops
/// steady-state pan traffic from roughly 100 KB per frame to hundreds of bytes: a pure pan
/// reorders nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct OrderUpdate {
    /// Epoch this order establishes. Referenced by [`CameraUpdate::order_epoch`].
    pub order_epoch: OrderEpoch,
    /// View this order belongs to.
    pub view: ViewId,
    /// Padding. Must be zero.
    pub _pad: u32,
    /// [`OrderEntry`] run in the payload region, in draw order.
    pub entries: Span,
}

/// The style's light — the sun.
///
/// mbgl uses this only for fill-extrusion. It is carried anyway because a consumer placing the
/// map in a world alongside its own 3D content needs both lit by the same sun; a model shaded
/// by a light pointing somewhere else than the style's is the giveaway that it was pasted on.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct Light {
    /// Direction towards the light, cartesian.
    pub direction: [f64; 3],
    /// Light color, RGBA.
    pub color: [f64; 4],
    /// Light intensity.
    pub intensity: f64,
    /// Non-zero when the light is anchored to the map and rotates with it; zero when it is
    /// anchored to the viewport and stays put as the map turns. mbgl's default is viewport.
    pub anchored_to_map: u8,
    /// Padding. Must be zero.
    pub _pad: [u8; 7],
}

/// A view's camera and frame-wide parameters, emitted only when a field changes (§6.3).
///
/// Which fields are authoritative depends on the view's [`CameraMode`] (DR-9):
///
/// - **Producer mode** — every field is authoritative. `proj_matrix` is the fused world-to-clip
///   and the consumer's own camera contributes nothing.
/// - **Consumer mode** — the consumer's camera is authoritative and `proj_matrix`,
///   `center_zoom0`, `bearing` and `pitch` are advisory: the producer computed them from a
///   camera it read back one frame stale over the reverse channel. The remaining fields still
///   are authoritative, because the producer is the only side that knows them.
///
/// `center_zoom0` is the map center at zoom zero, so 0..512 regardless of the map's zoom, and
/// it is scale-free on purpose. Sending it pre-multiplied by a frame's zoom scale couples it to
/// that frame: a consumer placing tiles from a slightly different zoom disagreed about scale by
/// over a million units at zoom 17, so the camera looked where the tiles were not and frames
/// came back empty — visible only while zooming, as flicker. Named regression test in §9.1.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct CameraUpdate {
    /// World-to-clip, column-major, f64 because the world coordinates it multiplies are large
    /// enough at high zoom that the precision matters.
    pub proj_matrix: [f64; 16],
    /// Map center at zoom zero. Scale-free.
    pub center_zoom0: [f64; 2],
    /// Bearing in degrees.
    pub bearing: f64,
    /// Pitch in degrees.
    pub pitch: f64,
    /// World pixels per meter at this zoom and latitude.
    ///
    /// A consumer owning the camera needs this and cannot get it from the projection: mbgl's
    /// matrix is not isotropic, because heights arrive in meters while x and y are in world
    /// pixels. Leave it out and buildings come out too tall by its reciprocal.
    pub pixels_per_meter: f64,
    /// Style light.
    pub light: Light,
    /// Frame this camera belongs to.
    pub frame_no: u64,
    /// Order this camera was computed against. A consumer must hold this epoch before
    /// applying the camera, or hold the camera until the order arrives (§4).
    pub order_epoch: OrderEpoch,
    /// View this camera belongs to.
    pub view: ViewId,
    /// Index into the draw order at which the opaque pass ends.
    pub opaque_pass_cutoff: u32,
    /// Depth range.
    pub depth_range_size: f32,
    /// Padding. Must be zero.
    pub _pad: u32,
}

impl AttributeDesc {
    /// The type the buffer supplies, or `None` if the discriminant is not one this build knows.
    #[must_use]
    pub const fn data_type(&self) -> Option<AttributeDataType> {
        AttributeDataType::from_repr(self.data_type)
    }

    /// The type the shader declares, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn declared_data_type(&self) -> Option<AttributeDataType> {
        AttributeDataType::from_repr(self.declared_data_type)
    }
}

impl GeometryAdd {
    /// The shader family, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn builtin_shader(&self) -> Option<BuiltIn> {
        BuiltIn::from_repr(self.builtin_shader)
    }

    /// The vertex type, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn vertex_type(&self) -> Option<AttributeDataType> {
        AttributeDataType::from_repr(self.vertex_type)
    }

    /// Why this geometry was announced, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn reason(&self) -> Option<AddReason> {
        AddReason::from_repr(self.reason)
    }
}

impl ViewDeclare {
    /// Which side owns this view's camera, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn camera_mode(&self) -> Option<CameraMode> {
        CameraMode::from_repr(self.camera_mode)
    }
}

impl TextureUpdate {
    /// The pixel format, or `None` if the discriminant is unrecognized.
    #[must_use]
    pub const fn format(&self) -> Option<TexturePixelType> {
        TexturePixelType::from_repr(self.format)
    }
}

/// An envelope record whose `#[repr(C)]` bytes are its wire form.
///
/// The bytes a record occupies on the ring are its in-memory bytes: that is the whole premise
/// of a flat ABI, and it is what the generated C header's `sizeof` and `offsetof` assertions
/// pin down. So the byte view lives here, in the crate that owns the layout, rather than in
/// whichever crate happens to need it — a producer serializing field by field would duplicate
/// the layout knowledge and could drift from the header without anything noticing.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]`, declare every padding byte as an explicit field, hold no
/// pointers, and — for [`WireRecord::from_bytes`] to be sound — contain no field with invalid
/// bit patterns. That last requirement is why no record here stores an enum: an out-of-range
/// discriminant in an enum field is undefined behavior, not a wrong value, so records carry raw
/// discriminants and expose them through accessors that go via `from_repr`.
///
/// Getting that backwards is easy and was in fact got backwards here first: the ingress rule
/// was written down, the validating constructors were built, and then the records were given
/// enum fields that could only be read by bypassing them. It surfaced the moment something
/// tried to read a record back off the ring.
pub unsafe trait WireRecord: Copy {
    /// This record as the bytes the ring carries.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: the trait's contract requires `#[repr(C)]` with all padding written, so
        // every byte of the value is initialized and reading them is defined.
        unsafe {
            core::slice::from_raw_parts(core::ptr::from_ref(self).cast::<u8>(), size_of::<Self>())
        }
    }

    /// Reads a record out of bytes taken off the ring.
    ///
    /// `None` when there are not enough bytes. Nothing else can fail, because the trait's
    /// contract confines implementors to fields where every bit pattern is a legal value — the
    /// enum discriminants a record carries are raw integers here, and turning one into an enum
    /// is a separate, checked step.
    ///
    /// The read is unaligned: a record sits wherever the ring's framing put it, which is
    /// 16-byte aligned in practice but not something this needs to assume.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < size_of::<Self>() {
            return None;
        }
        // SAFETY: the length is checked above, and the trait's contract makes every bit
        // pattern of `Self` a valid value, so an unaligned read of those bytes is defined.
        Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<Self>()) })
    }
}

macro_rules! wire_records {
    ($($record:ty),* $(,)?) => {
        $(
            // SAFETY: each is `#[repr(C)]` with explicit padding fields, asserted below.
            unsafe impl WireRecord for $record {}
        )*
    };
}

wire_records!(
    GeometryAdd,
    GeometryRemove,
    ViewDeclare,
    ViewUndeclare,
    ViewUse,
    ViewRelease,
    UboUpdate,
    TextureUpdate,
    StencilTiles,
    StencilTile,
    OrderEntry,
    OrderUpdate,
    CameraUpdate,
    AttributeDesc,
    Segment,
    TextureRef,
);

const _: () = {
    // Layout is protocol. These fire at compile time, on every target, which is how R-6
    // (riscv64 alignment and atomics) is caught in a build rather than in a soak. The C header
    // is generated from these same definitions and carries the mirror of each assertion.
    macro_rules! layout {
        ($t:ty, $size:expr, $align:expr) => {
            assert!(core::mem::size_of::<$t>() == $size);
            assert!(core::mem::align_of::<$t>() == $align);
        };
    }

    layout!(ViewId, 4, 4);
    layout!(GeometryId, 8, 8);
    layout!(TextureId, 8, 8);
    layout!(SlabId, 4, 4);
    layout!(OrderEpoch, 8, 8);
    layout!(TileId, 12, 4);
    layout!(Rect16, 8, 2);
    layout!(Extent, 8, 4);
    layout!(Span, 8, 4);
    layout!(SlabRef, 12, 4);
    layout!(AddReason, 1, 1);
    layout!(DrawFlags, 1, 1);
    layout!(AttributeDesc, 36, 4);
    layout!(Segment, 16, 4);
    layout!(TextureRef, 16, 8);
    layout!(GeometryAdd, 72, 8);
    layout!(GeometryRemove, 8, 8);
    layout!(ViewDeclare, 8, 4);
    layout!(ViewUndeclare, 4, 4);
    layout!(ViewUse, 40, 8);
    layout!(ViewRelease, 16, 8);
    layout!(UboUpdate, 24, 4);
    layout!(TextureUpdate, 64, 8);
    layout!(StencilTile, 76, 4);
    layout!(StencilTiles, 16, 4);
    layout!(OrderEntry, 32, 8);
    layout!(OrderUpdate, 24, 8);
    layout!(Light, 72, 8);
    layout!(CameraUpdate, 272, 8);

    // Nothing read in place out of the payload region may need more alignment than the
    // producer gives that region.
    assert!(core::mem::align_of::<AttributeDesc>() <= PAYLOAD_ALIGN);
    assert!(core::mem::align_of::<Segment>() <= PAYLOAD_ALIGN);
    assert!(core::mem::align_of::<TextureRef>() <= PAYLOAD_ALIGN);
    assert!(core::mem::align_of::<OrderEntry>() <= PAYLOAD_ALIGN);
    assert!(core::mem::align_of::<StencilTile>() <= PAYLOAD_ALIGN);
};

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// Field *order* is protocol just as much as size is, and the const block above cannot see
    /// it: swapping two same-sized fields keeps every size and alignment identical while
    /// silently reinterpreting the stream. These pin the offsets the generated C header
    /// mirrors, for the records where a swap would be both easy and quiet.
    #[test]
    fn field_offsets_are_pinned() {
        assert_eq!(offset_of!(AttributeDesc, attr_id), 0);
        assert_eq!(offset_of!(AttributeDesc, binding), 4);
        assert_eq!(offset_of!(AttributeDesc, source), 8);
        assert_eq!(offset_of!(AttributeDesc, offset), 20);
        assert_eq!(offset_of!(AttributeDesc, vertex_offset), 24);
        assert_eq!(offset_of!(AttributeDesc, stride), 28);
        assert_eq!(offset_of!(AttributeDesc, data_type), 32);
        assert_eq!(offset_of!(AttributeDesc, declared_data_type), 33);

        // The two spans here are adjacent and identically typed — exactly the pair a careless
        // edit reorders.
        assert_eq!(offset_of!(GeometryAdd, attrs), 32);
        assert_eq!(offset_of!(GeometryAdd, instance_attrs), 40);
        assert_eq!(offset_of!(GeometryAdd, segments), 48);
        assert_eq!(offset_of!(GeometryAdd, texture_refs), 56);

        assert_eq!(offset_of!(ViewUse, geometry), 0);
        assert_eq!(offset_of!(ViewUse, view), 8);
        assert_eq!(offset_of!(ViewUse, layer_index), 12);
        assert_eq!(offset_of!(ViewUse, sub_layer_index), 16);

        assert_eq!(offset_of!(CameraUpdate, proj_matrix), 0);
        assert_eq!(offset_of!(CameraUpdate, center_zoom0), 128);
        assert_eq!(offset_of!(CameraUpdate, bearing), 144);
        assert_eq!(offset_of!(CameraUpdate, pitch), 152);
        assert_eq!(offset_of!(CameraUpdate, pixels_per_meter), 160);
    }

    /// The reserved bytes are the whole reason DR-18 lands before the freeze rather than
    /// after: the §5.4 maxzoom clamp and view class go here without a new envelope. Pinning
    /// the size means an addition that overruns the reservation is a failing test rather
    /// than a silent ABI break.
    #[test]
    fn view_declare_reserves_room_for_per_view_configuration() {
        assert_eq!(size_of::<ViewDeclare>(), 8);
        assert_eq!(offset_of!(ViewDeclare, view), 0);
        assert_eq!(offset_of!(ViewDeclare, camera_mode), 4);
        assert_eq!(offset_of!(ViewDeclare, _reserved), 5);
        assert_eq!(size_of::<[u8; 3]>(), 3, "three bytes reserved");
    }

    /// Camera mode is per view, so it must not be reachable from a per-use record. If this
    /// ever compiles again, DR-18 has been undone.
    #[test]
    fn view_use_carries_no_per_view_state() {
        assert_eq!(size_of::<ViewUse>(), 40);
        assert_eq!(offset_of!(ViewUse, has_tile), 34);
        assert_eq!(offset_of!(ViewUse, _pad), 35);
    }

    #[test]
    fn add_reason_round_trips_and_rejects_unknown() {
        for reason in AddReason::ALL {
            assert_eq!(AddReason::from_repr(reason as u8), Some(reason));
        }
        assert_eq!(AddReason::from_repr(4), None);
        assert_eq!(AddReason::from_repr(u8::MAX), None);
    }

    #[test]
    fn draw_flags_reject_undefined_bits() {
        let opaque_quad = DrawFlags::ENABLE_DEPTH | DrawFlags::ENABLE_COLOR;
        assert_eq!(DrawFlags::from_bits(opaque_quad.bits()), Some(opaque_quad));
        assert!(opaque_quad.contains(DrawFlags::ENABLE_COLOR));
        assert!(!opaque_quad.contains(DrawFlags::IS_3D));

        assert_eq!(DrawFlags::from_bits(0b1_0000), None);
        assert_eq!(DrawFlags::from_bits(u8::MAX), None);
    }

    /// A depth- or stencil-only draw clears ENABLE_COLOR, so an empty mask has to be a legal
    /// value rather than an "unset" sentinel.
    #[test]
    fn empty_draw_flags_are_legal() {
        assert_eq!(DrawFlags::from_bits(0), Some(DrawFlags::NONE));
        assert_eq!(DrawFlags::NONE, DrawFlags::default());
    }

    #[test]
    fn texture_rect_cap_matches_the_array() {
        let update_rects_len =
            core::mem::size_of::<[Rect16; TEXTURE_RECT_CAP]>() / core::mem::size_of::<Rect16>();
        assert_eq!(update_rects_len, TEXTURE_RECT_CAP);
        assert_eq!(TEXTURE_RECT_CAP, 4, "§6.4 sets the spill threshold at four");
    }

    /// Every span-addressed element type must have a size that is its own stride, since the
    /// payload region is walked by `offset + index * size_of::<T>()`. A type whose alignment
    /// exceeded its size would break that walk.
    #[test]
    fn payload_element_types_are_tightly_strided() {
        fn check<T>(name: &str) {
            let size = core::mem::size_of::<T>();
            let align = core::mem::align_of::<T>();
            assert!(size > 0, "{name} is zero-sized");
            assert_eq!(
                size % align,
                0,
                "{name} stride {size} is not a multiple of {align}"
            );
        }
        check::<AttributeDesc>("AttributeDesc");
        check::<Segment>("Segment");
        check::<TextureRef>("TextureRef");
        check::<OrderEntry>("OrderEntry");
        check::<StencilTile>("StencilTile");
    }
}

#[cfg(test)]
mod span_tests {
    use super::*;

    #[test]
    fn extent_resolves_within_bounds() {
        let span = Span {
            offset: 16,
            count: 3,
        };
        let size = size_of::<Segment>();
        assert_eq!(
            span.extent::<Segment>(16 + 3 * size),
            Some((16, 16 + 3 * size))
        );
        // Exactly filling the region is legal; one byte short is not.
        assert_eq!(span.extent::<Segment>(16 + 3 * size - 1), None);
    }

    #[test]
    fn extent_rejects_a_span_past_the_end() {
        let span = Span {
            offset: 0,
            count: 2,
        };
        assert_eq!(span.extent::<OrderEntry>(size_of::<OrderEntry>()), None);
    }

    /// A span far larger than the region is rejected on its bound rather than on its
    /// arithmetic.
    ///
    /// The `checked_mul`/`checked_add` in `extent` cannot fail on a 64-bit target: the fields
    /// are `u32`, so the widest product any element type can produce still fits a `usize`
    /// comfortably. They are load-bearing on 32-bit targets, where `usize` is the same width
    /// as the fields and `count * size_of::<T>()` wraps — armv7 and riscv32 are exactly the
    /// class of target this producer is meant to run on, so the guards stay.
    #[test]
    fn extent_rejects_spans_larger_than_the_region() {
        let huge = Span {
            offset: 0,
            count: u32::MAX,
        };
        assert_eq!(huge.extent::<CameraUpdate>(4096), None);

        let far = Span {
            offset: u32::MAX,
            count: 1,
        };
        assert_eq!(far.extent::<OrderEntry>(4096), None);
    }

    /// An empty span is legal and resolves to an empty range — a geometry with no instanced
    /// attributes, a layer with nothing clipped.
    #[test]
    fn empty_spans_resolve_empty() {
        let empty = Span {
            offset: 0,
            count: 0,
        };
        assert_eq!(empty.extent::<AttributeDesc>(0), Some((0, 0)));
    }
}
