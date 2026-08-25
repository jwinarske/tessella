//! R0's other exit criterion: parked bytes == 0.
//!
//! §10 asks for two things from R0 — that the stream matches the probe, and that a parked view
//! emits nothing. This is the second, measured the only way that means anything: from the ring's
//! own head counter, over a real producer emitting real geometry from the hermetic style.
//!
//! Counting what the producer *believes* it wrote would prove nothing. The two numbers differ
//! exactly when something emits without meaning to, and that difference is the entire subject.

use tessella_capture_abi::envelope::GeometryId;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{Producer, Ring};
use tessella_orchestrate::binder::{FILL_FAMILY, attribute_ids, layout, permutation_key};
use tessella_orchestrate::damage::{CameraKey, DamageTracker, TrafficMeter};
use tessella_orchestrate::tile::{TileId, bucket_for, build_tile};
use tessella_orchestrate::{SlabArena, encode_fill};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");
const VIEW: ViewId = ViewId(0);

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features() -> Vec<tessella_source::GeoJsonFeature> {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    geojson::read(&source.data).expect("features")
}

fn camera() -> CameraKey {
    CameraKey {
        center_zoom0: [255.843_555_555_555_55, 170.258_551_462_728_78],
        zoom: 13.0,
        bearing: 0.0,
        pitch: 0.0,
        pixels_per_meter: 1.0,
    }
}

/// Emits the tile's fill geometry, exactly as a producer would.
fn emit_tile(producer: &mut Producer, arena: &mut SlabArena) {
    let style = style();
    let features = features();
    let buckets = build_tile(
        &style,
        "probe",
        TileId::new(13, 4093, 2723),
        &features,
        TilingOptions::default(),
    )
    .expect("tile builds");

    // The layer bucket carries its own paint buffer, so nothing here re-derives which vertices
    // belong to which feature — the tile builder already knew, and an estimate made here (the
    // vertex count divided by the feature count) is wrong the moment a ring is clipped away.
    let layer_bucket = bucket_for(&buckets, "fill-datadriven").expect("a fill layer");
    let bucket = layer_bucket.content.as_fill().expect("a fill bucket");

    let ids = attribute_ids(FILL_FAMILY);
    let key = permutation_key(&layer_bucket.paint, &ids);
    let vertex_layout = layout(&layer_bucket.binder, &ids, |attr_id| {
        tessella_capture_abi::declared_for(tessella_capture_abi::BuiltIn::FillShader, attr_id)
            .map(|attribute| (attribute.binding, attribute.declared))
    });

    let encoded = encode_fill(
        arena,
        GeometryId(1),
        bucket,
        &vertex_layout,
        layer_bucket.binder.data(),
        key,
    );
    tessella_orchestrate::emit::write(producer, &encoded).expect("the ring takes it");
}

/// The exit criterion. A view that has emitted its geometry and then parks writes **zero**
/// further bytes, over a thousand frames.
///
/// Zero rather than "few" is what makes this a guarantee. A producer re-emitting a
/// byte-identical camera every frame would satisfy any threshold above zero while keeping the
/// consumer's tick awake and, on a DVFS-governed part, the whole SoC out of idle (§12.8).
#[test]
fn a_parked_view_writes_no_ring_bytes() {
    let mut ring = Ring::new(64 * 1024);
    let producer = ring.producer();
    let mut arena = SlabArena::new();
    let mut tracker = DamageTracker::new();

    // First frame: everything is new, so everything is emitted.
    let first = tracker.begin_frame(VIEW, camera());
    assert!(first.camera && first.geometry);
    emit_tile(producer, &mut arena);
    let after_first = producer.head();
    assert!(after_first > 0, "the first frame writes something");

    // Then park.
    let mut meter = TrafficMeter::new(producer.head());
    for frame in 0..1000 {
        let work = tracker.begin_frame(VIEW, camera());
        assert!(work.is_idle(), "frame {frame} wanted work: {work:?}");
        if work.geometry {
            emit_tile(producer, &mut arena);
        }
        meter.frame();
    }

    let traffic = meter.traffic(producer.head());
    assert!(
        traffic.is_silent(),
        "a parked view wrote {} bytes over {} frames",
        traffic.bytes,
        traffic.frames
    );
    assert_eq!(traffic.frames, 1000);
    assert_eq!(producer.head(), after_first, "the ring did not move");
}

