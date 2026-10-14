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
use std::sync::Arc;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::envelope::{GeometryAdd, TextureId, ViewId, WireRecord as _};
use tessella_capture_abi::ring::Ring;
use tessella_glyph::atlas::Rect;
use tessella_glyph::sprite::IconPosition;
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame, Patterns};
use tessella_orchestrate::tile::{
    PatternLookup, TileId, build_mvt_tile_with_patterns, build_sourceless,
};
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

/// A pattern that varies with the feature, which resolves in the bucket rather than in the frame.
const DATA_DRIVEN: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
  "layers": [{"id": "f", "type": "fill", "source": "s", "source-layer": "water",
              "paint": {"fill-pattern": ["coalesce", ["get", "pattern"], "sand_noise"]}}]}"#;

/// Emits the style with and without sprites, returning shader counts and texture-ref totals.
fn emit_with(sprites: Option<&Patterns<'_>>) -> (BTreeMap<i32, usize>, u32) {
    emit_style(STYLE, sprites)
}

/// The same over an inline GeoJSON source, which is the path a line's per-feature pattern takes.
///
/// A vector tile's line bucket is built without the pattern lookup at all -- tessella#392 -- so a
/// data-driven `line-pattern` has rectangles only on this side. The features come from the style's
/// own `data`, as `capture-render` reads them.
fn emit_geojson(text: &str, sprites: Option<&Patterns<'_>>) -> (BTreeMap<i32, usize>, u32) {
    use tessella_orchestrate::tile::build_tile_with_patterns;
    use tessella_source::tiling::TilingOptions;

    let style = Style::parse(text).expect("parses");
    let Some(tessella_style::document::Source::Geojson(source)) = style.sources.get("s") else {
        panic!("the style's source is an inline GeoJSON");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_tile_with_patterns(
            &style,
            "s",
            id,
            &features,
            TilingOptions::default(),
            sprites.map(|sprites| sprites as &dyn PatternLookup),
        )
        .expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }
    emit_buckets(&style, &view, &tiles, &buckets, sprites)
}

/// The same, for a style of the caller's choosing.
fn emit_style(text: &str, sprites: Option<&Patterns<'_>>) -> (BTreeMap<i32, usize>, u32) {
    let style = Style::parse(text).expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        // With the sheet where there is one, which is the *bucket's* half of a pattern: a
        // data-driven one resolves to a pair of rectangles per feature here, and a build that
        // never saw the sprites carries none however the frame is emitted.
        let mut built = build_mvt_tile_with_patterns(
            &style,
            "s",
            id,
            &decoded,
            sprites.map(|sprites| sprites as &dyn PatternLookup),
        )
        .expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

    emit_buckets(&style, &view, &tiles, &buckets, sprites)
}

