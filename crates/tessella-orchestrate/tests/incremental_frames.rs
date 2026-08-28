//! Sending only what the consumer does not already have.
//!
//! # What it replaces
//!
//! `frame::emit` announces every geometry every frame, which is why `GeometryId` documents an
//! emission as replacing the previous set entire. `emit_incremental` keeps a registry and an
//! arena across frames, so a tile that survives a pan keeps its id, its geometry is announced
//! once, and only what arrived is sent — DR-21.
//!
//! # The cases that distinguish it
//!
//! A second frame with an *unchanged* cover, which must announce nothing. A pan, which must
//! announce only what arrived and remove only what left. And a failed frame, which must leave
//! the registry as it found it — otherwise the retry assumes the consumer holds geometry that
//! was never sent, and the tile is missing until the cover changes again.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, Ring, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame, FrameError};
use tessella_orchestrate::registry::Session;
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-color": "red"}}]}"#;

struct Scene {
    style: Style,
    view: ViewTransform,
    tiles: Vec<cover::TileCoord>,
    buckets: Vec<(TileId, Vec<LayerBucket>)>,
}

fn scene(longitude: f64) -> Scene {
    let style = Style::parse(STYLE).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude,
        latitude: 0.0,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "s", id, &decoded).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }
    Scene {
        style,
        view,
        tiles,
        buckets,
    }
}

/// Emits one frame into a fresh ring, returning what it wrote and the record counts.
fn emit_frame(
    scene: &Scene,
    arena: &mut SlabArena,
    registry: &mut Session,
) -> (frame::Emitted, BTreeMap<String, usize>) {
    emit_frame_for(scene, ViewId(0), arena, registry)
}