/// Churn wakes the view, and it goes quiet again afterwards. A gate that latched open would
/// pass the parked test only until something happened once.
#[test]
fn the_view_goes_quiet_again_after_churn() {
    let mut ring = Ring::new(64 * 1024);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    let mut tracker = DamageTracker::new();

    tracker.begin_frame(VIEW, camera());
    emit_tile(producer, &mut arena);

    for _ in 0..10 {
        assert!(tracker.begin_frame(VIEW, camera()).is_idle());
    }

    // A tile lands.
    tracker.mark_dirty(VIEW);
    let before = producer.head();
    let work = tracker.begin_frame(VIEW, camera());
    assert!(work.geometry);
    emit_tile(producer, &mut arena);
    assert!(producer.head() > before, "churn writes");

    // Drain so the ring has room, then confirm it settles.
    while consumer.peek().is_some() {
        let consumed = consumer.peek().expect("a record").consumed();
        consumer.advance(consumed);
    }

    let mut meter = TrafficMeter::new(producer.head());
    for _ in 0..100 {
        let work = tracker.begin_frame(VIEW, camera());
        assert!(work.is_idle());
        meter.frame();
    }
    assert!(meter.traffic(producer.head()).is_silent());
}

/// Two views park independently. One waking must not wake the other, or a cluster inset would
/// pay for every frame the main display moves (§5.2).
#[test]
fn one_views_churn_does_not_wake_another() {
    let mut ring = Ring::new(64 * 1024);
    let producer = ring.producer();
    let mut arena = SlabArena::new();
    let mut tracker = DamageTracker::new();
    let other = ViewId(1);

    tracker.begin_frame(VIEW, camera());
    tracker.begin_frame(other, camera());
    emit_tile(producer, &mut arena);

    tracker.mark_dirty(other);

    let mut meter = TrafficMeter::new(producer.head());
    for _ in 0..100 {
        assert!(
            tracker.begin_frame(VIEW, camera()).is_idle(),
            "the parked view stays parked"
        );
        // The other view does have work, but this test only emits for VIEW.
        let _ = tracker.begin_frame(other, camera());
        meter.frame();
    }
    assert!(meter.traffic(producer.head()).is_silent());
}

/// A zoom that stays between two integer levels emits camera bytes and no geometry (§13.1).
///
/// # Why this is a guarantee rather than an optimisation
///
/// Fractional zoom is the commonest thing a map does: every pinch, every fly-to, every inertial
/// settle spends most of its frames between integer levels. The cover does not change there —
/// the same tiles serve the whole interval — so nothing about the geometry is different, only
/// where the camera is looking from. DR-9 has Filament re-project, and §13.1 says the producer's
/// share is interpolation state only: mix factors and screen-space sizing, hundreds of bytes.
///
/// Emitting geometry anyway would not look wrong. It would cost a tile's worth of ring traffic
/// per frame during the one interaction a user performs most, which on a DVFS-governed part is
/// the difference between a gesture costing a few hundred bytes and one costing megabytes. That
/// is why the assertion is zero envelopes rather than a byte threshold: a threshold is satisfied
/// by re-emitting something slightly smaller.
#[test]
fn a_fractional_zoom_emits_no_geometry() {
    let mut ring = Ring::new(64 * 1024);
    let producer = ring.producer();
    let mut arena = SlabArena::new();
    let mut tracker = DamageTracker::new();

    let at = |zoom: f64| CameraKey { zoom, ..camera() };

    // Settle at 13.0 with everything emitted.
    let first = tracker.begin_frame(VIEW, at(13.0));
    assert!(first.camera && first.geometry);
    emit_tile(producer, &mut arena);
    let settled = producer.head();

    // Now zoom 13.0 → 13.9 without crossing 14. Sixty frames, the length of a real gesture, and
    // every one of them at a zoom the previous frame was not at.
    let mut geometry_frames = 0;
    let mut camera_frames = 0;
    for frame in 1..=60 {
        let zoom = 13.0 + 0.9 * (f64::from(frame) / 60.0);
        let work = tracker.begin_frame(VIEW, at(zoom));
        if work.geometry {
            geometry_frames += 1;
            emit_tile(producer, &mut arena);
        }
        if work.camera {
            camera_frames += 1;
        }
    }

    assert_eq!(
        geometry_frames, 0,
        "nothing landed, so no frame of the zoom should want geometry"
    );
    assert_eq!(
        camera_frames, 60,
        "every frame moved the camera, so every frame owes camera bytes"
    );
    assert_eq!(producer.head(), settled, "and no geometry reached the ring");
}

