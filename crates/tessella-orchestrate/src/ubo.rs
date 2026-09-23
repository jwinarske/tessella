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
use tessella_capture_abi::envelope::{Span, TextureFilter, UboUpdate, ViewId, WireRecord};
use tessella_capture_abi::generated::ubo_layouts;
use tessella_capture_abi::generated::ubo_slots;
use tessella_capture_abi::globe_ubo::GlobeBendUbo;
use tessella_capture_abi::ring::{Full, Producer};
use tessella_layout::raster::{self, RasterColor};
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
            // The plane's own nudge, unscaled, because `globe::clip_matrix` puts `w` in the
            // plane's convention: what reaches NDC is `nudge / w`, and the two `w`s now agree at
            // the point under the camera. So the same number separates the same layers by the
            // same amount on either projection.
            //
            // It used to be scaled to the frustum -- `(far - near)` -- which was a compensation
            // for a `w` measured in sphere radii, and it outlived the thing it compensated for.
            // The arithmetic says how badly: at z15 the span is 0.017 against a `w` of 1.5e-4, so
            // a nudge of `depth_offset * span` reaches 4.7 in NDC at style layer 28 and 11.3 at
            // layer 68, against the plane's 6e-5. Every family but the extrusions is painter
            // ordered with the depth test off, so nothing read it and nothing noticed; an
            // extrusion is the one that resolves in depth, and it was clipped through the near
            // plane by its own layer separation.
            placement[14] = -f64::from(depth);
            Ok(placement)
        }
    }
}

/// [`tile_matrix`] through the near-clipped projection, with no sublayer nudge.
///
/// What mbgl draws a fill-extrusion through, and only a fill-extrusion:
/// `FillExtrusionLayerTweaker` passes `nearClipped = true` where every other tweaker leaves it
/// false. See [`camera::near_clipped_proj_matrix`] for what the plane buys and what it cost here.
///
/// A globe takes the plain path. Its tiles are placed by the sphere's own clip matrix rather than
/// by a perspective projection, so there is no near plane in it to move.
fn near_clipped_tile_matrix(
    view: &ViewTransform,
    projection: ProjectionMode,
    z: u8,
    x: u32,
    y: u32,
    wrap: i32,
) -> Result<camera::Mat4, camera::CameraError> {
    match projection {
        ProjectionMode::Mercator => Ok(camera::multiply(
            &camera::near_clipped_proj_matrix(view)?,
            &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
        )),
        ProjectionMode::Globe => tile_matrix(view, projection, z, x, y, wrap, 0.0),
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

/// [`tile_matrix`] through the projection snapped to the pixel grid.
///
/// For the two families mbgl draws through `alignedProjMatrix`: raster and hillshade. See
/// [`camera::aligned_proj_matrix`]. A globe has no pixel grid to snap a tile to, so it takes the
/// plain placement.
///
/// # Errors
///
/// [`camera::CameraError`] when the view has no area.
pub fn aligned_tile_matrix(
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
            let mut clip = camera::aligned_proj_matrix(view)?;
            clip[14] -= f64::from(depth);
            Ok(camera::multiply(
                &clip,
                &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
            ))
        }
        ProjectionMode::Globe => tile_matrix(view, projection, z, x, y, wrap, depth),
    }
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

    /// As [`Self::for_tile`], through the projection snapped to the pixel grid.
    ///
    /// Raster and hillshade only, which are the tweakers mbgl hands `alignedProjMatrix`. See
    /// [`aligned_tile_matrix`].
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile_aligned(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
    ) -> Result<Self, camera::CameraError> {
        let matrix = aligned_tile_matrix(
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
            interpolations: [0.0, 0.0],
        })
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
        Self::for_tile_translated(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            layer_index,
            sub_layer_index,
            interpolations,
            [0.0, 0.0],
        )
    }

    /// As [`Self::for_tile_with`], offset by the layer's paint translate.
    ///
    /// `translate` is in tile units; [`paint_translate`] is what turns the property's screen
    /// pixels into them. Applied to the tile's matrix rather than folded into the geometry,
    /// which is what lets one set of vertices serve a layer that moves with the zoom.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    #[allow(clippy::too_many_arguments)]
    pub fn for_tile_translated(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        layer_index: i32,
        sub_layer_index: i32,
        interpolations: [f32; 2],
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        let mut matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut matrix, translate[0], translate[1], 0.0);
        }

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            interpolations,
        })
    }

    /// The entry for a background standing in for the oracle's clear.
    ///
    /// See [`crate::tile::background_covers_viewport`] for which background reaches this and why.
    /// There is no clear color on this wire, so the equivalent of clearing the renderable is a
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
    /// and the color pass that follows draw the *same* surfaces, and the color pass has to
    /// compare equal against what the depth pass wrote. One step of [`DEPTH_EPSILON`] is 9.3e-7
    /// of clip depth after the divide, against the 2e-4 that a 150-meter building spans in
    /// total, and `depth_probe`'s third phase puts the tolerance below that: at a separation of
    /// 1e-6 the color pass is rejected entirely and the buildings vanish.
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
        let matrix = near_clipped_tile_matrix(view, projection, z, x, y, wrap)?;

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            interpolations: [0.0, 0.0],
        })
    }
}

/// A layer's paint translate, in the tile units a drawable's matrix takes.
///
/// mbgl's `translatedMatrix`. The property is a pair of screen pixels and the matrix takes tile
/// units, so the conversion is mbgl's `pixelsToTileUnits`: the tile's own extent over the pixels
/// it covers at this zoom. A tile further from the view's zoom covers fewer pixels, so the same
/// offset is more of its units.
///
/// A `viewport` anchor turns with the camera, because the offset is a direction on the screen; a
/// `map` anchor does not, because it is a direction on the ground. `map` is the default and is
/// what a style asking for a fake third dimension uses -- a building top offset up and left of
/// its footprint has to stay there when the map turns.
///
/// Zero for a layer that does not name one, which is nearly all of them, and zero for a pair this
/// cannot read as two numbers.
///
/// # A line's x offset is not exact yet
///
/// `symbol_translate_p` and the circle and fill-extrusion halves of `paint_translate_p` hold at
/// zero against the oracle. A *line* does not: an offset along x leaves about 130 pixels in a
/// band at one tile edge, where the oracle draws road and this draws the ground under it.
///
/// What is known: the arithmetic is not the suspect. `line-translate: [0, 14]` is exactly zero,
/// and rendering the failing camera with either `TSF_NO_STENCIL` or `TSF_NO_SCISSOR` is also
/// exactly zero -- so this over-clips geometry the offset moved, rather than moving it wrongly.
/// It is not a seam gap either: two pixels of offset leave 67 differing pixels where fourteen
/// leave 98, which does not scale the way a gap would.
#[must_use]
pub fn paint_translate(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    translate: &str,
    anchor: &str,
    view: &ViewTransform,
    tile_z: u8,
) -> [f64; 2] {
    let pair = paint
        .get(translate)
        .and_then(|property| property.expression.evaluate(Some(view.zoom), None).ok());
    let Some(tessella_style::Value::Array(pair)) = pair else {
        return [0.0, 0.0];
    };
    let [Some(x), Some(y)] = [0, 1].map(|index| match pair.get(index) {
        Some(tessella_style::Value::Number(value)) => Some(*value),
        _ => None,
    }) else {
        return [0.0, 0.0];
    };
    if x == 0.0 && y == 0.0 {
        return [0.0, 0.0];
    }

    let viewport = matches!(
        paint
            .get(anchor)
            .and_then(|property| property.expression.evaluate(Some(view.zoom), None).ok()),
        Some(tessella_style::Value::String(ref name)) if name == "viewport"
    );
    let (x, y) = if viewport {
        // The screen direction taken back to the ground, which is the camera's rotation undone.
        let angle = -camera::bearing_radians(view);
        (
            x * angle.cos() - y * angle.sin(),
            x * angle.sin() + y * angle.cos(),
        )
    } else {
        (x, y)
    };

    let units = camera::EXTENT / (512.0 * 2f64.powf(view.zoom - f64::from(tile_z)));
    [x * units, y * units]
}

