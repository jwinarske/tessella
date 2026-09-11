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
use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::envelope::{Span, UboUpdate, ViewId, WireRecord};
use tessella_capture_abi::generated::ubo_layouts;
use tessella_capture_abi::generated::ubo_slots;
use tessella_capture_abi::globe_ubo::GlobeBendUbo;
use tessella_capture_abi::ring::{Full, Producer};
use tessella_layout::raster::{self, RasterColour};
use tessella_layout::symbol_layout::{Alignment, Alignments, Placement};
use tessella_style::Value;
use tessella_style::crossfade::Crossfade;
use tessella_style::property::{Binding, Color, DefaultValue, ResolvedProperty};
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;
use tessella_tile::globe;

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
    /// Symbol fade progress. Unused: the fades are per-symbol opacity in the geometry, not a
    /// frame-wide multiplier. Kept because the block's layout is generated and shared.
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

/// The tile-local matrix a drawable is placed by, for the projection the frame draws through.
///
/// Two different things depending on the surface, which is the whole of plan.md §13.4's producer
/// half beyond the cover policy:
///
/// - **Mercator**: `proj_matrix * placement`, reaching clip space. The consumer multiplies a
///   tile-local vertex through it and is done.
/// - **Globe**: `mercator_matrix_for_tile` alone, reaching *normalized Mercator*. Clip space is
///   two more steps -- the nonlinear bend onto the sphere, then `CameraUpdate::globe_matrix` --
///   and neither belongs in a matrix: the first is a pair of trig calls per vertex and the second
///   is per frame rather than per drawable.
///
/// # Where the depth offset goes
///
/// Coincident layers are separated by a bias on the projection's `[14]`, which under Mercator is
/// baked into each tile's matrix here. A globe has nowhere to put it: `globe_matrix` is one matrix
/// for the whole frame and the bias is per drawable.
///
/// So it rides in the placement matrix's own `[14]`, which is free. `mercator_matrix_for_tile`
/// scales z by one and translates it by zero -- tile geometry is 2D and its z is always zero, so
/// nothing reads that slot on the way in. The consumer applies it to the bent position's z after
/// `globe_matrix`, which is the same arithmetic in the same place, moved from a matrix the
/// producer could fold it into to one it cannot.
///
/// # Errors
///
/// [`camera::CameraError`] when the view has no area. A globe cannot produce one -- the placement
/// is a pure function of the tile address -- and the signature keeps it so that a caller does not
/// have to know which projection it is under.
pub fn tile_matrix(
    view: &ViewTransform,
    projection: ProjectionMode,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
    depth: f32,
) -> Result<camera::Mat4, camera::CameraError> {
    match projection {
        ProjectionMode::Mercator => {
            let mut clip = camera::proj_matrix(view)?;
            clip[14] -= f64::from(depth);
            Ok(camera::multiply(
                &clip,
                &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
            ))
        }
        ProjectionMode::Globe => {
            let mut placement = camera::mercator_matrix_for_tile(z, x, y, wrap);
            // Scaled to the frustum, not absolute. mbgl's nudge is a fixed distance in clip space
            // because its depth range is a fixed depth; a globe's is not -- the camera closes on
            // the surface as the zoom rises, so the span falls from 1.7 at z4 to 0.055 at z14 and
            // 0.027 at z16. Sent absolute, the nudge is 56% of the range at z14 and 112% at z16,
            // and the layer it belongs to is pushed through the near plane. The whole planet went
            // black from z14 up, and the frames below that were layers shuffled past each other.
            let (near, far) = globe::depth_range(view);
            placement[14] = -f64::from(depth) * (far - near);
            Ok(placement)
        }
    }
}

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
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
    ) -> Result<Self, camera::CameraError> {
        Self::for_tile_with(
            view,
            projection,
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
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile_with(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 2],
    ) -> Result<Self, camera::CameraError> {
        let matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            interpolations,
        })
    }

    /// The entry for a background standing in for the oracle's clear.
    ///
    /// See [`crate::tile::background_covers_viewport`] for which background reaches this and why.
    /// There is no clear colour on this wire, so the equivalent of clearing the renderable is a
    /// quad over the whole of it: the same pixels, and one drawable where the per-tile path has
    /// one per cover tile.
    ///
    /// The matrix takes the quad's own 0..`EXTENT` box to the clip cube and does not consult the
    /// camera at all — which is the point, because a clear does not either. That also means the
    /// drawable's geometry *and* its matrix are constant, so it is announced once and its UBO
    /// rewritten only for the depth nudge.
    ///
    /// Y is flipped, as the tile-to-clip path flips it: tile coordinates run down and clip runs
    /// up. It makes no difference to a quad that covers the cube either way, and it is what the
    /// orientation would have to be if anything textured ever came through here.
    #[must_use]
    pub fn for_viewport(layer_index: i32, sub_layer_index: i32) -> Self {
        let mut matrix = camera::identity();
        camera::translate_in_place(
            &mut matrix,
            -1.0,
            1.0,
            -f64::from(depth_offset(layer_index, sub_layer_index)),
        );
        let matrix = camera::scale(&matrix, 2.0 / camera::EXTENT, -2.0 / camera::EXTENT, 1.0);
        #[allow(clippy::cast_possible_truncation)]
        Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            // `BackgroundDrawableUBO` is a matrix and two pads: no property of a background
            // varies with zoom in a way a drawable entry carries.
            interpolations: [0.0, 0.0],
        }
    }

    /// As [`Self::for_tile_with`], for a layer that resolves in the depth buffer.
    ///
    /// # A 3D layer takes no sublayer nudge
    ///
    /// [`depth_offset`] reproduces mbgl's `depthModeForSublayer`, which divides a *flat* layer's
    /// depth range into [`SUBLAYERS`] steps so a fill's outline does not z-fight the fill it
    /// outlines. A fill-extrusion does not go through it: mbgl draws it under `depthModeFor3D`,
    /// which takes the whole depth range and has no sublayer term at all. Its drawables are
    /// separated by the geometry's own depth, which is the point of a 3D layer.
    ///
    /// Nudging one anyway is not the harmless bias it looks like. An extrusion's depth-only pass
    /// and the colour pass that follows draw the *same* surfaces, and the colour pass has to
    /// compare equal against what the depth pass wrote. One step of [`DEPTH_EPSILON`] is 9.3e-7
    /// of clip depth after the divide, against the 2e-4 that a 150-metre building spans in
    /// total, and `depth_probe`'s third phase puts the tolerance below that: at a separation of
    /// 1e-6 the colour pass is rejected entirely and the buildings vanish.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    pub fn for_tile_3d(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
    ) -> Result<Self, camera::CameraError> {
        let matrix = tile_matrix(view, projection, z, x, y, wrap, 0.0)?;

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            interpolations: [0.0, 0.0],
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
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 6],
    ) -> Result<Self, camera::CameraError> {
        let matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;

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
    // One implementation of this quantity, not two. It was computed here through the `libm`
    // *crate* and in `camera::pixels_to_tile_units` through the system one `tessella-tile` links
    // against — reciprocals of the same number by two routines free to round differently in the
    // last bit, with nothing comparing them. A line's width and a pitched label's plane both
    // read it, so a disagreement would be a hairline mismatch nothing would attribute.
    #[allow(clippy::cast_possible_truncation)]
    {
        1.0 / camera::pixels_to_tile_units(z, zoom) as f32
    }
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

/// The seven factors a dashed line drawable's UBO carries.
///
/// The first six are [`line_interpolations`]'s. The seventh is `line-floorwidth`'s, and it is
/// computed at the *integer* zoom: mbgl evaluates that property through
/// `DataDrivenPropertyEvaluator<float, true>`, whose one job is to floor the zoom, and its binder
/// floors it again when it reports a factor. The shader divides the distance along the line by
/// floor width, so a value that moved continuously with the zoom would make the dash pattern
/// breathe between levels instead of stepping at them.
#[must_use]
pub fn line_sdf_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
) -> [f32; 7] {
    let six = line_interpolations(paint, bucket_zoom, view_zoom);
    let floorwidth = paint
        .get("line-floorwidth")
        .map_or(0.0, |property| match property.binding {
            Binding::Attribute { interpolated: true } => property
                .expression
                .zoom_mix_factor(bucket_zoom, view_zoom.floor()),
            _ => 0.0,
        });
    [six[0], six[1], six[2], six[3], six[4], six[5], floorwidth]
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

/// One dashed line drawable's entry.
///
/// `LineSDFDrawableUBO`, which is the plain line's block with the dash texture's placement
/// wedged between the matrix and the ratio -- so it cannot share [`LineDrawableEntry`]'s packer
/// even though six of its seven mix factors are the same six.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineSdfDrawableEntry {
    /// Tile-local to clip, as the shaders take it.
    pub matrix: [f32; 16],
    /// Screen pixels per tile unit, inverted.
    pub ratio: f32,
    /// How the pattern being faded from is scaled: along the line, then across it.
    pub patternscale_a: [f32; 2],
    /// The same for the pattern being faded to.
    pub patternscale_b: [f32; 2],
    /// Where the `from` pattern's rows sit in the atlas.
    pub tex_y_a: f32,
    /// And the `to` pattern's.
    pub tex_y_b: f32,
    /// Mix factors for color, blur, opacity, gap width, offset, width and floor width.
    ///
    /// Seven where a plain line has six: the shader reads `floorwidth` as a property of its own
    /// because it divides the distance along the line by it, and mbgl gives it its own factor
    /// rather than reusing the width's -- the two differ, because floor width is evaluated at
    /// the integer zoom.
    pub interpolations: [f32; 7],
}

