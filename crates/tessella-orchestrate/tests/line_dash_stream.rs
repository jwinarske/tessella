//! A `line-dasharray` puts an atlas on the wire and names the SDF shader (§R3).
//!
//! # What a dashed line needs that a plain one does not
//!
//! Three things, and all three have to agree. The distance field itself, as a texture the
//! consumer has before any drawable names it. The `LineSDFShader` on the geometry, because the
//! dash is a fragment-stage lookup and the plain shader has nowhere to do it. And the atlas bound
//! to that geometry, because the shader samples one slot whatever the layer set.
//!
//! Any two without the third is a line that draws solid or draws nothing, which is exactly what
//! the §9.1 metric does not see: at threshold 48 the ten dashed layers of a real basemap scored
//! 0.000% while drawing 6,304 more pixels than the oracle.

use tessella_capture_abi::envelope::{GeometryAdd, TextureRef, TextureUpdate, WireRecord as _};
use tessella_capture_abi::ring::Ring;
use tessella_capture_abi::{BuiltIn, EnvelopeKind, ProjectionMode};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::frame::{self, Frame};
use tessella_orchestrate::tile::{TileId, build_mvt_tile, build_sourceless};
use tessella_source::mvt::Tile;
use tessella_style::Style;
use tessella_style::light::Light;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

const REAL_TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// One dashed layer and one plain one over the same features, so the difference is the dasharray
/// and nothing else.
const STYLE: &str = r##"{
  "version": 8,
  "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "plain", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#88a", "line-width": 2}},
    {"id": "dashed", "type": "line", "source": "src", "source-layer": "water",
     "paint": {"line-color": "#000", "line-width": 3, "line-dasharray": [2, 2]}}
  ]
}"##;

struct Sent {
    textures: Vec<TextureUpdate>,
    geometry: Vec<GeometryAdd>,
}

fn emit() -> Sent {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 0.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        built.extend(build_sourceless(&style, id).expect("the sourceless layers build"));
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }

    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &view,
            view_id: tessella_capture_abi::envelope::ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    let mut sent = Sent {
        textures: Vec::new(),
        geometry: Vec::new(),
    };
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::TextureUpdate => {
                sent.textures
                    .push(TextureUpdate::from_bytes(record.record).expect("a texture update"));
            }
            EnvelopeKind::GeometryAdd => {
                sent.geometry
                    .push(GeometryAdd::from_bytes(record.record).expect("a geometry add"));
            }
            _ => {}
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    sent
}

/// The dashed layer names the SDF shader and the plain one does not.
#[test]
fn a_dasharray_selects_the_sdf_shader() {
    let sent = emit();
    let dashed = sent
        .geometry
        .iter()
        .filter(|add| add.builtin_shader == BuiltIn::LineSDFShader as i32)
        .count();
    let plain = sent
        .geometry
        .iter()
        .filter(|add| add.builtin_shader == BuiltIn::LineShader as i32)
        .count();
    assert!(dashed > 0, "no drawable took the SDF shader");
    assert_eq!(
        dashed, plain,
        "one of each layer per tile, and the cover is the same cover"
    );
}

