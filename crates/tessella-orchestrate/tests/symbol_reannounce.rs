//! A symbol drawable is announced again every frame the camera moves.
//!
//! # Why it has to be
//!
//! Because a symbol's vertices are a function of the camera and nothing else on the wire is.
//! `write_line_positions` walks each label along its *projected* road and `write_opacity` bakes
//! the fade into the same buffer, so the bytes a frame encodes describe that frame's camera. A
//! fill's do not: its triangles are tile-local and the matrix that places them travels as a UBO,
//! which is why retention works for every other family.
//!
//! # What it caught
//!
//! Announcing a symbol once and retaining it thereafter. Settled frames were exact — the tile
//! arrived, its labels were announced under the camera that was current, and the camera stayed
//! there — and a moving one carried every label away from the street it names, because the
//! consumer was still drawing glyph positions computed for the zoom the tile landed at. A zoom
//! sweep showed downtown geometry captioned with the street names of a mile north.
//!
//! The control matters as much as the case: retention is the point of `emit_incremental`, so a
//! test that only asserts the symbol comes back would pass just as well with retention broken
//! altogether.

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{GeometryAdd, ViewId, WireRecord as _};
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

/// A labelled layer and an unlabelled one, so the retained family is the control.
const STYLE: &str = r##"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#f4f1ea"}},
    {"id": "ground", "type": "fill", "source": "src", "source-layer": "earth",
     "paint": {"fill-color": "#eee"}},
    {"id": "labels", "type": "symbol", "source": "src", "source-layer": "places",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 16}}]}"##;

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

fn view_at(zoom: f64) -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 13.404,
        latitude: 52.52,
        zoom,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// How many geometries of each family one frame announced.
struct Announced {
    symbols: usize,
    fills: usize,
}

#[allow(clippy::too_many_arguments)]
fn emit(
    style: &Style,
    view: &ViewTransform,
    tiles: &[cover::TileCoord],
    buckets: &[(TileId, Vec<LayerBucket>)],
    fonts: &Fonts,
    arena: &mut SlabArena,
    layouts: &mut frame::SymbolCache,
    session: &mut Session,
) -> Announced {
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    frame::emit_incremental(
        producer,
        arena,
        layouts,
        &mut frame::PlacementState::new(),
        &Frame {
            style,
            view,
            view_id: ViewId(0),
            tiles,
            buckets,
            origins: &[],
            light: &Light::default(),
            fonts: Some(fonts),
            patterns: None,
        },
        session,
    )
    .expect("the frame emits");

    let mut announced = Announced {
        symbols: 0,
        fills: 0,
    };
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::GeometryAdd
            && let Some(add) = GeometryAdd::from_bytes(record.record)
        {
            if add.builtin_shader == tessella_capture_abi::BuiltIn::SymbolSDFShader as i32 {
                announced.symbols += 1;
            } else {
                announced.fills += 1;
            }
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    announced
}

/// The camera moves over an unchanged cover: the labels come again, the fills do not.
#[test]
fn a_camera_move_re_announces_the_labels_and_nothing_else() {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = view_at(14.0);
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        buckets.push((id, built));
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

    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut session = Session::new();

    let first = emit(
        &style,
        &view,
        &tiles,
        &buckets,
        &fonts,
        &mut arena,
        &mut layouts,
        &mut session,
    );
    assert!(
        first.symbols > 0,
        "the first frame announced no labels at all, so the case never ran"
    );
    assert!(
        first.fills > 0,
        "the first frame announced no fills, so the control never ran"
    );

    // The same cover under a camera that moved. The zoom is nudged rather than the centre so the
    // cover is provably identical -- what is under test is retention across a *camera* change,
    // and a pan that changed the tile set would announce the labels for the ordinary reason.
    let moved = view_at(14.2);
    assert_eq!(
        cover::cover(&moved).expect("covers"),
        tiles,
        "the nudge changed the cover, so the second frame is not the case under test"
    );
    let second = emit(
        &style,
        &moved,
        &tiles,
        &buckets,
        &fonts,
        &mut arena,
        &mut layouts,
        &mut session,
    );

    assert_eq!(
        second.symbols, first.symbols,
        "the camera moved and the labels were not sent again: the consumer is drawing glyph \
         positions computed for the camera the tile arrived under"
    );
    assert_eq!(
        second.fills, 0,
        "a fill was announced twice: its vertices do not carry the camera, and re-sending them \
         is the retention this stream exists for"
    );
}
