//! One settled frame, emitted onto the ring.
//!
//! # Why this is a module and not a caller's business
//!
//! Everything here is protocol order, and protocol order is the part a caller cannot be trusted
//! to rediscover. A view must be declared before anything names it (DR-18); geometry must be on
//! the wire before the order that draws it; the order must precede the camera that names its
//! epoch, or a consumer holding a camera whose epoch it has not seen stalls a frame every frame
//! (§4). None of those are arithmetic, so none of them show up as a wrong pixel in a caller that
//! gets them wrong — the picture is simply late, or absent, or drawn against stale uniforms.
//!
//! It lived in a test until now, which meant the only correct driver of this producer was one
//! nothing shipped could call. A second copy in a tool would have been a second thing to keep in
//! step with the first.
//!
//! # What a caller still decides
//!
//! The buckets and the view. This takes a cover's worth of built buckets and a camera and emits
//! them; it does not build tiles, choose a cover, or own a source. That split is §5.1's: the
//! store is process-scoped and the frame is per view.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use tessella_capture_abi::envelope::{OrderEpoch, ViewId};
use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
use tessella_capture_abi::ring::{Full, Producer};
use tessella_capture_abi::{BuiltIn, CameraMode, declared_for};
use tessella_style::light::Light;
use tessella_style::property::Color;
use tessella_style::{LayerKind, Style};
use tessella_tile::cover::{TileCoord, ViewTransform};

use crate::binder::{
    CIRCLE_FAMILY, FILL_EXTRUSION_FAMILY, FILL_FAMILY, LINE_FAMILY, attribute_ids, layout,
    permutation_key,
};
use crate::camera::CameraBlock;
use crate::emit::SlabArena;
use crate::order::{self, DrawOrder};
use crate::tile::{Content, LayerBucket, TileId};
use crate::ubo::{self, DrawableEntry};
use crate::view::{GeometryBinding, ViewSession};
use crate::{emit, stencil, texture};

/// What went wrong emitting a frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The ring had no room. A frame is emitted whole or not at all.
    #[error("the ring is full")]
    Full,
    /// The camera could not be resolved into a matrix.
    #[error("camera: {0}")]
    Camera(alloc::string::String),
    /// The view could not be declared.
    #[error("view: {0}")]
    View(alloc::string::String),
}

impl From<Full> for FrameError {
    fn from(_: Full) -> Self {
        Self::Full
    }
}

/// What one frame put on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emitted {
    /// Geometries announced.
    pub geometries: usize,
    /// Per-view uses bound into the order.
    pub drawables: usize,
    /// The order epoch the camera names.
    pub epoch: OrderEpoch,
}

impl Default for Emitted {
    fn default() -> Self {
        Self {
            geometries: 0,
            drawables: 0,
            epoch: OrderEpoch(0),
        }
    }
}

/// One view's buckets, per tile of its cover.
pub struct Frame<'a> {
    /// The style the buckets were built from. Uniforms are read from it per layer.
    pub style: &'a Style,
    /// The camera.
    pub view: &'a ViewTransform,
    /// Which view this is.
    pub view_id: ViewId,
    /// The cover, for the clip masks.
    pub tiles: &'a [TileCoord],
    /// Built buckets, per tile, in cover order.
    pub buckets: &'a [(TileId, Vec<LayerBucket>)],
    /// The style light, which travels in the camera block (§2.2).
    pub light: &'a Light,
}

