//! One real feature, all the way from tile bytes to a placed, fading label.
//!
//! Every link in R2's chain has its own tests, and most are checked against mbgl. None of that
//! says the links fit together. A shaper measuring in one unit and a collision box scaling in
//! another are each correct and jointly wrong, and the only way to see it is to run a real
//! feature through the whole thing and look at what comes out the far end.
//!
//! The chain: decode a vendored Protomaps tile, resolve `text-field` against a feature, fetch
//! the glyph range its codepoints need, shape the text, pack the glyphs into the atlas, build
//! the quads, derive a collision box, assign a cross-tile identity, place it against its
//! neighbours, and step the fade.

use std::collections::BTreeSet;

use tessella_glyph::atlas::Atlas;
use tessella_glyph::pbf::{self, Glyph, Range};
use tessella_glyph::quads::{self, Placed as QuadGlyph};
use tessella_glyph::shaping::{self, Char, Options as ShapeOptions};
use tessella_glyph::text::ONE_EM;
use tessella_layout::symbol;
use tessella_place::cross_tile::{CrossTileIndex, Symbol};
use tessella_place::fade::Fades;
use tessella_place::feature::{Extent, Padding, collision_box};
use tessella_place::grid::GridIndex;
use tessella_place::placement::{Candidate, Rules, Shape, place};
use tessella_source::mvt::{GeomType, Tile};
use tessella_style::Layer;
use tessella_tile::renderables::DataTileId;

const TILE: &[u8] = include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt");
const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// A symbol layer labelling the `places` layer's points.
fn places_layer() -> Layer {
    serde_json::from_str(
        r#"{"id": "place-labels", "type": "symbol", "source": "v", "source-layer": "places",
            "layout": {"text-field": "{name}", "text-font": ["Noto Sans Regular"],
                       "text-size": 14, "text-max-width": 8}}"#,
    )
    .expect("a symbol layer")
}

/// One label carried through every stage, with what each stage produced.
struct Ran {
    text: String,
    lines: usize,
    quads: usize,
    extent: Extent,
}

/// Runs one feature's label through the whole chain.
fn run(text: &str, glyphs: &[Glyph], atlas: &mut Atlas) -> Ran {
    // Shaping needs an advance per character, which comes from the glyphs the manager holds.
    // A codepoint the font lacks has no advance and nothing to draw — which is the same shape
    // as a space, and the reason `Char` carries both facts.
    let chars: Vec<Char> = text
        .chars()
        .map(|character| {
            let codepoint = character as u32;
            match glyphs.iter().find(|glyph| glyph.id == codepoint) {
                #[allow(clippy::cast_precision_loss)]
                Some(glyph) if glyph.bitmap_size().is_some() => {
                    Char::new(codepoint, glyph.metrics.advance as f32)
                }
                #[allow(clippy::cast_precision_loss)]
                Some(glyph) => Char::blank(codepoint, glyph.metrics.advance as f32),
                None => Char::blank(codepoint, 0.0),
            }
        })
        .collect();

    let shaping = shaping::shape(
        &chars,
        &ShapeOptions {
            max_width: 8.0 * ONE_EM,
            ..ShapeOptions::default()
        },
    );

    // Pack every glyph the label uses, then build its quads from what the atlas gave back.
    for glyph in glyphs {
        if text.chars().any(|character| character as u32 == glyph.id) {
            atlas.add(glyph.id, glyph);
        }
    }
    let built = quads::glyph_quads(
        &shaping,
        |codepoint| {
            let glyph = glyphs.iter().find(|glyph| glyph.id == codepoint)?;
            Some(QuadGlyph {
                rect: atlas.get(codepoint)?,
                metrics: glyph.metrics,
            })
        },
        &quads::Options::default(),
    );

    Ran {
        text: text.to_string(),
        lines: shaping.lines.len(),
        quads: built.len(),
        extent: Extent {
            top: shaping.top,
            bottom: shaping.bottom,
            left: shaping.left,
            right: shaping.right,
        },
    }
}