/// Its atlas is uploaded, and before the geometry that samples it.
///
/// The order is the point, not an aesthetic: a texture reference the consumer has not been given
/// samples whatever was last at that slot, which for a distance field is a line dashing to
/// somebody else's rhythm rather than one that does not draw.
#[test]
fn the_atlas_goes_up_before_anything_names_it() {
    let style = Style::parse(STYLE).expect("the style parses");
    let view = camera::settled(&ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom: 0.0,
        width: 512.0,
        height: 512.0,
        bearing: 0.0,
        pitch: 0.0,
    });
    let tiles = cover::cover(&view).expect("covers");
    let decoded = Tile::decode(REAL_TILE).expect("the fixture decodes");
    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let mut built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        built.sort_by_key(|bucket| bucket.layer_index);
        buckets.push((id, built));
    }
    let mut ring = Ring::new(1 << 22);
    let (producer, consumer) = ring.split();
    let mut arena = SlabArena::new();
    frame::emit(
        producer,
        &mut arena,
        &Frame {
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &view,
            view_id: tessella_capture_abi::envelope::ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &Light::default(),
            fonts: None,
            patterns: None,
        },
    )
    .expect("the frame emits");

    let mut uploaded: Option<usize> = None;
    let mut named: Option<usize> = None;
    let mut texture = None;
    let mut index = 0usize;
    while let Some(record) = consumer.peek() {
        match record.kind {
            EnvelopeKind::TextureUpdate => {
                let update = TextureUpdate::from_bytes(record.record).expect("a texture update");
                // The two placeholders are 0 and 1; anything above them is an atlas.
                if update.texture.0 > 1 && uploaded.is_none() {
                    uploaded = Some(index);
                    texture = Some(update.texture);
                }
            }
            EnvelopeKind::GeometryAdd => {
                let add = GeometryAdd::from_bytes(record.record).expect("a geometry add");
                if add.builtin_shader == BuiltIn::LineSDFShader as i32 && named.is_none() {
                    named = Some(index);
                    let size = core::mem::size_of::<TextureRef>();
                    let start = add.texture_refs.offset as usize;
                    let bound: Vec<u64> = (0..add.texture_refs.count as usize)
                        .filter_map(|slot| {
                            record
                                .payload
                                .get(start + slot * size..)
                                .and_then(TextureRef::from_bytes)
                                .map(|reference| reference.texture.0)
                        })
                        .collect();
                    assert_eq!(
                        bound,
                        vec![texture.expect("an atlas went up first").0],
                        "the drawable names the atlas that went up"
                    );
                }
            }
            _ => {}
        }
        index += 1;
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    let uploaded = uploaded.expect("an atlas was uploaded");
    let named = named.expect("a drawable took the SDF shader");
    assert!(uploaded < named, "atlas at {uploaded}, drawable at {named}");
}

/// The placement the UBO carries, in the numbers the shader multiplies by.
///
/// A `[2, 2]` pattern under a three-pixel line repeats every twelve pixels: two units of dash and
/// two of gap, each unit a line width. The shader gets there by `linesofar * patternscale.x /
/// floorwidth`, where `linesofar` is in tile units -- sixteen to a pixel at a tile's own zoom --
/// so the scale has to be the reciprocal of the pattern's length in *pixels* times that sixteen.
/// Checked as the period rather than as the factor, because the period is the thing a reader can
/// hold a ruler against and the factor is three reciprocals deep.
#[test]
fn the_scale_puts_the_period_where_a_ruler_finds_it() {
    use tessella_glyph::dash::{Atlas, Cap};
    use tessella_orchestrate::ubo::DashPlacement;
    use tessella_style::crossfade::Crossfade;

    let (_, from, to) = Atlas::pair(&[2.0, 2.0], &[2.0, 2.0], Cap::Butt).expect("an atlas");
    let placement = DashPlacement {
        from,
        to,
        // A settled camera at an integer zoom: mbgl's fade is complete, so the pattern drawn is
        // the `to` one at its own scale.
        crossfade: Crossfade {
            from_scale: 0.5,
            to_scale: 1.0,
            t: 1.0,
        },
        pixel_ratio: 1.0,
    };
    // Sixteen tile units to the pixel, which is a tile drawn at its own zoom.
    let (_, scale_b) = placement.scales(16.0);

    let floorwidth = 3.0_f32;
    // One repeat is where the texture coordinate reaches one.
    let period_in_tile_units = floorwidth / scale_b[0];
    assert!(
        (period_in_tile_units / 16.0 - 12.0).abs() < 1e-3,
        "period was {} pixels",
        period_in_tile_units / 16.0
    );
    // And the pattern spans its rows exactly: a normal of plus or minus one times minus half the
    // height, either side of the center.
    assert!((scale_b[1] + to.height / 2.0).abs() < 1e-6);

    // Half a texel of the narrower pattern, which at a crossfade of one half is the `from` one.
    assert!(
        (placement.sdfgamma() - 0.25).abs() < 1e-6,
        "sdfgamma was {}",
        placement.sdfgamma()
    );
}