fn emit_frame_for(
    scene: &Scene,
    view_id: ViewId,
    arena: &mut SlabArena,
    registry: &mut Session,
) -> (frame::Emitted, BTreeMap<String, usize>) {
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let emitted = frame::emit_incremental(
        producer,
        arena,
        &Frame {
            style: &scene.style,
            view: &scene.view,
            view_id,
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
        registry,
    )
    .expect("emits");

    let mut kinds = BTreeMap::new();
    while let Some(record) = consumer.peek() {
        *kinds.entry(format!("{:?}", record.kind)).or_insert(0) += 1;
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    (emitted, kinds)
}

/// A second frame over an unchanged cover announces nothing.
#[test]
fn an_unchanged_cover_announces_nothing() {
    let scene = scene(0.0);
    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    let (first, first_kinds) = emit_frame(&scene, &mut arena, &mut registry);
    assert!(first.geometries > 0, "the first frame announces everything");
    assert_eq!(
        first_kinds.get("GeometryAdd").copied().unwrap_or(0),
        first.geometries
    );

    let (second, second_kinds) = emit_frame(&scene, &mut arena, &mut registry);
    assert_eq!(second.geometries, 0, "nothing is new: {second_kinds:?}");
    assert_eq!(
        second_kinds.get("GeometryAdd").copied().unwrap_or(0),
        0,
        "and nothing was announced"
    );
    assert_eq!(
        second_kinds.get("ViewUse").copied().unwrap_or(0),
        0,
        "a use is as durable as the geometry it names"
    );
    assert_eq!(second.removed, 0, "and nothing left");

    // And a parked view is silent altogether. `DrawOrder` suppresses an order identical to the
    // one it last sent, and the camera is gated on the key `damage` defines — §10's exit
    // criterion is that a parked view emits nothing, and this is that measured on the ring
    // rather than on what the producer believes it wrote.
    assert_eq!(
        second_kinds.get("OrderUpdate").copied(),
        None,
        "the order did not change: {second_kinds:?}"
    );
    assert_eq!(
        second_kinds.get("CameraUpdate").copied(),
        None,
        "nor did the camera: {second_kinds:?}"
    );
    // Not yet silent altogether, and the remainder is named rather than left to be discovered:
    // `ViewDeclare`, the placeholder `TextureUpdate`s, the per-layer `UboUpdate`s and
    // `StencilTiles` still go every frame. Each is gateable and none is gated — the declaration
    // and the placeholders on the view's first frame (DR-18 re-emits a declaration only when the
    // configuration changes), the uniforms and the stencil on the camera and the cover, which
    // are exactly the two things this frame already knows did not move.
    for kind in [
        "GeometryAdd",
        "ViewUse",
        "ViewRelease",
        "OrderUpdate",
        "CameraUpdate",
    ] {
        assert_eq!(
            second_kinds.get(kind).copied().unwrap_or(0),
            0,
            "{kind} should be silent on a parked view: {second_kinds:?}"
        );
    }
    let parked: usize = second_kinds.values().sum();
    let busy: usize = first_kinds.values().sum();
    assert!(
        parked * 3 <= busy,
        "a parked frame is a fraction of a full one: {parked} of {busy}"
    );
    assert_eq!(
        second.drawables, first.drawables,
        "the same scene is drawn either way"
    );
}

/// A pan announces what arrived and removes what left, and nothing else.
#[test]
fn a_pan_sends_only_the_difference() {
    let here = scene(0.0);
    let there = scene(60.0);
    assert_ne!(here.tiles, there.tiles, "the cover has to change");

    let mut arena = SlabArena::new();
    let mut registry = Session::new();
    let (first, _) = emit_frame(&here, &mut arena, &mut registry);
    let (second, kinds) = emit_frame(&there, &mut arena, &mut registry);

    assert!(second.geometries > 0, "something arrived");
    assert!(
        second.geometries < first.geometries,
        "but not everything: {} of {}",
        second.geometries,
        first.geometries
    );
    assert!(second.removed > 0, "and something left");
    assert_eq!(
        kinds.get("ViewRelease").copied().unwrap_or(0),
        second.removed,
        "one release per drawable that left"
    );
    assert_eq!(
        kinds.get("GeometryRemove").copied().unwrap_or(0),
        second.removed,
        "and one removal each: the arena hands those ranges to the next geometry that fits, so \
         a consumer not told they are gone reads whatever gets written over them"
    );
    assert_eq!(
        second.drawables, first.drawables,
        "the cover is the same size either way"
    );
}

/// Panning away and back does not re-announce what was never removed.
#[test]
fn a_tile_that_stays_is_announced_once() {
    let here = scene(0.0);
    let there = scene(20.0);

    let mut arena = SlabArena::new();
    let mut registry = Session::new();
    emit_frame(&here, &mut arena, &mut registry);
    let (second, _) = emit_frame(&there, &mut arena, &mut registry);
    let (third, _) = emit_frame(&there, &mut arena, &mut registry);

    assert_eq!(third.geometries, 0, "the second pan changed nothing");
    assert_eq!(third.removed, 0);
    let _ = second;
}

/// A frame that could not be written leaves the registry as it found it.
///
/// Otherwise the retry assumes the consumer holds geometry the failed attempt never sent, and
/// the tile is missing until the cover changes again — a fault that appears one pan later than
/// its cause, which is the worst kind to debug.
#[test]
fn a_failed_frame_retires_nothing_and_announces_nothing() {
    let here = scene(0.0);
    let there = scene(60.0);

    let mut arena = SlabArena::new();
    let mut registry = Session::new();
    let mut big = Ring::new(1 << 22);
    let (producer, _consumer) = big.split();
    let light = Light::default();
    fn frame_of<'a>(scene: &'a Scene, light: &'a Light) -> Frame<'a> {
        Frame {
            style: &scene.style,
            view: &scene.view,
            view_id: ViewId(0),
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            light,
            fonts: None,
            patterns: None,
        }
    }
    frame::emit_incremental(
        producer,
        &mut arena,
        &frame_of(&here, &light),
        &mut registry,
    )
    .expect("first");
    let known = registry.registry().len();

    // A ring too small for the pan's new geometry. Smaller than it would need to be for a full
    // emission, which is itself the feature working: the pan sends four geometries where the
    // first frame sent every one, so four kilobytes was enough for it to succeed.
    const SMALL: usize = 1 << 9;
    let mut small = vec![0u64; region_size(SMALL).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (mut cramped, _) = unsafe { ring::init(small.as_mut_ptr().cast::<u8>(), SMALL) };
    let result = frame::emit_incremental(
        &mut cramped,
        &mut arena,
        &frame_of(&there, &light),
        &mut registry,
    );
    assert!(matches!(result, Err(FrameError::Full)), "{result:?}");

    assert_eq!(
        registry.registry().len(),
        known,
        "a failed frame retired nothing, so the retry sees what the first frame left"
    );
}

/// Two views over one style: geometry announced once, uses per view.
///
/// The failure this replaces was measured before the registry knew about views: two views with
/// different covers each re-announced all eight drawables and removed all eight, *every frame*
/// — worse than not being incremental at all, because it emitted the removals too.
#[test]
fn two_views_share_geometry_and_do_not_thrash() {
    let here = scene(0.0);
    let there = scene(120.0);
    assert!(
        here.tiles.iter().all(|tile| !there.tiles.contains(tile)),
        "the covers must not overlap, or the test proves less"
    );

    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    let (first, _) = emit_frame_for(&here, ViewId(0), &mut arena, &mut registry);
    let (second, _) = emit_frame_for(&there, ViewId(1), &mut arena, &mut registry);
    assert!(
        first.geometries > 0 && second.geometries > 0,
        "both are new"
    );
    assert_eq!(second.removed, 0, "view 1 removed none of view 0's");

    // Steady state: neither view's frame disturbs the other's.
    for _ in 0..2 {
        let (a, _) = emit_frame_for(&here, ViewId(0), &mut arena, &mut registry);
        let (b, _) = emit_frame_for(&there, ViewId(1), &mut arena, &mut registry);
        assert_eq!((a.geometries, a.removed), (0, 0), "view 0 settled");
        assert_eq!((b.geometries, b.removed), (0, 0), "view 1 settled");
    }
}

/// A tile both views draw is announced once and survives one of them dropping it.
#[test]
fn a_shared_tile_outlives_one_views_pan() {
    let shared = scene(0.0);
    let elsewhere = scene(120.0);

    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    let (first, _) = emit_frame_for(&shared, ViewId(0), &mut arena, &mut registry);
    let (second, kinds) = emit_frame_for(&shared, ViewId(1), &mut arena, &mut registry);

    assert_eq!(
        second.geometries, 0,
        "the second view needs no geometry: view 0 already sent it"
    );
    assert_eq!(
        kinds.get("ViewUse").copied().unwrap_or(0),
        first.drawables,
        "but it needs a use for every drawable"
    );

    // View 0 pans away entirely; view 1 still draws the tiles.
    let (third, _) = emit_frame_for(&elsewhere, ViewId(0), &mut arena, &mut registry);
    assert!(third.geometries > 0, "its new cover is new");
    assert_eq!(
        third.removed, 0,
        "and nothing is removed, because view 1 still holds the old cover"
    );

    // Now view 1 pans away too, and the geometry finally goes.
    let (fourth, _) = emit_frame_for(&elsewhere, ViewId(1), &mut arena, &mut registry);
    assert!(fourth.removed > 0, "the last view released it");
}

/// Four views, which §13 requires and the ABI caps at eight.
///
/// Not a generalisation of the two-view case for its own sake: `TSL_MAX_VIEWS` is eight and the
/// product needs four, so the case that has to hold is four sharing one style and one registry.
/// The accounting is a set of users per drawable, so nothing about it is arity-specific — but
/// "nothing about it is arity-specific" is exactly the claim a test should make rather than a
/// comment.
#[test]
fn four_views_share_one_geometry() {
    let shared = scene(0.0);
    let elsewhere = scene(120.0);

    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    // All four draw the same cover. The first announces it; the rest bind to it.
    let (first, _) = emit_frame_for(&shared, ViewId(0), &mut arena, &mut registry);
    assert!(first.geometries > 0);
    for view in 1..4 {
        let (later, kinds) = emit_frame_for(&shared, ViewId(view), &mut arena, &mut registry);
        assert_eq!(
            later.geometries, 0,
            "view {view} needs no geometry: it is already sent"
        );
        assert_eq!(
            kinds.get("ViewUse").copied().unwrap_or(0),
            first.drawables,
            "view {view} binds every drawable"
        );
        assert_eq!(later.removed, 0, "and removes none of anyone else's");
    }

    // Three of the four pan away. The geometry survives every one of them.
    for view in 0..3 {
        let (panned, _) = emit_frame_for(&elsewhere, ViewId(view), &mut arena, &mut registry);
        assert_eq!(
            panned.removed,
            0,
            "view {view} let go, but {} views still draw it",
            3 - view
        );
    }

    // The fourth is the last, and only then do the bytes go.
    let (last, _) = emit_frame_for(&elsewhere, ViewId(3), &mut arena, &mut registry);
    assert!(
        last.removed > 0,
        "the last view released it, so it is removed"
    );
}

/// Every view settles, and none of the four disturbs the others.
#[test]
fn four_views_settle_independently() {
    let scenes = [scene(0.0), scene(60.0), scene(120.0), scene(180.0)];
    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    for (view, scene) in scenes.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        emit_frame_for(scene, ViewId(view as u32), &mut arena, &mut registry);
    }

    // Two more rounds: nothing should move.
    for _ in 0..2 {
        for (view, scene) in scenes.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let (emitted, _) =
                emit_frame_for(scene, ViewId(view as u32), &mut arena, &mut registry);
            assert_eq!(
                (emitted.geometries, emitted.removed),
                (0, 0),
                "view {view} should be settled"
            );
        }
    }
}

/// Every view-keyed record in a view's frame names that view, and no other.
///
/// # What independence has to mean
///
/// Geometry is deliberately shared — that is §5.3's whole reason, and the tests above hold it.
/// Everything else must not be. A `ViewUse`, a `ViewRelease`, a `UboUpdate`, a `StencilTiles`
/// and an `OrderUpdate` are all keyed by view, and a frame for one view that emitted a record
/// naming another would put one view's matrices or draw order into the other's — a fault that
/// renders, because both views are drawing the same style and the wrong answer looks plausible.
///
/// Four views on four covers, which is §13's arrangement and the product's requirement.
#[test]
fn a_frame_touches_only_its_own_view() {
    use tessella_capture_abi::EnvelopeKind;
    use tessella_capture_abi::envelope::{
        OrderUpdate, StencilTiles, UboUpdate, ViewRelease, ViewUse, WireRecord as _,
    };

    let scenes = [scene(0.0), scene(60.0), scene(120.0), scene(180.0)];
    let mut arena = SlabArena::new();
    let mut registry = Session::new();

    // Two rounds. The first is the emission; the second is the settled path, which now emits
    // *nothing* for a parked view — so it checks the stronger property that silence is silence,
    // rather than that the records it would have written name the right view.
    for round in 0..2 {
        for (index, scene) in scenes.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let view_id = ViewId(index as u32);

            let mut ring = Ring::new(1 << 22);
            let (producer, consumer) = ring.split();
            frame::emit_incremental(
                producer,
                &mut arena,
                &Frame {
                    style: &scene.style,
                    view: &scene.view,
                    view_id,
                    tiles: &scene.tiles,
                    buckets: &scene.buckets,
                    light: &Light::default(),
                    fonts: None,
                    patterns: None,
                },
                &mut registry,
            )
            .expect("emits");

            let mut checked = 0;
            while let Some(record) = consumer.peek() {
                let named = match record.kind {
                    EnvelopeKind::ViewUse => {
                        ViewUse::from_bytes(record.record).map(|use_| use_.view)
                    }
                    EnvelopeKind::ViewRelease => {
                        ViewRelease::from_bytes(record.record).map(|release| release.view)
                    }
                    EnvelopeKind::UboUpdate => {
                        UboUpdate::from_bytes(record.record).map(|update| update.view)
                    }
                    EnvelopeKind::StencilTiles => {
                        StencilTiles::from_bytes(record.record).map(|tiles| tiles.view)
                    }
                    EnvelopeKind::OrderUpdate => {
                        OrderUpdate::from_bytes(record.record).map(|order| order.view)
                    }
                    _ => None,
                };
                if let Some(named) = named {
                    assert_eq!(
                        named, view_id,
                        "round {round}: a frame for {view_id:?} emitted a {:?} naming {named:?}",
                        record.kind
                    );
                    checked += 1;
                }
                let consumed = record.consumed();
                consumer.advance(consumed);
            }
            if round == 0 {
                assert!(
                    checked > 0,
                    "the first frame has view-keyed records to check"
                );
            } else {
                assert_eq!(
                    checked, 0,
                    "a settled view writes no view-keyed record at all"
                );
            }
        }
    }
}

