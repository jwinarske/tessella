//! Every attribute a drawable sends binds at the slot the shader it names declares for it.
//!
//! # What the field means
//!
//! `AttributeDesc::binding` is the shader's `@location`, and §2.2 gives `-1` one meaning: *this
//! shader has no slot for this attribute*. A bucket's buffer serves a whole family -- the plain
//! fill does not declare `fill-outline-color` and its outline does -- so the bytes stay and the
//! descriptor says the consumer may drop them.
//!
//! So the invariant is an equality, not a bound: `binding` is what the generated table declares
//! for that shader and that attribute id, or `-1` when the table declares nothing. Either side
//! being wrong is a silent fault:
//!
//! - **A slot the shader does not declare** is the attribute arriving where another one belongs. A
//!   consumer that binds by slot then feeds the wrong bytes to the wrong input, or refuses the
//!   drawable if the types disagree.
//! - **`-1` for an attribute the shader *does* declare** is the attribute never arriving. The body
//!   falls back to the paint block, where a data-driven property's value is its fallback.
//!
//! # Why it took a second consumer to find
//!
//! Both faults were in this producer and neither showed on the Filament side, which resolves an
//! attribute by *name*: it reads the bytes whatever the slot says, so a wrong slot and a `-1` are
//! equally invisible there. The Vulkan consumer binds by slot, and found both the week it drew a
//! patterned extrusion for the first time.
//!
//! - A patterned extrusion was bound through the *plain* shader's table. That table declares a
//!   color the pattern's does not, so the base and the height sit one slot higher in it: the
//!   height arrived where `FillExtrusionPatternShader` declares `pattern_from`, and the consumer
//!   refused the drawable outright -- `DeclaredDisagrees { attr_id: 5, wire: Float2, table:
//!   UShort4 }`.
//! - The walls' data-driven paint was resolved through `declared_for`, which reads the per-vertex
//!   table. An instanced shader declares one attribute there -- the wall template's corner -- and
//!   everything else per *instance*, so every paint property answered `None` and bound at `-1`.
//!   `fill-extrusion-height` is data-driven in every real style and its fallback is zero, so every
//!   wall of every building was a quad of no height. They were built, uploaded, planned, recorded
//!   and rasterized nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use tessella_capture_abi::envelope::{
    AttributeDesc, GeometryAdd, TextureId, ViewId, WireRecord as _,
};
use tessella_capture_abi::generated::mbgl_enums::BuiltIn;
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{EnvelopeKind, ProjectionMode, declared_for};
use tessella_glyph::atlas::Rect;
use tessella_glyph::sprite::IconPosition;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::binder::instance_declared_for;
use tessella_orchestrate::frame::{self, Frame, Patterns};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::crossfade::ZoomHistory;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// A sprite the patterns below resolve to.
fn atlas() -> BTreeMap<String, IconPosition> {
    let mut positions = BTreeMap::new();
    positions.insert(
        "sand_noise".to_owned(),
        IconPosition {
            padded_rect: Rect {
                x: 56,
                y: 9,
                width: 52,
                height: 52,
            },
            pixel_ratio: 1.0,
            sdf: false,
            content: None,
            text_fit_width: None,
            text_fit_height: None,
        },
    );
    positions
}

/// One drawable's descriptors, with the shader that will read them.
struct Sent {
    shader: i32,
    per_vertex: Vec<AttributeDesc>,
    per_instance: Vec<AttributeDesc>,
}

/// Emits a style and returns what every `GeometryAdd` described.
fn sent(style: &str, sprites: bool) -> Vec<Sent> {
    let style = Style::parse(style).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 45.0,
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "s", id, &decoded).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pattern_positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

    let mut arena = SlabArena::new();
    let mut ring = Ring::new(1 << 24);
    let (producer, consumer) = ring.split();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: sprites.then_some(&patterns),
            states: None,
        },
    )
    .expect("emits");

    let size = core::mem::size_of::<AttributeDesc>();
    let decode = |payload: &[u8], span: tessella_capture_abi::envelope::Span| {
        let start = span.offset as usize;
        (0..span.count as usize)
            .filter_map(|index| AttributeDesc::from_bytes(&payload[start + index * size..]))
            .collect::<Vec<_>>()
    };

    let mut out = Vec::new();
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::GeometryAdd
            && let Some(add) = GeometryAdd::from_bytes(record.record)
        {
            out.push(Sent {
                shader: add.builtin_shader,
                per_vertex: decode(record.payload, add.attrs),
                per_instance: decode(record.payload, add.instance_attrs),
            });
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    out
}

/// The styles worth walking: the four extrusion shaders and the four fill ones.
const EXTRUSION: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "e", "type": "fill-extrusion", "source": "s", "source-layer": "water",
              "paint": {"fill-extrusion-height": ["get", "height"],
                        "fill-extrusion-base": ["get", "min_height"],
                        "fill-extrusion-opacity": 0.8}}]}"#;

