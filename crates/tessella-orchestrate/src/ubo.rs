//! Packing uniform buffers, and emitting them as `UboUpdate` (§6.3, DR-16).
//!
//! # One consolidated buffer per (view, layer), indexed rather than bound
//!
//! DR-16 settled the transport: a consolidated buffer per (view, layer) with `uboIndex` selecting
//! an entry, no per-drawable binding and no length ceiling. That makes a layer's uniforms one
//! write per frame rather than one per drawable, which is the difference between a handful of
//! writes and a few hundred on a four-view cluster.
//!
//! # The stride is the union's, not the block's
//!
//! A layer's drawable buffer is an array of the *union* of its drawable blocks. A plain fill
//! writes an 80-byte `FillDrawableUBO` into a 96-byte slot, because the pattern variants are
//! larger and set the stride for everyone. Packing at 80 would put every entry after the first
//! at the wrong offset — a layer whose tiles are drawn with each other's matrices, which is
//! plausible-looking output no size check would catch.
//!
//! # Order does not matter here, and that is a fact about the oracle
//!
//! mbgl's iteration over a layer's tiles is not deterministic: the same style at the same camera
//! permutes the consolidated buffer between runs, because the index is assigned from that
//! iteration. The probe canonicalizes by sorting 16-byte blocks, and the diff is a multiset
//! comparison for that reason. What is a protocol property is the set of entries and their
//! contents; which slot each lands in is not, and must not be asserted as though it were.

use alloc::vec::Vec;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{Span, UboUpdate, ViewId, WireRecord};
use tessella_capture_abi::generated::ubo_layouts;
use tessella_capture_abi::generated::ubo_slots;
use tessella_capture_abi::ring::{Full, Producer};
use tessella_style::property::{Binding, Color, ResolvedProperty};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

/// The layer index frame-wide buffers travel under.
///
/// `-1`, because they belong to the renderer rather than to any layer. A consumer that keyed
/// them by a real layer would attribute the camera's own parameters to whichever layer happened
/// to be numbered zero.
pub const FRAME_WIDE: i32 = -1;

/// The frame-wide paint parameters every shader reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlobalPaintParams {
    /// Pattern atlas dimensions. `64 x 64` for the empty atlas R0 has.
    pub pattern_atlas_texsize: [f32; 2],
    /// The inverse of pixels-to-clip on each axis: half the viewport, with y negated.
    pub units_to_pixels: [f32; 2],
    /// Viewport size. Named `world_size` in the shaders, which it is not.
    pub world_size: [f32; 2],
    /// Distance from camera to map center, in world pixels.
    pub camera_to_center_distance: f32,
    /// Symbol fade progress. Zero until R2 has symbols to fade.
    pub symbol_fade_change: f32,
    /// Viewport aspect ratio.
    pub aspect_ratio: f32,
    /// Device pixel ratio.
    pub pixel_ratio: f32,
    /// Map zoom, narrowed to `f32` as the shaders take it.
    pub map_zoom: f32,
}

impl GlobalPaintParams {
    /// The parameters for a view.
    ///
    /// `camera_to_center_distance` comes from the f64 field of view, matching
    /// [`tessella_tile::camera::camera_to_center_distance`] — the projection uses the f32 one,
    /// and mixing them up moves the far plane.
    #[must_use]
    pub fn for_view(view: &ViewTransform, pattern_atlas: [f32; 2], pixel_ratio: f32) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        Self {
            pattern_atlas_texsize: pattern_atlas,
            units_to_pixels: [(view.width / 2.0) as f32, (-view.height / 2.0) as f32],
            world_size: [view.width as f32, view.height as f32],
            camera_to_center_distance: camera::camera_to_center_distance(view.height) as f32,
            symbol_fade_change: 0.0,
            aspect_ratio: (view.width / view.height) as f32,
            pixel_ratio,
            map_zoom: view.zoom as f32,
        }
    }

    /// The block's bytes, laid out as the generated layout says.
    #[must_use]
    pub fn pack(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ubo_layouts::GLOBAL_PAINT_PARAMS_UBO.size as usize);
        push_f32s(&mut out, &self.pattern_atlas_texsize);
        push_f32s(&mut out, &self.units_to_pixels);
        push_f32s(&mut out, &self.world_size);
        push_f32s(
            &mut out,
            &[self.camera_to_center_distance, self.symbol_fade_change],
        );
        push_f32s(
            &mut out,
            &[self.aspect_ratio, self.pixel_ratio, self.map_zoom, 0.0],
        );
        debug_assert_eq!(
            out.len(),
            ubo_layouts::GLOBAL_PAINT_PARAMS_UBO.size as usize
        );
        out
    }
}