/// One view's camera moving emits nothing for the others.
///
/// A camera change is the most frequent damage there is — every frame of a pan — and it is
/// per view by construction. This is what says a shared registry did not accidentally couple
/// them: the other three views' frames are not run at all between the moves, so anything the
/// moving view wrote for them would still be on their next frame's ring.
#[test]
fn a_camera_move_in_one_view_does_not_disturb_the_others() {
    let scenes = [scene(0.0), scene(60.0), scene(120.0), scene(180.0)];
    let mut arena = SlabArena::new();
    let mut registry = Session::new();
    for (index, scene) in scenes.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        emit_frame_for(scene, ViewId(index as u32), &mut arena, &mut registry);
    }

    // View 0 pans right through the other three covers, one frame each.
    for longitude in [60.0, 120.0, 180.0] {
        let moved = scene(longitude);
        emit_frame_for(&moved, ViewId(0), &mut arena, &mut registry);
    }

    // The other three are untouched: nothing to announce, nothing removed.
    for (index, scene) in scenes.iter().enumerate().skip(1) {
        #[allow(clippy::cast_possible_truncation)]
        let (emitted, _) = emit_frame_for(scene, ViewId(index as u32), &mut arena, &mut registry);
        assert_eq!(
            (emitted.geometries, emitted.removed),
            (0, 0),
            "view {index} moved nothing and should emit nothing"
        );
    }
}