/// The two zoom-mix factors a fill drawable's UBO carries.
///
/// # The pair is not the same for both sublayers
///
/// A fill layer draws twice, and the two shaders read different properties: the triangles take
/// `fill-color` and `fill-opacity`, the outline takes `fill-outline-color` and `fill-opacity`.
/// They share the buffer and the opacity, and differ in the color — so a single pair used for
/// both would give the outline the fill's color ramp. mbgl builds them separately in
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

/// Whether a fill's outline is drawn as a polyline rather than as line primitives.
///
/// mbgl's condition, from the `MLN_TRIANGULATE_FILL_OUTLINES` arm of `render_fill_layer.cpp`:
/// the triangulated outline is used in the plain-fill branch -- so not for a patterned fill --
/// and only when the outline is not data-driven:
///
/// ```text
/// dataDrivenOutline = !outlineColor.isConstant() || !opacity.isConstant()
/// ```
///
/// The reason for that last part is the shader: `FillOutlineTriangulatedShader` declares two
/// attributes, the line family's position and data, and no paint of its own. Its color comes
/// from the layer's `outline_color` uniform, so a color that varies per feature has nowhere to
/// travel and the line-primitive path has to take it.
///
/// `has_pattern` is whether the layer resolved a pattern for this frame.
#[must_use]
pub fn fill_outline_triangulates(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    has_pattern: bool,
) -> bool {
    if has_pattern {
        return false;
    }
    let constant = |name: &str| {
        paint
            .get(name)
            .is_none_or(|property| !matches!(property.binding, Binding::Attribute { .. }))
    };
    constant("fill-outline-color") && constant("fill-opacity")
}

/// Whether a fill layer's outline draws *under* its triangles rather than over them.
///
/// mbgl's, from `render_fill_layer.cpp`:
///
/// ```text
/// builder->setSubLayerIndex(unevaluated.get<FillOutlineColor>().isUndefined() ? 2 : 0);
/// ```
///
/// A fill's triangles are sublayer 1. An outline the style did not ask for is sublayer 2 -- it
/// is the fill's own antialiasing, drawn in the fill's color over the fill's edge, and it
/// belongs on top. An outline the style *did* ask for is sublayer 0, underneath: it is a
/// different color from the fill, and mbgl draws it first so the fill covers its inner half.
/// Drawn on top instead, that inner half sits over the fill and the line reads twice as wide.
///
/// `unevaluated` is the style's own value, so this asks whether the layer wrote the property --
/// not what it evaluates to. A layer that sets `fill-outline-color` to the same color as its
/// fill still takes the first branch.
#[must_use]
pub fn fill_outline_under_fill(layer: &tessella_style::Layer) -> bool {
    layer.paint.contains_key("fill-outline-color")
}

/// Whether a fill layer draws an outline at all.
///
/// mbgl's `doOutline`, from `render_fill_layer.cpp`:
///
/// ```text
/// doOutline = evaluated.get<FillAntialias>() &&
///             (unevaluated.get<FillPattern>().isUndefined() ||
///              unevaluated.get<FillOutlineColor>().isUndefined())
/// ```
///
/// Two rules in one expression. `fill-antialias` is the plain one: a fill's outline is its
/// antialiasing, so turning the antialiasing off is turning the outline off, and the layer draws
/// one drawable rather than two.
///
/// The second is stranger and is mbgl's own comment -- "Outline does not default to fill in the
/// pattern case". A patterned fill whose outline color the style *did* write asks for a color
/// the pattern shaders have no uniform for, and rather than draw it in the wrong color mbgl
/// draws no outline. A patterned fill that wrote no outline color still gets one, because then
/// the outline is the pattern itself.
///
/// Both halves of the second rule read the style's own keys rather than the resolved map, which
/// is what `unevaluated` means: the question is what the style wrote, not what it evaluates to.
///
/// The antialias is read at zoom zero for the reason [`uniform_opacity`] gives -- it decides how
/// many drawables a layer becomes rather than what color it is, and that is settled where the
/// bucket is built. It is data-constant in the spec, so the only thing this misses is a style
/// that animates it with the camera.
#[must_use]
pub fn fill_draws_outline(
    layer: &tessella_style::Layer,
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
) -> bool {
    let antialias = uniform_number(paint, "fill-antialias", 0.0) != 0.0;
    antialias
        && (!layer.paint.contains_key("fill-pattern")
            || !layer.paint.contains_key("fill-outline-color"))
}

/// One triangulated fill-outline drawable's entry.
///
/// `FillOutlineTriangulatedDrawableUBO`: the matrix and the ratio, and nothing else. The ratio
/// is the line family's -- screen pixels per tile unit, inverted -- because the outline is
/// extruded sideways in tile units exactly as a line layer's quad is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FillOutlineTriangulatedEntry {
    /// Tile-local to clip.
    pub matrix: [f32; 16],
    /// Screen pixels per tile unit, inverted.
    pub ratio: f32,
}

impl FillOutlineTriangulatedEntry {
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
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        let mut matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;
        // The layer's `fill-translate`, which the outline takes as much as the interior does:
        // mbgl builds one `translatedMatrix` in `FillLayerTweaker::execute` and hands it to
        // every drawable the layer has. Left off here, the outline stayed on the footprint
        // while the fill moved off it, so a building top's outline sat two pixels down and
        // right of its own edge -- drawn under the fill, half of it was then covered and the
        // other half ran along the wrong side.
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut matrix, translate[0], translate[1], 0.0);
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            ratio: line_ratio(z, view.zoom),
        })
    }
}

