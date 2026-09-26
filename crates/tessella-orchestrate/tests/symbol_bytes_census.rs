//! Which symbol announcements carry bytes the consumer does not already have.
//!
//! The per-frame exception for symbols is written per bucket and the camera-dependence it exists
//! for is per drawable, so this counts the difference. A still camera should announce nothing
//! after the first frame; a zoom should announce the labels whose glyphs it moved and no more.
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

use std::sync::Arc;
use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::ProjectionMode;
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

/// A labeled layer and an unlabeled one, so the retained family is the control.
const STYLE: &str = r##"{"version": 8, "sources": {"src": {"type": "vector", "tiles": []}},
  "layers": [
    {"id": "bg", "type": "background", "paint": {"background-color": "#f4f1ea"}},
    {"id": "ground", "type": "fill", "source": "src", "source-layer": "earth",
     "paint": {"fill-color": "#eee"}},
    {"id": "labels", "type": "symbol", "source": "src", "source-layer": "places",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 16}},
    {"id": "roadnames", "type": "symbol", "source": "src", "source-layer": "roads",
     "layout": {"text-field": "{name}", "text-font": ["TestFont"], "text-size": 14,
                "symbol-placement": "line"}}]}"##;

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
        ground_below: 0.0,
    })
}

/// Each announced geometry's payload, by geometry id and family.
type Announcement = (u64, i32, Vec<(u32, Vec<u8>)>);

#[allow(clippy::too_many_arguments)]
fn emit_payloads(
    style: &Style,
    view: &ViewTransform,
    tiles: &[cover::TileCoord],
    buckets: &[(TileId, Arc<Vec<LayerBucket>>)],
    fonts: &Fonts,
    arena: &mut SlabArena,
    layouts: &mut frame::SymbolCache,
    placement: &mut frame::PlacementState,
    session: &mut Session,
) -> Vec<Announcement> {
    let mut ring = Ring::new(1 << 24);
    let (producer, consumer) = ring.split();
    frame::emit_incremental(
        producer,
        arena,
        layouts,
        placement,
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
            fonts: Some(fonts),
            patterns: None,
        },
        session,
    )
    .expect("the frame emits");

    let mut out = Vec::new();
    while let Some(record) = consumer.peek() {
        if record.kind == EnvelopeKind::GeometryAdd
            && let Some(add) = GeometryAdd::from_bytes(record.record)
        {
            // The descriptors live in the payload and their slab offsets move on every
            // re-encode, so the payload always differs. What the consumer actually reads -- and
            // what it repacks -- is the bytes those descriptors point at.
            let size = core::mem::size_of::<tessella_capture_abi::envelope::AttributeDesc>();
            let start = add.attrs.offset as usize;
            let mut attrs: Vec<(u32, Vec<u8>)> = Vec::new();
            for index in 0..add.attrs.count as usize {
                let Some(desc) = tessella_capture_abi::envelope::AttributeDesc::from_bytes(
                    &record.payload[start + index * size..],
                ) else {
                    continue;
                };
                let bytes = arena.resolve(desc.source).unwrap_or(&[]).to_vec();
                attrs.push((desc.attr_id, bytes));
            }
            attrs.sort_by_key(|(id, _)| *id);
            out.push((add.geometry.0, add.builtin_shader, attrs));
        }
        let consumed = record.consumed();
        consumer.advance(consumed);
    }
    out
}

#[test]
fn which_symbol_announcements_carry_different_bytes() {
    let style = Style::parse(STYLE).expect("the style parses");
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");
    let start = view_at(14.0);
    let tiles = cover::cover(&start).expect("covers");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        buckets.push((id, Arc::new(built)));
    }

    let mut fonts = Fonts::new("glyphs://{fontstack}/{range}.pbf");
    for (_, tile_buckets) in &buckets {
        for bucket in tile_buckets.iter() {
            if let Some(layout) = bucket.content.as_symbol() {
                fonts
                    .fetch(&layout.dependencies(), &Fixture)
                    .expect("glyphs");
            }
        }
    }

    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();

    let symbol = tessella_capture_abi::BuiltIn::SymbolSDFShader as i32;
    let mut previous: std::collections::BTreeMap<u64, Vec<(u32, Vec<u8>)>> = Default::default();
    let mut first_frame = 0usize;

    for step in 0..6 {
        // Frames 0-2 hold the camera still; 3-5 move it a little each time.
        // 0-2 still, 3 a pure pan, 4-5 a zoom nudge -- the change the flying-labels test uses.
        let view = match step {
            0..=2 => start,
            3 => {
                let mut moved = start;
                moved.longitude += 0.0004;
                camera::settled(&moved)
            }
            _ => {
                let mut moved = start;
                moved.zoom += 0.2 * f64::from(step - 3);
                camera::settled(&moved)
            }
        };
        let label = match step {
            0..=2 => "still",
            3 => "panned",
            _ => "zoomed",
        };
        let payloads = emit_payloads(
            &style,
            &view,
            &tiles,
            &buckets,
            &fonts,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        );
        let mut symbols = 0;
        let mut same = 0;
        let mut changed_attrs: std::collections::BTreeMap<u32, usize> = Default::default();
        for (id, family, attrs) in payloads {
            if family != symbol {
                continue;
            }
            symbols += 1;
            if let Some(was) = previous.get(&id) {
                if was == &attrs {
                    same += 1;
                } else {
                    for (attr_id, bytes) in &attrs {
                        let moved = was
                            .iter()
                            .find(|(old_id, _)| old_id == attr_id)
                            .is_none_or(|(_, old)| old != bytes);
                        if moved {
                            *changed_attrs.entry(*attr_id).or_default() += 1;
                        }
                    }
                }
            }
            previous.insert(id, attrs);
        }
        let which: Vec<String> = changed_attrs
            .iter()
            .map(|(id, n)| format!("attr{id}:{n}"))
            .collect();
        println!(
            "step {step} ({label}): symbols {symbols}, identical to last {same}, changed attrs [{}]",
            which.join(" ")
        );
        if step == 0 {
            first_frame = symbols;
        }
        match step {
            0 => assert!(
                symbols > 0,
                "the first frame announced nothing, so nothing is under test"
            ),
            1 | 2 => assert_eq!(
                symbols, 0,
                "the camera did not move and a symbol was announced anyway"
            ),
            _ => {}
        }
        if step == 5 {
            assert!(
                symbols > 0,
                "the zoom moved glyphs and nothing was announced"
            );
            assert!(
                symbols < first_frame,
                "the zoom announced every symbol, including those whose bytes it did not touch"
            );
        }
    }
}