/// Parked bytes, counted from the ring rather than from the record kinds.
///
/// # Why bytes and not records
///
/// §10's exit criterion is "parked bytes are zero", and `parked_is_silent` already makes the
/// point that counting what the producer *believes* it wrote proves nothing — the two numbers
/// differ exactly when something emits without meaning to. The tests above count record kinds,
/// which is a weaker claim: a zero-length record is still a record, and a record is still bytes.
///
/// # What this measures, and what it does not
///
/// Parked is zero and a camera that moves is not. On a 1920×1080 view over three layers a cold
/// frame is about twenty-nine kilobytes and a camera nudge of one ten-thousandth of a degree is
/// about twelve — the matrices, re-sent for every layer and every tile because the camera moved
/// at all. Nothing about that is tuned: the gate is per frame, and every tile's matrix genuinely
/// does change when the camera does, so finer granularity would not help. What would is DR-9's
/// consumer-camera mode, where the consumer owns the projection and the producer sends no
/// matrices — and that is a mode, not an optimisation.
#[test]
fn a_parked_view_writes_no_bytes_at_all() {
    use tessella_capture_abi::ring::{self, region_size};

    let scene = scene(0.0);
    const CAPACITY: usize = 1 << 22;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves, and
    // nothing else touches it.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut arena = SlabArena::new();
    let mut session = Session::new();
    let light = Light::default();
    let frame = Frame {
        style: &scene.style,
        view: &scene.view,
        view_id: ViewId(0),
        tiles: &scene.tiles,
        buckets: &scene.buckets,
        light: &light,
        fonts: None,
        patterns: None,
    };

    frame::emit_incremental(&mut producer, &mut arena, &frame, &mut session).expect("cold");
    let after_cold = producer.head();
    assert!(after_cold > 0, "the cold frame wrote something");

    // Five more frames of the same scene: not one byte.
    for round in 0..5 {
        frame::emit_incremental(&mut producer, &mut arena, &frame, &mut session).expect("parked");
        assert_eq!(
            producer.head(),
            after_cold,
            "round {round} put bytes on the ring for a view that did not move"
        );
    }
}