/// The dash placement a layer's drawables share, in the terms the UBO wants.
///
/// mbgl derives all of this from the `DashPatternTexture` at tweak time rather than storing it:
/// `patternscale` is the reciprocal of the pattern's length in tile units against its width in
/// the atlas, and `sdfgamma` is half a texel of the narrower of the two patterns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DashPlacement {
    /// The `from` pattern's place in the atlas.
    pub from: tessella_glyph::dash::Position,
    /// The `to` pattern's.
    pub to: tessella_glyph::dash::Position,
    /// How each is scaled, which is the crossfade's doing.
    pub crossfade: Crossfade,
    /// The view's device pixel ratio.
    pub pixel_ratio: f32,
}

impl DashPlacement {
    /// The two pattern widths after the crossfade has scaled them, in style units.
    fn widths(&self) -> (f32, f32) {
        (
            self.from.width * self.crossfade.from_scale,
            self.to.width * self.crossfade.to_scale,
        )
    }

    /// `patternscale_a` and `patternscale_b`, for a tile with this many tile units to the pixel.
    ///
    /// The x is the reciprocal of the pattern's length in tile units, so the texture coordinate
    /// the shader forms from `linesofar` advances by one per repeat. The y is minus half the
    /// pattern's height, which with a normal of plus or minus one spans exactly its rows.
    ///
    /// `tile_units_per_pixel` is mbgl's `tileID.pixelsToTileUnits(1, intZoom)`, and `intZoom` is
    /// the *camera's* integer zoom rather than the tile's own level -- so a parent standing in
    /// for a finer tile has its dashes stretched by however far it has been stretched, which is
    /// why this is per drawable and `sdfgamma` is not.
    #[must_use]
    pub fn scales(&self, tile_units_per_pixel: f32) -> ([f32; 2], [f32; 2]) {
        let (width_a, width_b) = self.widths();
        let along = |width: f32| {
            let units = width * tile_units_per_pixel;
            if units == 0.0 { 0.0 } else { 1.0 / units }
        };
        (
            [along(width_a), -self.from.height / 2.0],
            [along(width_b), -self.to.height / 2.0],
        )
    }

    /// `sdfgamma`: the antialiasing width the fragment stage smoothsteps over.
    ///
    /// Half a texel of the narrower pattern, expressed in the units the shader measures the
    /// distance field in. The atlas is [`tessella_glyph::dash::WIDTH`] wide and the field is
    /// stored over 256 levels, so the two cancel and what is left is the reciprocal of the
    /// pattern's width in device pixels.
    #[must_use]
    pub fn sdfgamma(&self) -> f32 {
        let (width_a, width_b) = self.widths();
        let narrower = width_a.min(width_b);
        #[allow(clippy::cast_precision_loss)]
        let atlas = tessella_glyph::dash::WIDTH as f32;
        let denominator = narrower * 256.0 * self.pixel_ratio;
        if denominator == 0.0 {
            0.0
        } else {
            atlas / denominator / 2.0
        }
    }
}

