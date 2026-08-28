//! An ancestor is held until its descendants are on the GPU, not until they are built (§13.2).
//!
//! # The gap this closes
//!
//! §13.2's never-blank bullet asks for retention "until every covering descendant's buckets are
//! consumer-**acknowledged** via the reverse-channel epoch — mbgl retains until *built*, and the
//! build→GPU-upload gap is exactly where its single-frame holes come from".
//!
//! The substitution itself has been right since R1.5: `tessella_tile::renderables` is mbgl's
//! `updateRenderables` transcribed and checked against mbgl's own expectations. What was missing
//! is on this side of the seam. `TileState::renderable` is the caller's to define, the caller
//! defined it as *built*, and so the algorithm dropped an ancestor the instant a descendant's
//! buckets existed — a frame or more before the consumer had uploaded them. Between those two
//! moments the map has nothing to draw there.
//!
//! # Why the producer can answer it
//!
//! Because it wrote the records and knows where each one landed, and the consumer publishes one
//! number: how far it has uploaded through. `announced_through` is the furthest position a
//! tile's drawables were announced at, and the comparison against
//! `ReverseChannel::acked_geometry` is the whole of the acknowledgement. No new field, on either
//! side — the reverse channel has carried the acked position since DR-10.

use std::collections::BTreeMap;

use tessella_capture_abi::envelope::{TileId as WireTileId, ViewId};
use tessella_capture_abi::reverse::ReverseChannel;
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::order::wrapped_tile_of;
use tessella_orchestrate::registry::Session;
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};
use tessella_tile::renderables::{
    DataTileId, Necessity, Pyramid, RenderTileId, TileState, update_renderables,
};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "sea", "type": "fill", "source": "src", "source-layer": "water",
     "paint": {"fill-color": "#20344c"}}
  ]
}"##;

struct Scene {
    style: Style,
    view: ViewTransform,
    tiles: Vec<cover::TileCoord>,
    buckets: Vec<(TileId, Vec<LayerBucket>)>,
}

fn scene() -> Scene {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 512.0,
        height: 512.0,
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
    Scene {
        style,
        view,
        tiles,
        buckets,
    }
}

/// Emits one frame and returns the session that remembers where everything landed.
fn emit(scene: &Scene) -> (Session, u64) {
    let mut ring = Ring::new(1 << 22);
    let (producer, _consumer) = ring.split();
    let mut arena = SlabArena::new();
    let mut session = Session::new();
    frame::emit_incremental(
        producer,
        &mut arena,
        &Frame {
            style: &scene.style,
            view: &scene.view,
            view_id: ViewId(0),
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
        &mut session,
    )
    .expect("the frame emits");
    let head = producer.head();
    (session, head)
}

/// Nothing is acknowledged until the consumer says so, and then it is.
#[test]
fn acknowledgement_follows_the_consumer_not_the_build() {
    let scene = scene();
    let (session, head) = emit(&scene);
    let wire: Vec<WireTileId> = scene
        .tiles
        .iter()
        .map(|tile| wrapped_tile_of(tile.z, tile.x, tile.y, tile.wrap))
        .collect();
    assert!(!wire.is_empty(), "the cover has tiles in it");

    let reverse = ReverseChannel::new();
    assert_eq!(reverse.acked_geometry(), 0, "nothing has been uploaded");
    for tile in &wire {
        assert!(
            session.geometry().announced_through(*tile).is_some(),
            "every tile of the cover was announced"
        );
        assert!(
            !session
                .geometry()
                .is_acknowledged(*tile, reverse.acked_geometry()),
            "and none of them is on the GPU yet, however built its buckets are"
        );
    }

    // The consumer uploads everything.
    reverse.ack_geometry(head);
    for tile in &wire {
        assert!(
            session
                .geometry()
                .is_acknowledged(*tile, reverse.acked_geometry()),
            "now it is"
        );
    }
}

/// A tile is not acknowledged because most of it arrived.
#[test]
fn a_tile_is_acknowledged_only_when_all_of_it_is() {
    let scene = scene();
    let (session, _) = emit(&scene);
    let tile = scene.tiles.first().expect("a cover tile");
    let wire = wrapped_tile_of(tile.z, tile.x, tile.y, tile.wrap);

    let announced = session
        .geometry()
        .announced_through(wire)
        .expect("it was announced");
    assert!(
        !session.geometry().is_acknowledged(wire, announced - 1),
        "one byte short of its last drawable is not acknowledged"
    );
    assert!(
        session.geometry().is_acknowledged(wire, announced),
        "and exactly there is"
    );
}

/// The property: the ancestor survives the gap that "built" would drop it in.
///
/// Four children of one ideal tile, all built, none uploaded. Under `renderable = built` the
/// algorithm takes the children and the ancestor goes; under `renderable = acknowledged` it
/// keeps the ancestor, which is what is actually on screen. The frame between those two answers
/// is mbgl's single-frame hole.
#[test]
fn an_ancestor_is_held_across_the_upload_gap() {
    let ideal = DataTileId {
        overscaled_z: 4,
        wrap: 0,
        z: 4,
        x: 8,
        y: 5,
    };
    let ancestor = DataTileId {
        overscaled_z: 3,
        wrap: 0,
        z: 3,
        x: 4,
        y: 2,
    };

    /// A pyramid holding one ancestor and the four children of the ideal tile.
    struct Pair {
        states: BTreeMap<DataTileId, TileState>,
        drawn: Vec<DataTileId>,
    }

    impl Pyramid for Pair {
        fn get(&mut self, id: DataTileId) -> Option<TileState> {
            self.states.get(&id).copied()
        }
        fn create(&mut self, id: DataTileId) -> Option<TileState> {
            self.states.get(&id).copied()
        }
        fn retain(&mut self, _: DataTileId, _: Necessity) {}
        fn render(&mut self, _: RenderTileId, data: DataTileId) {
            self.drawn.push(data);
        }
    }

    // The children are built either way; what changes is whether their bytes are on the GPU.
    let pyramid = |children_ready: bool| {
        let mut states = BTreeMap::new();
        // The ideal tile is still in flight, which is the state a crossing spends its first
        // frames in: something has to stand in for it.
        states.insert(
            ideal,
            TileState {
                renderable: false,
                loaded: false,
                tried_cache: true,
            },
        );
        states.insert(
            ancestor,
            TileState {
                renderable: true,
                loaded: true,
                tried_cache: true,
            },
        );
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            states.insert(
                DataTileId {
                    overscaled_z: 5,
                    wrap: 0,
                    z: 5,
                    x: ideal.x * 2 + dx,
                    y: ideal.y * 2 + dy,
                },
                TileState {
                    renderable: children_ready,
                    loaded: true,
                    tried_cache: true,
                },
            );
        }
        Pair {
            states,
            drawn: Vec::new(),
        }
    };

    let mut built_only = pyramid(true);
    update_renderables(&mut built_only, &[ideal], &[], 0..=16, None);
    assert_eq!(
        built_only.drawn.len(),
        4,
        "with renderable meaning built, the four children are drawn the moment they exist"
    );
    assert!(
        !built_only.drawn.contains(&ancestor),
        "and the ancestor is let go — a frame before the consumer has their bytes"
    );

    let mut acknowledged = pyramid(false);
    update_renderables(&mut acknowledged, &[ideal], &[], 0..=16, None);
    assert_eq!(
        acknowledged.drawn,
        vec![ancestor],
        "with renderable meaning acknowledged, what is drawn is what is on the GPU: the \
         ancestor, blurry, rather than a hole"
    );
}