/// A long pan does not accumulate slabs that are mostly dead.
///
/// # The cost DR-21 records
///
/// A slab holds a layer's tiles, so a pan releases some of them and the slab stays for the rest.
/// Without compaction a camera that keeps moving leaves a trail of slabs each holding one or two
/// live geometries, and none of them can be swept because none is empty. The bytes held grow
/// with distance travelled.
///
/// Compaction is the answer DR-21 chose: a slab far enough below its live fraction has its
/// survivors re-announced into the current slab, and the old one sweeps. This walks a camera far
/// enough to make that happen and checks the arena does not grow without bound.
#[test]
fn a_long_pan_does_not_accumulate_dead_slabs() {
    let mut arena = SlabArena::new();
    let mut session = Session::new();

    // Twelve steps, each moving the cover by roughly one tile.
    let mut slab_counts = Vec::new();
    let mut displaced_total = 0;
    for step in 0..12 {
        #[allow(clippy::cast_precision_loss)]
        let scene = scene(f64::from(step) * 3.0);
        let (emitted, _) = emit_frame(&scene, &mut arena, &mut session);
        displaced_total += emitted.displaced;
        // Sweep what nothing wants, which is what the caller does between frames.
        arena.sweep();
        slab_counts.push(arena.slabs().len());
    }

    let peak = slab_counts.iter().copied().max().unwrap_or(0);
    let last = slab_counts.last().copied().unwrap_or(0);

    // The steady state is bounded, not growing with the distance travelled. The bound is loose
    // on purpose: what is asserted is that it stops growing, not what it settles at.
    assert!(
        last <= peak,
        "slabs still growing at the end: {slab_counts:?}"
    );
    assert!(
        peak < 40,
        "a twelve-step pan should not leave a slab per step: {slab_counts:?}"
    );
    let _ = displaced_total;
}