/// The number of sublayers a layer's depth range is divided into. mbgl's `numSublayers`.
pub const SUBLAYERS: i32 = 3;

/// One step of the depth bias, as mbgl's `depthEpsilon` is on a non-OpenGL backend.
///
/// `1 / 2^11`, not the `1 / 2^16` the OpenGL build uses. DR-16 makes Vulkan the only backend, so
/// there is one value here rather than a choice — and it is the coarser of the two, which is why
/// the bias is visible in the matrix at all.
pub const DEPTH_EPSILON: f32 = 1.0 / 2048.0;

/// How far a drawable's depth is nudged toward the viewer.
///
/// # The same field name, two different numbers
///
/// mbgl offsets element 14 of the projection per drawable so that a layer's sublayers resolve
/// against each other in the depth buffer — a fill's outline must not z-fight with the fill it
/// outlines. The offset is `((1 + currentLayer) * numSublayers - subLayerIndex) * depthEpsilon`.
///
/// The trap is `currentLayer`. During the render passes it is a depth slot that runs *opposite*
/// the style order, which is what [`crate::order`] sorts by. During the *tweaker* pass — which
/// is where this offset is computed — the renderer walks the layer groups bottom to top counting
/// up, so it is the style index. Same field, same frame, two values, and using the render pass's
/// value here biases every layer by the wrong amount in a way that still looks like a plausible
/// depth ordering.
#[must_use]
pub fn depth_offset(layer_index: i32, sub_layer_index: i32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    {
        (((1 + layer_index) * SUBLAYERS - sub_layer_index) as f32) * DEPTH_EPSILON
    }
}

/// One drawable's entry in a layer's consolidated buffer.
///
/// The two interpolation factors are the zoom mix for the layer's data-driven properties —
/// `color_t` and `opacity_t` for a fill, `outline_color_t` and `opacity_t` for its outline. They
/// are zero for a property that does not vary with zoom, which is every R0 property: §13.1's
/// packed min/max design puts the endpoints in the vertices and leaves one scalar per frame here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawableEntry {
    /// Tile-local to clip, as the shaders take it.
    pub matrix: [f32; 16],
    /// The layer's two zoom-interpolation factors.
    pub interpolations: [f32; 2],
}

impl DrawableEntry {
    /// The entry for a tile under a view, biased for its layer and sublayer.
    ///
    /// The bias is applied to the *projection* before the tile placement multiplies through it,
    /// which is what mbgl does and is not the same as biasing the product: the placement's last
    /// column would scale the offset. It also means this matrix is not the one `StencilTiles`
    /// carries — that one has no bias, because a clip mask does not participate in depth
    /// ordering — and the two must not be shared even though they look interchangeable.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has bearing or pitch.
    pub fn for_tile(
        view: &ViewTransform,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
    ) -> Result<Self, camera::CameraError> {
        Self::for_tile_with(
            view,
            z,
            x,
            y,
            wrap,
            layer_index,
            sub_layer_index,
            [0.0, 0.0],
        )
    }

    /// As [`Self::for_tile`], with the layer's zoom-mix factors.
    ///
    /// Split out rather than folded in because the factors need the layer's resolved paint and
    /// the tile's bucket zoom, neither of which a matrix needs. Use
    /// [`fill_interpolations`] to compute them; passing zeros is correct exactly when no paint
    /// property of the layer varies with zoom.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has bearing or pitch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile_with(
        view: &ViewTransform,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 2],
    ) -> Result<Self, camera::CameraError> {
        let mut projection = camera::proj_matrix(view)?;
        projection[14] -= f64::from(depth_offset(layer_index, sub_layer_index));
        let matrix = camera::multiply(
            &projection,
            &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
        );

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            interpolations,
        })
    }
}