/// Packs a layer's dashed line drawable buffer at the union's stride.
#[must_use]
pub fn pack_line_sdf_drawable_buffer(entries: &[LineSdfDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &entry.patternscale_a);
        push_f32s(&mut out, &entry.patternscale_b);
        push_f32s(&mut out, &[entry.tex_y_a, entry.tex_y_b, entry.ratio]);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// Packs `LineSDFTilePropsUBO`, one entry per drawable, at the union's stride.
///
/// One placement repeated rather than one per drawable computed separately, as a pattern's is:
/// a dasharray cannot vary per feature, so every tile of the cover gets the same two numbers.
#[must_use]
pub fn pack_line_sdf_tile_props(placement: &DashPlacement, count: usize, stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(count * stride);
    for _ in 0..count {
        let start = out.len();
        push_f32s(&mut out, &[placement.sdfgamma(), placement.crossfade.t]);
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
/// A uniform property's value at zoom zero, for a decision that is not per frame.
///
/// `fill-extrusion-opacity` decides how many drawables a layer becomes rather than what colour
/// it is, and that has to be settled where the bucket is built. A zoom-varying opacity would
/// change the count between frames whatever this read, so the bucket's own zoom is as good an
/// answer as exists on this side.
#[must_use]
pub fn uniform_opacity(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
) -> f32 {
    uniform_number(paint, name, 0.0)
}

pub(crate) fn uniform_number(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> f32 {
    let Some(property) = paint.get(name) else {
        return 0.0;
    };
    // A boolean counts as a number here, because that is what the buffer holds.
    //
    // A props block has no room for a bool and mbgl writes one as 1.0 or 0.0, so a boolean paint
    // property arrives at the shader as a float. Falling through to zero instead is not a
    // neutral default: `fill-extrusion-vertical-gradient` defaults to *true*, and the shader
    // reads it as `(1 - g) + g * factor`, so a zero turns the gradient off entirely. Every wall
    // came out one flat shade where the oracle darkens it toward the ground, and a style setting
    // the property explicitly got the same zero, since the value is a boolean too.
    #[allow(clippy::cast_possible_truncation)]
    let default = match property.spec.default {
        DefaultValue::Number(number) => number as f32,
        DefaultValue::Boolean(flag) => f32::from(u8::from(flag)),
        _ => 0.0,
    };
    #[allow(clippy::cast_possible_truncation)]
    uniform_value(property, zoom)
        .and_then(|value| {
            value
                .as_number()
                .map(|number| number as f32)
                .or_else(|| value.as_bool().map(|flag| f32::from(u8::from(flag))))
        })
        .unwrap_or(default)
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
        // At the integer zoom, because mbgl evaluates this one property through
        // `DataDrivenPropertyEvaluator<float, true>` -- and the SDF shader divides the distance
        // along the line by it, so evaluating at the fractional zoom would make a dash pattern
        // stretch continuously instead of stepping at each level.
        uniform_number(paint, "line-floorwidth", zoom.floor()),
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

/// A background layer's properties, from its resolved paint.
#[must_use]
pub fn background_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_background_props(
        uniform_color(paint, "background-color", zoom),
        uniform_number(paint, "background-opacity", zoom),
    )
}

/// One fill-extrusion drawable's entry.
///
/// A different shape from every other drawable block, and the difference is load-bearing. Where
/// a fill's entry is a matrix and two mix factors, an extrusion's carries `height_factor` — what
/// turns a height in metres into the tile-space z the shader raises a wall to — and the tile's
/// pixel coordinate, split across two floats because the shader needs more precision in it than
/// one `f32` holds at a high zoom.
///
/// Packing a fill's entry into this shape is not a near miss: the mix factors land where the
/// pixel coordinate and the height factor belong, so `height_factor` reads as whatever the
/// colour interpolation happened to be — zero, for a constant colour — and every building comes
/// out flat. It draws, and it draws a fill layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionDrawableEntry {
    /// Tile-local to clip.
    pub matrix: [f32; 16],
    /// The tile's pixel coordinate, high and low halves.
    pub pixel_coord_upper: [f32; 2],
    /// The low halves.
    pub pixel_coord_lower: [f32; 2],
    /// What a height in metres multiplies by.
    pub height_factor: f32,
    /// Tile units per pixel, inverted.
    pub tile_ratio: f32,
    /// Mix factors for base, height and colour, in that order.
    pub interpolations: [f32; 3],
}

impl ExtrusionDrawableEntry {
    /// The entry for a tile under a view.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    ///
    /// No layer or sublayer index: the matrix takes neither, because a 3D layer's depth range is
    /// the whole buffer. See [`DrawableEntry::for_tile_3d`].
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        interpolations: [f32; 3],
    ) -> Result<Self, camera::CameraError> {
        // `for_tile_3d`, not `for_tile_with`: mbgl's `depthModeFor3D` has no sublayer term, and
        // applying the flat-layer one here separates this drawable's depth from the depth pass
        // that precedes it by more than the comparison tolerates.
        let matrix = DrawableEntry::for_tile_3d(view, projection, z, x, y, wrap)?.matrix;

        let origin = PixelOrigin::of(view, z, x, y, wrap);

        Ok(Self {
            matrix,
            pixel_coord_upper: origin.upper,
            pixel_coord_lower: origin.lower,
            height_factor: height_factor(z),
            tile_ratio: origin.tile_ratio,
            interpolations,
        })
    }
}

/// A tile's pixel origin at the *integer* zoom, split so a shader can reconstruct it.
///
/// Shared by the two families that tile something across the world rather than across the tile:
/// a fill-extrusion, whose walls take a pattern along their length, and a fill-pattern, whose
/// sprite has to line up across a tile boundary. Both read the same three values under the same
/// names, and mbgl computes them once in `LayerTweaker` for the same reason.
#[derive(Debug, Clone, Copy)]
pub struct PixelOrigin {
    /// The high half of the origin, in pixels.
    pub upper: [f32; 2],
    /// The low half.
    pub lower: [f32; 2],
    /// `1 / pixelsToTileUnits(1, integerZoom)` -- the tile's extent over the pixels it covers.
    pub tile_ratio: f32,
}

impl PixelOrigin {
    /// The origin for one tile under a view.
    #[must_use]
    pub fn of(view: &ViewTransform, z: u8, x: u32, y: u32, wrap: i32) -> Self {
        // `tileSizeAtNearestZoom` is floored in mbgl, and the floor matters at a fractional zoom.
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let integer_zoom = view.zoom.floor() as i32;
        let zoom_scale = 2f64.powi(i32::from(z));
        let nearest_zoom_scale = 2f64.powi(integer_zoom - i32::from(z));
        let tile_size_at_nearest = (512.0 * nearest_zoom_scale).floor();
        #[allow(clippy::cast_possible_truncation)]
        let pixel_x = (tile_size_at_nearest * (f64::from(x) + f64::from(wrap) * zoom_scale)) as i32;
        #[allow(clippy::cast_possible_truncation)]
        let pixel_y = (tile_size_at_nearest * f64::from(y)) as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let tile_ratio = (512.0 * nearest_zoom_scale / camera::EXTENT) as f32;
        #[allow(clippy::cast_precision_loss)]
        Self {
            upper: [(pixel_x >> 16) as f32, (pixel_y >> 16) as f32],
            lower: [(pixel_x & 0xffff) as f32, (pixel_y & 0xffff) as f32],
            tile_ratio,
        }
    }
}

/// One fill-pattern drawable's entry.
///
/// A patterned fill does *not* take the plain fill layout. `FillPatternDrawableUBO` puts the
/// tile's pixel origin and ratio where `FillDrawableUBO` puts its zoom-mix factors, so writing
/// one where the other is expected leaves `tile_ratio` at zero -- and a zero ratio takes the
/// world position out of `patternPos` entirely, so every fragment samples the same point of the
/// sprite. That point is the rectangle's own corner, which in the atlas is the padding around
/// it: alpha 36 of 255, which is why the pattern drew at a seventh of its strength and looked
/// like a wash rather than a texture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PatternDrawableEntry {
    /// Tile-local to clip.
    pub matrix: [f32; 16],
    /// The tile's pixel origin, high half.
    pub pixel_coord_upper: [f32; 2],
    /// The low half.
    pub pixel_coord_lower: [f32; 2],
    /// The tile's extent over the pixels it covers.
    pub tile_ratio: f32,
    /// Mix factors for the from-pattern, the to-pattern and the opacity, in that order.
    pub interpolations: [f32; 3],
}

impl PatternDrawableEntry {
    /// The entry for one tile under a view.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 3],
    ) -> Result<Self, camera::CameraError> {
        // The flat-layer matrix, nudge included: a patterned fill is still a fill, and its
        // outline still has to sort above its triangles.
        let matrix = DrawableEntry::for_tile_with(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            layer_index,
            sub_layer_index,
            [0.0, 0.0],
        )?
        .matrix;
        let origin = PixelOrigin::of(view, z, x, y, wrap);
        Ok(Self {
            matrix,
            pixel_coord_upper: origin.upper,
            pixel_coord_lower: origin.lower,
            tile_ratio: origin.tile_ratio,
            interpolations,
        })
    }
}

