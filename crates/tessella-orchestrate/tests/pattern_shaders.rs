//! A layer with a pattern binds different shaders and a texture; without sprites it does not.
//!
//! # What the oracle gives
//!
//! `tests/golden/pattern_style.dump`: the pattern fill layers are `sh0013` at sub-layer 1 and
//! `sh0014` at sub-layer 2, each with `tex slot=0`. A plain fill in `hermetic_style.dump` is
//! `sh0011` and `sh0012` with no texture at all. So a pattern is not a fill with a flag set —
//! it is four shaders where a fill has two, chosen by whether the pattern resolves.
//!
//! # Why a frame with no sprites still draws
//!
//! A pattern's sprites are a fetch. A caller that has not made that round trip has nothing to
//! pass, and the alternative to drawing the layer plain is not drawing it — which would make a
//! sprite sheet that is slow to arrive look like a style error.

use std::collections::BTreeMap;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{GeometryAdd, TextureId, ViewId, WireRecord as _};
use tessella_capture_abi::ring::Ring;
use tessella_glyph::atlas::Rect;
use tessella_glyph::sprite::IconPosition;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame, Patterns};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::crossfade::ZoomHistory;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

const STYLE: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-pattern": "sand_noise"}}]}"#;

/// Emits the style with and without sprites, returning shader counts and texture-ref totals.
fn emit_with(sprites: Option<&Patterns<'_>>) -> (BTreeMap<i32, usize>, u32) {
    let style = Style::parse(STYLE).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "s", id, &decoded).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut arena = SlabArena::new();
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
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
            patterns: sprites,
        },
    )
    .expect("emits");

    let mut shaders = BTreeMap::new();
    let mut textures = 0;
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::GeometryAdd
            && let Some(add) = GeometryAdd::from_bytes(record.record)
        {
            *shaders.entry(add.builtin_shader).or_insert(0) += 1;
            textures += add.texture_refs.count;
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    (shaders, textures)
}

fn atlas() -> BTreeMap<String, IconPosition> {
    let mut positions = BTreeMap::new();
    // The fifty-by-fifty rectangle the capture's constant fill layer decoded to.
    positions.insert(
        "sand_noise".to_owned(),
        IconPosition {
            padded_rect: Rect {
                x: 56,
                y: 9,
                width: 50,
                height: 50,
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

/// With sprites, the oracle's pattern shaders and a texture on every drawable.
#[test]
fn a_resolved_pattern_binds_the_pattern_shaders() {
    let positions = atlas();
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        history: ZoomHistory::new(),
    };
    let (shaders, textures) = emit_with(Some(&patterns));

    // 13 is FillPatternShader and 14 FillOutlinePatternShader, which is what `sh0013` and
    // `sh0014` in the capture are.
    assert_eq!(shaders.get(&13).copied(), shaders.get(&14).copied());
    assert!(shaders.contains_key(&13), "no pattern shader: {shaders:?}");
    assert!(
        !shaders.contains_key(&11),
        "a plain fill survived: {shaders:?}"
    );
    assert_eq!(
        textures as usize,
        shaders.values().sum::<usize>(),
        "every pattern drawable binds the atlas exactly once"
    );
}

/// Without them the layer draws plain, rather than not drawing.
#[test]
fn an_unresolved_pattern_draws_as_a_fill() {
    let (shaders, textures) = emit_with(None);
    assert!(shaders.contains_key(&11), "no plain fill: {shaders:?}");
    assert!(shaders.contains_key(&12), "no outline: {shaders:?}");
    assert!(!shaders.contains_key(&13), "a pattern shader with no atlas");
    assert_eq!(textures, 0, "nothing to bind");
}

/// A sprite the atlas does not hold is not a pattern.
///
/// The layer names `sand_noise`; this atlas has something else. Binding the pattern shader
/// anyway would sample whatever was packed at a rectangle that describes nothing.
#[test]
fn a_missing_sprite_draws_as_a_fill() {
    let mut positions = atlas();
    positions.remove("sand_noise");
    positions.insert("something_else".to_owned(), atlas()["sand_noise"]);
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        history: ZoomHistory::new(),
    };
    let (shaders, textures) = emit_with(Some(&patterns));
    assert!(
        shaders.contains_key(&11),
        "should fall back to a fill: {shaders:?}"
    );
    assert_eq!(textures, 0);
}
