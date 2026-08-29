//! R0's exit criterion: the stream matches the probe on the hermetic style (§10, §9.1).
//!
//! The pieces are each checked against the golden dump in their own tests — vertices, painter
//! order, clip masks, the camera block, the uniform buffers. This runs the producer end to end
//! and checks what actually reaches the ring: that every envelope kind the frame needs is
//! emitted, in an order a consumer can act on, and that the totals agree with the oracle's.
//!
//! # Why the whole stream is a different test from the sum of its parts
//!
//! Each piece being right does not make the stream right. A producer can compute a correct
//! camera block and never send it, or send it before the order it names, or emit a `ViewUse`
//! for a view it never declared. Those are protocol faults rather than arithmetic ones, and
//! they are invisible to a test that calls one function and inspects its return value.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::{OrderEpoch, ViewId};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{CameraMode, EnvelopeKind};
use tessella_orchestrate::camera::CameraBlock;
use tessella_orchestrate::order::{self, DrawOrder};
use tessella_orchestrate::tile::{TileId as BuildTile, build_sourceless, build_tile};
use tessella_orchestrate::ubo::{self, DrawableEntry, GlobalPaintParams};
use tessella_orchestrate::view::{GeometryBinding, ViewSession};
use tessella_orchestrate::{stencil, texture};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::light::Light;
use tessella_style::property::Color;
use tessella_style::{Source, Style};
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");
const DUMP: &str = include_str!("../../../tests/golden/hermetic_style.dump");