/// A fill-pattern layer's consolidated drawable buffer.
#[must_use]
pub fn pack_fill_pattern_drawable_buffer(entries: &[PatternDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = alloc::vec![0u8; stride * entries.len()];
    for (entry, slot) in entries.iter().zip(out.chunks_exact_mut(stride)) {
        let mut at = 0usize;
        let put = |slot: &mut [u8], at: &mut usize, values: &[f32]| {
            for value in values {
                slot[*at..*at + 4].copy_from_slice(&value.to_le_bytes());
                *at += 4;
            }
        };
        put(slot, &mut at, &entry.matrix);
        put(slot, &mut at, &entry.pixel_coord_upper);
        put(slot, &mut at, &entry.pixel_coord_lower);
        put(slot, &mut at, &[entry.tile_ratio]);
        put(slot, &mut at, &entry.interpolations);
    }
    out
}

/// A fill-extrusion layer's consolidated drawable buffer.
#[must_use]
pub fn pack_extrusion_drawable_buffer(entries: &[ExtrusionDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &entry.pixel_coord_upper);
        push_f32s(&mut out, &entry.pixel_coord_lower);
        push_f32s(&mut out, &[entry.height_factor, entry.tile_ratio]);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// The base, height and colour mix factors an extrusion's drawable block carries.
#[must_use]
pub fn extrusion_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
) -> [f32; 3] {
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
        factor("fill-extrusion-base"),
        factor("fill-extrusion-height"),
        factor("fill-extrusion-color"),
    ]
}

/// A fill-extrusion layer's evaluated properties, from its paint and the style light.
///
/// Three of the five blocks are the light, which is why it is a parameter rather than something
/// read from the paint: an extrusion is the first thing here whose colour depends on more than
/// its own layer, and a build that packed the paint and left the light at zero draws every
/// building flat black.
#[must_use]
pub fn fill_extrusion_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
    light: &tessella_style::light::Light,
) -> Vec<u8> {
    let color = light.color;
    let fill = uniform_color(paint, "fill-extrusion-color", zoom);
    pack_fill_extrusion_props(
        [fill.r, fill.g, fill.b, fill.a],
        [color.r, color.g, color.b],
        light.cartesian(),
        uniform_number(paint, "fill-extrusion-base", zoom),
        uniform_number(paint, "fill-extrusion-height", zoom),
        light.intensity,
        uniform_number(paint, "fill-extrusion-vertical-gradient", zoom),
        uniform_number(paint, "fill-extrusion-opacity", zoom),
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
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        extrude_scale: [f32; 2],
        interpolations: [f32; 7],
    ) -> Result<Self, camera::CameraError> {
        let matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;

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
    /// [`camera::CameraError`] when the view has no area.
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
        size: tessella_layout::size::EvaluatedSize,
        is_text: bool,
        alignments: Alignments,
        placement: Placement,
        surface: ProjectionMode,
        variable: bool,
    ) -> Result<Self, camera::CameraError> {
        // The matrices here are the plane's under either projection, and a globe uses only some
        // of them: the consumer bends the anchor itself from `globe_ubo`, so `matrix` and the
        // viewport `label_plane_matrix` never reach an anchored symbol. `coord_matrix` does, and
        // it is the one that has to be right.
        let mut projection = camera::proj_matrix(view)?;
        projection[14] -= f64::from(depth_offset(layer_index, sub_layer_index));
        let tile = camera::multiply(
            &projection,
            &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
        );

        // A label pitched with the map lies flat on the ground, and its `coord_matrix` is built
        // from the tile's own *plane* matrix to get it back to clip. On a sphere the ground is not
        // a plane and that matrix takes the label wherever Mercator would have put it -- which for
        // a line-placed label is off the screen entirely: 36 renderables submitted at Berlin z13
        // and not one dark pixel in the frame.
        //
        // `symbol-placement: line` defaults its rotation to the map and its pitch follows, so this
        // is every street name rather than an unusual case. A globe stands them upright instead,
        // which is what GL JS's globe does below a steep pitch and is a label that reads rather
        // than one that is absent. Laying them on the sphere is the tangent-frame work §13.4
        // describes, and is not a matrix swap.
        let pitch_with_map = effective_pitch(alignments.pitch, surface) == Alignment::Map;
        let rotate_with_map = effective_rotation(alignments.rotation, surface) == Alignment::Map;
        let along_line = alignments.along_line(placement);

        // The identity for a label the producer has already projected. Two cases reach it, and
        // they are the same case: the frame wrote label-plane coordinates into the dynamic
        // buffer, so a plane here would project them a second time.
        //
        // A label walked along a line is the first -- the projection *is* the walk, point by
        // point along the projected road. A variable-anchored one is the second: which of the
        // anchors it took is decided against the collision index, in screen pixels, and the only
        // place that decision can reach the vertices is the position the frame writes. mbgl
        // splits the same way on `hasVariablePlacement`.
        let plane = if along_line || variable {
            camera::identity()
        } else if pitch_with_map {
            camera::label_plane_matrix_on_map(z, view.zoom, view.bearing, rotate_with_map)
        } else {
            camera::label_plane_matrix(&tile, view.width, view.height)
        };

        let coord = if pitch_with_map {
            camera::gl_coord_matrix_on_map(&tile, z, view.zoom, view.bearing, rotate_with_map)
        } else {
            camera::gl_coord_matrix(view.width, view.height)
        };

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| tile[index] as f32),
            label_plane_matrix: core::array::from_fn(|index| plane[index] as f32),
            coord_matrix: core::array::from_fn(|index| coord[index] as f32),
            texsize,
            texsize_icon,
            // Per drawable, not per layer. A symbol layer's two halves draw through different
            // shaders and the flag is what tells one from the other: the shader takes
            // `fontScale = is_text ? size / 24 : size`, because `text-size` names a size in
            // pixels and `icon-size` is a multiplier on a sprite that already has one.
            //
            // Hardcoded true, an icon was scaled as though its sprite were type. With this
            // layer's `text-size` of 11 that is a factor of 11/24, and a 17x16 marker drew at
            // 3x4 where the oracle draws it at 17x16.
            is_text,
            rotate_symbol: alignments.rotate_in_shader(placement),
            pitch_with_map,
            // Which of the shader's three size branches this drawable takes. Both flags were
            // hardcoded true, so every label read the uniform -- and the uniform is the layer's
            // size with no feature in hand, which for a size that varies per feature cannot be
            // evaluated at all and fell back to the spec's sixteen. A capital was set at the
            // size of a village. See `tessella_layout::size::SizeBinding`.
            is_size_zoom_constant: size.zoom_constant,
            is_size_feature_constant: size.feature_constant,
            is_offset: false,
            size_t: size.size_t,
            size: size.size,
            interpolations: [0.0; 5],
        })
    }
}

