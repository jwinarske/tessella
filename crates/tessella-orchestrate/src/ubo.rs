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
use tessella_style::Value;
use tessella_style::property::{Binding, Color, DefaultValue, ResolvedProperty};
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

/// One line drawable's entry.
///
/// Unlike a fill's, this carries a `ratio` as well as its mix factors — the line shader needs
/// tile units per screen pixel to turn `line-width` into an extrusion, and that is a function
/// of the camera's zoom against the tile's, so it cannot live in the vertex the way the width
/// endpoints do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineDrawableEntry {
    /// Tile-local to clip, as the shaders take it.
    pub matrix: [f32; 16],
    /// Screen pixels per tile unit, inverted.
    pub ratio: f32,
    /// Mix factors for colour, blur, opacity, gap width, offset and width, in that order.
    pub interpolations: [f32; 6],
}

impl LineDrawableEntry {
    /// The entry for a tile under a view.
    ///
    /// The argument list mirrors [`DrawableEntry::for_tile_with`] deliberately, so the two
    /// paths read the same at their call sites; grouping them would make one of the pair
    /// diverge in shape from the other for no gain.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has bearing or pitch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 6],
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
            ratio: line_ratio(z, view.zoom),
            interpolations,
        })
    }
}

/// Tile units per screen pixel at this zoom, inverted — the line shader's `ratio`.
///
/// mbgl computes it as `1 / tileID.pixelsToTileUnits(1, zoom)`, which expands to
/// `2^(zoom - z) * tileSize / EXTENT`, or `2^(zoom - z) / 16`. It is `0.0625` for a tile drawn
/// at its own zoom, which is what the golden dump carries.
///
/// Computed in `f32` throughout, because mbgl does: the zoom reaches `pixelsToTileUnits` as a
/// float and the extent and tile size are cast to float before the division.
#[must_use]
pub fn line_ratio(z: u8, zoom: f64) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let (zoom, z) = (zoom as f32, f32::from(z));
    // mbgl's EXTENT and tileSize_D, cast to float exactly as it casts them.
    let tile_units_per_pixel = 8192.0f32 / (512.0f32 * libm::powf(2.0, zoom - z));
    1.0 / tile_units_per_pixel
}

/// The six zoom-mix factors a line drawable's UBO carries.
///
/// The order is the UBO's, which is not the property table's: colour, blur, opacity, gap width,
/// offset, width. `line-floorwidth` is absent — it mirrors `line-width` and the shader reads
/// the width factor for both — so the seven binders map onto six slots.
#[must_use]
pub fn line_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
) -> [f32; 6] {
    let factor = |name: &str| {
        paint
            .get(name)
            .map_or(0.0, |property| match property.binding {
                Binding::Attribute { interpolated: true } => {
                    property.expression.zoom_mix_factor(bucket_zoom, view_zoom)
                }
                _ => 0.0,
            })
    };
    [
        factor("line-color"),
        factor("line-blur"),
        factor("line-opacity"),
        factor("line-gap-width"),
        factor("line-offset"),
        factor("line-width"),
    ]
}

