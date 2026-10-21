// SPDX-License-Identifier: BSD-2-Clause
//! A region that fills compacts and goes on drawing, rather than failing for ever.
//!
//! # What was wrong
//!
//! `tessella.h` says of `TESSELLA_REGION_FULL`: "The frame compacts and the next tick retries."
//! It did not. Compaction ran only at the top of a tick, behind
//! `waste >= COMPACTION_FLOOR && waste >= used / 2`, and a region that has just refused a frame
//! does not reliably satisfy the second -- the cursor is near the ceiling, so half of it is a
//! large number while the dead space below it may be smaller. So the frame failed, the arena
//! rewound to exactly where it had been, and the next tick failed in the same place, for as long
//! as the map was up.
//!
//! The arena bump allocates, so this is reachable by any map whose camera keeps moving: tiles
//! are retired and their replacements allocated above them, and the dead space below the cursor
//! comes back only when compaction runs. Found on a full-screen z13 pane of the harbor demo at
//! 1920x1150, which froze after about twenty seconds. What it looks like is the worst part --
//! the tick runs, the camera advances, the producer composites at its usual rate, and the
//! consumer redraws the frame it already holds. Every counter moves and the picture does not.
//!
//! # What is asserted
//!
//! That a map driven through more churn than its region can hold keeps emitting. The region here
//! is small so the condition is reached in a test rather than after twenty seconds of a
//! real one; nothing else about it is special.

use std::sync::Arc;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::mapping::Mapping;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
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

/// Every cover tile answers with the same real tile's buckets, as `frame_loop` does.
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

fn view(longitude: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude,
        latitude: 51.505,
        zoom: 4.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    })
}

/// Slots enough for the covers below, which is not what this is about.
const SLOTS: usize = 4096;
/// A ring big enough that filling it is never what is being measured.
const CAPACITY: usize = 1 << 24;

#[test]
fn a_region_that_fills_compacts_and_keeps_drawing() {
    let style = Style::parse(STYLE).expect("the style parses");
    let tiles = Warm::new(&style);

    let mut ring_region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves.
    let (mut producer, mut consumer) =
        unsafe { ring::init(ring_region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    // Small on purpose: the whole point is to reach the full condition, and a map in the field
    // reaches it with a region thousands of times this size and a camera that keeps moving.
    let mut bytes = vec![0u8; 7 << 20];
    // SAFETY: `bytes` outlives the arena and the map, and nothing else writes it.
    let arena = SlabArena::in_region(
        unsafe { Mapping::new(bytes.as_mut_ptr(), bytes.len()) },
        SLOTS,
    );
    let mut map = Map::with_arena(style, view(-0.11), ViewId(0), arena);

    // Pan far enough, for long enough, that the region must be reused many times over.
    let mut emitted = 0usize;
    let mut failed = 0usize;
    let mut failed_in_the_last_quarter = 0usize;
    const ROUNDS: usize = 600;
    for round in 0..ROUNDS {
        #[allow(clippy::cast_precision_loss)]
        let longitude = -0.11 + (round as f64) * 0.37;
        map.look_at(view(longitude));
        match map.tick(&mut producer, &tiles) {
            Ok(Tick::Emitted(_)) => emitted += 1,
            Ok(Tick::Idle) => {}
            Err(_) => {
                failed += 1;
                if round >= ROUNDS * 3 / 4 {
                    failed_in_the_last_quarter += 1;
                }
            }
        }
        // Drained, so the ring is never what refuses a frame.
        while let Some(record) = consumer.peek() {
            let consumed = record.consumed();
            consumer.advance(consumed);
        }
    }

    assert!(
        emitted > 0,
        "nothing was emitted at all, so the region was never exercised"
    );
    // Deliberately no assertion that anything failed. With the retry in place a full region is
    // recovered inside the tick that hit it, so a healthy map may report none at all -- and a
    // test that demanded a failure would be asserting the defect.
    // The claim: a full region is a frame skipped, not a map that stops. Without the retry the
    // first failure is permanent and the whole last quarter fails.
    assert_eq!(
        failed_in_the_last_quarter,
        0,
        "the map was still failing {failed_in_the_last_quarter} of its last {} ticks ({failed} \
         in all): a full region wedged it rather than compacting",
        ROUNDS / 4
    );
}