/// The gamma a symbol's distance field is sampled with, for this camera.
///
/// mbgl's `gammaScale`. One for a label standing up on screen: its glyphs are the size they were
/// laid out at, so the field's ramp needs no correction.
///
/// A label lying flat on the ground is a different picture. Pitched away from the camera it
/// covers fewer screen pixels than it was laid out for, so a fixed ramp is sampled across too few
/// of them and the text thins to nothing at the horizon; scaling by the cosine of the pitch times
/// the camera distance widens the ramp to match. It is the same correction a mipmap makes, done
/// in the shader because a distance field has no mip levels to choose between.
#[must_use]
pub fn symbol_gamma_scale(view: &ViewTransform, pitch: Alignment) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    match pitch {
        Alignment::Map => {
            // Radians. `ViewTransform::pitch` is degrees -- every field of it that is an angle
            // is -- and `cos` is not. At fifteen degrees this read cos(15 radians), which is
            // *negative*: the ramp inverted and every line label drew as an opaque slab with its
            // text over it. At zero the two agree, which is why a map that had only ever been
            // rendered flat could not find it.
            (camera::pitch_radians(view).cos() * camera::camera_to_center_distance(view.height))
                as f32
        }
        Alignment::Viewport => 1.0,
    }
}

/// The pitch alignment a symbol is actually drawn with, which on a globe is not the one the style
/// asked for.
///
/// A label pitched with the map lies on the ground, and `coord_matrix` returns it to clip through
/// the tile's plane matrix. On a sphere there is no such plane, so a globe stands these upright --
/// see [`SymbolDrawableEntry::for_tile`].
///
/// It has to be asked once and used everywhere, because two places read it and they cancel against
/// each other. `symbol_gamma_scale` answers `cos(pitch) * camera_to_center_distance` for a
/// map-pitched label -- 1152 at pitch zero -- and the on-map `coord_matrix` puts the same figure
/// into the fragment's `clip.w`; the shader divides by one and multiplies by the other, so the SDF
/// edge lands where it should. Switch the matrix and not the gamma and the two stop cancelling:
/// the smoothstep narrows by a factor of a thousand, which is a hard step rather than an edge, and
/// street names come out aliased and thin.
#[must_use]
pub const fn effective_pitch(pitch: Alignment, projection: ProjectionMode) -> Alignment {
    match projection {
        ProjectionMode::Globe => Alignment::Viewport,
        ProjectionMode::Mercator => pitch,
    }
}