/// Emits a frame from buckets already built, returning shader counts and texture-ref totals.
fn emit_buckets(
    style: &Style,
    view: &tessella_tile::cover::ViewTransform,
    tiles: &[tessella_tile::cover::TileCoord],
    buckets: &[(TileId, Arc<Vec<tessella_orchestrate::tile::LayerBucket>>)],
    sprites: Option<&Patterns<'_>>,
) -> (BTreeMap<i32, usize>, u32) {
    let mut arena = SlabArena::new();
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style,
            view,
            view_id: ViewId(0),
            tiles,
            buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: sprites,
            states: None,
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
    // What the atlas reports for the fifty-by-fifty sprite the capture's constant fill layer
    // used: the sprite plus a pixel each side, which `atlas_rect` insets back off.
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

/// With sprites, the oracle's pattern shaders and a texture on every drawable.
#[test]
fn a_resolved_pattern_binds_the_pattern_shaders() {
    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
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

/// A pattern that varies with the feature is a pattern, and takes the pattern shader.
///
/// It did not. A constant `fill-pattern` is one rectangle pair the *frame* looks up; a data-driven
/// one is a pair per feature, resolved while the bucket is built -- and a frame-level lookup cannot
/// answer it at all, because `image_at` says a data-driven source "cannot be answered by a zoom
/// alone". Reading only the frame's half left the layer with no atlas, so it was bound through the
/// *plain* table with its rectangles still in the vertices, at the slots the pattern table gives
/// them.
///
/// What that drew is nothing, loudly: the consumer refuses the drawable --
/// `DeclaredDisagrees { attr_id: 4, wire: UShort4, table: Float4 }`, the rectangles arriving where
/// `FillShader` declares its color -- so a capture of the style came back three geometries short.
/// The attributes are why it is worth a test of its own rather than a line in
/// `attribute_bindings.rs`: with the atlas missing the layer now draws *nothing at all*, so there
/// is no drawable left for a binding check to look at.
#[test]
fn a_data_driven_pattern_takes_the_pattern_shader() {
    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    let (shaders, textures) = emit_style(DATA_DRIVEN, Some(&patterns));

    assert!(shaders.contains_key(&13), "no pattern shader: {shaders:?}");
    assert!(
        !shaders.contains_key(&11),
        "a plain fill drew instead: {shaders:?}"
    );
    assert_eq!(
        shaders.get(&13).copied(),
        shaders.get(&14).copied(),
        "the outline did not follow the triangles: {shaders:?}"
    );
    assert_eq!(
        textures as usize,
        shaders.values().sum::<usize>(),
        "every pattern drawable binds the atlas exactly once"
    );
}

/// And a line's, which resolves the same way and was dropped rather than refused.
///
/// A data-driven `line-pattern` is the same two rectangles a fill puts at ids four and five, "at
/// different slots because the line shader has already spent its low bindings". The frame sends
/// them only where it has an atlas -- `pattern_vertices: atlas.and(..)` -- so a layer whose pattern
/// resolved per feature and not at the frame kept its rectangles in the bucket and lost them on the
/// way out, and the line drew plain in the `line-color` default. Quieter than the fill's refusal
/// and the same cause.
///
/// Over a GeoJSON source, because that is the path that resolves a line's rectangles: the MVT one
/// builds a line bucket without asking the lookup at all, so a data-driven `line-pattern` on vector
/// tiles has none to send. That is tessella#392 and not this.
#[test]
fn a_data_driven_line_pattern_takes_the_pattern_shader() {
    use tessella_capture_abi::generated::mbgl_enums::BuiltIn;

    const LINE: &str = r#"{"version": 8,
      "sources": {"s": {"type": "geojson", "data": {"type": "FeatureCollection", "features": [
        {"type": "Feature", "properties": {},
         "geometry": {"type": "LineString", "coordinates": [[-1, 0], [1, 0], [1, 1]]}}]}}},
      "layers": [{"id": "l", "type": "line", "source": "s",
                  "paint": {"line-pattern": ["coalesce", ["get", "pattern"], "sand_noise"],
                            "line-width": 4}}]}"#;

    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    let (shaders, _) = emit_geojson(LINE, Some(&patterns));

    assert!(
        shaders.contains_key(&(BuiltIn::LinePatternShader as i32)),
        "no line pattern shader: {shaders:?}"
    );
    assert!(
        !shaders.contains_key(&(BuiltIn::LineShader as i32)),
        "a plain line drew instead: {shaders:?}"
    );
}

/// And the same over a vector tile, which is the path that resolved nothing.
///
/// tessella#392. The MVT line arm built its bucket without ever asking the pattern lookup, so a
/// `line-pattern` that varies with the feature had rectangles on the GeoJSON side and none here --
/// the one place the two paths disagreed about what a pattern is. The frame cannot make up the
/// difference: its own lookup "cannot be answered by a zoom alone" for a data-driven source, so the
/// layer had no pattern from either half and drew plain, and after tessella#391 drew nothing.
#[test]
fn a_data_driven_line_pattern_resolves_on_a_vector_tile() {
    use tessella_capture_abi::generated::mbgl_enums::BuiltIn;

    const LINE: &str = r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
      "layers": [{"id": "l", "type": "line", "source": "s", "source-layer": "admin",
                  "paint": {"line-pattern": ["coalesce", ["get", "pattern"], "sand_noise"],
                            "line-width": 4}}]}"#;

    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    let (shaders, _) = emit_style(LINE, Some(&patterns));

    assert!(
        shaders.contains_key(&(BuiltIn::LinePatternShader as i32)),
        "no line pattern shader: {shaders:?}"
    );
    assert!(
        !shaders.contains_key(&(BuiltIn::LineShader as i32)),
        "a plain line drew instead: {shaders:?}"
    );
}

/// Without them the layer draws nothing, rather than drawing plain.
///
/// tessella#383. It drew plain -- 11 for the triangles and 15 for the outline -- and the plain
/// family paints the layer's evaluated `fill-color`, which a style that asked for a pattern never
/// set. That is the spec default `#000000`, so a park waiting on its sprites came out black, which
/// is the loudest possible wrong answer and was found by replaying a capture rather than by
/// reading the code.
///
/// mbgl branches on the style's own property rather than on whether a sprite resolved
/// (`render_fill_layer.cpp:347`) and draws nothing where the program for it is missing (`:464`).
/// Absent beats wrong: the layer appears when its sheet lands.
#[test]
fn an_unresolved_pattern_draws_nothing() {
    let (shaders, textures) = emit_with(None);
    assert!(!shaders.contains_key(&11), "a plain fill: {shaders:?}");
    assert!(!shaders.contains_key(&15), "an outline: {shaders:?}");
    assert!(!shaders.contains_key(&13), "a pattern shader with no atlas");
    assert_eq!(textures, 0, "nothing to bind");
    // The style is the one patterned layer, so an empty frame is the whole statement. That it is
    // the *layer* and not the frame is what `a_resolved_pattern_binds_the_pattern_shaders` says:
    // the same style with a sheet draws 13 and 14.
    assert!(shaders.is_empty(), "something drew: {shaders:?}");
}