/// The cover is the same set of tiles across a whole integer level, and changes at the boundary.
///
/// This is the fact the invariant above rests on, and it lives here rather than being inferred
/// from it. The damage tracker knows nothing about zoom levels — its `geometry` flag means
/// "something landed", not "the camera crossed a level" — so a test that asked the *tracker*
/// whether crossing 14 wants geometry would be asking the wrong component, and would pass or
/// fail for reasons unrelated to what §13.1 promises. What actually makes a crossing expensive
/// is that the cover changes and new tiles have to be fetched and built.
#[test]
fn a_cover_is_constant_within_an_integer_level() {
    let tiles = |zoom: f64| {
        let view = tessella_tile::cover::ViewTransform {
            longitude: -0.11,
            latitude: 51.505,
            zoom,
            width: 1024.0,
            height: 768.0,
            bearing: 0.0,
            pitch: 0.0,
        };
        let mut out: Vec<_> = tessella_tile::cover::cover(&view)
            .expect("covers")
            .into_iter()
            .map(|tile| (tile.z, tile.x, tile.y))
            .collect();
        out.sort_unstable();
        out
    };

    let base = tiles(13.0);
    assert!(!base.is_empty());
    for zoom in [13.01, 13.25, 13.5, 13.75, 13.99] {
        assert_eq!(tiles(zoom), base, "the cover moved at zoom {zoom}");
    }
    assert_ne!(
        tiles(14.0),
        base,
        "crossing into the next level is where the cover changes"
    );
}

/// A pan that does not change the cover costs one camera block a frame and nothing else (§9.3).
///
/// # What "pure" means here, and why it is worth a name
///
/// §6's contract has three cases: parked emits nothing, pure camera motion emits camera-block
/// bytes only, churn emits bytes proportional to the churn. The middle one is the one a user
/// spends most of their time in — a drag is camera motion and nothing else for every frame that
/// does not pull a new tile into view — and it is the one where a regression hides best, because
/// re-emitting geometry during a pan looks perfectly correct.
///
/// So the budget is stated as an identity rather than a bound: bytes per frame *equals* one
/// camera block. A bound of "at most a few hundred bytes" is satisfied by a producer that has
/// started sending something small every frame that it did not send before.
#[test]
fn a_pure_pan_costs_one_camera_block_a_frame() {
    use tessella_capture_abi::envelope::OrderEpoch;
    use tessella_orchestrate::camera::CameraBlock;
    use tessella_style::light::Light;
    use tessella_tile::cover::ViewTransform;

    let view_at = |longitude: f64| ViewTransform {
        longitude,
        latitude: 51.505,
        zoom: 13.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    };

    // A pan small enough to stay inside one integer level's cover: the tiles do not change, so
    // by §6 nothing but the camera owes anything.
    let base = view_at(-0.11);
    let cover_of = |view: &ViewTransform| {
        let mut tiles: Vec<_> = tessella_tile::cover::cover(view)
            .expect("covers")
            .into_iter()
            .map(|tile| (tile.z, tile.x, tile.y))
            .collect();
        tiles.sort_unstable();
        tiles
    };
    let settled_cover = cover_of(&base);

    let mut ring = Ring::new(64 * 1024);
    let producer = ring.producer();
    let mut arena = SlabArena::new();
    let mut tracker = DamageTracker::new();

    let key_at = |view: &ViewTransform| CameraKey {
        center_zoom0: tessella_tile::projection::center_zoom0(view.longitude, view.latitude),
        ..camera()
    };

    // Settle.
    let first = tracker.begin_frame(VIEW, key_at(&base));
    assert!(first.camera && first.geometry);
    emit_tile(producer, &mut arena);
    CameraBlock::new(&base, &Light::default(), OrderEpoch(1), 0, 0)
        .expect("an unrotated camera")
        .write(producer)
        .expect("writes");
    let mut head = producer.head();

    let mut sizes = Vec::new();
    for frame in 1..=40 {
        let view = view_at(-0.11 + 0.00002 * f64::from(frame));
        assert_eq!(
            cover_of(&view),
            settled_cover,
            "frame {frame} moved the cover; this is not a pure pan"
        );

        let work = tracker.begin_frame(VIEW, key_at(&view));
        assert!(work.camera, "frame {frame} moved and owes a camera");
        assert!(!work.geometry, "frame {frame} wanted geometry during a pan");

        CameraBlock::new(&view, &Light::default(), OrderEpoch(1), 0, 0)
            .expect("an unrotated camera")
            .write(producer)
            .expect("writes");

        sizes.push(producer.head() - head);
        head = producer.head();
    }

    let block = sizes[0];
    assert!(block > 0, "a camera block is not nothing");
    assert!(
        sizes.iter().all(|size| *size == block),
        "a pan's per-frame cost varied: {sizes:?}"
    );
}