fn probe() -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// Emits one settled frame of the hermetic style, returning the envelope kinds in order.
fn emit_frame() -> Vec<EnvelopeKind> {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = geojson::read(&source.data).expect("features");

    let view_id = ViewId(0);
    let view = probe();
    let tiles = cover::cover(&view).expect("covers");

    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut session = ViewSession::new();

    // DR-18: the view is declared before anything names it.
    session
        .declare(producer, view_id, CameraMode::Producer)
        .expect("declares");

    // Frame-wide state the shaders read whatever the style says.
    for upload in texture::placeholders() {
        texture::write(producer, &upload).expect("writes");
    }
    let global = GlobalPaintParams::for_view(&view, [64.0, 64.0], 1.0).pack();
    ubo::write(
        producer,
        view_id,
        ubo::FRAME_WIDE,
        tessella_capture_abi::generated::ubo_slots::ID_GLOBAL_PAINT_PARAMS_UBO,
        &global,
    )
    .expect("writes");

    // Geometry, then the uses that bind it into this view's order.
    let mut draw_order = DrawOrder::new(style.layers.len() as u32);
    let mut next_id = 0;
    let mut by_layer: BTreeMap<i32, Vec<GeometryBinding>> = BTreeMap::new();

    for tile in &tiles {
        // The source's layers plus the source-less ones. A background reads no source, so it
        // is not in a source's pass and must be taken once per tile rather than once per
        // source — which for a single-source style is the same thing, and would not be for two.
        let id = BuildTile::new(tile.z, tile.x, tile.y);
        let mut buckets = build_tile(&style, "probe", id, &features, TilingOptions::default())
            .expect("tile builds");
        buckets.extend(build_sourceless(&style, id).expect("background builds"));
        buckets.sort_by_key(|bucket| bucket.layer_index);
        for binding in order::bindings_for(
            view_id,
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            &mut next_id,
            true,
        ) {
            by_layer
                .entry(binding.layer_index)
                .or_default()
                .push(binding);
            session.use_geometry(producer, binding).expect("uses");
            draw_order.bind(binding);
        }
    }

    // Per-layer state: clip masks for tiled layers, uniforms for all of them.
    for (layer_index, bindings) in &by_layer {
        let tiled = bindings.iter().any(|binding| {
            binding
                .flags
                .contains(tessella_capture_abi::envelope::DrawFlags::ENABLE_STENCIL)
        });
        if tiled {
            let set = stencil::clip_set(&view, *layer_index, &tiles).expect("clips");
            stencil::write(producer, view_id, &set).expect("writes");
        }

        // Each layer kind has its own drawable block, its own tile-properties block and its
        // own evaluated-properties block, at its own slots and strides. Emitting the fill's for
        // every tiled layer — which this did while the line layer did not exist — writes a
        // line layer's uniforms into the shape a fill shader reads.
        use tessella_capture_abi::generated::{ubo_layouts, ubo_slots};
        use tessella_style::LayerKind;

        let layer = &style.layers[usize::try_from(*layer_index).expect("a layer index")];
        let paint = tessella_style::property::resolve_paint(layer).expect("resolves");

        let matrices = |sub_layer_index: i32| {
            bindings
                .iter()
                .filter(move |binding| binding.sub_layer_index == sub_layer_index)
                .map(|binding| binding.tile.expect("a tiled drawable"))
        };
        let entries = |sub_layer_index: i32| {
            matrices(sub_layer_index)
                .map(|tile| {
                    DrawableEntry::for_tile_with(
                        &view,
                        tile.z,
                        tile.x,
                        tile.y,
                        i32::from(tile.wrap),
                        *layer_index,
                        sub_layer_index,
                        ubo::fill_interpolations(
                            &paint,
                            f64::from(tile.z),
                            view.zoom,
                            sub_layer_index,
                        ),
                    )
                    .expect("an unrotated camera")
                })
                .collect::<Vec<_>>()
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
                    *layer_index,
                    ubo::drawable_slot(),
                    &buffer,
                )
                .expect("writes");

                let props =
                    ubo::pack_background_props(Color::parse("#101418").expect("a color"), 1.0);
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_BACKGROUND_PROPS_UBO,
                    &props,
                )
                .expect("writes");
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
                    *layer_index,
                    ubo::drawable_slot(),
                    &buffer,
                )
                .expect("writes");

                let tile_props = ubo::pack_tile_props_buffer(
                    all.len(),
                    ubo_layouts::FILL_TILE_PROPS_UNION_UBO.stride,
                );
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_FILL_TILE_PROPS_UBO,
                    &tile_props,
                )
                .expect("writes");

                let props = ubo::fill_props_from_paint(&paint, view.zoom);
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_FILL_EVALUATED_PROPS_UBO,
                    &props,
                )
                .expect("writes");
            }
            LayerKind::Line => {
                let line: Vec<ubo::LineDrawableEntry> = matrices(0)
                    .map(|tile| {
                        ubo::LineDrawableEntry::for_tile(
                            &view,
                            tile.z,
                            tile.x,
                            tile.y,
                            i32::from(tile.wrap),
                            *layer_index,
                            0,
                            ubo::line_interpolations(&paint, f64::from(tile.z), view.zoom),
                        )
                        .expect("an unrotated camera")
                    })
                    .collect();
                let buffer = ubo::pack_line_drawable_buffer(
                    &line,
                    ubo_layouts::LINE_DRAWABLE_UNION_UBO.stride,
                );
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_LINE_DRAWABLE_UBO,
                    &buffer,
                )
                .expect("writes");

                let tile_props = ubo::pack_tile_props_buffer(
                    line.len(),
                    ubo_layouts::LINE_TILE_PROPS_UNION_UBO.stride,
                );
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_LINE_TILE_PROPS_UBO,
                    &tile_props,
                )
                .expect("writes");

                let props = ubo::line_props_from_paint(&paint, view.zoom);
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_LINE_EVALUATED_PROPS_UBO,
                    &props,
                )
                .expect("writes");
            }
            LayerKind::Circle => {
                let pitch_with_map = false; // the style leaves the viewport default
                let circles: Vec<ubo::CircleDrawableEntry> = matrices(0)
                    .map(|tile| {
                        ubo::CircleDrawableEntry::for_tile(
                            &view,
                            tile.z,
                            tile.x,
                            tile.y,
                            i32::from(tile.wrap),
                            *layer_index,
                            0,
                            ubo::circle_extrude_scale(pitch_with_map, tile.z, &view),
                            ubo::circle_interpolations(&paint, f64::from(tile.z), view.zoom),
                        )
                        .expect("an unrotated camera")
                    })
                    .collect();
                let buffer = ubo::pack_circle_drawable_buffer(
                    &circles,
                    ubo_layouts::CIRCLE_DRAWABLE_UBO.stride,
                );
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_CIRCLE_DRAWABLE_UBO,
                    &buffer,
                )
                .expect("writes");

                // No tile-properties block: a circle has no pattern variant to need one, which
                // is why the oracle writes two blocks for this layer where a fill gets three.
                let props = ubo::circle_props_from_paint(&paint, view.zoom);
                ubo::write(
                    producer,
                    view_id,
                    *layer_index,
                    ubo_slots::ID_CIRCLE_EVALUATED_PROPS_UBO,
                    &props,
                )
                .expect("writes");
            }
            _ => {}
        }
    }

    // The order, then the camera naming its epoch — never the other way round.
    let emitted = draw_order.emit(producer, view_id).expect("emits");
    let cutoff = draw_order.opaque_cutoff();
    CameraBlock::new(&view, &Light::default(), emitted.epoch, 0, cutoff)
        .expect("an unrotated camera")
        .for_view(view_id)
        .write(producer)
        .expect("writes");

    let mut kinds = Vec::new();
    while let Some(record) = consumer.peek() {
        kinds.push(record.kind);
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    kinds
}

