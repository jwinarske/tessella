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
            fonts: None,
            patterns: None,
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

/// A symbol layer reaches the wire: shaped quads, the atlas they sample, and its size.
///
/// Three things have to arrive together and each is useless without the others. The quads carry
/// per-corner texture coordinates, so a consumer can draw a letter rather than a box — but only
/// if the atlas they index was uploaded, and only if the drawable block says how big it is,
/// because the shader divides by that to reach `0..1`. A size that disagrees with the texture
/// stretches every glyph by the ratio between them, which reads as a font problem.
#[test]
fn a_symbol_layer_carries_its_quads_and_its_atlas() {
    use tessella_glyph::fonts::Fonts;
    use tessella_storage::source::{FetchError, FileSource, Response};

    const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

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

    // A city tile rather than the world one: labels need features that carry a name, and the
    // world fixture's water has no text to shape.
    const BERLIN: &[u8] =
        include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

    let style = Style::parse(
        r#"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
            "layers": [{"id": "labels", "type": "symbol", "source": "src",
                        "source-layer": "places",
                        "layout": {"text-field": "{name}", "text-font": ["TestFont"],
                                   "text-size": 16}}]}"#,
    )
    .expect("the style parses");

    let view = view();
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        buckets.push((id, built));
    }

    // The round trip between the two phases of shaping: the buckets say which glyphs they want,
    // and only once those are here can the quads be made.
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

    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            light: &Light::default(),
            patterns: None,
            fonts: Some(&fonts),
        },
    )
    .expect("the frame emits");

    let mut atlas_uploads = 0;
    let mut symbol_geometries = 0;
    let mut atlas_size = None;
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::TextureUpdate => atlas_uploads += 1,
            EnvelopeKind::GeometryAdd => {
                use tessella_capture_abi::envelope::{GeometryAdd, WireRecord as _};
                if let Some(add) = GeometryAdd::from_bytes(record.record)
                    && add.builtin_shader == tessella_capture_abi::BuiltIn::SymbolSDFShader as i32
                {
                    symbol_geometries += 1;
                    assert!(
                        add.texture_refs.count > 0,
                        "a symbol geometry names the atlas it samples"
                    );
                }
            }
            EnvelopeKind::UboUpdate => {
                use tessella_capture_abi::envelope::{UboUpdate, WireRecord as _};
                if let Some(update) = UboUpdate::from_bytes(record.record)
                    && update.slot
                        == tessella_capture_abi::generated::ubo_slots::ID_SYMBOL_DRAWABLE_UBO
                {
                    // `texsize` is at offset 192, past the three matrices.
                    let start = update.data.offset as usize + 192;
                    let read = |at: usize| {
                        record
                            .payload
                            .get(at..at + 4)
                            .map(|four| f32::from_le_bytes([four[0], four[1], four[2], four[3]]))
                    };
                    atlas_size = Some((read(start), read(start + 4)));
                }
            }
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }

    assert!(symbol_geometries > 0, "no symbol geometry was announced");
    assert!(
        atlas_uploads > 2,
        "the glyph atlas is uploaded beside the two placeholders, got {atlas_uploads}"
    );
    let (Some(width), Some(height)) = atlas_size.expect("a symbol drawable block") else {
        panic!("the drawable block carries no atlas size");
    };
    assert!(
        width > 0.0 && height > 0.0,
        "the atlas size is {width}x{height}, which divides every texture coordinate to zero"
    );
}

/// A raster layer reaches the wire: the quad, the picture, and the reference between them.
///
/// A raster tile *is* its picture, so the two travel together or neither means anything. The
/// geometry carries a texture reference and the image is uploaded ahead of it — a reference to a
/// texture the consumer has not been given samples whatever was last at that slot, which on a
/// tiled source is a neighbouring tile's photograph.
#[test]
fn a_raster_layer_carries_its_quad_and_its_picture() {
    use std::sync::Arc;
    use tessella_capture_abi::envelope::{GeometryAdd, TextureUpdate, WireRecord as _};
    use tessella_orchestrate::tile::build_raster_tile;
    use tessella_source::image::Image;

    let style = Style::parse(
        r#"{"version": 8, "sources": {"src": {"type": "raster", "tiles": [], "tileSize": 512}},
            "layers": [{"id": "sat", "type": "raster", "source": "src"}]}"#,
    )
    .expect("the style parses");

    // Four pixels of RGBA, which is a picture as far as the protocol is concerned.
    let picture = Arc::new(Image {
        width: 2,
        height: 2,
        pixels: vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
    });

    let view = view();
    let tiles = cover::cover(&view).expect("covers");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let built = build_raster_tile(
            &style,
            "src",
            Arc::clone(&picture),
            &[tessella_tile::mask::WHOLE_TILE],
        )
        .expect("the raster tile builds");
        buckets.push((id, built));
    }

    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
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
    )
    .expect("the frame emits");

    let mut named: Vec<u64> = Vec::new();
    let mut uploaded: Vec<u64> = Vec::new();
    let mut upload_seen_first = true;
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::TextureUpdate => {
                if let Some(update) = TextureUpdate::from_bytes(record.record) {
                    uploaded.push(update.texture.0);
                }
            }
            EnvelopeKind::GeometryAdd => {
                if let Some(add) = GeometryAdd::from_bytes(record.record)
                    && add.builtin_shader == tessella_capture_abi::BuiltIn::RasterShader as i32
                {
                    // Two references to one picture. The raster shader samples two images so
                    // it can crossfade a tile against its parent while the child loads; with no
                    // parent in hand both slots get the same one, which is what mbgl binds.
                    let refs = read_texture_refs(record.payload, add);
                    assert_eq!(refs.len(), 2, "a raster quad binds both image slots");
                    assert_eq!(refs[0], refs[1], "the two slots hold different pictures");
                    if !uploaded.contains(&refs[0]) {
                        upload_seen_first = false;
                    }
                    named.push(refs[0]);
                }
            }
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }

    assert!(!named.is_empty(), "no raster geometry was announced");
    assert!(
        upload_seen_first,
        "a raster quad named a texture that had not been uploaded"
    );
    // One picture per tile, not one shared between them: each tile fetched its own.
    assert_eq!(
        named.len(),
        tiles.len(),
        "one raster drawable per tile of the cover"
    );
    let distinct: std::collections::BTreeSet<u64> = named.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        tiles.len(),
        "two tiles share one texture id"
    );
}

/// The texture ids a geometry names.
fn read_texture_refs(payload: &[u8], add: tessella_capture_abi::envelope::GeometryAdd) -> Vec<u64> {
    use tessella_capture_abi::envelope::{TextureRef, WireRecord as _};
    let size = core::mem::size_of::<TextureRef>();
    let start = add.texture_refs.offset as usize;
    (0..add.texture_refs.count as usize)
        .filter_map(|index| {
            payload
                .get(start + index * size..)
                .and_then(TextureRef::from_bytes)
                .map(|reference| reference.texture.0)
        })
        .collect()
}
