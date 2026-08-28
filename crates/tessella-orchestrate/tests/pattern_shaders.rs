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
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
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
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    let (shaders, textures) = emit_with(Some(&patterns));
    assert!(
        shaders.contains_key(&11),
        "should fall back to a fill: {shaders:?}"
    );
    assert_eq!(textures, 0);
}

/// The atlas goes up as a texture, and the placements reach slot 4.
///
/// The last two pieces: a drawable that references a texture the consumer was never given
/// samples whatever was last at that slot, and a pattern shader with no rectangles has nothing
/// to sample even when the texture is there.
#[test]
fn the_atlas_is_uploaded_and_the_placements_are_written() {
    use tessella_capture_abi::envelope::{TextureUpdate, UboUpdate};

    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

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
    let mut ring = Ring::new(1 << 24);
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
            patterns: Some(&patterns),
        },
    )
    .expect("emits");

    let mut atlas_uploaded = false;
    let mut slot4 = Vec::new();
    let mut first_texture = None;
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::TextureUpdate => {
                if let Some(update) = TextureUpdate::from_bytes(record.record) {
                    if update.texture == TextureId(20) {
                        atlas_uploaded = true;
                        assert_eq!(update.size.width, 512);
                        assert_eq!(update.size.height, 512);
                    }
                    first_texture.get_or_insert(update.texture);
                }
            }
            EnvelopeKind::UboUpdate => {
                if let Some(update) = UboUpdate::from_bytes(record.record)
                    && update.slot == 4
                    && update.layer_index >= 0
                {
                    let start = update.data.offset as usize;
                    let end = start + update.data.count as usize;
                    if let Some(bytes) = record.payload.get(start..end) {
                        slot4 = bytes.to_vec();
                    }
                }
            }
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }

    assert!(atlas_uploaded, "the atlas never reached the wire");
    assert!(!slot4.is_empty(), "nothing was written to slot 4");
    assert_eq!(slot4.len() % 48, 0, "whole blocks of the pattern layout");

    // The rectangle the atlas placed sand_noise at: [56, 9, 106, 59], twice, then the size.
    let word = |at: usize| f32::from_le_bytes(slot4[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(
        [word(0), word(4), word(8), word(12)],
        [56.0, 9.0, 106.0, 59.0]
    );
    assert_eq!(
        [word(16), word(20), word(24), word(28)],
        [56.0, 9.0, 106.0, 59.0]
    );
    assert_eq!([word(32), word(36)], [512.0, 512.0], "the atlas size");
}

/// A history nobody updated behaves as a camera that has not moved, not as one at zoom zero.
///
/// A default `ZoomHistory` has `last_integer_zoom` of zero, so `z > last_integer_zoom` is true
/// at every positive zoom and the crossfade reports the camera zooming *in* from the bottom of
/// the world. That gives `from_scale` of two where it should be a half — a pattern drawn at four
/// times the intended size, with nothing reporting it. Seeding on read gives what mbgl's first
/// `update` gives.
#[test]
fn an_unseeded_history_does_not_look_like_zooming_in() {
    let positions = atlas();
    let pixels = vec![0u8; 4];
    let patterns = Patterns {
        texture: TextureId(20),
        size: [1, 1],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

    // At an integer zoom the camera has not crossed anything, which is `from_scale` of a half —
    // the value the oracle's capture carries at zoom thirteen.
    assert_eq!(patterns.crossfade(13.0).from_scale, 0.5);
    // Above the integer it has, which is two.
    assert_eq!(patterns.crossfade(13.5).from_scale, 2.0);
}
