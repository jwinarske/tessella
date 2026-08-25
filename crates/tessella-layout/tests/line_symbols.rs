//! Labels that follow a line: a road name repeated along the road.
//!
//! The anchors are `line_anchors`' business and checked against mbgl there. What this checks is
//! the wiring: one shaping serving every repetition, the along-line offset reaching the shader
//! rather than the corners, and each repetition owning its own slice of the shared buffer.

use tessella_glyph::atlas::{Atlas, Rect};
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol_bucket::{
    Glyphs, LineLabel, LineOptions, SymbolOptions, build_line_symbols,
};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

struct Font {
    glyphs: Vec<Glyph>,
    atlas: Atlas,
}

impl Font {
    fn new(pack: &str) -> Self {
        let glyphs = pbf::parse(
            Range {
                first: 0,
                last: 255,
            },
            GLYPHS,
        )
        .expect("the range parses");
        let mut atlas = Atlas::new(512, 512);
        for glyph in &glyphs {
            if pack.chars().any(|character| character as u32 == glyph.id) {
                atlas.add(glyph.id, glyph);
            }
        }
        Self { glyphs, atlas }
    }
}

impl Glyphs for Font {
    fn metrics(&self, codepoint: u32) -> Option<(Metrics, bool)> {
        let glyph = self.glyphs.iter().find(|glyph| glyph.id == codepoint)?;
        Some((glyph.metrics, glyph.bitmap_size().is_some()))
    }
    fn rect(&self, codepoint: u32) -> Option<Rect> {
        self.atlas.get(codepoint)
    }
}

/// A long straight road across the tile.
fn road() -> Vec<(f32, f32)> {
    (0..=20i16)
        .map(|index| (f32::from(index) * 200.0 + 100.0, 4000.0))
        .collect()
}

fn label(text: &str, line: Vec<(f32, f32)>) -> LineLabel {
    LineLabel {
        text: text.to_string(),
        line,
    }
}

/// A name repeats along its road, once per anchor.
#[test]
fn a_name_repeats_along_its_road() {
    let font = Font::new("Main Street");
    let (buffers, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions::default(),
    );

    assert!(
        laid.len() > 2,
        "a long road carries several: {}",
        laid.len()
    );
    // Every repetition draws the same glyphs.
    let glyphs = laid[0].glyphs;
    assert!(glyphs > 0);
    assert!(laid.iter().all(|entry| entry.glyphs == glyphs));
    assert_eq!(buffers.glyphs(), glyphs * laid.len());
}

/// Each repetition sits at its own anchor, spaced along the line.
#[test]
fn the_repetitions_are_spaced_along_the_line() {
    let font = Font::new("Main Street");
    let (_, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions::default(),
    );

    let mut anchors: Vec<f32> = laid.iter().map(|entry| entry.anchor.0).collect();
    anchors.sort_by(f32::total_cmp);

    // Strictly increasing, and by roughly the spacing.
    for pair in anchors.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap > 0.0, "two repetitions at the same place: {anchors:?}");
        assert!(gap >= 200.0, "closer than the spacing: {gap}");
    }
    // The road runs along y = 4000, so every anchor does too.
    assert!(
        laid.iter()
            .all(|entry| (entry.anchor.1 - 4000.0).abs() < 1.0)
    );
}

/// The along-line position is in the offset, not in the corners.
///
/// The shader projects a line-following label before placing each glyph, so the distance along
/// the line has to reach it separately. Baked into the corners it would lay the label out flat
/// and then bend it, putting every glyph but the first in the wrong place.
#[test]
fn the_along_line_position_is_not_in_the_corners() {
    let font = Font::new("Main Street");
    let (buffers, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions::default(),
    );

    // Within one repetition, every glyph's corner offsets fall in a narrow band around zero —
    // they describe the glyph's own box, not its place in the word.
    let first = laid[0].vertices.clone();
    let corners: Vec<i16> = buffers.vertices[first]
        .iter()
        .map(|vertex| vertex.pos_offset[2])
        .collect();
    let spread =
        f32::from(*corners.iter().max().expect("some") - *corners.iter().min().expect("some"))
            / 32.0;

    assert!(
        spread < 60.0,
        "the corners span {spread} units, which is the whole label rather than one glyph"
    );
}

/// And it is recorded, one distance per glyph, advancing across the word.
///
/// The other half of the test above, and the half that was missing: proving the distance is not
/// in the corners says nothing about whether it survived at all. It had not — every glyph of a
/// repetition carried the same position, so a rasterizer drawing from these buffers stacked the
/// whole word on one letter. That is mbgl's `PlacedSymbol::glyphOffsets`, and it is per quad
/// rather than per vertex because the four corners of a glyph share one place in the word.
#[test]
fn the_along_line_position_is_recorded_per_glyph() {
    let font = Font::new("Main Street");
    let (buffers, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions::default(),
    );

    assert_eq!(
        buffers.glyph_offsets.len(),
        buffers.vertices.len() / 4,
        "one per quad"
    );

    // "Main Street" is eleven characters, of which the space has no quad.
    let first = laid[0].vertices.clone();
    let offsets = &buffers.glyph_offsets[first.start / 4..first.end / 4];
    assert_eq!(offsets.len(), 10);

    // Left to right, and spanning the width of the word rather than a fraction of it.
    for pair in offsets.windows(2) {
        assert!(pair[1] > pair[0], "{offsets:?} does not advance");
    }
    let span = offsets.last().expect("some") - offsets[0];
    assert!(
        span > 40.0,
        "the word spans {span} units, which is narrower than one glyph"
    );

    // Centred on the anchor: mbgl shapes about the middle, so the first glyph sits left of it.
    assert!(
        offsets[0] < 0.0 && *offsets.last().expect("some") > 0.0,
        "{offsets:?} is not centred on the anchor"
    );
}