/// The rotation alignment a symbol is drawn with, which on a globe is not the one the style asked
/// for.
///
/// Same reason as [`effective_pitch`]: the shader turns the quad by the map bearing on top of
/// whatever the label plane already did, and a globe walks its line labels in screen space, where
/// the bearing is already in the angles. Turning them again double-counts it.
///
/// This is the drawable flag only. `Alignments::along_line` reads the style's own value, and a
/// line label that stopped being along-line would lose the identity label plane the walk depends
/// on.
#[must_use]
pub const fn effective_rotation(rotation: Alignment, projection: ProjectionMode) -> Alignment {
    match projection {
        ProjectionMode::Globe => Alignment::Viewport,
        ProjectionMode::Mercator => rotation,
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
/// `gamma_scale` comes from [`symbol_gamma_scale`], which is one for a label standing up on
/// screen and the pitch correction for one lying flat.
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

/// Where a pattern's two images sit in the atlas, and how big the atlas is.
///
/// One of these per drawable, matching `FillPatternTilePropsUBO` — `pattern_from` and
/// `pattern_to` as `vec4` rectangles, then the atlas size as a `vec2` and two words of padding.
/// `LinePatternTilePropsUBO` and the background's are the same shape at their own slots.
///
/// # The rectangles are atlas positions, not sheet coordinates
///
/// mbgl uploads a tile's sprites into a process-wide `DynamicTextureAtlas` and gets back where
/// they landed, so a rectangle here names a place in that shared atlas rather than in the sprite
/// sheet the style pointed at. Two tiles using one sprite name one rectangle; nothing is
/// duplicated per tile but the position map.
///
/// # Both rectangles are the same for a constant pattern, and that is correct
///
/// A pattern that does not vary with zoom still fades — between two copies of one image, which
/// is a no-op that costs nothing and keeps one code path. The capture shows it: for a constant
/// `fill-pattern` the two `vec4`s are byte-identical, twenty-four blocks of the rectangle to
/// twelve of the size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternPlacement {
    /// The image being faded from, as `tlbr` in atlas pixels.
    pub from: [u16; 4],
    /// The image being faded to.
    pub to: [u16; 4],
    /// The atlas's dimensions.
    pub texsize: [u16; 2],
}

/// The atlas rectangle a sprite occupies, as the shader reads it.
///
/// mbgl's `ImagePosition::tlbr()`: the reported rectangle inset by one on every side, so a
/// sampler reading between these corners never touches the border.
///
/// # The padding is two and one of it is reported
///
/// [`atlas::PADDING`] is two — one so linear filtering cannot pull a neighbour's pixels in, and
/// one handed back inside the reported rectangle so a distance field has something to read at
/// the glyph's own edge. So a sprite of width `W` occupies a slot of `W + 4` and
/// [`IconPosition::padded_rect`] reports `W + 2`: the sprite plus a pixel each side, which is
/// the same convention as mbgl's `paddedRect` and takes the same inset.
///
/// Getting this backwards is not visible in a count. `grass_pattern` is fifty by fifty, so its
/// reported rectangle is fifty-two; passing that through unchanged names a fifty-two pixel
/// pattern and samples a one-pixel border of whatever was packed beside it, on every tile.
///
/// [`IconPosition`]: tessella_glyph::sprite::IconPosition
/// [`IconPosition::padded_rect`]: tessella_glyph::sprite::IconPosition::padded_rect
/// [`atlas::PADDING`]: tessella_glyph::atlas::PADDING
#[must_use]
pub fn atlas_rect(position: &tessella_glyph::sprite::IconPosition) -> [u16; 4] {
    let rect = position.padded_rect;
    #[allow(clippy::cast_possible_truncation)]
    [
        (rect.x + 1) as u16,
        (rect.y + 1) as u16,
        (rect.x + rect.width).saturating_sub(1) as u16,
        (rect.y + rect.height).saturating_sub(1) as u16,
    ]
}

/// Where a pattern's two images sit, given the atlas each was packed into.
///
/// `from` and `to` are the pair [`tessella_style::crossfade::faded`] chose — the level being
/// left and the level being entered. They are the same sprite for a pattern that does not vary
/// with zoom, and the block carries the rectangle twice, which is what the oracle does.
///
/// `None` when either image is missing from the atlas. A pattern naming a sprite the sheet does
/// not have has nothing to draw, and drawing it against a stale rectangle would sample whatever
/// was packed there instead — a different sprite, at full opacity, with nothing reporting it.
#[must_use]
pub fn pattern_placement(
    from: Option<&tessella_glyph::sprite::IconPosition>,
    to: Option<&tessella_glyph::sprite::IconPosition>,
    texsize: [u16; 2],
) -> Option<PatternPlacement> {
    Some(PatternPlacement {
        from: atlas_rect(from?),
        to: atlas_rect(to?),
        texsize,
    })
}

/// A background pattern's block, which is a third shape again.
///
/// `BackgroundPatternPropsUBO` splits each rectangle into a `tl` and a `br` `vec2` where a fill
/// and a line carry one `vec4`, adds the images' display sizes, and puts the crossfade's two
/// scales and its mix in the last sixteen bytes beside the layer's opacity. Sixty-four bytes,
/// like a line's, arranged differently.
///
/// It is also the *props* block rather than a tile-props one: a background has no tiles to vary
/// over, so there is one of these for the layer where a fill writes one per drawable.
///
/// # Display size is not the rectangle's size
///
/// `pattern_size` is the sprite's size in *logical* pixels — its own dimensions over its pixel
/// ratio — where the rectangle is in atlas pixels. They agree at a ratio of one and diverge on a
/// retina sheet, and using the rectangle's width for both draws a `@2x` pattern at half scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackgroundPatternPlacement {
    /// Where the two images sit, top-left and bottom-right.
    pub placement: PatternPlacement,
    /// The images' sizes in logical pixels, `from` then `to`.
    pub display: [[f32; 2]; 2],
    /// The crossfade.
    pub crossfade: tessella_style::crossfade::Crossfade,
    /// `background-opacity`.
    pub opacity: f32,
}