/// Every envelope kind R0's frame needs reaches the ring.
#[test]
fn the_frame_emits_every_kind_r0_needs() {
    let kinds = emit_frame();
    for required in [
        EnvelopeKind::ViewDeclare,
        EnvelopeKind::TextureUpdate,
        EnvelopeKind::UboUpdate,
        EnvelopeKind::ViewUse,
        EnvelopeKind::StencilTiles,
        EnvelopeKind::OrderUpdate,
        EnvelopeKind::CameraUpdate,
    ] {
        assert!(kinds.contains(&required), "{required:?} was never emitted");
    }
}

/// The view is declared before anything names it (DR-18).
///
/// `ViewUse` carries a view id and `ViewDeclare` carries the per-view state that id means. A use
/// arriving first names a view the consumer has no configuration for, and its only principled
/// response is to drop the drawable.
#[test]
fn the_view_is_declared_before_it_is_used() {
    let kinds = emit_frame();
    let declare = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::ViewDeclare)
        .expect("a declaration");
    let first_use = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::ViewUse)
        .expect("a use");
    assert!(declare < first_use, "{declare} vs {first_use}");
}

/// The order precedes the camera that names its epoch (§4).
///
/// A consumer holding a camera whose epoch it has not seen must hold the camera. Emitting them
/// the other way round makes that rule fire every frame, which turns a correctness guard into a
/// frame of latency.
#[test]
fn the_order_precedes_the_camera_naming_it() {
    let kinds = emit_frame();
    let order = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::OrderUpdate)
        .expect("an order");
    let camera = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::CameraUpdate)
        .expect("a camera");
    assert!(order < camera, "{order} vs {camera}");
}

/// Geometry is on the wire before the order that draws it.
#[test]
fn geometry_precedes_the_order_that_draws_it() {
    let kinds = emit_frame();
    let last_use = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::ViewUse)
        .expect("a use");
    let order = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::OrderUpdate)
        .expect("an order");
    assert!(last_use < order, "{last_use} vs {order}");
}

