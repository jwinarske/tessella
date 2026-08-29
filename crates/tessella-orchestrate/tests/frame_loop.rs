//! The warm loop: what a tick costs when nothing happened, and what it sends when something did.
//!
//! # Why this is the test that matters
//!
//! §13.1's invariant and §9.3's counters both reduce to one claim — traffic is proportional to
//! change — and until now nothing drove a *sequence* of frames to check it. `frame::emit` was
//! called from thirteen test files, each emitting one frame by hand, which can only ever show
//! that a frame is well-formed. Whether the second frame costs anything is a different question
//! and it is the one a running map lives or dies on.

use std::sync::Arc;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::map::{Map, Tick, Tiles};
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

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

/// Every cover tile answers with the same real tile's buckets.
///
/// The addresses differ and the geometry does not, which is what this test wants: it is about
/// how many *records* a second frame sends, and one tile's worth of real features is enough to
/// make a frame that is not trivially empty.
struct Warm {
    buckets: Arc<Vec<LayerBucket>>,
}

impl Warm {
    fn new(style: &Style) -> Self {
        let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
        let built = build_mvt_tile(style, "src", TileId::new(0, 0, 0), &decoded).expect("builds");
        Self {
            buckets: Arc::new(built),
        }
    }
}

impl Tiles for Warm {
    fn buckets(&self, _tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        Some(Arc::clone(&self.buckets))
    }
}

fn view(zoom: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// A ring big enough that filling it is never what a test is measuring.
const CAPACITY: usize = 1 << 24;

/// A settled map sends nothing at all, and keeps sending nothing.
///
/// The first tick emits because the map has never drawn. Every one after it is idle, and idle
/// has to mean *no bytes*: a loop that re-sent an unchanged frame would satisfy every
/// well-formedness test in the suite and still hold a consumer's upload path awake forever.
#[test]
fn a_settled_map_emits_once_and_then_nothing() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    let first = map
        .tick(&mut producer, &tiles)
        .expect("the first frame emits");
    let Tick::Emitted(emitted) = first else {
        panic!("the first tick must emit: nothing has been drawn yet");
    };
    assert!(
        emitted.geometries > 0,
        "the first frame carried no geometry"
    );

    for round in 0..8 {
        assert_eq!(
            map.tick(&mut producer, &tiles).expect("a settled tick"),
            Tick::Idle,
            "tick {round} sent something on a map that had not moved"
        );
    }
}

/// A camera that moves emits again; one that moves back to where it was does too.
///
/// The second half is the part worth asserting. It would be easy to write a tracker that
/// remembered only the *first* camera and reported every later frame idle, and every test that
/// only ever moves away would pass.
#[test]
fn moving_the_camera_reopens_the_gate() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    map.tick(&mut producer, &tiles).expect("the first frame");
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled"),
        Tick::Idle
    );

    map.look_at(view(5.0));
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the moved frame"),
            Tick::Emitted(_)
        ),
        "a moved camera must emit"
    );
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled again"),
        Tick::Idle
    );

    map.look_at(view(4.0));
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the returned frame"),
            Tick::Emitted(_)
        ),
        "moving back is still moving"
    );
}

/// A tile landing emits, without the camera having moved.
#[test]
fn a_tile_landing_emits() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    map.tick(&mut producer, &tiles).expect("the first frame");
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled"),
        Tick::Idle
    );

    map.mark_dirty();
    assert!(
        matches!(
            map.tick(&mut producer, &tiles).expect("the tile frame"),
            Tick::Emitted(_)
        ),
        "a source reporting churn must emit"
    );
    assert_eq!(
        map.tick(&mut producer, &tiles).expect("settled after"),
        Tick::Idle,
        "the dirty flag is consumed, not sticky"
    );
}

/// A pan re-announces only what entered, not the whole cover.
///
/// This is the claim `emit_incremental` exists to make and the one a frame loop can break
/// without failing anything else: a tile that stays in view keeps its geometry id, so its bytes
/// are not sent again. If the loop handed the emitter a fresh registry each frame — or rebuilt
/// the cover into different ids — every surviving tile would be re-announced and the frame would
/// still be perfectly well-formed.
///
/// Measured on this fixture: twenty-seven geometries on the first frame and nine on the pan, so
/// two thirds of the cover survived and cost nothing. The assertion is the inequality rather
/// than the numbers, which move with the fixture and the viewport.
#[test]
fn a_pan_sends_only_what_entered() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: as above.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut map = Map::new(style, view(4.0), ViewId(0));
    let Tick::Emitted(first) = map.tick(&mut producer, &tiles).expect("the first frame") else {
        panic!("the first tick emits");
    };

    // A nudge small enough that most of the cover survives it.
    let mut nudged = view(4.0);
    nudged.longitude += 0.35;
    map.look_at(camera::settled(&nudged));
    let Tick::Emitted(panned) = map.tick(&mut producer, &tiles).expect("the panned frame") else {
        panic!("a moved camera emits");
    };

    assert!(
        panned.geometries < first.geometries,
        "the pan announced {} geometries against the first frame's {} — a surviving tile's \
         bytes are being sent again",
        panned.geometries,
        first.geometries
    );
}