/// Packs `BackgroundPatternPropsUBO`, one block for the layer.
#[must_use]
pub fn pack_background_pattern_props(entry: &BackgroundPatternPlacement) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    // Each rectangle as its two corners rather than as one four-vector.
    for rect in [entry.placement.from, entry.placement.to] {
        for value in rect {
            out.extend_from_slice(&f32::from(value).to_le_bytes());
        }
    }
    for size in entry.display {
        for value in size {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    for value in [
        entry.crossfade.from_scale,
        entry.crossfade.to_scale,
        entry.crossfade.t,
        entry.opacity,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    debug_assert_eq!(out.len(), 64);
    out
}

/// The sprite's size in logical pixels, which is what `pattern_size` carries.
///
/// mbgl's `ImagePosition::displaySize`. Its own dimensions over its pixel ratio, so a `@2x`
/// sprite occupying a hundred atlas pixels is fifty logical ones and draws at the size the style
/// author drew it.
#[must_use]
pub fn display_size(position: &tessella_glyph::sprite::IconPosition) -> [f32; 2] {
    let (width, height) = position.display_size();
    #[allow(clippy::cast_possible_truncation)]
    [width as f32, height as f32]
}

/// A line pattern's block, which carries more than a fill's.
///
/// `LinePatternTilePropsUBO` is sixty-four bytes to a fill's forty-eight, and the difference is
/// the two fields a line needs and a fill does not: a `scale` vector and the fade itself.
///
/// # Where each part comes from
///
/// `scale` is `[pixel_ratio, 1 / pixels_to_tile_units(1, intZoom), from_scale, to_scale]`. The
/// second is how many tile units a pixel is worth at this tile's own level, inverted — a
/// pattern is authored in pixels and drawn in tile units, and without it the pattern's size
/// tracks the zoom instead of the ground. The last two are the crossfade's, and they are what
/// keeps a pattern the same size on the ground while the image under it changes: the level
/// being left is drawn at twice or half the size of the one being entered.
///
/// `fade` is the crossfade's `t` — the first place in this stream where the mix reaches a
/// shader at all. A fill's block has nowhere to put it, which is why a fill's pattern cannot
/// visibly fade and a line's can.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinePatternPlacement {
    /// Where the two images sit, and how big the atlas is.
    pub placement: PatternPlacement,
    /// The device pixel ratio.
    pub pixel_ratio: f32,
    /// Tile units per pixel at this tile's own level, inverted.
    pub units_per_pixel: f32,
    /// The crossfade.
    pub crossfade: tessella_style::crossfade::Crossfade,
}

/// Packs `LinePatternTilePropsUBO`, one entry per drawable.
#[must_use]
pub fn pack_line_pattern_tile_props(entries: &[LinePatternPlacement]) -> Vec<u8> {
    const STRIDE: usize = 64;
    let mut out = Vec::with_capacity(entries.len() * STRIDE);
    for entry in entries {
        for rect in [entry.placement.from, entry.placement.to] {
            for value in rect {
                out.extend_from_slice(&f32::from(value).to_le_bytes());
            }
        }
        for value in [
            entry.pixel_ratio,
            entry.units_per_pixel,
            entry.crossfade.from_scale,
            entry.crossfade.to_scale,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        for value in entry.placement.texsize {
            out.extend_from_slice(&f32::from(value).to_le_bytes());
        }
        out.extend_from_slice(&entry.crossfade.t.to_le_bytes());
        out.extend_from_slice(&0f32.to_le_bytes());
    }
    debug_assert_eq!(out.len(), entries.len() * STRIDE);
    out
}

/// Packs `FillPatternTilePropsUBO`, one entry per drawable.
///
/// The rectangles are written as `f32` although they are whole pixels: the shader declares
/// `vec4`, and DR-6's generated layout says so. Writing them as integers would pack the same
/// number of bytes and be read as denormals.
#[must_use]
pub fn pack_pattern_tile_props(entries: &[PatternPlacement]) -> Vec<u8> {
    const STRIDE: usize = 48;
    let mut out = Vec::with_capacity(entries.len() * STRIDE);
    for entry in entries {
        for rect in [entry.from, entry.to] {
            for value in rect {
                out.extend_from_slice(&f32::from(value).to_le_bytes());
            }
        }
        for value in entry.texsize {
            out.extend_from_slice(&f32::from(value).to_le_bytes());
        }
        // pad1 and pad2, which the block declares and the shader ignores. Zero rather than
        // uninitialized: the buffer is compared byte for byte against the oracle's.
        out.extend_from_slice(&0f32.to_le_bytes());
        out.extend_from_slice(&0f32.to_le_bytes());
    }
    debug_assert_eq!(out.len(), entries.len() * STRIDE);
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

/// Packs `FillExtrusionPropsUBO`.
///
/// Five sixteen-byte blocks, and three of them are the *light*. An extrusion is the first thing
/// in this build whose colour depends on more than its paint: `light-color`, `light-position`
/// and `light-intensity` come from the style's top-level `light` block, and a wall's shade is
/// the dot product of its normal with that direction. A build that packed the paint and left the
/// light at zero draws every building flat black.
///
/// `light_position` is the *cartesian* form of the style's spherical `[radial, azimuth, polar]`,
/// and it is rotated by the negated bearing when the light's anchor is `viewport` rather than
/// `map` — mbgl's `FillExtrusionBucket::lightPosition`. Anchored to the map it does not move with
/// the camera, which is what makes a city look lit rather than painted.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn pack_fill_extrusion_props(
    color: [f32; 4],
    light_color: [f32; 3],
    light_position: [f32; 3],
    base: f32,
    height: f32,
    light_intensity: f32,
    vertical_gradient: f32,
    opacity: f32,
) -> Vec<u8> {
    const SIZE: usize = 80;
    let mut out = Vec::with_capacity(SIZE);
    push_f32s(&mut out, &color);
    push_f32s(
        &mut out,
        &[light_color[0], light_color[1], light_color[2], 0.0],
    );
    push_f32s(
        &mut out,
        &[
            light_position[0],
            light_position[1],
            light_position[2],
            base,
        ],
    );
    push_f32s(
        &mut out,
        &[height, light_intensity, vertical_gradient, opacity],
    );
    // `fade`, `from_scale` and `to_scale` belong to the pattern path, which no golden carries.
    // Written as the identity rather than left out: the block is a fixed size and the shader
    // reads every word of it.
    push_f32s(&mut out, &[0.0, 1.0, 1.0, 0.0]);
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// The uniform slot a mesh drawable's placement is written to.
///
/// # The first slot this build chooses rather than transcribes
///
/// Every other slot in this crate comes out of the generated table, evaluated from mbgl's own
/// chain of anonymous enums under DR-6. mbgl has no mesh layer, so there is no slot to read — and
/// this is the one place a number is picked here instead.
///
/// It is picked with a gap rather than adjacent to mbgl's range, and the gap is asserted. mbgl's
/// slots run zero to eight with `MAX_UBO_COUNT_PER_SHADER` at nine; taking nine would collide
/// the moment mbgl added one shader's worth of buffer. Sixteen leaves room for mbgl to grow to
/// fifteen, and the compile-time assertion below is what turns a future collision into a build
/// failure rather than two things writing the same slot.
pub const MESH_DRAWABLE_UBO: u32 = 16;

const _: () = assert!(
    MESH_DRAWABLE_UBO > tessella_capture_abi::generated::ubo_slots::MAX_UBO_COUNT_PER_SHADER,
    "mbgl's slots have grown into the one this build chose for meshes"
);

/// Bytes one mesh placement occupies in the layer's consolidated buffer.
///
/// A matrix and nothing else. It was a matrix and a height factor until the factor turned out
/// not to be a conversion — see [`MeshPlacement`] — and a `mat4` is already the sixteen-byte
/// alignment every block on this protocol has, so nothing pads it. Sized here rather than read
/// from a generated layout for the reason the slot is.
pub const MESH_DRAWABLE_UBO_SIZE: usize = 64;

/// Converts a height in metres into the vertical unit the tile matrix works in.
///
/// mbgl's `heightFactor`, `-numTiles / tileSize_D / 8.0`, and what it is *for* is narrower than
/// it looks: it walks a pattern up an extrusion's wall. The whole shader set uses it once, in
/// `fill_extrusion_pattern`'s `vec2 pos = vec2(edgedistance, z * drawable.height_factor)`.
///
/// It is **not** the conversion from metres to the shader's z. Nothing converts: the position
/// shader passes the height straight in, `gl_Position = matrix * vec4(pos, z, 1.0)`, because
/// `getWorldToCamera` has already scaled the matrix's third column by `pixelsPerMeter`. Reading
/// it as the conversion scales a building by the tile count — four thousand at z14 — which is
/// how it was read here until a pitched render showed one swallowing the map.
///
/// There is no latitude term, and that is not an omission: the pattern walks in the same
/// Mercator-scaled units the geometry is drawn in.
#[must_use]
pub fn height_factor(z: u8) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        -(f64::from(1u32 << z.min(30)) / 512.0 / 8.0) as f32
    }
}

/// Where one mesh tile goes.
///
/// # A matrix and nothing else, because the matrix already converts
///
/// This used to carry `height_factor` beside the matrix, described as what a height in metres
/// multiplies by. That was wrong, and wrong by a factor of four thousand at z14.
///
/// mbgl's fill-extrusion shader settles it:
/// `gl_Position = drawable.matrix * vec4(in_position + decimals, z, 1.0)`, with `z` the height in
/// **metres** and no conversion in front of it. It needs none: `getWorldToCamera` scales the
/// matrix's third column by `pixelsPerMeter`, precisely so a height in metres and a position in
/// tile units can share one matrix. `heightFactor` appears once in the whole shader set, in the
/// *pattern* variant, walking a texture up a wall — `vec2(edgedistance, z * height_factor)` —
/// which is not a conversion of the position and has no meaning for a mesh at all.
///
/// The measurement behind the original claim still holds: a buildings mesh really is tile units
/// in x and y and metres in z, across 972 nodes of a real store. What was wrong was the
/// conversion, not the convention — and the conversion is the matrix's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshPlacement {
    /// Tile-local to clip, as every other drawable matrix on this protocol is. Its third column
    /// carries `pixelsPerMeter`, so a mesh's metres go in unscaled.
    pub matrix: [f32; 16],
}