const PATTERNED_EXTRUSION: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "e", "type": "fill-extrusion", "source": "s", "source-layer": "water",
              "paint": {"fill-extrusion-pattern": "sand_noise",
                        "fill-extrusion-height": ["get", "height"],
                        "fill-extrusion-base": ["get", "min_height"],
                        "fill-extrusion-opacity": 0.8}}]}"#;

const FILL: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-color": ["get", "color"], "fill-opacity": ["get", "o"],
                        "fill-outline-color": ["get", "edge"]}}]}"#;

const PATTERNED_FILL: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-pattern": "sand_noise", "fill-opacity": ["get", "o"]}}]}"#;

/// A pattern that varies with the feature, which resolves somewhere else entirely.
///
/// A constant pattern is one rectangle pair the *frame* looks up; this is a pair per feature,
/// resolved while the bucket is built. Both are the same shader and the same table, and reading
/// only the first bound this through the plain one -- with the rectangles still in the vertices, at
/// the slots the pattern table gives them. The consumer refuses it outright:
/// `DeclaredDisagrees { attr_id: 4, wire: UShort4, table: Float4 }`, the rectangles arriving where
/// `FillShader` declares its color.
const DATA_DRIVEN_FILL: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-pattern": ["coalesce", ["get", "pattern"], "sand_noise"],
                        "fill-opacity": ["get", "o"]}}]}"#;

/// Every descriptor binds where its shader declares it, or at -1 where it declares nothing.
#[test]
fn every_attribute_binds_where_its_shader_declares_it() {
    let mut checked = 0;
    let mut instanced = 0;
    for (what, style, sprites) in [
        ("the extrusion", EXTRUSION, false),
        ("the patterned extrusion", PATTERNED_EXTRUSION, true),
        ("the fill", FILL, false),
        ("the patterned fill", PATTERNED_FILL, true),
        ("the data-driven fill", DATA_DRIVEN_FILL, true),
    ] {
        for drawable in sent(style, sprites) {
            // The ground is tessella's own shader and has no row in mbgl's tables; its attributes
            // come from `terrain_ubo` instead. Nothing else here is outside the enum.
            let Some(shader) = BuiltIn::from_repr(drawable.shader) else {
                continue;
            };
            for (rate, descs) in [
                ("per vertex", &drawable.per_vertex),
                ("per instance", &drawable.per_instance),
            ] {
                for desc in descs {
                    let declared = if rate == "per instance" {
                        instance_declared_for(shader, desc.attr_id)
                    } else {
                        declared_for(shader, desc.attr_id)
                    };
                    let want = declared.map_or(-1, |attribute| attribute.binding);
                    assert_eq!(
                        desc.binding, want,
                        "{what}: {shader:?} attribute {} {rate} binds at {} and the table says {want}",
                        desc.attr_id, desc.binding
                    );
                    if let Some(attribute) = declared {
                        assert_eq!(
                            desc.declared_data_type, attribute.declared as u8,
                            "{what}: {shader:?} attribute {} {rate} declares the wrong type",
                            desc.attr_id
                        );
                    }
                    checked += 1;
                }
            }
            instanced += usize::from(!drawable.per_instance.is_empty());
        }
    }
    assert!(
        checked > 40,
        "walked {checked} descriptors, which is too few to be these four styles"
    );
    assert!(
        instanced > 0,
        "no instanced drawable was walked, and that is the half that was wrong"
    );
}

/// And the data-driven paint of an instanced drawable arrives at all.
///
/// The assertion above is the general one and this is the case it was written for, stated so a
/// reader of a failure knows which half broke: the walls' base and height bound at -1, which is
/// the wire saying the shader has no slot for them. It declares both, per instance.
#[test]
fn the_walls_data_driven_paint_is_not_dropped() {
    for (what, style, sprites) in [
        ("plain", EXTRUSION, false),
        ("patterned", PATTERNED_EXTRUSION, true),
    ] {
        let walls: Vec<_> = sent(style, sprites)
            .into_iter()
            .filter(|drawable| !drawable.per_instance.is_empty())
            .collect();
        assert!(!walls.is_empty(), "{what}: no instanced drawable");
        for drawable in walls {
            let bound: Vec<u32> = drawable
                .per_instance
                .iter()
                .filter(|desc| desc.binding >= 0)
                .map(|desc| desc.attr_id)
                .collect();
            // 3 is the base and 5 the height, which are the two every real style drives.
            assert!(
                bound.contains(&3) && bound.contains(&5),
                "{what}: the walls dropped the base or the height: {bound:?}"
            );
        }
    }
}