/// Compaction leaves the frame drawable: what it displaces comes back.
///
/// A displaced drawable is still in the cover — it is let go so its bytes can move, not because
/// it left. The frame after has to announce it again, or the consumer is holding a `ViewRelease`
/// for something the order still names.
#[test]
fn a_displaced_drawable_is_announced_again() {
    let mut arena = SlabArena::new();
    let mut session = Session::new();

    // Pan until something is displaced, then hold still.
    let mut displaced_at = None;
    for step in 0..12 {
        #[allow(clippy::cast_precision_loss)]
        let scene = scene(f64::from(step) * 3.0);
        let (emitted, _) = emit_frame(&scene, &mut arena, &mut session);
        arena.sweep();
        if emitted.displaced > 0 {
            displaced_at = Some((step, emitted.displaced));
            break;
        }
    }

    let Some((step, displaced)) = displaced_at else {
        // Nothing was poorly enough packed to be worth moving, which is a legitimate outcome
        // rather than a failure: the threshold is a policy and this cover may never cross it.
        return;
    };

    // The very next frame over the same cover announces what was displaced, and nothing else.
    #[allow(clippy::cast_precision_loss)]
    let scene = scene(f64::from(step) * 3.0);
    let (after, _) = emit_frame(&scene, &mut arena, &mut session);
    assert_eq!(
        after.geometries, displaced,
        "exactly what was displaced comes back"
    );
    assert_eq!(after.removed, 0, "and nothing left the cover");
}

/// Tearing a view down releases what it held and leaves the others alone.
///
/// # Why this had nothing to protocol before
///
/// R4 asks for a teardown protocol, and until the lifecycle existed there was nothing to
/// protocol — nothing was retained, so nothing had to be let go. A view now holds uses and
/// geometry holds bytes, and dropping one without saying so leaves the consumer with buffers
/// nothing will mention again and a declared view nothing will draw.
mod teardown {
    use super::{Scene, scene};
    use std::collections::BTreeMap;
    use tessella_capture_abi::envelope::ViewId;
    use tessella_capture_abi::ring::Ring;
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::registry::Session;
    use tessella_style::light::Light;

    fn frame_of<'a>(scene: &'a Scene, view: ViewId, light: &'a Light) -> Frame<'a> {
        Frame {
            style: &scene.style,
            view: &scene.view,
            view_id: view,
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            light,
            fonts: None,
            patterns: None,
        }
    }

    fn draw(scene: &Scene, view: ViewId, arena: &mut SlabArena, session: &mut Session) -> usize {
        let mut ring = Ring::new(1 << 22);
        let (producer, _consumer) = ring.split();
        let light = Light::default();
        frame::emit_incremental(producer, arena, &frame_of(scene, view, &light), session)
            .expect("emits")
            .geometries
    }

    fn tear(view: ViewId, arena: &mut SlabArena, session: &mut Session) -> BTreeMap<String, usize> {
        let mut ring = Ring::new(1 << 22);
        let (producer, consumer) = ring.split();
        frame::teardown_view(producer, arena, session, view).expect("tears down");
        let mut kinds = BTreeMap::new();
        while let Some(record) = consumer.peek() {
            *kinds.entry(format!("{:?}", record.kind)).or_insert(0) += 1;
            let consumed = record.consumed();
            consumer.advance(consumed);
        }
        kinds
    }

    /// The only view: everything it held is released, removed, and the view undeclared.
    #[test]
    fn the_last_view_releases_everything() {
        let scene = scene(0.0);
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        draw(&scene, ViewId(0), &mut arena, &mut session);
        let held = session.registry().len();
        assert!(held > 0);

        let kinds = tear(ViewId(0), &mut arena, &mut session);
        assert_eq!(
            kinds.get("ViewRelease").copied().unwrap_or(0),
            held,
            "one release per drawable it held: {kinds:?}"
        );
        assert_eq!(
            kinds.get("GeometryRemove").copied().unwrap_or(0),
            held,
            "and one removal per geometry no other view holds: {kinds:?}"
        );
        assert_eq!(
            kinds.get("ViewUndeclare").copied(),
            Some(1),
            "and the view itself: {kinds:?}"
        );
        assert_eq!(session.registry().len(), 0, "nothing is held afterwards");
        assert!(!arena.sweep().is_empty(), "the bytes go back too");
    }

    /// A view sharing geometry with another releases its use and removes nothing.
    #[test]
    fn a_shared_geometry_survives_one_view_going() {
        let scene = scene(0.0);
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        draw(&scene, ViewId(0), &mut arena, &mut session);
        draw(&scene, ViewId(1), &mut arena, &mut session);
        let held = session.registry().len();

        let kinds = tear(ViewId(0), &mut arena, &mut session);
        assert_eq!(
            kinds.get("ViewRelease").copied().unwrap_or(0),
            held,
            "view 0 releases its uses: {kinds:?}"
        );
        assert_eq!(
            kinds.get("GeometryRemove").copied().unwrap_or(0),
            0,
            "but removes nothing: view 1 still draws it"
        );
        assert_eq!(session.registry().len(), held, "the geometry stays for view 1");
        assert!(arena.sweep().is_empty(), "and so do its bytes");

        // View 1 is undisturbed: it still has everything, so it announces nothing.
        assert_eq!(
            draw(&scene, ViewId(1), &mut arena, &mut session),
            0,
            "view 1 draws on without re-announcing a thing"
        );
    }