/// Emits a whole frame: state, geometry, uniforms, order, camera — in that order.
///
/// # Errors
///
/// [`FrameError`] when the ring fills, the camera will not resolve, or the view will not declare.
pub fn emit(
    producer: &mut Producer,
    arena: &mut SlabArena,
    frame: &Frame<'_>,
) -> Result<Emitted, FrameError> {
    let Frame {
        style,
        view,
        view_id,
        tiles,
        buckets,
        light,
    } = *frame;

    let mut session = ViewSession::new();
    session
        .declare(producer, view_id, CameraMode::Producer)
        .map_err(|error| FrameError::View(alloc::format!("{error}")))?;

    // Frame-wide state the shaders read whatever the style says. The placeholders matter: a
    // shader samples its texture slots unconditionally, so a drawable whose layer binds none
    // still reads whatever was last at that slot.
    for upload in texture::placeholders() {
        texture::write(producer, &upload)?;
    }
    let global = ubo::GlobalPaintParams::for_view(view, [64.0, 64.0], 1.0).pack();
    ubo::write(
        producer,
        view_id,
        ubo::FRAME_WIDE,
        ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
        &global,
    )?;

    #[allow(clippy::cast_possible_truncation)]
    let mut draw_order = DrawOrder::new(style.layers.len() as u32);
    let mut next_id = 0;
    let mut by_layer: BTreeMap<i32, Vec<GeometryBinding>> = BTreeMap::new();
    let mut emitted = Emitted::default();

    for (tile, tile_buckets) in buckets {
        let at = order::tile_of(tile.z, tile.x, tile.y);
        let bindings = order::bindings_for(view_id, at, tile_buckets, &mut next_id);

        // A binding per drawable, and a bucket may produce two of them — a fill's triangles and
        // its outline. Each gets its own geometry id from `bindings_for`, so each is announced
        // separately rather than one being bound to an id nothing declared.
        let mut binding_index = 0;
        for bucket in tile_buckets {
            if !bucket.content.has_data() {
                continue;
            }
            let drawables = bucket.drawable_count();
            for _ in 0..drawables {
                let Some(binding) = bindings.get(binding_index) else {
                    break;
                };
                binding_index += 1;
                if let Some(encoded) = encode(arena, bucket, binding.geometry) {
                    emit::write(producer, &encoded)?;
                    emitted.geometries += 1;
                }
            }
        }

        for binding in bindings {
            by_layer
                .entry(binding.layer_index)
                .or_default()
                .push(binding);
            session
                .use_geometry(producer, binding)
                .map_err(|error| FrameError::View(alloc::format!("{error}")))?;
            draw_order.bind(binding);
            emitted.drawables += 1;
        }
    }

    for (layer_index, bindings) in &by_layer {
        write_layer_state(producer, frame, *layer_index, bindings, tiles)?;
    }

    // The order, then the camera naming its epoch — never the other way round.
    let order = draw_order.emit(producer, view_id)?;
    emitted.epoch = order.epoch;
    CameraBlock::new(view, light, order.epoch, 0, draw_order.opaque_cutoff())
        .map_err(|error| FrameError::Camera(alloc::format!("{error}")))?
        .for_view(view_id)
        .write(producer)?;

    Ok(emitted)
}

