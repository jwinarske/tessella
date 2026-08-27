//! The shared frame emitter puts a whole frame on the wire, in protocol order.
//!
//! `whole_stream` asserts the same rules against a driver written inside the test. That driver
//! was the only correct one this producer had, which meant nothing shipped could emit a frame —
//! and a tool that wanted to would have had to write a second copy of the ordering rules to keep
//! in step with the first. This exercises the module instead.

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::Ring;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
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
     "paint": {"line-color": "#88a", "line-width": 1.5}},
    {"id": "blocks", "type": "fill-extrusion", "source": "src", "source-layer": "water",
     "paint": {"fill-extrusion-height": 20}}
  ]
}"##;

fn view() -> ViewTransform {
    camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 0.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// Emits one frame and returns the envelope kinds in the order they reached the ring.
fn emit_frame() -> (Vec<EnvelopeKind>, frame::Emitted, usize) {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = view();
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

    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    let emitted = frame::emit(
        producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
        },
    )
    .expect("the frame emits");

    let mut kinds = Vec::new();
    while let Some(record) = consumer.peek() {
        kinds.push(record.kind);
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    (kinds, emitted, tiles.len())
}

/// Every kind a frame needs reaches the ring, geometry included.
///
/// `GeometryAdd` is the one that was missing in practice: three bucket kinds had no encoder, so
/// a line, a circle and an extrusion were built and then went nowhere. A consumer sees that as a
/// layer the style does not draw, which is indistinguishable from a style that does not draw it.
#[test]
fn a_frame_emits_every_kind_it_needs() {
    let (kinds, emitted, _) = emit_frame();
    for required in [
        EnvelopeKind::ViewDeclare,
        EnvelopeKind::TextureUpdate,
        EnvelopeKind::UboUpdate,
        EnvelopeKind::GeometryAdd,
        EnvelopeKind::ViewUse,
        EnvelopeKind::StencilTiles,
        EnvelopeKind::OrderUpdate,
        EnvelopeKind::CameraUpdate,
    ] {
        assert!(kinds.contains(&required), "{required:?} was never emitted");
    }
    assert!(emitted.geometries > 0, "no geometry was announced");
    assert!(emitted.drawables > 0, "no drawable was bound");
}

/// The view is declared before anything names it (DR-18).
#[test]
fn the_view_is_declared_before_it_is_used() {
    let (kinds, _, _) = emit_frame();
    let declare = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::ViewDeclare)
        .expect("a declaration");
    let first_use = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::ViewUse)
        .expect("a use");
    assert!(declare < first_use, "{declare} vs {first_use}");
}

/// Geometry is announced before the use that binds it, and before the order that draws it.
#[test]
fn geometry_precedes_the_use_and_the_order() {
    let (kinds, _, _) = emit_frame();
    let first_add = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::GeometryAdd)
        .expect("an add");
    let first_use = kinds
        .iter()
        .position(|kind| *kind == EnvelopeKind::ViewUse)
        .expect("a use");
    let last_use = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::ViewUse)
        .expect("a use");
    let order = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::OrderUpdate)
        .expect("an order");
    assert!(first_add < first_use, "{first_add} vs {first_use}");
    assert!(last_use < order, "{last_use} vs {order}");
}

/// The order precedes the camera that names its epoch (§4).
#[test]
fn the_order_precedes_the_camera_naming_it() {
    let (kinds, _, _) = emit_frame();
    let order = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::OrderUpdate)
        .expect("an order");
    let camera = kinds
        .iter()
        .rposition(|kind| *kind == EnvelopeKind::CameraUpdate)
        .expect("a camera");
    assert!(order < camera, "{order} vs {camera}");
}

/// One geometry per drawable, so nothing is bound to an id nothing declared.
///
/// A fill is the case that makes this worth asserting: its triangles and its outline are two
/// drawables over one bucket, and `bindings_for` gives each its own geometry id. Announcing the
/// bucket once would leave the second use naming an id the consumer never received.
#[test]
fn every_drawable_has_a_geometry_of_its_own() {
    let (_, emitted, tiles) = emit_frame();
    assert_eq!(
        emitted.geometries + tiles,
        emitted.drawables,
        "one geometry per drawable, less one synthesized background quad per tile"
    );
}