/// Reads the fixture's place names, in tile order.
fn place_names() -> Vec<(String, (f32, f32))> {
    let tile = Tile::decode(TILE).expect("the fixture decodes");
    let layer = tile
        .layers
        .iter()
        .find(|layer| layer.name == "places")
        .expect("a places layer");
    let style = places_layer();

    let mut out = Vec::new();
    for feature in layer.features() {
        if feature.geom_type() != GeomType::Point {
            continue;
        }
        let Some(label) = symbol::label(&style, 5.0, &feature) else {
            continue;
        };
        let anchor = feature
            .rings()
            .next()
            .and_then(|ring| ring.first().copied())
            .expect("a point has a coordinate");
        #[allow(clippy::cast_precision_loss)]
        out.push((label.text, (anchor[0] as f32, anchor[1] as f32)));
    }
    out
}

/// A real tile's place names shape, pack and become quads.
///
/// The joint assertion: every stage produced something consistent with the one before it. A
/// label with lines and no quads means the atlas and the shaper disagree about which glyphs
/// exist; a label with quads and no extent means the shaper measured nothing it drew.
#[test]
fn a_real_tile_becomes_labels() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
    let names = place_names();
    assert!(names.len() > 20, "only {} labels", names.len());

    let mut atlas = Atlas::new(1024, 1024);
    let mut drew = 0usize;

    for (text, _) in &names {
        let ran = run(text, &glyphs, &mut atlas);
        assert!(ran.lines >= 1, "{:?} shaped to nothing", ran.text);

        // Every label whose glyphs are all in this range must draw, and its box must have area.
        let in_range = text.chars().all(|character| {
            let codepoint = character as u32;
            codepoint == 0x20 || glyphs.iter().any(|glyph| glyph.id == codepoint)
        });
        if in_range {
            assert!(ran.quads > 0, "{:?} produced no quads", ran.text);
            assert!(
                ran.extent.right > ran.extent.left && ran.extent.bottom > ran.extent.top,
                "{:?} has an empty box: {:?}",
                ran.text,
                ran.extent
            );
            drew += 1;
        }
    }

    assert!(drew > 20, "only {drew} labels drew");
    assert!(!atlas.is_empty(), "the atlas holds the glyphs they used");
}

/// A label's box grows with its text, and a wrapped one is taller and narrower.
///
/// The check that shaping and the collision box agree about units. If the shaper measured in ems
/// and the box scaled in pixels, both would be internally consistent and every label on the map
/// would reserve the wrong amount of space.
#[test]
fn a_longer_label_reserves_more_space() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
    let mut atlas = Atlas::new(1024, 1024);

    let short = run("Rome", &glyphs, &mut atlas);
    let long = run("Constantinople", &glyphs, &mut atlas);

    assert_eq!(short.lines, 1);
    assert!(
        long.extent.right - long.extent.left > short.extent.right - short.extent.left,
        "the longer name should be wider: {:?} vs {:?}",
        long.extent,
        short.extent
    );

    // Wrapped at eight ems, a long two-word name takes two lines and is narrower than it would
    // be on one.
    let wrapped = run("Buenos Aires Province", &glyphs, &mut atlas);
    assert!(wrapped.lines >= 2, "{:?}", wrapped.text);
    assert!(
        wrapped.extent.bottom - wrapped.extent.top > short.extent.bottom - short.extent.top,
        "two lines should be taller than one"
    );
}

