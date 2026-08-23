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
use tessella_orchestrate::binder::{FeatureVertices, layout, pack_attributes};
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
        TileId::new(13, 4093, 2723),
        &features,
        TilingOptions::default(),
    )
    .expect("tile builds");

    let layer = style.layer("fill-datadriven").expect("the layer");
    let paint = tessella_style::property::resolve_paint(layer).expect("resolves");
    let ids = [
        ("idFillColorVertexAttribute", 1u32),
        ("idFillOpacityVertexAttribute", 2),
        ("idFillOutlineColorVertexAttribute", 3),
    ]
    .into_iter()
    .map(|(name, id)| (String::from(name), id))
    .collect();
    let vertex_layout = layout(&paint, &ids, |attr_id| {
        tessella_capture_abi::declared_for(tessella_capture_abi::BuiltIn::FillShader, attr_id)
            .map(|attribute| (attribute.binding, attribute.declared))
    });

    let bucket = bucket_for(&buckets, "fill-datadriven")
        .and_then(|b| b.content.as_fill())
        .expect("a fill bucket");

    let polygons: Vec<_> = features
        .iter()
        .filter(|f| f.geometry.type_name() == "Polygon")
        .collect();
    let per_feature = bucket.vertices.len() / polygons.len().max(1);
    let entries: Vec<FeatureVertices<'_>> = polygons
        .iter()
        .map(|feature| FeatureVertices {
            feature: *feature,
            vertices: per_feature,
        })
        .collect();

    let packed = pack_attributes(&vertex_layout, &paint, &entries, None).expect("packs");
    let encoded = encode_fill(arena, GeometryId(1), bucket, &vertex_layout, &packed);
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