/// A sprite the atlas does not hold is not a pattern, and is not a plain fill either.
///
/// The layer names `sand_noise`; this atlas has something else. Binding the pattern shader anyway
/// would sample whatever was packed at a rectangle that describes nothing -- and drawing it plain
/// paints a color the style never set, which is tessella#383. This one does not resolve when the
/// sheet lands, because the sheet has landed: a style naming a sprite its sheet does not hold stays
/// absent, which is the answer mbgl gives it too.
#[test]
fn a_missing_sprite_draws_nothing() {
    let mut positions = atlas();
    positions.remove("sand_noise");
    positions.insert("something_else".to_owned(), atlas()["sand_noise"]);
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };
    let (shaders, textures) = emit_with(Some(&patterns));
    assert!(
        !shaders.contains_key(&11),
        "fell back to a fill: {shaders:?}"
    );
    assert!(shaders.is_empty(), "something drew: {shaders:?}");
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
        revision: 0,
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
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built =
            build_mvt_tile_with_patterns(&style, "s", id, &decoded, None).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

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
            patterns: Some(&patterns),
            states: None,
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

    // The sprite's own bounds, inset from the rectangle the atlas reports: a fifty-pixel sprite
    // reported at (56, 9) over fifty-two becomes [57, 10, 107, 60].
    let word = |at: usize| f32::from_le_bytes(slot4[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(
        [word(0), word(4), word(8), word(12)],
        [57.0, 10.0, 107.0, 60.0]
    );
    assert_eq!(
        [word(16), word(20), word(24), word(28)],
        [57.0, 10.0, 107.0, 60.0]
    );
    assert_eq!(
        [word(8) - word(0), word(12) - word(4)],
        [50.0, 50.0],
        "fifty pixels wide, which is the sprite"
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
        revision: 0,
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

/// An extrusion with a pattern takes shader 18 and writes the fill's block at slot 4.
///
/// `FillExtrusionTilePropsUBO` is `FillPatternTilePropsUBO`'s fields exactly — two rectangles,
/// an atlas size, two pads, forty-eight bytes — so it takes the same packer. The capture's
/// `ubo layer:5 slot=4` is 1152 bytes, which is twenty-four drawables of it.
///
/// The oracle emits `sh0018` *and* `sh0019` per tile, at five vertices and four: its instanced
/// extrusion draws the roof and the walls as separate drawables. This build emits one, so it
/// takes the pattern variant of the one it emits. Splitting them is a question about the
/// extrusion's geometry that predates patterns and wants its own change.
#[test]
fn an_extrusion_pattern_binds_its_own_shader() {
    use tessella_capture_abi::envelope::UboUpdate;

    let positions = atlas();
    let pixels = vec![0u8; 512 * 512 * 4];
    let patterns = Patterns {
        revision: 0,
        texture: TextureId(20),
        size: [512, 512],
        positions: &positions,
        pixels: &pixels,
        history: ZoomHistory::new(),
    };

    let style = Style::parse(
        r#"{"version": 8, "sources": {"s": {"type": "vector", "tiles": []}},
            "layers": [{"id": "e", "type": "fill-extrusion", "source": "s",
                        "source-layer": "water",
                        "paint": {"fill-extrusion-pattern": "sand_noise",
                                  "fill-extrusion-height": 20,
                                  "fill-extrusion-opacity": 0.8}}]}"#,
    )
    .expect("parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 2.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
        ground_below: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built =
            build_mvt_tile_with_patterns(&style, "s", id, &decoded, None).expect("builds");
        built.extend(build_sourceless(&style, id).expect("background"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, Arc::new(built)));
    }

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
            patterns: Some(&patterns),
            states: None,
        },
    )
    .expect("emits");

    let mut shaders = BTreeMap::new();
    let mut slot4 = Vec::new();
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::GeometryAdd => {
                if let Some(add) = GeometryAdd::from_bytes(record.record) {
                    *shaders.entry(add.builtin_shader).or_insert(0) += 1;
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

    // 18 is FillExtrusionPatternShader; 16 is the plain one it replaces.
    assert!(
        shaders.contains_key(&18),
        "no extrusion pattern shader: {shaders:?}"
    );
    assert!(
        !shaders.contains_key(&16),
        "a plain extrusion survived: {shaders:?}"
    );

    assert!(!slot4.is_empty(), "no tile props were written");
    assert_eq!(slot4.len() % 48, 0, "whole blocks of the fill's layout");
    let word = |at: usize| f32::from_le_bytes(slot4[at..at + 4].try_into().expect("four bytes"));
    assert_eq!(
        [word(8) - word(0), word(12) - word(4)],
        [50.0, 50.0],
        "the sprite's own size"
    );
    assert_eq!([word(32), word(36)], [512.0, 512.0], "the atlas size");
}