/// One shaping serves every repetition.
///
/// Asserted through its consequence: every repetition is byte-identical but for its anchor. If
/// each were shaped afresh the glyphs would still match, but the work would be redone for every
/// repetition of every road name on the tile.
#[test]
fn every_repetition_draws_identical_glyphs() {
    let font = Font::new("Main Street");
    let (buffers, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions::default(),
    );
    assert!(laid.len() >= 2);

    let first = &buffers.vertices[laid[0].vertices.clone()];
    let second = &buffers.vertices[laid[1].vertices.clone()];
    assert_eq!(first.len(), second.len());

    for (a, b) in first.iter().zip(second) {
        assert_eq!(a.pos_offset[2..], b.pos_offset[2..], "same corner offsets");
        assert_eq!(a.data, b.data, "same texels and size");
    }
    assert_ne!(
        first[0].pos_offset[0], second[0].pos_offset[0],
        "and different anchors"
    );
}

/// A centred label appears once.
#[test]
fn a_centred_label_appears_once() {
    let font = Font::new("Main Street");
    let (_, laid) = build_line_symbols(
        &[label("Main Street", road())],
        &font,
        &LineOptions {
            centred: true,
            ..LineOptions::default()
        },
    );

    assert_eq!(laid.len(), 1);
    // The road runs from x=100 to x=4100, so its middle is at 2100.
    assert!(
        (laid[0].anchor.0 - 2100.0).abs() < 2.0,
        "{:?}",
        laid[0].anchor
    );
}

/// A label too long for its line is not placed.
#[test]
fn a_label_longer_than_its_road_is_not_placed() {
    let font = Font::new("Extraordinarily Long Boulevard");
    let stub = vec![(100.0, 100.0), (130.0, 100.0)];
    let (buffers, laid) = build_line_symbols(
        &[label("Extraordinarily Long Boulevard", stub)],
        &font,
        &LineOptions::default(),
    );

    assert!(
        laid.is_empty(),
        "{} repetitions on a 30-unit line",
        laid.len()
    );
    assert!(buffers.is_empty());
}

/// Two roads share one buffer, and each repetition indexes its own vertices.
#[test]
fn two_roads_share_a_buffer_without_sharing_vertices() {
    let font = Font::new("Main Street High Road");
    let other: Vec<(f32, f32)> = (0..=20i16)
        .map(|index| (f32::from(index) * 200.0 + 100.0, 6000.0))
        .collect();
    let (buffers, laid) = build_line_symbols(
        &[label("Main Street", road()), label("High Road", other)],
        &font,
        &LineOptions::default(),
    );

    assert!(laid.len() >= 4, "both roads repeat");

    // The ranges tile the buffer exactly, in order, without gaps or overlap.
    let mut next = 0usize;
    for entry in &laid {
        assert_eq!(entry.vertices.start, next, "a gap or an overlap");
        next = entry.vertices.end;
    }
    assert_eq!(next, buffers.vertices.len());

    // And every index reaches a vertex that exists.
    assert!(
        buffers
            .indices
            .iter()
            .all(|index| usize::from(*index) < buffers.vertices.len())
    );
}

/// A tighter maximum angle drops repetitions on a bend.
#[test]
fn a_bend_thins_the_repetitions() {
    let font = Font::new("Winding Way");
    // A zigzag: straight runs joined by right angles.
    let zigzag: Vec<(f32, f32)> = (0..=24i16)
        .map(|index| {
            let step = f32::from(index) * 120.0;
            let wobble = if index % 2 == 0 { 0.0 } else { 300.0 };
            (step + 100.0, 4000.0 + wobble)
        })
        .collect();

    let generous = build_line_symbols(
        &[label("Winding Way", zigzag.clone())],
        &font,
        &LineOptions {
            max_angle: core::f32::consts::PI,
            ..LineOptions::default()
        },
    );
    let strict = build_line_symbols(
        &[label("Winding Way", zigzag)],
        &font,
        &LineOptions {
            max_angle: core::f32::consts::PI / 16.0,
            ..LineOptions::default()
        },
    );

    assert!(!generous.1.is_empty());
    assert!(
        strict.1.len() < generous.1.len(),
        "a strict angle should drop some: {} vs {}",
        strict.1.len(),
        generous.1.len()
    );
}

/// A line-placed label never wraps, however long its text.
///
/// It follows the line, and a second line of text would have to follow it too — offset from the
/// first along a curve, which is not something the along-line projection can express. mbgl sets
/// the wrap width to zero for line placement for the same reason.
///
/// Asserted through the extent's height: one line however many characters, where a point label
/// of the same text at the same width would take several.
#[test]
fn a_line_label_never_wraps() {
    use tessella_glyph::text::ONE_EM;

    let text = "Extraordinarily Long Boulevard Of Broken Dreams";
    let font = Font::new(text);
    let (_, laid) = build_line_symbols(
        &[label(text, road())],
        &font,
        &LineOptions {
            // A width that would wrap this text several times if it were honoured.
            symbol: SymbolOptions {
                max_width_ems: 4.0,
                ..SymbolOptions::default()
            },
            spacing: 2000.0,
            max_angle: core::f32::consts::PI,
            ..LineOptions::default()
        },
    );

    assert!(!laid.is_empty(), "the label should place somewhere");
    for entry in &laid {
        let (top, bottom, _, _) = entry.extent;
        assert!(
            (bottom - top - ONE_EM).abs() < 1.0,
            "the label is {} tall, which is more than one line",
            bottom - top
        );
    }
}