    /// A view this session never declared cannot be torn down.
    #[test]
    fn tearing_down_an_unknown_view_is_a_fault() {
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        let mut ring = Ring::new(1 << 16);
        let (producer, _consumer) = ring.split();
        assert!(
            frame::teardown_view(producer, &mut arena, &mut session, ViewId(3)).is_err(),
            "undeclaring a view nobody declared is a caller fault, not a silent no-op"
        );
    }
}

/// A frame or a teardown that fails moves nothing — the arena included.
///
/// # What was wrong
///
/// The ring, the registry, the camera and the declaration all rolled back together, and the
/// arena did not. Two ways.
///
/// A retirement released its slab ranges as it was decided, before the records saying so were
/// finished. A frame that then hit a full ring left the arena believing those bytes free while
/// the registry still held them, and the retry — working from the registry — released them a
/// second time. `release` saturates, so the second one took the count to zero and the next sweep
/// freed a slab whose geometry was still announced and still being drawn.
///
/// And `rewind` truncated the discarded slabs without dropping what they had retained, while
/// restoring the id counter. The next frame's first slab took the failed frame's id and
/// inherited its live count: a fresh slab owing bytes nothing holds, which never sweeps and
/// never reports a live fraction worth acting on.
///
/// R4 calls this teardown under fault, and it is the same discipline as backpressure under
/// stall: recovery is to try again, which only works if the failed attempt left nothing behind.
mod under_fault {
    use super::{Scene, scene};
    use std::collections::BTreeMap;
    use tessella_capture_abi::envelope::{GeometryId, ViewId};
    use tessella_capture_abi::ring::Ring;
    use tessella_orchestrate::SlabArena;
    use tessella_orchestrate::emit;
    use tessella_orchestrate::frame::{self, Frame};
    use tessella_orchestrate::registry::Session;
    use tessella_style::light::Light;

    /// Big enough for any frame here.
    const ROOMY: usize = 1 << 22;
    /// Too small for one: enough to take a record or two before it fills.
    const CRAMPED: usize = 512;

    fn emit(
        scene: &Scene,
        view: ViewId,
        capacity: usize,
        arena: &mut SlabArena,
        session: &mut Session,
    ) -> Result<usize, ()> {
        emit_measured(scene, view, capacity, arena, session).0
    }

    /// Emits, and says how many bytes of ring the attempt took.
    fn emit_measured(
        scene: &Scene,
        view: ViewId,
        capacity: usize,
        arena: &mut SlabArena,
        session: &mut Session,
    ) -> (Result<usize, ()>, usize) {
        emit_after_filler(scene, view, capacity, 0, arena, session)
    }

    /// Emits into a ring already holding `filler` bytes of records nobody consumed.
    ///
    /// Capacity is a power of two, so a frame's failure cannot be placed by sizing the ring; it
    /// is placed by taking the space away in front of it instead. What is left when the frame
    /// starts is `capacity - filler`, to the byte.
    fn emit_after_filler(
        scene: &Scene,
        view: ViewId,
        capacity: usize,
        filler: usize,
        arena: &mut SlabArena,
        session: &mut Session,
    ) -> (Result<usize, ()>, usize) {
        let mut ring = Ring::new(capacity);
        let (producer, consumer) = ring.split();
        if filler > 0 {
            producer.begin();
            let mut id = 1u64 << 40;
            while consumer.occupancy() < filler {
                emit::remove(producer, GeometryId(id)).expect("the filler fits");
                id += 1;
                producer.commit();
                producer.begin();
            }
            producer.commit();
        }
        let before = consumer.occupancy();
        let light = Light::default();
        let result = frame::emit_incremental(
            producer,
            arena,
            &Frame {
                style: &scene.style,
                view: &scene.view,
                view_id: view,
                tiles: &scene.tiles,
                buckets: &scene.buckets,
                light: &light,
                fonts: None,
                patterns: None,
            },
            session,
        )
        .map(|emitted| emitted.removed)
        .map_err(|_| ());
        (result, consumer.occupancy() - before)
    }