/// The counts agree with the oracle's: one clip set per tiled layer, one use per drawable.
#[test]
fn the_counts_agree_with_the_oracle() {
    let kinds = emit_frame();

    let uses = kinds
        .iter()
        .filter(|kind| **kind == EnvelopeKind::ViewUse)
        .count();
    assert_eq!(
        uses, 37,
        "six tiles of background, two fills and a line, plus one circle"
    );

    // The oracle emits a clip set for each of its three tiled layers — the two fills and the
    // line — and this build now produces all three.
    let oracle_sets = DUMP
        .lines()
        .filter(|line| line.starts_with("stencil layer="))
        .count();
    assert_eq!(oracle_sets, 3, "the oracle clips three layers");
    let mine = kinds
        .iter()
        .filter(|kind| **kind == EnvelopeKind::StencilTiles)
        .count();
    assert_eq!(mine, oracle_sets, "and so does this build");

    // Every uniform buffer the oracle writes. Its fourteen are one frame-wide block, two for
    // the background, three each for the two fills and the line, and two for the circle.
    //
    // Counted rather than assumed equal to the layer count, because the blocks are not uniform
    // across kinds: a background has no tile-properties block and a line's sits at a different
    // slot from a fill's. Emitting one shape everywhere was what this harness used to do, and
    // it went unnoticed while every tiled layer happened to be a fill.
    let oracle_ubos = DUMP.lines().filter(|line| line.starts_with("ubo ")).count();
    assert_eq!(oracle_ubos, 14, "the oracle writes fourteen");
    let mine = kinds
        .iter()
        .filter(|kind| **kind == EnvelopeKind::UboUpdate)
        .count();
    assert_eq!(mine, oracle_ubos, "and so does this build");

    // Two placeholder textures, as the oracle lists.
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == EnvelopeKind::TextureUpdate)
            .count(),
        2
    );
    assert_eq!(
        DUMP.lines()
            .filter(|line| line.starts_with("texture "))
            .count(),
        2
    );
}

/// A second identical frame writes nothing: R0's other exit criterion (§6.5, DR-8).
///
/// The frame above is a cold start, so everything is new. What DR-8 requires is that a settled
/// view goes silent, and the order and clip sets are where that is decided — they are the two
/// channels that would otherwise re-send a list proportional to the scene every frame.
#[test]
fn a_settled_frame_goes_quiet() {
    let style = Style::parse(HERMETIC).expect("style parses");
    let view = probe();
    let tiles = cover::cover(&view).expect("covers");

    let mut ring = Ring::new(1 << 20);
    let (producer, _consumer) = ring.split();

    let mut draw_order = DrawOrder::new(style.layers.len() as u32);
    let mut next_id = 0;
    for tile in &tiles {
        let buckets = build_tile(
            &style,
            "probe",
            BuildTile::new(tile.z, tile.x, tile.y),
            &[],
            TilingOptions::default(),
        )
        .expect("tile builds");
        for binding in order::bindings_for(
            ViewId(0),
            order::tile_of(tile.z, tile.x, tile.y),
            &buckets,
            &mut next_id,
            true,
        ) {
            draw_order.bind(binding);
        }
    }
    let mut sets = stencil::ClipSets::new();
    let clip = stencil::clip_set(&view, 1, &tiles).expect("clips");

    draw_order.emit(producer, ViewId(0)).expect("emits");
    sets.emit(producer, ViewId(0), &clip).expect("emits");
    let settled = producer.head();

    for _frame in 0..500 {
        assert!(!draw_order.emit(producer, ViewId(0)).expect("emits").changed);
        assert!(!sets.emit(producer, ViewId(0), &clip).expect("emits"));
    }
    assert_eq!(producer.head(), settled, "five hundred settled frames");
}

/// The epoch the camera names is the one the order established.
#[test]
fn the_camera_names_the_order_it_was_computed_against() {
    let mut ring = Ring::new(1 << 16);
    let (producer, _consumer) = ring.split();
    let mut draw_order = DrawOrder::new(5);
    draw_order.bind(GeometryBinding {
        geometry: tessella_capture_abi::envelope::GeometryId(0),
        view: ViewId(0),
        layer_index: 1,
        sub_layer_index: 1,
        tile: Some(order::tile_of(13, 4093, 2724)),
        pass: tessella_orchestrate::view::fill_pass(),
        flags: tessella_orchestrate::view::tiled_flags(),
    });

    let emitted = draw_order.emit(producer, ViewId(0)).expect("emits");
    let block = CameraBlock::new(&probe(), &Light::default(), emitted.epoch, 0, 0)
        .expect("an unrotated camera");
    assert_eq!(block.record.order_epoch, emitted.epoch);
    assert_ne!(
        emitted.epoch,
        OrderEpoch(0),
        "a real epoch, not the default"
    );
}