/// Packs a layer's line drawable buffer at the union's stride.
#[must_use]
pub fn pack_line_drawable_buffer(entries: &[LineDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &[entry.ratio]);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// Packs `LineEvaluatedPropsUBO`.
///
/// # Not from the generated table
///
/// DR-6's generator lists this block in `UNPARSED`: it declines to model `LineExpressionMask`
/// rather than guess at it. The offsets here are transcribed from `line_layer_ubo.hpp`'s own
/// offset comments, and the size is asserted against the `3 * 16` its `static_assert` fixes.
///
/// # Every value is the constant-or-default
///
/// mbgl fills this with `evaluated.get<P>().constantOr(P::defaultValue())`, so a property that
/// varies per feature contributes its *spec default* here and its real values through the
/// vertex attributes. That is not a fallback for something missing: the shader reads this slot
/// only for the properties the permutation left as uniforms, and writing the data-driven ones'
/// evaluated values instead would put one feature's colour into a layer-wide uniform.
///
/// The expression mask is zero. It selects mbgl's Metal-only GPU expression evaluation, which
/// the probe disables outright (§3.1 wants data-driven properties as attributes or UBO fields,
/// not as trees the GPU walks).
#[must_use]
pub fn pack_line_props(
    color: Color,
    blur: f32,
    opacity: f32,
    gapwidth: f32,
    offset: f32,
    width: f32,
    floorwidth: f32,
) -> Vec<u8> {
    const SIZE: usize = 48;
    let mut out = Vec::with_capacity(SIZE);
    push_color(&mut out, color);
    push_f32s(
        &mut out,
        &[blur, opacity, gapwidth, offset, width, floorwidth],
    );
    // expressionMask and pad1.
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0f32.to_le_bytes());
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// A property's value as a layer-wide uniform, or `None` when it does not have one.
///
/// mbgl's `evaluated.get<P>().constantOr(P::defaultValue())` in two halves. A property bound as
/// an attribute has no single value for the layer, so it yields `None` and the caller supplies
/// the spec default — which is what the shader will read for it, and is *not* a stand-in for a
/// missing value: the permutation tells the shader to take that property from the vertex.
///
/// A camera-only property does have one: it is constant across every feature at a given zoom,
/// which is why it is a uniform at all, so it is evaluated here at the view's zoom.
fn uniform_value(property: &ResolvedProperty, zoom: f64) -> Option<Value> {
    match property.binding {
        Binding::Attribute { .. } => None,
        Binding::Uniform => property.expression.evaluate(Some(zoom), None).ok(),
    }
}

/// A colour-typed property's uniform value, falling back to its spec default.
fn uniform_color(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> Color {
    let Some(property) = paint.get(name) else {
        return Color::transparent();
    };
    let default = match property.spec.default {
        DefaultValue::Color(color) => color,
        _ => Color::transparent(),
    };
    uniform_value(property, zoom)
        .and_then(|value| tessella_style::property::as_color(&value).ok())
        .unwrap_or(default)
}

/// A number-typed property's uniform value, falling back to its spec default.
fn uniform_number(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> f32 {
    let Some(property) = paint.get(name) else {
        return 0.0;
    };
    #[allow(clippy::cast_possible_truncation)]
    let default = match property.spec.default {
        DefaultValue::Number(number) => number as f32,
        _ => 0.0,
    };
    #[allow(clippy::cast_possible_truncation)]
    uniform_value(property, zoom)
        .and_then(|value| value.as_number())
        .map_or(default, |number| number as f32)
}

/// A line layer's evaluated properties, from its resolved paint.
///
/// The crossfade scalars a pattern would need are absent because a pattern is not implemented;
/// this block has no room for them in any case, which is why the pattern variants are separate
/// shaders with their own tile-props block.
#[must_use]
pub fn line_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_line_props(
        uniform_color(paint, "line-color", zoom),
        uniform_number(paint, "line-blur", zoom),
        uniform_number(paint, "line-opacity", zoom),
        uniform_number(paint, "line-gap-width", zoom),
        uniform_number(paint, "line-offset", zoom),
        uniform_number(paint, "line-width", zoom),
        uniform_number(paint, "line-floorwidth", zoom),
    )
}

/// A fill layer's evaluated properties, from its resolved paint.
///
/// The two crossfade scalars are the pattern's, and are the values mbgl writes when no pattern
/// is set: a fade of one and scales of one half and one.
#[must_use]
pub fn fill_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_fill_props(
        uniform_color(paint, "fill-color", zoom),
        uniform_color(paint, "fill-outline-color", zoom),
        uniform_number(paint, "fill-opacity", zoom),
        1.0,
        0.5,
        1.0,
    )
}

/// One circle drawable's entry.
///
/// Its `extrude_scale` is the counterpart of a line's `ratio`: what turns `circle-radius` into
/// a quad size. Which units it is in depends on `circle-pitch-alignment` — see
/// [`circle_extrude_scale`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircleDrawableEntry {
    /// Tile-local to clip, as the shaders take it.
    pub matrix: [f32; 16],
    /// Radius units, per [`circle_extrude_scale`].
    pub extrude_scale: [f32; 2],
    /// Mix factors for colour, radius, blur, opacity, stroke colour, stroke width and stroke
    /// opacity, in that order.
    pub interpolations: [f32; 7],
}

impl CircleDrawableEntry {
    /// The entry for a tile under a view.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has bearing or pitch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        extrude_scale: [f32; 2],
        interpolations: [f32; 7],
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
            extrude_scale,
            interpolations,
        })
    }
}

