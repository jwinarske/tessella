//! One real frame, emitted into a freestanding region.
//!
//! Shared by the consumers that read it: tessella's own C walker, and the Fluorite mirror's C++
//! reader. Both are handed the same bytes, which is the point — a fixture each of them built
//! for itself would let them agree with their own assumptions rather than with the producer.
//!
//! Included by each test binary in turn rather than shared through a library, which is how a
//! Rust integration test shares anything — so an item one binary does not use is dead there and
//! nowhere else.

#![allow(dead_code, unreachable_pub)]

use tessella_capture_abi::envelope::{Extent, GeometryId, Rect16, TextureId, ViewId};
use tessella_capture_abi::generated::mbgl_enums::TexturePixelType;
use tessella_capture_abi::ring::{self, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::emit;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::texture;
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

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
     "paint": {"fill-extrusion-height": 20, "fill-extrusion-opacity": 0.8}}
  ]
}"##;

/// Ring capacity. A power of two, as the control block requires.
const CAPACITY: usize = 1 << 24;

/// Emits one frame into a freestanding region and returns it with the packed slabs.
///
/// `ring::init` over a buffer of this test's own, rather than `Ring::new`, because what a C
/// consumer is handed is a region -- and the point here is to produce exactly the bytes that
/// would cross a mapping, not a Rust object that happens to contain them.
pub fn emit_frame() -> (Vec<u8>, Vec<u8>, frame::Emitted) {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 3.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 45.0,
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

    // Eight-aligned by construction, which `init` requires.
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: the buffer is `region_size(CAPACITY)` bytes, eight-aligned because it is a
    // `Vec<u64>`, outlives both halves, and nothing else touches it.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut arena = SlabArena::new();
    let emitted = frame::emit(
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
    )
    .expect("the frame emits");

    // A texture with a *rect list*, which the frame itself never produces here: the only
    // textures a styleless frame uploads are mbgl's two bootstraps, and both are whole-texture
    // uploads. The rect path is the one with a rule the header has to carry — rows strided by
    // `w * bytes-per-pixel`, and the pixel bytes accounting for exactly the rectangles named —
    // so leaving it unexercised would leave the interesting half of the record unproven.
    //
    // Two rectangles in opposite corners rather than one, because that is the case the list
    // exists for: a union over them uploads the whole atlas (§6.4).
    let damage = [
        Rect16 {
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        },
        Rect16 {
            x: 60,
            y: 60,
            w: 2,
            h: 3,
        },
    ];
    let format = TexturePixelType::Alpha;
    let bytes: usize = damage
        .iter()
        .map(|rect| usize::from(rect.w) * usize::from(rect.h) * format.channels() as usize)
        .sum();
    let upload = texture::regions(
        TextureId(64),
        Extent {
            width: 64,
            height: 64,
        },
        format,
        &damage,
        &vec![0xA5; bytes],
    )
    .expect("two rectangles are within the cap");
    texture::write(&mut producer, &upload).expect("the upload writes");

    // A mesh, a retirement and a teardown, none of which a settled first frame produces. Each is
    // a record a mirror must act on and none of them draws anything, so a fixture without them
    // would leave the walks compiled and never run — the failure mode this whole test exists to
    // avoid, one level up.
    let mesh = GeometryId(4096);
    let encoded = emit::encode_mesh(&mut arena, mesh, b"glTF\x02\x00\x00\x00");
    emit::write_mesh(&mut producer, &encoded).expect("the mesh writes");
    emit::remove(&mut producer, mesh).expect("the retirement writes");

    arena.seal();

    let bytes = region
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .take(region_size(CAPACITY))
        .collect();
    (bytes, arena.pack(), emitted)
}
