//! A frame reaches the ring whole, or it does not reach it.
//!
//! # What was wrong
//!
//! `FrameError::Full` has always been documented "a frame is emitted whole or not at all", and
//! it was not: `Producer::write` published `head` per record, so a ring that filled partway
//! through left everything up to that point visible. Measured on a 4 KiB ring, `emit` returned
//! `Full` and left thirty-seven records behind — a view declaration, two textures, uniforms and
//! thirty-odd `GeometryAdd`s, with no `OrderUpdate` and no `CameraUpdate`.
//!
//! That is not a frame missing its tail. It is a consumer holding geometry it can never draw,
//! because nothing told it where in the order the geometry goes, and never release, because the
//! producer's retry encodes the same buckets under fresh ids and forgets the old ones.
//!
//! # Why backpressure is the case that matters
//!
//! A full ring is not an error condition, it is the ordinary consequence of a consumer that
//! stalled for a frame — a compositor that missed a vsync, a driver that blocked on an upload.
//! Recovery is to try again next frame, which only works if the failed attempt left nothing
//! behind. R4 calls this backpressure under stall.

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame, FrameError};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#101418"}},
    {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
     "paint": {"fill-color": "#20344c"}},
    {"id": "banks", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#88a", "line-width": 1.5}}
  ]
}"##;

/// A style, the view over it, its cover and the buckets built for it.
type Scene = (
    Style,
    ViewTransform,
    Vec<cover::TileCoord>,
    Vec<(TileId, Vec<LayerBucket>)>,
);

fn scene() -> Scene {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        built.extend(build_sourceless(&style, id).expect("the background builds"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }
    (style, view, tiles, buckets)
}

/// A ring too small for the frame publishes nothing at all.
#[test]
fn a_frame_that_will_not_fit_leaves_nothing_behind() {
    let (style, view, tiles, buckets) = scene();

    // Large enough that the frame gets well underway before it runs out, which is the case that
    // used to leave the most behind. A ring so small that the first record fails would pass a
    // weaker version of this test for the wrong reason.
    const CAPACITY: usize = 1 << 12;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves, and
    // nothing else touches it.
    let (mut producer, mut consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut arena = SlabArena::new();
    let result = frame::emit(
        &mut producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    );

    assert!(
        matches!(result, Err(FrameError::Full)),
        "expected the ring to fill: {result:?}"
    );

    let mut kinds = Vec::new();
    while let Some(record) = consumer.peek() {
        kinds.push(record.kind);
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    assert!(
        kinds.is_empty(),
        "a failed frame published {} records: {kinds:?}",
        kinds.len()
    );

    // And the arena kept nothing either: the records that would have named these slabs were
    // discarded, so keeping them would leak a cover's geometry per failed frame.
    assert!(
        arena.slabs().next().is_none(),
        "a failed frame left {} slabs in the arena",
        arena.slabs().count()
    );
}

/// Retrying on a ring that has room succeeds, and the retry is a whole frame.
///
/// The point of discarding the first attempt is that the second is uncontaminated — no geometry
/// registered twice, no order referring to ids from the attempt that failed.
#[test]
fn the_retry_after_a_full_ring_is_a_whole_frame() {
    let (style, view, tiles, buckets) = scene();
    let build = |producer: &mut _, arena: &mut _| {
        frame::emit(
            producer,
            arena,
            &Frame {
                style: &style,
                view: &view,
                view_id: ViewId(0),
                tiles: &tiles,
                buckets: &buckets,
                light: &Light::default(),
                fonts: None,
                patterns: None,
            },
        )
    };

    const SMALL: usize = 1 << 12;
    let mut small = vec![0u64; region_size(SMALL).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) = unsafe { ring::init(small.as_mut_ptr().cast::<u8>(), SMALL) };
    let mut arena = SlabArena::new();
    assert!(build(&mut producer, &mut arena).is_err(), "the ring fills");

    // A ring with room, and the same arena the failed attempt used.
    const LARGE: usize = 1 << 20;
    let mut large = vec![0u64; region_size(LARGE).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, mut consumer) =
        unsafe { ring::init(large.as_mut_ptr().cast::<u8>(), LARGE) };
    let emitted = build(&mut producer, &mut arena).expect("the retry fits");

    let mut geometries = 0;
    let mut orders = 0;
    let mut cameras = 0;
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::GeometryAdd => geometries += 1,
            EnvelopeKind::OrderUpdate => orders += 1,
            EnvelopeKind::CameraUpdate => cameras += 1,
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }

    assert_eq!(geometries, emitted.geometries, "every geometry, once");
    assert_eq!(orders, 1, "one order");
    assert_eq!(cameras, 1, "one camera, naming that order's epoch");

    // Slab ids restart where the failed attempt began, so the retry's references resolve against
    // a table with no gap in it.
    assert!(arena.slabs().next().is_some(), "the retry allocated");
    assert_eq!(
        arena.slabs().next().map(|slab| slab.id),
        Some(0),
        "the rewind reissued ids from the mark rather than skipping past the failed attempt"
    );
}