/// The units `circle-radius` is measured in, which the pitch alignment decides.
///
/// Aligned to the *viewport* — the spec's default, and the odd one out among the anchor-style
/// enums — a circle keeps its size on screen however the map is pitched, so the scale is
/// `pixelsToGLUnits`: two over the viewport's width and minus two over its height. Aligned to
/// the *map* it lies flat and scales with the tile, so the scale is tile units per pixel on
/// both axes.
///
/// Two different quantities behind one field, which is why this takes the alignment rather than
/// defaulting it: a viewport-aligned circle given the map scale is wrong by the zoom factor and
/// looks like a radius bug.
#[must_use]
pub fn circle_extrude_scale(pitch_with_map: bool, z: u8, view: &ViewTransform) -> [f32; 2] {
    if pitch_with_map {
        let tile_units = 1.0 / line_ratio(z, view.zoom);
        [tile_units, tile_units]
    } else {
        #[allow(clippy::cast_possible_truncation)]
        [2.0 / view.width as f32, -2.0 / view.height as f32]
    }
}

/// The seven zoom-mix factors a circle drawable's UBO carries, in the UBO's own order.
#[must_use]
pub fn circle_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
) -> [f32; 7] {
    let factor = |name: &str| {
        paint
            .get(name)
            .map_or(0.0, |property| match property.binding {
                Binding::Attribute { interpolated: true } => {
                    property.expression.zoom_mix_factor(bucket_zoom, view_zoom)
                }
                _ => 0.0,
            })
    };
    [
        factor("circle-color"),
        factor("circle-radius"),
        factor("circle-blur"),
        factor("circle-opacity"),
        factor("circle-stroke-color"),
        factor("circle-stroke-width"),
        factor("circle-stroke-opacity"),
    ]
}

