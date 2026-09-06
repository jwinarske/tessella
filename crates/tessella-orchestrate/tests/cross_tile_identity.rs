//! A label keeps its identity from one frame to the next.
//!
//! # What this replaces
//!
//! `cross_tile_id: base + index` — an ordinal into whatever order a frame happened to walk its
//! buckets in, handed out from a counter reset to one every frame. Two things followed. Nothing
//! carried a label's number across a frame boundary, so [`crate::symbols::ViewSymbols`] was built
//! fresh each frame and its fades never ran; and at a zoom crossing, where a tile is replaced by
//! four children and "Detroit" is a different instance at a different index, a label inherited
//! the fade of whatever sorted into its slot.
//!
//! `CrossTileIndex` was written, documented and tested against mbgl's `CrossTileSymbolLayerIndex`
//! and never wired to anything but its own tests. This is the wiring, and what it is worth.
//!
//! # What is asserted, and why it is not the index's own tests
//!
//! `cross_tile.rs` tests the matching. What that cannot see is whether the frame hands it the
//! right tile, the right key and the right anchor — and the key is the one to get wrong, because
//! it lives on the *pending* symbol rather than on the laid-out instance: a line label is one
//! pending symbol and one instance per anchor, so reading it off the instance would key every
//! label on a road to the same string and collapse them all onto one identity.
//!
//! So these drive whole frames and read the identities the index actually issued.

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

/// Point labels and line labels together: the key comes off the pending symbol, and a line label
/// is the case where that differs from the instance.
const STYLE: &str = r##"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "places", "type": "symbol", "source": "src", "source-layer": "places",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 16}},
    {"id": "roads", "type": "symbol", "source": "src", "source-layer": "roads",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 14,
                "symbol-placement": "line"}}]}"##;

/// Three neighbouring tiles at the fixture's own zoom. The same features are built into each, so
/// every tile carries the same label texts at the same tile-local anchors and a different patch of
/// the world -- which is what keeps their identities distinct while making the walk order the only
/// thing that separates them.
const TILE_A: TileId = TileId::new(14, 8802, 5373);
const TILE_B: TileId = TileId::new(14, 8803, 5373);
const TILE_C: TileId = TileId::new(14, 8801, 5373);

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
    buckets: Vec<(TileId, Vec<LayerBucket>)>,
    origins: Vec<Option<std::sync::Arc<Vec<LayerBucket>>>>,
    fonts: Fonts,
}

/// The scene, with the buckets behind an `Arc` because that is the identity the frame keys a
/// bucket's layout and its identities on -- a store hands the same `Arc` back for an unchanged
/// tile, and a different one for a re-parse.
fn scene(ids: &[TileId]) -> Scene {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 13.404,
        latitude: 52.52,
        zoom: 14.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");

    let mut tiles = Vec::new();
    let mut buckets = Vec::new();
    let mut origins = Vec::new();
    for id in ids {
        let built = build_mvt_tile(&style, "src", *id, &decoded).expect("the tile builds");
        origins.push(Some(std::sync::Arc::new(built.clone())));
        buckets.push((*id, built));
        tiles.push(cover::TileCoord {
            z: id.z,
            x: id.x,
            y: id.y,
            wrap: 0,
        });
    }

    let mut fonts = Fonts::new("glyphs://{fontstack}/{range}.pbf");
    for (_, tile_buckets) in &buckets {
        for bucket in tile_buckets {
            if let Some(layout) = bucket.content.as_symbol() {
                fonts
                    .fetch(&layout.dependencies(), &Fixture)
                    .expect("glyphs");
            }
        }
    }

    Scene {
        style,
        view,
        tiles,
        buckets,
        origins,
        fonts,
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
            style: &scene.style,
            view: &scene.view,
            view_id: ViewId(0),
            tiles: &scene.tiles,
            buckets: &scene.buckets,
            origins: &scene.origins,
            light: &Light::default(),
            fonts: Some(&scene.fonts),
            patterns: None,
        },
        session,
    )
    .expect("the frame emits");
    // Drained so the ring does not fill across the frames below.
    while let Some(record) = consumer.peek() {
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
}