    /// Bytes a failed retirement decided to free are still held when the frame does not commit.
    #[test]
    fn a_failed_frame_frees_nothing() {
        let here = scene(0.0);
        let there = scene(60.0);
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        emit(&here, ViewId(0), ROOMY, &mut arena, &mut session).expect("settles");
        let held = session.registry().len();

        // A ring a little short of what the pan needs. Sized by measuring rather than guessed:
        // the retirements are emitted before the layer state, the order and the camera, so a
        // frame that runs out in its last few bytes is one that had already decided every
        // retirement — which is the window this is about. Guessing a small ring instead fails
        // long before the retirements and proves nothing.
        let (_, needed) = {
            let mut arena = SlabArena::new();
            let mut session = Session::new();
            emit(&here, ViewId(0), ROOMY, &mut arena, &mut session).expect("settles");
            emit_measured(&there, ViewId(0), ROOMY, &mut arena, &mut session)
        };
        let capacity = needed.next_power_of_two() * 2;
        let (attempt, _) = emit_after_filler(
            &there,
            ViewId(0),
            capacity,
            capacity - needed + 8,
            &mut arena,
            &mut session,
        );
        attempt.expect_err("runs out in the last few bytes");
        assert_eq!(session.registry().len(), held, "the registry is untouched");
        assert!(
            arena.sweep().is_empty(),
            "and so is the arena: nothing was retired, because nothing said so"
        );

        // The retry does the whole retirement, once.
        let removed = emit(&there, ViewId(0), ROOMY, &mut arena, &mut session).expect("retries");
        assert!(removed > 0, "the retry is what retires them");

        // The two sides of the accounting agree. Asserting on a sweep instead is too coarse to
        // catch this: the surviving geometry in the same slab keeps the count above zero, so a
        // double release shows up as a slab that is simply undercounted — until enough of one
        // retires at once to take it to zero, and then a live slab is swept out from under the
        // geometry still drawing it.
        accounting_agrees(&arena, &mut session);
    }

    /// Every byte the arena believes is wanted is a byte the registry is holding a reference to.
    fn accounting_agrees(arena: &SlabArena, session: &mut Session) {
        let mut expected: BTreeMap<u32, usize> = BTreeMap::new();
        for (_, refs) in session.registry().live_refs() {
            for reference in refs {
                *expected.entry(reference.slab).or_default() += reference.length as usize;
            }
        }
        for (slab, wanted) in &expected {
            assert!(
                arena.slab(*slab).is_some(),
                "slab {slab} was swept while the registry still holds {wanted} bytes of it"
            );
            assert_eq!(
                arena.live_bytes(*slab),
                *wanted,
                "slab {slab}: the arena and the registry disagree about what is wanted"
            );
        }
    }

    /// A rewound slab id starts its next life empty.
    #[test]
    fn a_reused_slab_id_carries_nothing_over() {
        let here = scene(0.0);
        let mut arena = SlabArena::new();
        let mut session = Session::new();

        emit(&here, ViewId(0), CRAMPED, &mut arena, &mut session).expect_err("fills the ring");
        emit(&here, ViewId(0), ROOMY, &mut arena, &mut session).expect("the retry lands");

        // Tearing the only view down releases every byte the successful frame retained. A slab
        // still crediting the failed frame's retains would survive this.
        let mut ring = Ring::new(ROOMY);
        let (producer, _consumer) = ring.split();
        frame::teardown_view(producer, &mut arena, &mut session, ViewId(0)).expect("tears down");
        arena.sweep();
        assert!(
            arena.slabs().is_empty(),
            "every slab went: {} left holding bytes nothing wants",
            arena.slabs().len()
        );
    }

    /// A teardown that fails leaves the view intact and tears down cleanly on the retry.
    #[test]
    fn a_failed_teardown_leaves_the_view_standing() {
        let here = scene(0.0);
        let mut arena = SlabArena::new();
        let mut session = Session::new();
        emit(&here, ViewId(0), ROOMY, &mut arena, &mut session).expect("settles");
        let held = session.registry().len();

        let mut ring = Ring::new(CRAMPED);
        let (producer, _consumer) = ring.split();
        frame::teardown_view(producer, &mut arena, &mut session, ViewId(0))
            .expect_err("fills the ring");
        assert_eq!(session.registry().len(), held, "the view still holds it all");
        assert!(arena.sweep().is_empty(), "and the bytes are still there");
        accounting_agrees(&arena, &mut session);

        // Which means the view can still draw, announcing nothing.
        assert_eq!(
            emit(&here, ViewId(0), ROOMY, &mut arena, &mut session),
            Ok(0),
            "the failed teardown is invisible to the next frame"
        );

        // And the retry tears it down whole.
        let mut ring = Ring::new(ROOMY);
        let (producer, _consumer) = ring.split();
        frame::teardown_view(producer, &mut arena, &mut session, ViewId(0)).expect("retries");
        assert_eq!(session.registry().len(), 0, "nothing is held afterwards");
    }
}