/// Packs a layer's circle drawable buffer at the union's stride.
#[must_use]
pub fn pack_circle_drawable_buffer(entries: &[CircleDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &entry.extrude_scale);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// Packs `CircleEvaluatedPropsUBO`.
///
/// The two flags are integers, not floats, and are the only non-float fields in any of these
/// blocks — so a packer that pushed them as `1.0` would write `0x3f800000` where the shader
/// reads `1`.
///
/// The argument list is the block's field list, in the block's order. Grouping it would put a
/// struct between the header's offsets and this function, which is the one place they have to
/// be checkable against each other.
#[must_use]
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub fn pack_circle_props(
    color: Color,
    stroke_color: Color,
    radius: f32,
    blur: f32,
    opacity: f32,
    stroke_width: f32,
    stroke_opacity: f32,
    scale_with_map: bool,
    pitch_with_map: bool,
) -> Vec<u8> {
    const SIZE: usize = 64;
    let mut out = Vec::with_capacity(SIZE);
    push_color(&mut out, color);
    push_color(&mut out, stroke_color);
    push_f32s(
        &mut out,
        &[radius, blur, opacity, stroke_width, stroke_opacity],
    );
    out.extend_from_slice(&i32::from(scale_with_map).to_le_bytes());
    out.extend_from_slice(&i32::from(pitch_with_map).to_le_bytes());
    out.extend_from_slice(&0f32.to_le_bytes());
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// A circle layer's evaluated properties, from its resolved paint.
#[must_use]
pub fn circle_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_circle_props(
        uniform_color(paint, "circle-color", zoom),
        uniform_color(paint, "circle-stroke-color", zoom),
        uniform_number(paint, "circle-radius", zoom),
        uniform_number(paint, "circle-blur", zoom),
        uniform_number(paint, "circle-opacity", zoom),
        uniform_number(paint, "circle-stroke-width", zoom),
        uniform_number(paint, "circle-stroke-opacity", zoom),
        uniform_enum(paint, "circle-pitch-scale", zoom) == "map",
        uniform_enum(paint, "circle-pitch-alignment", zoom) == "map",
    )
}

/// An enum-typed property's uniform value, falling back to its spec default.
fn uniform_enum(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> alloc::string::String {
    use alloc::string::ToString;

    let Some(property) = paint.get(name) else {
        return alloc::string::String::new();
    };
    let default = match property.spec.default {
        DefaultValue::Enum(name) => name,
        _ => "",
    };
    uniform_value(property, zoom)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_else(|| default.to_string())
}

/// One entry of a symbol layer's `SymbolDrawableUBO` array.
///
/// Three matrices, because a symbol is drawn in three spaces at once. `matrix` places the tile
/// the way every other layer's does; `label_plane_matrix` takes tile coordinates into the screen
/// units the label was *laid out* in, which is where a line label's glyphs are walked along; and
/// `coord_matrix` takes that plane back to clip space. Baking them into one would work for a
/// point label and put every glyph of a line label in the wrong place, since the walk has to
/// happen between the two.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolDrawableEntry {
    /// Tile-local to clip.
    pub matrix: [f32; 16],
    /// Tile-local to the plane the label was laid out in.
    pub label_plane_matrix: [f32; 16],
    /// That plane back to clip.
    pub coord_matrix: [f32; 16],
    /// The glyph atlas, in pixels.
    pub texsize: [f32; 2],
    /// The sprite sheet, in pixels. Zero when the style has no sprite.
    ///
    /// Both sizes ride in every entry whether or not the drawable uses them, because one shader
    /// samples both textures and the buffer is its interface — the same reason the evaluated
    /// props carry an icon half for a layer with no icons.
    pub texsize_icon: [f32; 2],
    /// Whether this drawable is the text half rather than the icon half.
    pub is_text: bool,
    /// `text-rotation-alignment: map` under a viewport-aligned pitch.
    pub rotate_symbol: bool,
    /// `text-pitch-alignment: map`.
    pub pitch_with_map: bool,
    /// Whether the size is the same at every zoom, and the same for every feature.
    ///
    /// Two separate flags because the shader takes three paths: a constant needs no
    /// interpolation, a zoom curve interpolates between the two packed sizes, and a data-driven
    /// one reads the size out of the vertex. Setting the wrong pair draws every label at the
    /// wrong size in a way that looks like a font problem.
    pub is_size_zoom_constant: bool,
    /// Whether the size is constant across features.
    pub is_size_feature_constant: bool,
    /// Whether `text-offset` is set.
    pub is_offset: bool,
    /// Where between the two packed sizes this zoom falls.
    pub size_t: f32,
    /// The size itself, when it is constant.
    pub size: f32,
    /// Mix factors for fill colour, halo colour, opacity, halo width and halo blur.
    pub interpolations: [f32; 5],
}

impl SymbolDrawableEntry {
    /// The entry for one tile's symbols under a view.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has bearing or pitch.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        texsize: [f32; 2],
        texsize_icon: [f32; 2],
        size: f32,
    ) -> Result<Self, camera::CameraError> {
        let mut projection = camera::proj_matrix(view)?;
        projection[14] -= f64::from(depth_offset(layer_index, sub_layer_index));
        let tile = camera::multiply(
            &projection,
            &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
        );

        // The label plane is built from the tile matrix *without* the depth offset — mbgl passes
        // the drawable's own matrix, and the offset is already in it. Passing an unoffset one
        // would put the label plane a hair in front of the geometry it belongs to.
        let plane = camera::label_plane_matrix(&tile, view.width, view.height);
        let coord = camera::gl_coord_matrix(view.width, view.height);

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| tile[index] as f32),
            label_plane_matrix: core::array::from_fn(|index| plane[index] as f32),
            coord_matrix: core::array::from_fn(|index| coord[index] as f32),
            texsize,
            texsize_icon,
            is_text: true,
            rotate_symbol: false,
            pitch_with_map: false,
            // A constant `text-size` is constant in both senses, which is the common case and
            // the only one this build produces.
            is_size_zoom_constant: true,
            is_size_feature_constant: true,
            is_offset: false,
            size_t: 0.0,
            size,
            interpolations: [0.0; 5],
        })
    }
}