impl MeshPlacement {
    /// The placement of one model tile under a view.
    ///
    /// The matrix is [`DrawableEntry`]'s, not a second one computed alongside it. A mesh sits in
    /// the same tile space as every other layer and takes the same layer and sublayer depth
    /// bias, so a parallel implementation here would be a second thing to keep in step with
    /// mbgl's bias arithmetic — and the two would agree until the day they did not.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
    ) -> Result<Self, camera::CameraError> {
        let entry = DrawableEntry::for_tile(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            layer_index,
            sub_layer_index,
        )?;
        Ok(Self {
            matrix: entry.matrix,
        })
    }
}

/// Packs the placement of each mesh drawable in a layer.
///
/// One entry per mesh in the layer's draw order, at `stride`, which is what a consolidated
/// uniform buffer is: the consumer reads entry `i` for the `i`-th mesh named by that layer's
/// `ViewUse` records.
///
/// # Why a mesh needs a matrix at all when its glTF carries node transforms
///
/// The node matrices place a building *within its tile*. Nothing in the file says where the tile
/// is, what the camera is doing, or how a metre relates to a tile unit at this zoom — and none of
/// that is the asset's to know. It is the producer's, which is the whole division this stream
/// draws: the consumer's loader owns what the mesh is made of, and the producer owns whether and
/// where it is drawn.
#[must_use]
pub fn pack_mesh_drawable_buffer(placements: &[MeshPlacement], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = alloc::vec![0u8; placements.len() * stride];
    for (placement, slot) in placements.iter().zip(out.chunks_mut(stride)) {
        for (index, value) in placement.matrix.iter().enumerate() {
            slot[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// Packs `RasterDrawableUBO`: one matrix, and nothing else.
///
/// The smallest drawable buffer of any layer, because a raster tile carries no per-feature
/// anything — the image is a texture and the colour adjustment is the layer's, so what is left
/// per drawable is where the tile goes.
#[must_use]
pub fn pack_raster_drawable_buffer(matrices: &[[f32; 16]], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = alloc::vec![0u8; matrices.len() * stride];
    for (matrix, slot) in matrices.iter().zip(out.chunks_mut(stride)) {
        for (index, value) in matrix.iter().enumerate() {
            slot[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// Packs `RasterEvaluatedPropsUBO`.
///
/// `tl_parent`, `scale_parent` and `fade_t` describe a tile fading in over the parent standing in
/// for it, which is what `raster-fade-duration` times. They are written as *not fading* — the
/// parent at the origin, unscaled, the fade complete — because a still frame is what every
/// capture is and a value invented for the transition would be a number on the wire nothing
/// produced.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn pack_raster_props(
    colour: RasterColour,
    opacity: f32,
    brightness_low: f32,
    brightness_high: f32,
    buffer_scale: f32,
) -> Vec<u8> {
    const SIZE: usize = 64;
    let mut out = Vec::with_capacity(SIZE);
    push_f32s(&mut out, &colour.spin_weights);
    // The parent's top-left and scale: no parent, so the identity.
    push_f32s(&mut out, &[0.0, 0.0, 1.0, buffer_scale]);
    push_f32s(
        &mut out,
        &[
            // Fade complete.
            1.0,
            opacity,
            brightness_low,
            brightness_high,
            colour.saturation_factor,
            colour.contrast_factor,
            0.0,
            0.0,
        ],
    );
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// A raster layer's evaluated properties, from its resolved paint.
#[must_use]
pub fn raster_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    let colour = RasterColour {
        spin_weights: raster::spin_weights(uniform_number(paint, "raster-hue-rotate", zoom)),
        saturation_factor: raster::saturation_factor(uniform_number(
            paint,
            "raster-saturation",
            zoom,
        )),
        contrast_factor: raster::contrast_factor(uniform_number(paint, "raster-contrast", zoom)),
    };
    pack_raster_props(
        colour,
        uniform_number(paint, "raster-opacity", zoom),
        uniform_number(paint, "raster-brightness-min", zoom),
        uniform_number(paint, "raster-brightness-max", zoom),
        1.0,
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

/// Builds the anchored bend's block for a tile, depth bias included.
///
/// The direct bend puts the layer's depth nudge in the placement's `[14]` and lets the shader carry
/// it out to clip `z`. There is no placement here -- the coefficients *are* clip space -- so the
/// nudge is added to the anchor's `z` instead, which is the same arithmetic one step earlier.
///
/// Scaled to the frustum for the reason [`tile_matrix`] gives: a globe's depth span falls from 1.7
/// at z4 to 0.055 at z14, so an absolute nudge is most of the range up there.
#[must_use]
pub fn globe_bend_block(
    view: &ViewTransform,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
    layer_index: i32,
    sub_layer_index: i32,
) -> GlobeBendUbo {
    let bend = globe::anchored_bend(view, z, x, y, wrap);
    let (near, far) = globe::depth_range(view);
    let bias = -f64::from(depth_offset(layer_index, sub_layer_index)) * (far - near);

    // The bias goes on before the scale, not after. Scaling every coefficient multiplies `w` too,
    // and the offset that reaches NDC is `bias / w` -- so a bias added afterwards would be divided
    // by the larger `w` and land smaller by exactly that factor, which is a layer separation
    // quietly reduced to nothing.
    let scale = globe::clip_w_scale(view);
    #[allow(clippy::cast_possible_truncation)]
    let row = |v: [f64; 4]| -> [f32; 4] { core::array::from_fn(|i| (v[i] * scale) as f32) };
    let mut anchor = bend.anchor;
    anchor[2] += bias;
    GlobeBendUbo {
        anchor: row(anchor),
        d_u: row(bend.d_u),
        d_v: row(bend.d_v),
        d_uu: row(bend.d_uu),
        d_vv: row(bend.d_vv),
        d_uv: row(bend.d_uv),
    }
}

/// Packs a layer's anchored-bend blocks, one per drawable, in the order the drawables were sent.
///
/// The same shape as [`pack_drawable_buffer`]: the consumer indexes it by the drawable's own UBO
/// index, so a gap would put every later drawable on its neighbour's tile.
#[must_use]
pub fn pack_globe_bend_buffer(blocks: &[GlobeBendUbo]) -> Vec<u8> {
    let stride = GlobeBendUbo::STRIDE as usize;
    let mut out = Vec::with_capacity(blocks.len() * stride);
    for block in blocks {
        out.extend_from_slice(&block.to_bytes());
    }
    out
}