/// The two zoom-mix factors a fill drawable's UBO carries.
///
/// # The pair is not the same for both sublayers
///
/// A fill layer draws twice, and the two shaders read different properties: the triangles take
/// `fill-color` and `fill-opacity`, the outline takes `fill-outline-color` and `fill-opacity`.
/// They share the buffer and the opacity, and differ in the colour — so a single pair used for
/// both would give the outline the fill's colour ramp. mbgl builds them separately in
/// `FillLayerTweaker::execute`, and so does this.
///
/// `bucket_zoom` is the tile's overscaled zoom, the same one its endpoints were evaluated at;
/// `view_zoom` is the camera's, which is where the fractional part enters. At an exactly
/// integer camera zoom over a tile of that zoom every factor is zero, which is why a capture
/// at integer zoom cannot tell a correct implementation from one that never computes this.
#[must_use]
pub fn fill_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
    sub_layer_index: i32,
) -> [f32; 2] {
    let factor = |name: &str| {
        paint
            .get(name)
            .map_or(0.0, |property| match property.binding {
                // Only an attribute mixes: a uniform already holds the value for this zoom.
                Binding::Attribute { interpolated: true } => {
                    property.expression.zoom_mix_factor(bucket_zoom, view_zoom)
                }
                _ => 0.0,
            })
    };

    let color = if sub_layer_index == 2 {
        "fill-outline-color"
    } else {
        "fill-color"
    };
    [factor(color), factor("fill-opacity")]
}

/// Packs a layer's drawable buffer at a union's stride.
///
/// Every entry is padded out to `stride` with zeros. mbgl value-initializes the vector before
/// assigning the variant, so the bytes past a smaller block are zero there too — and a diff of
/// the whole buffer would catch it if they were not.
#[must_use]
pub fn pack_drawable_buffer(entries: &[DrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// A layer's tile-properties buffer, which R0 fills with zeros.
///
/// The union holds only pattern variants — `FillPatternTilePropsUBO` and its outline twin — so a
/// layer with no `fill-pattern` has nothing to put in it. The buffer is still emitted at full
/// size, because the shader indexes it by the same `uboIndex` as the drawable buffer and a short
/// one would read past the end.
#[must_use]
pub fn pack_tile_props_buffer(drawables: usize, stride: u32) -> Vec<u8> {
    alloc::vec![0u8; drawables * stride as usize]
}

/// Packs `FillEvaluatedPropsUBO`.
#[must_use]
pub fn pack_fill_props(
    color: Color,
    outline_color: Color,
    opacity: f32,
    fade: f32,
    from_scale: f32,
    to_scale: f32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(ubo_layouts::FILL_EVALUATED_PROPS_UBO.size as usize);
    push_color(&mut out, color);
    push_color(&mut out, outline_color);
    push_f32s(&mut out, &[opacity, fade, from_scale, to_scale]);
    debug_assert_eq!(
        out.len(),
        ubo_layouts::FILL_EVALUATED_PROPS_UBO.size as usize
    );
    out
}

/// Packs `BackgroundPropsUBO`.
#[must_use]
pub fn pack_background_props(color: Color, opacity: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(ubo_layouts::BACKGROUND_PROPS_UBO.size as usize);
    push_color(&mut out, color);
    push_f32s(&mut out, &[opacity, 0.0, 0.0, 0.0]);
    debug_assert_eq!(out.len(), ubo_layouts::BACKGROUND_PROPS_UBO.size as usize);
    out
}

/// Writes a buffer as an `UboUpdate`.
///
/// The write is absolute, not a delta, which is what makes §4's latest-wins coalescing exact: a
/// consumer that dropped every earlier write for a slot and kept the last one has the right
/// bytes, because the last one describes the whole buffer.
///
/// # Errors
///
/// [`Full`] when the ring cannot take it.
pub fn write(
    producer: &mut Producer,
    view: ViewId,
    layer_index: i32,
    slot: u32,
    data: &[u8],
) -> Result<(), Full> {
    #[allow(clippy::cast_possible_truncation)]
    let record = UboUpdate {
        view,
        layer_index,
        slot,
        _pad: 0,
        data: Span {
            offset: 0,
            // A byte count here, not an element count: the payload is the buffer itself.
            count: data.len() as u32,
        },
    };
    producer.write(EnvelopeKind::UboUpdate, record.as_bytes(), data)
}

/// The slot a layer's drawable buffer occupies. Background and fill share it.
#[must_use]
pub const fn drawable_slot() -> u32 {
    ubo_slots::ID_FILL_DRAWABLE_UBO
}

fn push_f32s(out: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn push_color(out: &mut Vec<u8>, color: Color) {
    push_f32s(out, &[color.r, color.g, color.b, color.a]);
}