/// Encodes one bucket for the wire, or `None` for a kind that carries no vertex buffer.
///
/// A background is the one that legitimately carries none: it fills the viewport, so its quad is
/// something the consumer synthesizes rather than something the producer sends (§2.2).
fn encode(
    arena: &mut SlabArena,
    bucket: &LayerBucket,
    geometry: tessella_capture_abi::envelope::GeometryId,
) -> Option<emit::Encoded> {
    let bind = |family: &[BuiltIn], shader: BuiltIn| {
        let ids = attribute_ids(family);
        let key = permutation_key(&bucket.paint, &ids);
        let vertex_layout = layout(&bucket.binder, &ids, |attr_id| {
            declared_for(shader, attr_id).map(|a| (a.binding, a.declared))
        });
        (vertex_layout, key)
    };

    match &bucket.content {
        Content::Fill(fill) => {
            let (vertex_layout, key) = bind(FILL_FAMILY, BuiltIn::FillShader);
            Some(emit::encode_fill(
                arena,
                geometry,
                fill,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Line(line) => {
            let (vertex_layout, key) = bind(LINE_FAMILY, BuiltIn::LineShader);
            Some(emit::encode_line(
                arena,
                geometry,
                line,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Circle(circle) => {
            let (vertex_layout, key) = bind(CIRCLE_FAMILY, BuiltIn::CircleShader);
            Some(emit::encode_circle(
                arena,
                geometry,
                circle,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        Content::Fill3d(extrusion) => {
            let (vertex_layout, key) = bind(FILL_EXTRUSION_FAMILY, BuiltIn::FillExtrusionShader);
            Some(emit::encode_extrusion(
                arena,
                geometry,
                extrusion,
                &vertex_layout,
                bucket.binder.data(),
                key,
            ))
        }
        // A background's quad is the consumer's to synthesize; a raster and a symbol need a
        // texture this function is not given, so they are emitted by their own paths.
        _ => None,
    }
}

/// Writes one layer's clip masks and uniform blocks.
///
/// # Why the blocks are per kind and not one shape
///
/// Each layer kind has its own drawable block, its own tile-properties block and its own
/// evaluated-properties block, at its own slots and strides. Writing a fill's for every tiled
/// layer — which this did while the line layer did not exist — puts a line layer's uniforms into
/// the shape a fill shader reads, and the shader has no way to know.
fn write_layer_state(
    producer: &mut Producer,
    frame: &Frame<'_>,
    layer_index: i32,
    bindings: &[GeometryBinding],
    tiles: &[TileCoord],
) -> Result<(), FrameError> {
    let Frame {
        style,
        view,
        view_id,
        ..
    } = *frame;

    let tiled = bindings.iter().any(|binding| {
        binding
            .flags
            .contains(tessella_capture_abi::envelope::DrawFlags::ENABLE_STENCIL)
    });
    if tiled {
        let set = stencil::clip_set(view, layer_index, tiles)
            .map_err(|error| FrameError::Camera(alloc::format!("{error}")))?;
        stencil::write(producer, view_id, &set)?;
    }

    let Some(layer) = usize::try_from(layer_index)
        .ok()
        .and_then(|index| style.layers.get(index))
    else {
        return Ok(());
    };
    let Ok(paint) = tessella_style::property::resolve_paint(layer) else {
        return Ok(());
    };

    let matrices = |sub_layer_index: i32| {
        bindings
            .iter()
            .filter(move |binding| binding.sub_layer_index == sub_layer_index)
            .filter_map(|binding| binding.tile)
    };
    let entries = |sub_layer_index: i32| -> Vec<DrawableEntry> {
        matrices(sub_layer_index)
            .filter_map(|tile| {
                DrawableEntry::for_tile_with(
                    view,
                    tile.z,
                    tile.x,
                    tile.y,
                    i32::from(tile.wrap),
                    layer_index,
                    sub_layer_index,
                    ubo::fill_interpolations(&paint, f64::from(tile.z), view.zoom, sub_layer_index),
                )
                .ok()
            })
            .collect()
    };

    match layer.kind {
        LayerKind::Background => {
            let buffer = ubo::pack_drawable_buffer(
                &entries(0),
                ubo_layouts::BACKGROUND_DRAWABLE_UNION_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo::drawable_slot(),
                &buffer,
            )?;

            let color = paint
                .get("background-color")
                .and_then(|property| property.expression.evaluate(Some(view.zoom), None).ok())
                .and_then(|value| value.as_str().and_then(|text| Color::parse(text).ok()))
                .unwrap_or_else(Color::black);
            let props = ubo::pack_background_props(color, 1.0);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_BACKGROUND_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Fill => {
            // Triangles then outline, which is the order the oracle's buffer is in.
            let mut all = entries(1);
            all.extend(entries(2));
            let buffer =
                ubo::pack_drawable_buffer(&all, ubo_layouts::FILL_DRAWABLE_UNION_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo::drawable_slot(),
                &buffer,
            )?;

            let tile_props = ubo::pack_tile_props_buffer(
                all.len(),
                ubo_layouts::FILL_TILE_PROPS_UNION_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_TILE_PROPS_UBO,
                &tile_props,
            )?;

            let props = ubo::fill_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Line => {
            let line: Vec<ubo::LineDrawableEntry> = matrices(0)
                .filter_map(|tile| {
                    ubo::LineDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                        ubo::line_interpolations(&paint, f64::from(tile.z), view.zoom),
                    )
                    .ok()
                })
                .collect();
            let buffer =
                ubo::pack_line_drawable_buffer(&line, ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_DRAWABLE_UBO,
                &buffer,
            )?;

            let tile_props = ubo::pack_tile_props_buffer(
                line.len(),
                ubo_layouts::LINE_TILE_PROPS_UNION_UBO.stride,
            );
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_TILE_PROPS_UBO,
                &tile_props,
            )?;

            let props = ubo::line_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_LINE_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::Circle => {
            let pitch_with_map = false;
            let circles: Vec<ubo::CircleDrawableEntry> = matrices(0)
                .filter_map(|tile| {
                    ubo::CircleDrawableEntry::for_tile(
                        view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        layer_index,
                        0,
                        ubo::circle_extrude_scale(pitch_with_map, tile.z, view),
                        ubo::circle_interpolations(&paint, f64::from(tile.z), view.zoom),
                    )
                    .ok()
                })
                .collect();
            let buffer =
                ubo::pack_circle_drawable_buffer(&circles, ubo_layouts::CIRCLE_DRAWABLE_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_CIRCLE_DRAWABLE_UBO,
                &buffer,
            )?;

            // No tile-properties block: a circle has no pattern variant to need one, which is
            // why the oracle writes two blocks for this layer where a fill gets three.
            let props = ubo::circle_props_from_paint(&paint, view.zoom);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_CIRCLE_EVALUATED_PROPS_UBO,
                &props,
            )?;
        }
        LayerKind::FillExtrusion => {
            // Both passes: a translucent extrusion takes a depth pass in front of its colour
            // pass, and both read the same drawable buffer.
            let mut all = entries(0);
            all.extend(entries(1));
            let buffer =
                ubo::pack_drawable_buffer(&all, ubo_layouts::FILL_EXTRUSION_DRAWABLE_UBO.stride);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EXTRUSION_DRAWABLE_UBO,
                &buffer,
            )?;

            let props = ubo::fill_extrusion_props_from_paint(&paint, view.zoom, frame.light);
            ubo::write(
                producer,
                view_id,
                layer_index,
                ubo_slots::ID_FILL_EXTRUSION_PROPS_UBO,
                &props,
            )?;
        }
        _ => {}
    }
    Ok(())
}