/// The strongest form of the still-frame guarantee, on the hardest scene: a parked view with two
/// symbol layers writes **zero** ring bytes.
///
/// The test above counts symbol *announcements* and asserts none on a still camera. This counts
/// every byte, which is a different claim -- an announcement is one record kind among six, and §6.5
/// asks for silence, not for quiet. A symbol scene is where that is hardest: glyph atlases are
/// rebuilt from the style each frame, and symbol vertices are a function of the camera, so both of
/// the reasons a frame has to re-send something are present at once.
#[test]
fn a_parked_symbol_scene_writes_no_bytes_at_all() {
    use tessella_capture_abi::ring::{self, region_size};

    let style = Style::parse(STYLE).expect("the style parses");
    let decoded = Tile::decode(BERLIN).expect("the fixture decodes");
    let view = view_at(14.0);
    let tiles = cover::cover(&view).expect("covers");

    let mut buckets = Vec::new();
    for tile in &tiles {
        let id = TileId::new(tile.z, tile.x, tile.y);
        let built = build_mvt_tile(&style, "src", id, &decoded).expect("the tile builds");
        buckets.push((id, Arc::new(built)));
    }

    let mut fonts = Fonts::new("glyphs://{fontstack}/{range}.pbf");
    for (_, tile_buckets) in &buckets {
        for bucket in tile_buckets.iter() {
            if let Some(layout) = bucket.content.as_symbol() {
                fonts
                    .fetch(&layout.dependencies(), &Fixture)
                    .expect("glyphs");
            }
        }
    }

    const CAPACITY: usize = 1 << 24;
    let mut region = vec![0u64; region_size(CAPACITY).div_ceil(8)];
    // SAFETY: sized by `region_size`, eight-aligned as a `Vec<u64>`, outlives both halves, and
    // nothing else touches it.
    let (mut producer, _consumer) =
        unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), CAPACITY) };

    let mut arena = SlabArena::new();
    let mut layouts = frame::SymbolCache::default();
    let mut placement = frame::PlacementState::new();
    let mut session = Session::new();
    let light = Light::default();

    // The caches are carried across frames, unlike the census above, which rebuilds them per frame
    // to isolate one frame's payloads. Retention is the whole subject here.
    let emit = |producer: &mut _,
                arena: &mut SlabArena,
                layouts: &mut frame::SymbolCache,
                placement: &mut frame::PlacementState,
                session: &mut Session| {
        let frame = Frame {
            published_projection: None,
            projection: ProjectionMode::Mercator,
            style: &style,
            view: &view,
            view_id: ViewId(0),
            tiles: &tiles,
            buckets: &buckets,
            origins: &[],
            light: &light,
            fonts: Some(&fonts),
            patterns: None,
        };
        frame::emit_incremental(producer, arena, layouts, placement, &frame, session)
    };

    emit(
        &mut producer,
        &mut arena,
        &mut layouts,
        &mut placement,
        &mut session,
    )
    .expect("cold");
    let after_cold = producer.head();
    assert!(after_cold > 0, "the cold frame wrote something");

    // Past `TEXTURE_GRACE`, so a glyph atlas aged out of the registry would show up here.
    for round in 0..20 {
        emit(
            &mut producer,
            &mut arena,
            &mut layouts,
            &mut placement,
            &mut session,
        )
        .expect("parked");
        assert_eq!(
            producer.head(),
            after_cold,
            "round {round} wrote {} bytes for a view that did not move",
            producer.head() - after_cold
        );
    }
}