/// Packs a layer's triangulated outline blocks at the fill union's stride.
#[must_use]
pub fn pack_fill_outline_triangulated_buffer(
    entries: &[FillOutlineTriangulatedEntry],
    stride: u32,
) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &[entry.ratio]);
        out.resize(start + stride, 0);
    }
    out
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
    /// Mix factors for color, blur, opacity, gap width, offset and width, in that order.
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
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        let mut matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;
        // The layer's own offset -- see `paint_translate`. Applied to the tile's matrix rather
        // than to its geometry, so one set of vertices still serves every zoom the layer is
        // drawn at.
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut matrix, translate[0], translate[1], 0.0);
        }
        let matrix = matrix;

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
/// The order is the UBO's, which is not the property table's: color, blur, opacity, gap width,
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

/// A gradient line layer's consolidated drawable buffer.
///
/// `LineGradientDrawableUBO`: the plain block with its first mix factor, the color's, left out --
/// the ramp is the color, so there is no attribute for a factor to mix. Blur, opacity, gap width,
/// offset and width follow the ratio at 68 through 84, one slot earlier than a plain line's.
#[must_use]
pub fn pack_line_gradient_drawable_buffer(entries: &[LineDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &[entry.ratio]);
        push_f32s(&mut out, &entry.interpolations[1..]);
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
/// evaluated values instead would put one feature's color into a layer-wide uniform.
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

/// A color-typed property's uniform value, falling back to its spec default.
pub(crate) fn uniform_color(
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

/// A list-valued color property's uniform value: every color it resolves to, in order.
///
/// The spec's `colorArray`, which is one color or a list of them -- so a style that writes a
/// single color and one that writes a list of one are the same thing here, as they are in mbgl.
/// Empty only when the layer does not set the property and has no default to fall back on; the
/// caller pads.
pub(crate) fn uniform_colors(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> Vec<Color> {
    let Some(property) = paint.get(name) else {
        return Vec::new();
    };
    let default = match property.spec.default {
        DefaultValue::Color(color) => alloc::vec![color],
        _ => Vec::new(),
    };
    let Some(value) = uniform_value(property, zoom) else {
        return default;
    };
    // A list, or the one value a list of one would hold.
    if let Some(items) = value.as_array()
        && !matches!(value, tessella_style::value::Value::Color(_))
    {
        let colors: Vec<Color> = items
            .iter()
            .filter_map(|item| tessella_style::property::as_color(item).ok())
            .collect();
        if colors.len() == items.len() {
            return colors;
        }
    }
    tessella_style::property::as_color(&value)
        .map(|color| alloc::vec![color])
        .unwrap_or(default)
}

/// A list-valued number property's uniform value: every number it resolves to, in order.
///
/// [`uniform_colors`]'s counterpart, for the spec's `numberArray`.
pub(crate) fn uniform_numbers(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    name: &str,
    zoom: f64,
) -> Vec<f32> {
    let Some(property) = paint.get(name) else {
        return Vec::new();
    };
    #[allow(clippy::cast_possible_truncation)]
    let default = match property.spec.default {
        DefaultValue::Number(number) => alloc::vec![number as f32],
        _ => Vec::new(),
    };
    let Some(value) = uniform_value(property, zoom) else {
        return default;
    };
    #[allow(clippy::cast_possible_truncation)]
    if let Some(items) = value.as_array() {
        let numbers: Vec<f32> = items
            .iter()
            .filter_map(|item| item.as_number().map(|number| number as f32))
            .collect();
        if numbers.len() == items.len() {
            return numbers;
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    value
        .as_number()
        .map_or(default, |number| alloc::vec![number as f32])
}

/// A number-typed property's uniform value, falling back to its spec default.
/// A uniform property's value at zoom zero, for a decision that is not per frame.
///
/// `fill-extrusion-opacity` decides how many drawables a layer becomes rather than what color
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
/// turns a height in meters into the tile-space z the shader raises a wall to — and the tile's
/// pixel coordinate, split across two floats because the shader needs more precision in it than
/// one `f32` holds at a high zoom.
///
/// Packing a fill's entry into this shape is not a near miss: the mix factors land where the
/// pixel coordinate and the height factor belong, so `height_factor` reads as whatever the
/// color interpolation happened to be — zero, for a constant color — and every building comes
/// out flat. It draws, and it draws a fill layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtrusionDrawableEntry {
    /// Tile-local to clip.
    pub matrix: [f32; 16],
    /// The tile's pixel coordinate, high and low halves.
    pub pixel_coord_upper: [f32; 2],
    /// The low halves.
    pub pixel_coord_lower: [f32; 2],
    /// What a height in meters multiplies by.
    pub height_factor: f32,
    /// Tile units per pixel, inverted.
    pub tile_ratio: f32,
    /// Mix factors for base, height and color, in that order.
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
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        // `for_tile_3d`, not `for_tile_with`: mbgl's `depthModeFor3D` has no sublayer term, and
        // applying the flat-layer one here separates this drawable's depth from the depth pass
        // that precedes it by more than the comparison tolerates.
        // The near-clipped matrix taken in doubles rather than through `for_tile_3d`, which
        // hands back the `f32` the UBO carries: the offset below is in tile units and applying
        // it after the narrowing would round it away on a tile whose units are small.
        let mut matrix = near_clipped_tile_matrix(view, projection, z, x, y, wrap)?;
        // The layer's own offset -- see `paint_translate`. Applied to the tile's matrix rather
        // than to its geometry, so one set of vertices still serves every zoom the layer is
        // drawn at.
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut matrix, translate[0], translate[1], 0.0);
        }
        #[allow(clippy::cast_possible_truncation)]
        let matrix: [f32; 16] = core::array::from_fn(|index| matrix[index] as f32);

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

/// The base, height and color mix factors an extrusion's drawable block carries.
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

/// The direction a fill-extrusion's walls are lit from, in the space their normals are in.
///
/// mbgl's `FillExtrusionBucket::lightPosition`. The style gives a direction relative to *north*;
/// a viewport-anchored light is one that stays where the screen is while the map turns beneath
/// it, so the direction is turned by the camera's bearing before the shader dots it with a
/// normal. Anchored to the map it does not turn, which is what makes a city look lit rather than
/// painted.
///
/// The arithmetic is mbgl's rather than the algebra's, for the reason
/// [`tessella_style::light`] gives for the cartesian form: `sin` and `cos` are taken in `f64`,
/// then every matrix element is narrowed to `f32` *before* it multiplies anything and the
/// products are accumulated in `f32`. `mat3::rotate` of the identity leaves
/// `[c, s, 0, -s, c, 0, 0, 0, 1]` and `transformMat3f` reads it by column, which is the pair of
/// signs below.
///
/// mbgl turns by `-state.getBearing()`, and its state bearing is `deg2rad(-degrees)`, so the
/// angle wanted here is the bearing itself — degrees clockwise from north, as `ViewTransform`
/// carries it.
#[allow(clippy::cast_possible_truncation)]
fn extrusion_light_position(light: &tessella_style::light::Light, bearing: f64) -> [f32; 3] {
    let cartesian = light.cartesian();
    if light.anchor == tessella_style::light::Anchor::Map {
        return cartesian;
    }
    let (sin, cos) = bearing.to_radians().sin_cos();
    let (sin, cos) = (sin as f32, cos as f32);
    [
        cos * cartesian[0] - sin * cartesian[1],
        sin * cartesian[0] + cos * cartesian[1],
        cartesian[2],
    ]
}

/// A fill-extrusion layer's evaluated properties, from its paint and the style light.
///
/// Three of the five blocks are the light, which is why it is a parameter rather than something
/// read from the paint: an extrusion is the first thing here whose color depends on more than
/// its own layer, and a build that packed the paint and left the light at zero draws every
/// building flat black.
///
/// `bearing` is here for the same reason it is on [`hillshade_props_from_paint`]: the default
/// light anchor is the viewport, so the block has to be rewritten whenever the camera turns.
#[must_use]
pub fn fill_extrusion_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
    light: &tessella_style::light::Light,
    bearing: f64,
) -> Vec<u8> {
    let color = light.color;
    let fill = uniform_color(paint, "fill-extrusion-color", zoom);
    pack_fill_extrusion_props(
        [fill.r, fill.g, fill.b, fill.a],
        [color.r, color.g, color.b],
        extrusion_light_position(light, bearing),
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
    /// Mix factors for color, radius, blur, opacity, stroke color, stroke width and stroke
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
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        let mut matrix = tile_matrix(
            view,
            projection,
            z,
            x,
            y,
            wrap,
            depth_offset(layer_index, sub_layer_index),
        )?;
        // The layer's own offset -- see `paint_translate`. Applied to the tile's matrix rather
        // than to its geometry, so one set of vertices still serves every zoom the layer is
        // drawn at.
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut matrix, translate[0], translate[1], 0.0);
        }
        let matrix = matrix;

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

/// One heatmap drawable's entry.
///
/// Two departures from [`CircleDrawableEntry`], both of which read as bugs if assumed away.
/// Its `extrude_scale` is a *scalar*, not a pair: a heatmap has no `pitch-alignment`, so the
/// radius is always in tile units and both axes carry the same number. And it mixes only two
/// properties where a circle mixes seven.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatmapDrawableEntry {
    /// Tile-local to clip, as the shaders take it.
    pub matrix: [f32; 16],
    /// Tile units per pixel — mbgl's `tileID.pixelsToTileUnits(1, zoom)`.
    pub extrude_scale: f32,
    /// Mix factors for weight then radius, in that order.
    pub interpolations: [f32; 2],
}

impl HeatmapDrawableEntry {
    /// The entry for a tile under a view.
    ///
    /// # No layer or sublayer, and so no depth offset
    ///
    /// Every other drawable's matrix is nudged by [`depth_offset`] so a layer's sublayers
    /// resolve against each other. A heatmap's is not, and the reason is one line of
    /// `LayerTweaker::multiplyWithProjectionMatrix`: the nudge is applied only
    /// `if (!drawable.getIs3D() && drawable.getEnableDepth())`. The heatmap builder calls
    /// `setEnableDepth(false)` — the oracle's `flags=0001`, color and nothing else — so the
    /// projection reaches the matrix unmodified.
    ///
    /// Passing a layer index here and offsetting by it is wrong by `3/2048` in element 14,
    /// which is what the golden caught and what nothing else would have: the layer is drawn
    /// into an offscreen target with no depth buffer to disagree with.
    ///
    /// # Errors
    ///
    /// [`camera::CameraError`] when the view has no area.
    pub fn for_tile(
        view: &ViewTransform,
        projection: ProjectionMode,
        z: u8,
        x: u32,
        y: u32,
        wrap: i32,
        interpolations: [f32; 2],
    ) -> Result<Self, camera::CameraError> {
        let matrix = tile_matrix(view, projection, z, x, y, wrap, 0.0)?;

        #[allow(clippy::cast_possible_truncation)]
        Ok(Self {
            matrix: core::array::from_fn(|index| matrix[index] as f32),
            extrude_scale: heatmap_extrude_scale(z, view),
            interpolations,
        })
    }
}

/// Tile units per pixel, which is what `heatmap-radius` is scaled by.
///
/// The map-aligned half of [`circle_extrude_scale`] and nothing else — there is no viewport
/// case to choose between, because the spec gives a heatmap no pitch alignment.
#[must_use]
pub fn heatmap_extrude_scale(z: u8, view: &ViewTransform) -> f32 {
    1.0 / line_ratio(z, view.zoom)
}

/// The two zoom-mix factors a heatmap drawable's UBO carries, in the UBO's own order.
#[must_use]
pub fn heatmap_interpolations(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    bucket_zoom: f64,
    view_zoom: f64,
) -> [f32; 2] {
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
    [factor("heatmap-weight"), factor("heatmap-radius")]
}

/// Packs a layer's heatmap drawable buffer at the union's stride.
#[must_use]
pub fn pack_heatmap_drawable_buffer(entries: &[HeatmapDrawableEntry], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(entries.len() * stride);
    for entry in entries {
        let start = out.len();
        push_f32s(&mut out, &entry.matrix);
        push_f32s(&mut out, &[entry.extrude_scale]);
        push_f32s(&mut out, &entry.interpolations);
        out.resize(start + stride, 0);
    }
    out
}

/// Packs `HeatmapEvaluatedPropsUBO`.
///
/// `weight` and `radius` are here *and* bound as attributes when they are data-driven, which is
/// mbgl's `constantOr(default)`: the block always carries a number, and the shader's
/// `#pragma mapbox: initialize` decides whether it is the one that is read. Writing a zero here
/// for a data-driven property would be correct for the shader and wrong for the buffer
/// comparison against the oracle, which is how the difference shows up.
#[must_use]
pub fn pack_heatmap_props(weight: f32, radius: f32, intensity: f32) -> Vec<u8> {
    const SIZE: usize = 16;
    let mut out = Vec::with_capacity(SIZE);
    push_f32s(&mut out, &[weight, radius, intensity, 0.0]);
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// A heatmap layer's evaluated properties, from its resolved paint.
#[must_use]
pub fn heatmap_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> Vec<u8> {
    pack_heatmap_props(
        uniform_number(paint, "heatmap-weight", zoom),
        uniform_number(paint, "heatmap-radius", zoom),
        uniform_number(paint, "heatmap-intensity", zoom),
    )
}

/// A heatmap layer's second-pass block, from its resolved paint and the frame's size.
///
/// `heatmap-opacity` is the only paint property the pass reads: the color comes from the ramp
/// texture and the density from what the first pass drew, so there is nothing else of the
/// layer's in it.
#[must_use]
pub fn heatmap_texture_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
    width: u32,
    height: u32,
) -> Vec<u8> {
    pack_heatmap_texture_props(
        width,
        height,
        uniform_number(paint, "heatmap-opacity", zoom),
    )
}

/// Packs `HeatmapTexturePropsUBO` — the second pass, which draws the offscreen target.
///
/// Its matrix is not a tile matrix and takes no view: the pass is a screen-aligned quad, and
/// mbgl builds `ortho(0, width, height, 0, -1, 1)` over the *backend* size. The quad's vertices
/// are the unit square, scaled to the world size in the vertex shader.
#[must_use]
pub fn pack_heatmap_texture_props(width: u32, height: u32, opacity: f32) -> Vec<u8> {
    const SIZE: usize = 80;
    let mut out = Vec::with_capacity(SIZE);
    push_f32s(&mut out, &screen_ortho(width, height));
    push_f32s(&mut out, &[opacity, 0.0, 0.0, 0.0]);
    debug_assert_eq!(out.len(), SIZE);
    out
}

/// `ortho(0, width, height, 0, -1, 1)`, column-major, as mbgl's `matrix::ortho` builds it.
///
/// Top-left origin: `bottom` is the height and `top` is zero, so the `y` scale is negative and
/// a quad at `y = 0` lands at the top of the frame. Swapping them mirrors the pass vertically,
/// which against a symmetric heatmap can look almost right.
#[must_use]
#[allow(clippy::cast_precision_loss)]
fn screen_ortho(width: u32, height: u32) -> [f32; 16] {
    let (left, right, bottom, top, near, far) =
        (0.0f32, width as f32, height as f32, 0.0f32, -1.0f32, 1.0f32);
    let mut m = [0.0f32; 16];
    m[0] = 2.0 / (right - left);
    m[5] = 2.0 / (top - bottom);
    m[10] = -2.0 / (far - near);
    m[12] = -(right + left) / (right - left);
    m[13] = -(top + bottom) / (top - bottom);
    m[14] = -(far + near) / (far - near);
    m[15] = 1.0;
    m
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
    /// Mix factors for fill color, halo color, opacity, halo width and halo blur.
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
        translate: [f64; 2],
    ) -> Result<Self, camera::CameraError> {
        // The matrices here are the plane's under either projection, and a globe uses only some
        // of them: the consumer bends the anchor itself from `globe_ubo`, so `matrix` and the
        // viewport `label_plane_matrix` never reach an anchored symbol. `coord_matrix` does, and
        // it is the one that has to be right.
        let mut projection = camera::proj_matrix(view)?;
        projection[14] -= f64::from(depth_offset(layer_index, sub_layer_index));
        let mut tile = camera::multiply(
            &projection,
            &camera::matrix_for_tile(z, x, y, wrap, view.zoom),
        );
        // The layer's own offset, before anything is derived from this matrix. mbgl translates
        // the tile matrix and builds the label-plane matrices from the result, so a translated
        // label moves with its anchor rather than sliding against it -- see `paint_translate`,
        // and `is_text` above for which of the two property pairs applies.
        if translate != [0.0, 0.0] {
            camera::translate_in_place(&mut tile, translate[0], translate[1], 0.0);
        }
        let tile = tile;

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
            // A globe's `matrix` is the *placement*, not the plane's tile-to-clip: the consumer
            // bends a tile-local point through it to normalized Mercator and takes the sphere
            // from there, which is what `fill`'s globe matrix is and what the direct-bend
            // symbol material walks. Nothing on a globe reads the plane's version -- the
            // anchored form takes the expansion instead -- so this is a slot a globe was
            // leaving unused rather than a value it was overwriting.
            matrix: core::array::from_fn(|index| match surface {
                ProjectionMode::Globe => camera::mercator_matrix_for_tile(z, x, y, wrap)[index],
                ProjectionMode::Mercator => tile[index],
            } as f32),
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
/// edge lands where it should. Switch the matrix and not the gamma and the two stop canceling:
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
/// [`atlas::PADDING`] is two — one so linear filtering cannot pull a neighbor's pixels in, and
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
/// Ninety-six bytes: text color, halo color, opacity, halo width and blur, then the same five
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
/// in this build whose color depends on more than its paint: `light-color`, `light-position`
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

/// Converts a height in meters into the vertical unit the tile matrix works in.
///
/// mbgl's `heightFactor`, `-numTiles / tileSize_D / 8.0`, and what it is *for* is narrower than
/// it looks: it walks a pattern up an extrusion's wall. The whole shader set uses it once, in
/// `fill_extrusion_pattern`'s `vec2 pos = vec2(edgedistance, z * drawable.height_factor)`.
///
/// It is **not** the conversion from meters to the shader's z. Nothing converts: the position
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
/// This used to carry `height_factor` beside the matrix, described as what a height in meters
/// multiplies by. That was wrong, and wrong by a factor of four thousand at z14.
///
/// mbgl's fill-extrusion shader settles it:
/// `gl_Position = drawable.matrix * vec4(in_position + decimals, z, 1.0)`, with `z` the height in
/// **meters** and no conversion in front of it. It needs none: `getWorldToCamera` scales the
/// matrix's third column by `pixelsPerMeter`, precisely so a height in meters and a position in
/// tile units can share one matrix. `heightFactor` appears once in the whole shader set, in the
/// *pattern* variant, walking a texture up a wall — `vec2(edgedistance, z * height_factor)` —
/// which is not a conversion of the position and has no meaning for a mesh at all.
///
/// The measurement behind the original claim still holds: a buildings mesh really is tile units
/// in x and y and meters in z, across 972 nodes of a real store. What was wrong was the
/// conversion, not the convention — and the conversion is the matrix's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshPlacement {
    /// Tile-local to clip, as every other drawable matrix on this protocol is. Its third column
    /// carries `pixelsPerMeter`, so a mesh's meters go in unscaled.
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
/// is, what the camera is doing, or how a meter relates to a tile unit at this zoom — and none of
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
/// anything — the image is a texture and the color adjustment is the layer's, so what is left
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

/// One of a location indicator's drawables: where the puck is and what color this half is.
///
/// Both of the accuracy circle's drawables take the same matrix and differ only in the color,
/// which is why mbgl keeps the color in the *drawable* block rather than in an evaluated-props
/// one: the interior and the border are one shader over one vertex buffer, and the color is the
/// only thing that tells them apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocationIndicatorEntry {
    /// World pixels around the puck to clip.
    pub matrix: [f32; 16],
    /// `accuracy-radius-color` for the interior, `accuracy-radius-border-color` for the border.
    pub color: Color,
}

/// The matrix a puck's accuracy circle is placed by.
///
/// mbgl's three lines, in order: the camera's projection, then a translation to the puck's own
/// position in world pixels, multiplied on the right so the vertices' offsets are applied first.
///
/// No depth nudge. mbgl hands the tweaker `params.projectionMatrix` untouched, and the layer's
/// drawables are built with `setEnableDepth(false)` -- there is nothing for a sub-layer bias to
/// separate them against.
///
/// # Errors
///
/// [`camera::CameraError`] when the view has no area, which is [`camera::proj_matrix`]'s.
pub fn location_indicator_matrix(
    view: &ViewTransform,
    location: [f64; 2],
) -> Result<[f32; 16], camera::CameraError> {
    let world = camera::world_size(view.zoom);
    let position = tessella_tile::projection::project(location[0], location[1], world);
    let mut matrix = camera::proj_matrix(view)?;
    camera::translate_in_place(&mut matrix, position[0], position[1], 0.0);
    #[allow(clippy::cast_possible_truncation)]
    Ok(core::array::from_fn(|index| matrix[index] as f32))
}

/// Where the color sits in `LocationIndicatorDrawableUBO`, after the matrix.
const LOCATION_INDICATOR_COLOR_AT: usize = 64;

/// Packs `LocationIndicatorDrawableUBO`: a matrix and a color.
#[must_use]
pub fn pack_location_indicator_drawable_buffer(
    entries: &[LocationIndicatorEntry],
    stride: u32,
) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = alloc::vec![0u8; entries.len() * stride];
    for (entry, slot) in entries.iter().zip(out.chunks_mut(stride)) {
        for (index, value) in entry.matrix.iter().enumerate() {
            slot[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        let color = [entry.color.r, entry.color.g, entry.color.b, entry.color.a];
        for (index, value) in color.iter().enumerate() {
            let at = LOCATION_INDICATOR_COLOR_AT + index * 4;
            slot[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// How a raster layer's imagery is sampled, from `raster-resampling`.
///
/// mbgl's `render_raster_layer.cpp` makes the same read:
///
/// ```cpp
/// const bool nearest = evaluated.get<RasterResampling>() == RasterResamplingType::Nearest;
/// const auto filter = nearest ? gfx::TextureFilterType::Nearest : gfx::TextureFilterType::Linear;
/// ```
///
/// Not a subtlety. Magnified sixteen times over a checkerboard, `mbgl-render` puts 138 distinct
/// colors on the frame under `linear` and two under `nearest`, and 240,640 of 262,144 pixels
/// differ by more than the parity threshold -- 92% of the frame.
///
/// Anything other than `nearest`, the absent property included, is linear: that is the spec's
/// default and mbgl's comparison is against `Nearest` alone.
#[must_use]
pub fn raster_filter(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> TextureFilter {
    if uniform_enum(paint, "raster-resampling", zoom) == "nearest" {
        TextureFilter::Nearest
    } else {
        TextureFilter::Linear
    }
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
    color: RasterColor,
    opacity: f32,
    brightness_low: f32,
    brightness_high: f32,
    buffer_scale: f32,
) -> Vec<u8> {
    const SIZE: usize = 64;
    let mut out = Vec::with_capacity(SIZE);
    push_f32s(&mut out, &color.spin_weights);
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
            color.saturation_factor,
            color.contrast_factor,
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
    let color = RasterColor {
        spin_weights: raster::spin_weights(uniform_number(paint, "raster-hue-rotate", zoom)),
        saturation_factor: raster::saturation_factor(uniform_number(
            paint,
            "raster-saturation",
            zoom,
        )),
        contrast_factor: raster::contrast_factor(uniform_number(paint, "raster-contrast", zoom)),
    };
    pack_raster_props(
        color,
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
    let bias = -f64::from(depth_offset(layer_index, sub_layer_index));

    // The rows arrive scaled: `anchored_bend` walks `globe::clip_matrix`, which carries the `w`
    // convention, and the bias is the plane's own number for the reason the globe arm of
    // `tile_matrix` gives -- the two `w`s agree, so the same nudge separates the same layers.
    #[allow(clippy::cast_possible_truncation)]
    let row = |v: [f64; 4]| -> [f32; 4] { core::array::from_fn(|i| v[i] as f32) };
    let mut anchor = bend.anchor;
    anchor[2] += bias;
    GlobeBendUbo {
        anchor: row(anchor),
        d_u: row(bend.d_u),
        d_v: row(bend.d_v),
        d_uu: row(bend.d_uu),
        d_vv: row(bend.d_vv),
        d_uv: row(bend.d_uv),
        d_h: row(bend.d_h),
    }
}

/// Packs a layer's anchored-bend blocks, one per drawable, in the order the drawables were sent.
///
/// The same shape as [`pack_drawable_buffer`]: the consumer indexes it by the drawable's own UBO
/// index, so a gap would put every later drawable on its neighbor's tile.
#[must_use]
pub fn pack_globe_bend_buffer(blocks: &[GlobeBendUbo]) -> Vec<u8> {
    let stride = GlobeBendUbo::STRIDE as usize;
    let mut out = Vec::with_capacity(blocks.len() * stride);
    for block in blocks {
        out.extend_from_slice(&block.to_bytes());
    }
    out
}

/// Packs `HillshadeTilePropsUBO`, which is per drawable because `latrange` is per tile.
///
/// `method` and `num_lights` travel even though only the standard method is drawn: they are the
/// block's shape, the consumer reads them, and a block that carried less would have to change
/// format the day the other four arrive.
#[must_use]
pub fn pack_hillshade_tile_props(
    lat_range: [f32; 2],
    exaggeration: f32,
    method: i32,
    lights: i32,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(ubo_layouts::HILLSHADE_TILE_PROPS_UBO.size as usize);
    push_f32s(&mut out, &lat_range);
    push_f32s(&mut out, &[exaggeration]);
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&lights.to_le_bytes());
    push_f32s(&mut out, &[0.0, 0.0, 0.0]);
    debug_assert_eq!(
        out.len(),
        ubo_layouts::HILLSHADE_TILE_PROPS_UBO.size as usize
    );
    out
}

/// The buffer of per-drawable tile props, one block a drawable at the layout's stride.
#[must_use]
pub fn pack_hillshade_tile_props_buffer(blocks: &[Vec<u8>], stride: u32) -> Vec<u8> {
    let stride = stride as usize;
    let mut out = Vec::with_capacity(blocks.len() * stride);
    for block in blocks {
        let start = out.len();
        out.extend_from_slice(block);
        out.resize(start + stride, 0);
    }
    out
}

/// Packs `HillshadeEvaluatedPropsUBO` from a layer's paint.
///
/// # The four lights
///
/// mbgl's `hillshade-illumination-direction`, `-altitude`, `-highlight-color` and `-shadow-color`
/// are each a *list*, padded to the longest of the four, and up to four of them reach the shader.
/// A style that writes a single value -- which is every style anyone has written -- gets one
/// light, and the other three slots are zero.
///
/// Altitudes and azimuths travel in radians because the shader takes them that way;
/// `getIlluminationProperties` converts once and this does the same.
///
/// # The viewport anchor
///
/// `hillshade-illumination-anchor: viewport` subtracts the camera's bearing from every azimuth,
/// so the light stays where the screen is rather than where north is. mbgl does it in the tweaker,
/// per frame, which is why `bearing` is a parameter here rather than something read from the
/// paint: the block is rewritten whenever the camera turns.
#[must_use]
pub fn hillshade_props_from_paint(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
    bearing: f64,
) -> Vec<u8> {
    let accent = uniform_color(paint, "hillshade-accent-color", zoom);
    let mut shadows = uniform_colors(paint, "hillshade-shadow-color", zoom);
    let mut highlights = uniform_colors(paint, "hillshade-highlight-color", zoom);
    let mut azimuths = uniform_numbers(paint, "hillshade-illumination-direction", zoom);
    let mut altitudes = uniform_numbers(paint, "hillshade-illumination-altitude", zoom);

    // As many lights as the longest of the four lists, capped at the four the block holds, and
    // the shorter lists padded by repeating their own last entry -- mbgl's `getIlluminationProperties`
    // exactly. Repeating rather than defaulting is what makes a style that writes four
    // directions and one color light all four in that one color.
    let lights = hillshade_lights(&[
        shadows.len(),
        highlights.len(),
        azimuths.len(),
        altitudes.len(),
    ]);
    pad_to(&mut shadows, lights, Color::transparent());
    pad_to(&mut highlights, lights, Color::transparent());
    pad_to(&mut azimuths, lights, 0.0);
    pad_to(&mut altitudes, lights, 0.0);

    #[allow(clippy::cast_possible_truncation)]
    let anchored_bearing = if hillshade_anchor_is_viewport(paint) {
        bearing as f32
    } else {
        0.0
    };

    let mut out = Vec::with_capacity(ubo_layouts::HILLSHADE_EVALUATED_PROPS_UBO.size as usize);
    push_color(&mut out, accent);
    for slot in 0..MAX_HILLSHADE_LIGHTS {
        let altitude = altitudes.get(slot).copied().unwrap_or(0.0);
        push_f32s(&mut out, &[altitude.to_radians()]);
    }
    for slot in 0..MAX_HILLSHADE_LIGHTS {
        let azimuth = azimuths.get(slot).copied().unwrap_or(0.0);
        push_f32s(
            &mut out,
            &[azimuth.to_radians() - anchored_bearing.to_radians()],
        );
    }
    // Four slots each, because the block is `vec4[4]`. A light the style did not write leaves
    // its slot transparent, which is what a shader reading past `num_lights` would find.
    for slot in 0..MAX_HILLSHADE_LIGHTS {
        push_color(
            &mut out,
            shadows
                .get(slot)
                .copied()
                .unwrap_or_else(Color::transparent),
        );
    }
    for slot in 0..MAX_HILLSHADE_LIGHTS {
        push_color(
            &mut out,
            highlights
                .get(slot)
                .copied()
                .unwrap_or_else(Color::transparent),
        );
    }
    debug_assert_eq!(
        out.len(),
        ubo_layouts::HILLSHADE_EVALUATED_PROPS_UBO.size as usize
    );
    out
}

/// How many lights the block carries, from the four lists' lengths.
///
/// mbgl's rule: the longest of them, never more than the block holds, and never fewer than one
/// -- `padArray` pushes a default into an empty list before it pads, so a layer that writes none
/// of the four still lights once.
#[must_use]
pub fn hillshade_lights(lengths: &[usize]) -> usize {
    lengths
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .clamp(1, MAX_HILLSHADE_LIGHTS)
}

/// How many lights a hillshade layer may carry, which is what the block has room for.
pub const MAX_HILLSHADE_LIGHTS: usize = 4;

/// Grows `values` to `len` by repeating its last entry, or by `empty` when it has none.
fn pad_to<T: Copy>(values: &mut Vec<T>, len: usize, empty: T) {
    if values.is_empty() {
        values.push(empty);
    }
    while values.len() < len {
        let last = values[values.len() - 1];
        values.push(last);
    }
}

/// Which lighting method the layer asks for, as the shader's own numbering.
///
/// mbgl's `HillshadeMethodType`, and the order is *its* order rather than the spec's listing:
/// `standard, combined, igor, multidirectional, basic`, cast straight to an int and read by the
/// shader's `switch`. A name the enum does not have reads as the spec's default, which is what
/// an unset property gives too.
#[must_use]
pub fn hillshade_method(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> i32 {
    match uniform_enum(paint, "hillshade-method", zoom).as_str() {
        "combined" => 1,
        "igor" => 2,
        "multidirectional" => 3,
        "basic" => 4,
        _ => 0,
    }
}

/// How many lights this layer's paint asks for, which is what the shader iterates over.
///
/// The same rule [`hillshade_props_from_paint`] pads to, asked separately because the count
/// travels in the *tile* block and the lights themselves travel in the evaluated one.
#[must_use]
pub fn hillshade_light_count(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
    zoom: f64,
) -> i32 {
    let lights = hillshade_lights(&[
        uniform_colors(paint, "hillshade-shadow-color", zoom).len(),
        uniform_colors(paint, "hillshade-highlight-color", zoom).len(),
        uniform_numbers(paint, "hillshade-illumination-direction", zoom).len(),
        uniform_numbers(paint, "hillshade-illumination-altitude", zoom).len(),
    ]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    {
        lights as i32
    }
}

/// Whether the light is anchored to the viewport rather than to north.
///
/// The spec's default is `viewport`, which is why an absent property answers true: a style that
/// says nothing wants the light to follow the screen.
#[must_use]
pub fn hillshade_anchor_is_viewport(
    paint: &alloc::collections::BTreeMap<&'static str, ResolvedProperty>,
) -> bool {
    let Some(property) = paint.get("hillshade-illumination-anchor") else {
        return true;
    };
    property
        .expression
        .evaluate(Some(0.0), None)
        .ok()
        .and_then(|value| value.as_str().map(alloc::string::ToString::to_string))
        .is_none_or(|anchor| anchor != "map")
}

/// Packs `ColorReliefTilePropsUBO`.
///
/// Every field is the *source's* rather than the tile's -- the unpack vector, the padded width,
/// and how many stops the ramp has -- so every drawable of a layer gets the same block. It is
/// per drawable anyway, because that is the slot mbgl writes it in and the consumer indexes it by
/// `ubo_index` like any other.
#[must_use]
pub fn pack_color_relief_tile_props(unpack: [f32; 4], stride: f32, stops: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(ubo_layouts::COLOR_RELIEF_TILE_PROPS_UBO.size as usize);
    push_f32s(&mut out, &unpack);
    push_f32s(&mut out, &[stride, stride]);
    out.extend_from_slice(&stops.to_le_bytes());
    push_f32s(&mut out, &[0.0]);
    debug_assert_eq!(
        out.len(),
        ubo_layouts::COLOR_RELIEF_TILE_PROPS_UBO.size as usize
    );
    out
}

/// Packs `ColorReliefEvaluatedPropsUBO`, which is an opacity and three words of padding.
#[must_use]
pub fn pack_color_relief_props(opacity: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(ubo_layouts::COLOR_RELIEF_EVALUATED_PROPS_UBO.size as usize);
    push_f32s(&mut out, &[opacity, 0.0, 0.0, 0.0]);
    debug_assert_eq!(
        out.len(),
        ubo_layouts::COLOR_RELIEF_EVALUATED_PROPS_UBO.size as usize
    );
    out
}

/// The elevation stops as the float texture the shader searches.
///
/// RGBA with the elevation in red and the other three zero, which is mbgl's "RGBA float for
/// compatibility" -- a one-channel float texture is not something every backend has, and a stop
/// table is a few hundred texels at most, so the three wasted channels cost nothing worth the
/// portability.
#[must_use]
pub fn pack_relief_elevation_stops(ramp: &tessella_style::ramp::ReliefRamp) -> Vec<u8> {
    let mut out = Vec::with_capacity(ramp.len() * 16);
    for elevation in &ramp.elevations {
        push_f32s(&mut out, &[*elevation, 0.0, 0.0, 0.0]);
    }
    out
}

/// The colors at those stops, as the bytes of a one-row texture.
///
/// Straight RGBA, not premultiplied: the shader mixes two of them against each other and then
/// scales by opacity, which is a blend between colors rather than a composite over anything.
#[must_use]
pub fn pack_relief_color_stops(ramp: &tessella_style::ramp::ReliefRamp) -> Vec<u8> {
    let mut out = Vec::with_capacity(ramp.len() * 4);
    for color in &ramp.colors {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let channel = |component: f32| (component.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        out.extend_from_slice(&[
            channel(color.r),
            channel(color.g),
            channel(color.b),
            channel(color.a),
        ]);
    }
    out
}

#[cfg(test)]
mod translate_tests {
    use super::{ViewTransform, paint_translate};

    fn paint(
        json: &str,
    ) -> alloc::collections::BTreeMap<&'static str, tessella_style::property::ResolvedProperty>
    {
        let style = tessella_style::Style::parse(json).expect("style parses");
        tessella_style::property::resolve_paint(style.layer("l").expect("the layer"))
            .expect("the paint resolves")
    }

    fn view_at(zoom: f64, bearing: f64) -> ViewTransform {
        ViewTransform {
            longitude: 0.0,
            latitude: 0.0,
            zoom,
            width: 1024.0,
            height: 768.0,
            bearing,
            pitch: 0.0,
            ground_below: 0.0,
        }
    }

    /// The property is screen pixels and the matrix takes tile units.
    ///
    /// A z14 tile drawn at zoom 19 is stretched over thirty-two times its own width, so one of
    /// its units is half a pixel and a two-pixel offset is one unit. Getting this backwards moves
    /// a building top by a tile rather than by a hair.
    #[test]
    fn a_translate_is_the_screen_pixels_in_the_tiles_own_units() {
        let paint = paint(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s",
                  "paint": {"fill-translate": [-2, -2]}}]}"#,
        );
        let at = paint_translate(
            &paint,
            "fill-translate",
            "fill-translate-anchor",
            &view_at(19.0, 0.0),
            14,
        );
        assert_eq!(at, [-1.0, -1.0]);

        // And at the tile's own zoom, where a unit is an eight-thousandth of the tile.
        let at = paint_translate(
            &paint,
            "fill-translate",
            "fill-translate-anchor",
            &view_at(14.0, 0.0),
            14,
        );
        assert_eq!(at, [-32.0, -32.0]);
    }

    /// A `map` anchor is a direction on the ground and does not turn; `viewport` is a direction
    /// on the screen and does.
    #[test]
    fn only_a_viewport_translate_turns_with_the_camera() {
        let ground = paint(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s",
                  "paint": {"fill-translate": [8, 0]}}]}"#,
        );
        let screen = paint(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s",
                  "paint": {"fill-translate": [8, 0],
                            "fill-translate-anchor": "viewport"}}]}"#,
        );
        let turned = view_at(14.0, 90.0);
        let of = |paint: &_| {
            paint_translate(
                paint,
                "fill-translate",
                "fill-translate-anchor",
                &turned,
                14,
            )
        };

        let [x, y] = of(&ground);
        assert!((x - 128.0).abs() < 1e-9 && y.abs() < 1e-9, "{x} {y}");

        // A quarter turn puts the screen's x on the ground's y. Which sign is the camera's
        // convention and is the half worth asserting.
        let [x, y] = of(&screen);
        assert!(x.abs() < 1e-9, "{x} should be off the ground's x axis");
        assert!(
            (y.abs() - 128.0).abs() < 1e-9,
            "{y} should carry the whole offset"
        );
    }

    /// Nearly every layer names none, and the matrix must not move for them.
    #[test]
    fn a_layer_with_no_translate_moves_nothing() {
        let paint = paint(
            r#"{"version": 8, "sources": {}, "layers": [
                 {"id": "l", "type": "fill", "source": "s", "paint": {}}]}"#,
        );
        assert_eq!(
            paint_translate(
                &paint,
                "fill-translate",
                "fill-translate-anchor",
                &view_at(19.0, 0.0),
                14
            ),
            [0.0, 0.0]
        );
    }
}