/// Packs the `SymbolDrawableUBO` array.
///
/// `stride` is the layout's, which is 272 against a size of 260 — the padding is between
/// entries, not inside one, and using the size as the stride puts every entry after the first
/// twelve bytes early.
#[must_use]
pub fn pack_symbol_drawable_buffer(entries: &[SymbolDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = alloc::vec![0u8; entries.len() * stride];
    for (entry, slot) in entries.iter().zip(out.chunks_mut(stride)) {
        let mut at = 0usize;
        let put_f32s = |slot: &mut [u8], at: &mut usize, values: &[f32]| {
            for value in values {
                slot[*at..*at + 4].copy_from_slice(&value.to_le_bytes());
                *at += 4;
            }
        };
        put_f32s(slot, &mut at, &entry.matrix);
        put_f32s(slot, &mut at, &entry.label_plane_matrix);
        put_f32s(slot, &mut at, &entry.coord_matrix);
        put_f32s(slot, &mut at, &entry.texsize);
        put_f32s(slot, &mut at, &entry.texsize_icon);
        for flag in [
            entry.is_text,
            entry.rotate_symbol,
            entry.pitch_with_map,
            entry.is_size_zoom_constant,
            entry.is_size_feature_constant,
            entry.is_offset,
        ] {
            slot[at..at + 4].copy_from_slice(&i32::from(flag).to_le_bytes());
            at += 4;
        }
        put_f32s(slot, &mut at, &[entry.size_t, entry.size]);
        put_f32s(slot, &mut at, &entry.interpolations);
        debug_assert_eq!(at, 260);
    }
    out
}

/// Packs `SymbolTilePropsUBO`, one entry per drawable.
///
/// Sixteen bytes each: which of a symbol's two halves this drawable draws, whether it is the
/// halo pass, and the gamma scale.
///
/// `is_halo` is a *second drawable over the same geometry*, not a flag on one — mbgl draws the
/// halo first and the fill over it, so a layer with `text-halo-width` emits twice. This build
/// draws no halo, which is why the oracle's two entries are both `is_halo = 0`.
///
/// `gamma_scale` is one at pitch zero. Pitched, mbgl scales it by the drawable's perspective
/// ratio so distant text does not thin out; there is no pitch here, and inventing the pitched
/// value would put a number on the wire nothing produced.
#[must_use]
pub fn pack_symbol_tile_props(
    drawables: usize,
    is_text: bool,
    is_halo: bool,
    gamma: f32,
) -> Vec<u8> {
    const STRIDE: usize = 16;
    let mut out = Vec::with_capacity(drawables * STRIDE);
    for _ in 0..drawables {
        out.extend_from_slice(&i32::from(is_text).to_le_bytes());
        out.extend_from_slice(&i32::from(is_halo).to_le_bytes());
        out.extend_from_slice(&gamma.to_le_bytes());
        out.extend_from_slice(&0f32.to_le_bytes());
    }
    debug_assert_eq!(out.len(), drawables * STRIDE);
    out
}

/// Packs `SymbolEvaluatedPropsUBO`.
///
/// Ninety-six bytes: text colour, halo colour, opacity, halo width and blur, then the same five
/// again for icons. Both halves are always present whether or not the layer draws icons, because
/// one shader serves both and the buffer is its interface.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn pack_symbol_props(
    text_color: Color,
    text_halo_color: Color,
    text_opacity: f32,
    text_halo_width: f32,
    text_halo_blur: f32,
    icon_color: Color,
    icon_halo_color: Color,
    icon_opacity: f32,
    icon_halo_width: f32,
    icon_halo_blur: f32,
) -> Vec<u8> {
    const SIZE: usize = 96;
    let mut out = Vec::with_capacity(SIZE);
    push_color(&mut out, text_color);
    push_color(&mut out, text_halo_color);
    push_f32s(
        &mut out,
        &[text_opacity, text_halo_width, text_halo_blur, 0.0],
    );
    push_color(&mut out, icon_color);
    push_color(&mut out, icon_halo_color);
    push_f32s(
        &mut out,
        &[icon_opacity, icon_halo_width, icon_halo_blur, 0.0],
    );
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// A symbol layer's evaluated properties, from its resolved paint.
///
/// The icon half is filled from the style's own defaults rather than left zero. `icon-color`
/// defaults to black and `text-color` to black too — a layer that names neither still has both,
/// and writing zeros for the half a layer does not use would put a transparent black on the wire
/// where the oracle has an opaque one.
#[must_use]
pub fn symbol_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_symbol_props(
        uniform_color(paint, "text-color", zoom),
        uniform_color(paint, "text-halo-color", zoom),
        uniform_number(paint, "text-opacity", zoom),
        uniform_number(paint, "text-halo-width", zoom),
        uniform_number(paint, "text-halo-blur", zoom),
        uniform_color(paint, "icon-color", zoom),
        uniform_color(paint, "icon-halo-color", zoom),
        uniform_number(paint, "icon-opacity", zoom),
        uniform_number(paint, "icon-halo-width", zoom),
        uniform_number(paint, "icon-halo-blur", zoom),
    )
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
