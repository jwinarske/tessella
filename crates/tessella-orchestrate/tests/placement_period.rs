// SPDX-License-Identifier: BSD-2-Clause
//! Placement waits for the crossfade, and the crossfade takes a frame at a time.
//!
//! # What was wrong
//!
//! Two gaps against mbgl, both invisible to every parity render and both plain on a turning map.
//!
//! `place_symbols` ran every frame. mbgl will not: `PlacementController::placementIsRecent`
//! holds a placement for `Placement::getUpdatePeriod`, and mbgl's own comment says why -- "Even
//! if transitionOptions.duration is set to a value < 300ms, we still wait for this default
//! transition duration before attempting another placement operation." Without the wait a label
//! is re-decided part way through its fade. The input that keeps changing is
//! `placement_tile_order`, which rotates each tile's position by the bearing, so on a map turning
//! at 0.4 degrees a second two neighboring place names traded their space about 2.4 times a
//! second.
//!
//! And the fades were stepped with `settle`, which drives them to their resting values in up to
//! eight steps. That is right for a still picture -- mbgl's static mode, where `symbolFadeChange`
//! is one -- and wrong for an animating one: a 300 ms crossfade finished in two frames, about
//! 50 ms. Measured on the running map, fade-ins took a median of two frames. A label that should
//! dissolve into its neighbor blinked instead, which is what was reported as flickering labels.
//!
//! # Why these drive whole frames
//!
//! Both are properties of the frame path rather than of the pieces. `PlacementState` can answer
//! whether a placement is recent, and `ViewSymbols` can step a fade by one increment; neither
//! says whether `emit` consults the one or calls the other. So these run `emit_incremental` and
//! read what it left behind.

use std::sync::Arc;
use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::Ring;
use tessella_glyph::fonts::Fonts;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::registry::Session;
use tessella_orchestrate::tile::{LayerBucket, TileId, build_mvt_tile};
use tessella_source::mvt::Tile;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");
const BERLIN: &[u8] =
    include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// Enough labels to compete, which is what makes a placement worth keeping.
const STYLE: &str = r##"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "places", "type": "symbol", "source": "src", "source-layer": "places",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 16}},
    {"id": "roads", "type": "symbol", "source": "src", "source-layer": "roads",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 14,
                "symbol-placement": "line"}}]}"##;

const TILE: TileId = TileId::new(14, 8802, 5373);

/// One sixtieth of a second, in milliseconds. A frame of a map somebody is watching.
const FRAME_MILLIS: f64 = 1000.0 / 60.0;

struct Fixture;
impl FileSource for Fixture {
    fn fetch(&self, _url: &str) -> Result<Response, FetchError> {
        Ok(Response {
            status: 200,
            body: GLYPHS.to_vec(),
            ..Response::default()
        })
    }
}

struct Scene {
    style: Style,
    view: ViewTransform,
    tiles: Vec<cover::TileCoord>,
    buckets: Vec<(TileId, Arc<Vec<LayerBucket>>)>,
    origins: Vec<Option<Arc<Vec<LayerBucket>>>>,
    fonts: Fonts,
}

fn scene() -> Scene {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 13.404,
        latitude: 52.52,
        zoom: 14.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");
    let built = build_mvt_tile(&style, "src", TILE, &decoded).expect("the tile builds");
    Scene {
        fonts: {
            let mut fonts = Fonts::new("glyphs://{fontstack}/{range}.pbf");
            for bucket in &built {
                if let Some(layout) = bucket.content.as_symbol() {
                    fonts
                        .fetch(&layout.dependencies(), &Fixture)
                        .expect("glyphs");
                }
            }
            fonts
        },
        origins: vec![Some(Arc::new(built.clone()))],
        buckets: vec![(TILE, Arc::new(built))],
        tiles: vec![cover::TileCoord {
            z: TILE.z,
            x: TILE.x,
            y: TILE.y,
            wrap: 0,
        }],
        style,
        view,
    }
}

fn emit(
    scene: &Scene,
    arena: &mut SlabArena,
    layouts: &mut frame::SymbolCache,
    placement: &mut frame::PlacementState,
    session: &mut Session,
) {
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    frame::emit_incremental(
        producer,
        arena,
        layouts,
        placement,
        &Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style: &scene.style,
            view: &scene.view,
            view_id: ViewId(0),
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            origins: &scene.origins,
            light: &Light::default(),
            fonts: Some(&scene.fonts),
            patterns: None,
            states: None,
        },
        session,
    )
    .expect("the frame emits");
    while let Some(record) = consumer.peek() {
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
}

/// A map with the fades running places once and then holds it for the fade's length.
///
/// mbgl's `placementChanged`. Without the wait every frame places, so the count climbs with the
/// frames and a label can be re-decided part way through its crossfade.
#[test]
fn placement_waits_for_the_crossfade_before_running_again() {
    let scene = scene();
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    // The first frame has nothing to keep, so it places.
    placement.advance(FRAME_MILLIS);
    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    assert_eq!(
        placement.placements(),
        1,
        "the first frame did not place, so nothing below is under test"
    );

    // Ten more frames is 167 ms, comfortably inside the 300 ms fade.
    for _ in 0..10 {
        placement.advance(FRAME_MILLIS);
        emit(
            &scene,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
    }
    assert_eq!(
        placement.placements(),
        1,
        "placement ran again inside the fade; mbgl waits for getUpdatePeriod"
    );

    // Past 300 ms it places again, or a map would never reconsider at all.
    placement.advance(200.0);
    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    assert_eq!(
        placement.placements(),
        2,
        "placement never ran again, so the wait has no end"
    );
}

/// A still picture places every frame, which is what every capture depends on.
///
/// mbgl's `!transitionsEnabled()` branch: `placementIsRecent` is false in static map mode, so
/// `mbgl-render` -- and every parity render measured against it -- places on each frame it draws.
/// A map told nothing about time is that map.
#[test]
fn a_still_picture_places_every_frame() {
    let scene = scene();
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    for frame in 1..=4 {
        emit(
            &scene,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
        assert_eq!(
            placement.placements(),
            frame,
            "a static frame skipped placement, which moves every parity render"
        );
    }
}

/// A crossfade is still running after more frames than a settle would have taken.
///
/// `settle` advances the fades up to eight times per call, so a fade driven by it is over in two
/// or three frames however long the duration says. Stepped once per frame at 60 Hz a 300 ms fade
/// needs about eighteen, so six frames in it is still moving.
#[test]
fn a_crossfade_outlasts_the_frames_a_settle_would_take() {
    let scene = scene();
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    placement.advance(FRAME_MILLIS);
    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    assert!(
        placement.fading() > 0,
        "the first frame started no fade at all, so nothing here is under test"
    );

    // Six frames is 100 ms of a 300 ms fade. A settle would have finished it by the third.
    for _ in 0..5 {
        placement.advance(FRAME_MILLIS);
        emit(
            &scene,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
    }
    assert!(
        placement.fading() > 0,
        "the fade was over within six frames, which is a settle rather than a step"
    );

    // And it does end: a fade that never arrives is its own defect.
    for _ in 0..60 {
        placement.advance(FRAME_MILLIS);
        emit(
            &scene,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
    }
    assert_eq!(
        placement.fading(),
        0,
        "the fade never reached its resting value"
    );
}