/// A settled scene keeps its numbers.
///
/// The weakest of these, and recorded as such: a settled scene walks its buckets in the same
/// order every frame, so an ordinal is stable across it too and this passes with the defect in
/// place. It is a regression guard rather than the case that discriminates — the next test is
/// that one. Kept because it is the cheapest thing that fails if identities stop being stable at
/// all.
#[test]
fn a_settled_scene_keeps_its_numbers() {
    let scene = scene(&[TILE_A, TILE_B]);
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    let first = placement.identities();
    let named: usize = first.values().map(Vec::len).sum();
    assert!(
        named > 0,
        "the first frame named no labels at all, so nothing here is under test"
    );

    for frame_number in 2..=8 {
        emit(
            &scene,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
        assert_eq!(
            placement.identities(),
            first,
            "frame {frame_number} gave {named} labels that never moved different numbers: the \
             fades are keyed by these, so every one of them restarts"
        );
    }
}

/// A tile arriving does not renumber the labels already on the map.
///
/// **The test the ordinal fails.** `base + index` numbered each bucket from wherever the previous
/// bucket's run ended, so inserting a tile ahead of another in the walk shifted every number after
/// it. Nothing about those labels changed — the same names on the same ground — but the fades are
/// keyed by the number, so each one inherited the state of whatever now held its old slot and the
/// whole tile blinked.
///
/// This is the same shape as the zoom crossing the index exists for, without having to reproject
/// a fixture into child tiles: what matters is that the walk changes and the numbers do not.
#[test]
fn a_new_tile_does_not_renumber_the_others() {
    let held = scene(&[TILE_A, TILE_B]);
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    emit(
        &held,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    let before = placement.identities();
    assert!(
        before.values().map(Vec::len).sum::<usize>() > 0,
        "no labels were named"
    );

    // The same two tiles, with a third ahead of them. `TILE_C` sorts first, so every bucket after
    // it moves down the walk.
    let wider = scene(&[TILE_C, TILE_A, TILE_B]);
    emit(
        &wider,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    let after = placement.identities();

    let mut compared = 0;
    for (key, was) in &before {
        let now = after
            .get(key)
            .unwrap_or_else(|| panic!("{key:?} left the map when a tile was added beside it"));
        assert_eq!(
            was, now,
            "{key:?} was renumbered because another tile arrived: the fades are keyed by these, \
             so every one of its labels inherits a stranger's state"
        );
        compared += was.len();
    }
    assert!(compared > 0, "nothing was compared");
    assert!(
        after.len() > before.len(),
        "the third tile was not named, so the walk did not actually change"
    );
}

/// A tile re-parsed into a fresh bucket list keeps the identities its labels had.
///
/// This is the path the memo cannot serve: the `Arc` differs, so the frame asks the index, and
/// the index has to recognise the labels by text and position. It is the same matching a zoom
/// crossing needs, exercised without having to reproject a fixture into child tiles.
#[test]
fn a_re_parsed_tile_keeps_its_labels_identities() {
    let mut scene = scene(&[TILE_A, TILE_B]);
    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    let first = placement.identities();
    assert!(
        first.values().map(Vec::len).sum::<usize>() > 0,
        "no labels were named"
    );

    // The same tiles, the same features, a different allocation -- what a store does when a tile
    // is re-parsed. The layouts cache keys on this `Arc` too, so the layout is redone as well.
    scene.origins = scene
        .buckets
        .iter()
        .map(|(_, built)| Some(std::sync::Arc::new(built.clone())))
        .collect();

    emit(
        &scene,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    );
    assert_eq!(
        placement.identities(),
        first,
        "a re-parse of the same ground was given new numbers, so the index is not matching by \
         text and position -- which is what a zoom crossing depends on"
    );
}

/// The fixture has both kinds of label, so the key really is read off the pending symbol.
///
/// Without this the test above passes on a scene of point labels alone, where reading the key off
/// the instance and off the pending symbol happen to agree.
#[test]
fn the_scene_exercises_line_labels() {
    let scene = scene(&[TILE_A, TILE_B]);
    let mut lines = 0;
    let mut points = 0;
    for (_, tile_buckets) in &scene.buckets {
        for bucket in tile_buckets {
            let Some(layout) = bucket.content.as_symbol() else {
                continue;
            };
            if layout.pending.is_empty() {
                continue;
            }
            match layout.placement {
                tessella_layout::symbol_layout::Placement::Line => lines += layout.pending.len(),
                _ => points += layout.pending.len(),
            }
        }
    }
    assert!(points > 0, "no point labels in the fixture scene");
    assert!(
        lines > 0,
        "no line labels in the fixture scene, so the pending-symbol key is not exercised"
    );
}
