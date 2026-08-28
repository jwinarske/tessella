//! Laying a layer's labels out into one tile's buffers.
//!
//! The glue that was living in a test until now. What it has to get right is the seam between
//! labels: they share one buffer per layer per tile — which is what the golden shows mbgl doing —
//! so a second label's indices must reach its own vertices, and a label whose glyphs are still
//! arriving must not take the others down with it.

use tessella_glyph::atlas::{Atlas, Rect};
use tessella_glyph::pbf::{self, Glyph, Metrics, Range};
use tessella_layout::symbol_bucket::{Glyphs, Label, SymbolOptions, build_symbols};

const GLYPHS: &[u8] = include_bytes!("../../../tests/glyph-fixtures/TestFont/0-255.pbf");

/// A font with its glyphs packed into an atlas.
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

fn label(text: &str, anchor: (f32, f32)) -> Label {
    Label {
        pending: 0,
        sections: vec![tessella_layout::symbol::Section { text: text.to_string(), scale: 1.0 }],
        text: text.to_string(),
        anchor,
    }
}

/// A label becomes four vertices and six indices per glyph.
#[test]
fn a_label_becomes_quads() {
    let font = Font::new("Alpha");
    let (buffers, laid) = build_symbols(
        &[label("Alpha", (1000.0, 2000.0))],
        &font,
        &SymbolOptions::default(),
    );

    assert_eq!(buffers.glyphs(), 5);
    assert_eq!(buffers.vertices.len(), 20);
    assert_eq!(buffers.indices.len(), 30);
    assert_eq!(laid.len(), 1);
    assert_eq!(laid[0].glyphs, 5);
    assert_eq!(laid[0].anchor, (1000.0, 2000.0));
}

/// Two labels share one buffer, and the second's indices reach its own vertices.
///
/// One buffer per layer per tile is what the golden shows mbgl emitting — its twelve-glyph
/// drawable is two labels, not two drawables. A second label indexing from zero would draw the
/// first label's glyphs twice and leave its own invisible.
#[test]
fn two_labels_share_a_buffer_without_sharing_vertices() {
    let font = Font::new("AlphaBravo");
    let (buffers, laid) = build_symbols(
        &[
            label("Alpha", (1000.0, 1000.0)),
            label("Bravo", (2000.0, 2000.0)),
        ],
        &font,
        &SymbolOptions::default(),
    );

    assert_eq!(buffers.glyphs(), 10, "five glyphs each");
    assert_eq!(
        laid.iter().map(|entry| entry.glyphs).collect::<Vec<_>>(),
        [5, 5]
    );

    // The second label's first triangle starts where the first label's vertices ended.
    assert_eq!(buffers.indices[0], 0);
    assert_eq!(
        buffers.indices[30], 20,
        "the sixth quad indexes from twenty"
    );

    // And every index reaches a vertex that exists.
    let count = buffers.vertices.len();
    assert!(
        buffers
            .indices
            .iter()
            .all(|index| usize::from(*index) < count)
    );
}

/// Each label's vertices carry its own anchor.
///
/// The anchor is per label and the buffer is per layer, so a builder that wrote one anchor for
/// the whole buffer would stack every label of a tile in one place.
#[test]
fn each_label_keeps_its_own_anchor() {
    let font = Font::new("AlphaBravo");
    let (buffers, _) = build_symbols(
        &[
            label("Alpha", (1000.0, 1000.0)),
            label("Bravo", (2000.0, 2000.0)),
        ],
        &font,
        &SymbolOptions::default(),
    );

    assert_eq!(buffers.vertices[0].pos_offset[0], 1000);
    assert_eq!(
        buffers.vertices[19].pos_offset[0], 1000,
        "still the first label"
    );
    assert_eq!(buffers.vertices[20].pos_offset[0], 2000, "the second label");
    assert_eq!(buffers.vertices[39].pos_offset[1], 2000);
}

/// A label whose glyphs are not packed yet draws the ones that are.
///
/// A map that waited for a whole font before drawing anything would show nothing during a pan
/// into new text. Drawing part of a label is what mbgl does too, and it is far better than
/// drawing none of it.
#[test]
fn a_partly_packed_label_draws_what_it_has() {
    // Only the glyphs of "Alp" are in the atlas.
    let font = Font::new("Alp");
    let (buffers, laid) = build_symbols(
        &[label("Alpha", (0.0, 0.0))],
        &font,
        &SymbolOptions::default(),
    );

    assert_eq!(buffers.glyphs(), 3, "A, l and p");
    assert_eq!(laid[0].glyphs, 3);
    assert!(
        laid[0].extent.3 > laid[0].extent.2,
        "and it still measured the whole label for collision"
    );
}

/// A label of nothing but unknown codepoints draws nothing and breaks nothing.
#[test]
fn an_unknown_label_draws_nothing() {
    let font = Font::new("Alpha");
    let (buffers, laid) = build_symbols(
        &[label("\u{4e2d}\u{6587}", (0.0, 0.0))],
        &font,
        &SymbolOptions::default(),
    );

    assert!(buffers.is_empty());
    assert_eq!(laid[0].glyphs, 0);
}

/// Letter spacing widens the label and reaches the extent placement uses.
#[test]
fn letter_spacing_widens_the_label() {
    let font = Font::new("Alpha");
    let tight = build_symbols(
        &[label("Alpha", (0.0, 0.0))],
        &font,
        &SymbolOptions::default(),
    );
    let loose = build_symbols(
        &[label("Alpha", (0.0, 0.0))],
        &font,
        &SymbolOptions {
            letter_spacing: 4.0,
            ..SymbolOptions::default()
        },
    );

    let width =
        |laid: &[tessella_layout::symbol_bucket::LaidOut]| laid[0].extent.3 - laid[0].extent.2;
    assert!(
        width(&loose.1) > width(&tight.1),
        "{} vs {}",
        width(&loose.1),
        width(&tight.1)
    );
    assert_eq!(
        loose.0.glyphs(),
        tight.0.glyphs(),
        "the same glyphs, further apart"
    );
}

/// Wrapping a label makes it taller and puts every glyph in the same buffer.
#[test]
fn a_wrapped_label_stays_one_buffer() {
    let font = Font::new("Alpha Bravo Charlie");
    let (buffers, laid) = build_symbols(
        &[label("Alpha Bravo Charlie", (0.0, 0.0))],
        &font,
        &SymbolOptions {
            max_width_ems: 4.0,
            ..SymbolOptions::default()
        },
    );

    // Seventeen letters and two spaces; the spaces have no glyph.
    assert_eq!(buffers.glyphs(), 17);
    assert_eq!(laid[0].glyphs, 17);
    let height = laid[0].extent.1 - laid[0].extent.0;
    assert!(height > 24.0, "wrapped to more than one line: {height}");
}

/// No labels is an empty buffer, not a panic.
#[test]
fn no_labels_is_an_empty_buffer() {
    let font = Font::new("");
    let (buffers, laid) = build_symbols(&[], &font, &SymbolOptions::default());
    assert!(buffers.is_empty());
    assert!(laid.is_empty());
}