/// The whole per-view half, over a real tile's labels.
///
/// Boxes from the shaper, identities from the cross-tile index, a placement pass, and a fade.
/// What it asserts is that the pieces compose: crowded labels lose, the survivors are the ones
/// the fades track, and the frame settles.
#[test]
fn a_tile_of_labels_places_and_settles() {
    let glyphs = pbf::parse(
        Range {
            first: 0,
            last: 255,
        },
        GLYPHS,
    )
    .expect("the range parses");
    let names = place_names();
    let mut atlas = Atlas::new(1024, 1024);

    // Shape each label and turn it into a collision box at its anchor — in *screen* space.
    //
    // This is the one place the units have to be got right, and the first version of this test
    // got them wrong: anchors arrive in tile coordinates, 0..8192 across, while a shaped label
    // measures in screen pixels and is some tens across. Mixed, every label is a speck on a
    // vast plane and nothing ever collides — all seventy-five placed, which is what gave it
    // away.
    //
    // Labels compete for *screen*, not for ground. Two towns a kilometre apart collide at z5
    // and not at z14, and the same two labels collide on a phone and not on a wall display.
    // So the anchor is projected first: a tile drawn 512 pixels wide over an extent of 8192.
    const TILE_PIXELS: f32 = 512.0;
    const EXTENT: f32 = 8192.0;
    let to_screen = |anchor: (f32, f32)| {
        (
            anchor.0 * TILE_PIXELS / EXTENT,
            anchor.1 * TILE_PIXELS / EXTENT,
        )
    };

    let mut symbols: Vec<Symbol> = Vec::new();
    let mut boxes = Vec::new();
    for (text, anchor) in &names {
        let ran = run(text, &glyphs, &mut atlas);
        let Some(placed) = collision_box(
            ran.extent,
            to_screen(*anchor),
            1.0,
            Padding::uniform(2.0),
            0.0,
        ) else {
            continue;
        };
        // The cross-tile index works in tile coordinates, which is right: identity is about
        // where a label is on the ground, and the ground does not move when the camera does.
        symbols.push(Symbol::new(text.clone(), *anchor));
        boxes.push(placed);
    }
    assert!(symbols.len() > 20);

    // Identities, so the fades have something stable to key on.
    let mut index = CrossTileIndex::new();
    let tile = DataTileId::overscaled(5, 0, 5, 16, 11);
    assert!(index.add_bucket(tile, 1, &mut symbols));
    let unique: BTreeSet<u32> = symbols.iter().map(|symbol| symbol.cross_tile_id).collect();
    assert_eq!(
        unique.len(),
        symbols.len(),
        "every label got its own identity"
    );

    // Place them against each other. A z5 tile of place names is crowded, so some must lose —
    // a pass where everything fits would not be exercising collision at all.
    let candidates: Vec<Candidate> = symbols
        .iter()
        .zip(&boxes)
        .map(|(symbol, placed)| Candidate {
            cross_tile_id: symbol.cross_tile_id,
            text: Some(Shape::Box(*placed)),
            vertical_text: None,
            icon: None,
        })
        .collect();
    // A grid the size of the tile on screen, in cells of roughly a label's height.
    let mut grid: GridIndex<u32> = GridIndex::new(TILE_PIXELS, TILE_PIXELS, 32);
    let placed = place(&candidates, &Rules::default(), &mut grid);

    let drawn = placed.iter().filter(|symbol| symbol.text).count();
    println!("  placed {drawn} of {} labels at z5", placed.len());
    assert!(drawn > 0, "nothing was placed");
    assert!(
        drawn < placed.len(),
        "a crowded z5 tile should have lost some labels: {drawn} of {}",
        placed.len()
    );

    // Fade what was placed, and check the frame settles rather than churning forever.
    let mut fades = Fades::new();
    let frame: Vec<(u32, bool, bool)> = placed
        .iter()
        .map(|symbol| (symbol.cross_tile_id, symbol.text, symbol.icon))
        .collect();
    for _ in 0..6 {
        fades.step(0.25, frame.clone(), false);
    }
    assert_eq!(
        fades.len(),
        drawn,
        "only the drawn labels are still tracked"
    );
    assert!(fades.settled(), "the frame should have gone quiet");
}
