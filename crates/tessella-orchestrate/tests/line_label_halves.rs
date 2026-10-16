// SPDX-License-Identifier: BSD-2-Clause
//! Walking a line label's two shapings, and what counts as a label with no room.
//!
//! A label that permits vertical writing is shaped both ways and the two halves share one vertex
//! range. Splitting that walk in two is what lets the vertical half answer the question that is
//! its own — see `line_projection.rs` — and it is also where "this label did not fit" stopped
//! meaning what it says.

use tessella_layout::symbol_bucket::{LaidOut, SizeRange, SymbolBuffers, Vertical};
use tessella_orchestrate::symbols::{FrameLabel, ViewSymbols};

/// A road running left to right at y = 100.
const ROAD: [(f32, f32); 2] = [(0.0, 100.0), (400.0, 100.0)];

/// A label of `glyphs` quads on `ROAD`, anchored in the middle of it.
fn label(glyphs: usize, vertical: Option<Vertical>) -> (LaidOut, SymbolBuffers) {
    let mut buffers = SymbolBuffers::default();
    for index in 0..glyphs {
        #[allow(clippy::cast_precision_loss)]
        let offset = (index as f32 - glyphs as f32 / 2.0) * 20.0;
        buffers.add_quad(
            (200.0, 100.0),
            [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)],
            (offset, 0.0),
            (0, 0, 10, 10),
            SizeRange { min: 0.0, max: 0.0 },
            true,
            1.0,
        );
    }
    let laid = LaidOut {
        pending: 0,
        anchor: (200.0, 100.0),
        extent: (-10.0, 10.0, -40.0, 40.0),
        glyphs,
        content_margins: None,
        segment: 0,
        line: alloc_line(),
        vertical,
        vertices: 0..buffers.vertices.len(),
    };
    (laid, buffers)
}

fn alloc_line() -> std::sync::Arc<Vec<(f32, f32)>> {
    std::sync::Arc::new(ROAD.to_vec())
}

fn frame_label<'a>(laid: &LaidOut, line: &'a [(f32, f32)]) -> FrameLabel<'a> {
    FrameLabel {
        cross_tile_id: 1,
        laid_out: laid.clone(),
        icon: None,
        variable: &[],
        variable_offset: [0.0, 0.0],
        variable_radial: false,
        line,
        perspective: 1.0,
        glyph_reach: None,
    }
}

fn walk(laid: &LaidOut, buffers: &mut SymbolBuffers) -> Vec<u32> {
    let mut view = ViewSymbols::default();
    let labels = [frame_label(laid, &ROAD)];
    view.write_line_positions(
        &labels,
        |point| point,
        |point| point,
        16.0,
        true,
        1.0,
        buffers,
    )
}

/// A symbol with an icon and no text is not a label that would not fit.
///
/// It reaches the walk with nothing to place, and reporting it as having no room is how the
/// caller hides its icon. Twenty oneway arrows went off a street tile that way, and the example
/// suite is what caught it: the five-camera sweep has no icon-only line symbol in it.
#[test]
fn a_symbol_with_no_glyphs_is_not_a_label_without_room() {
    let (laid, mut buffers) = label(0, None);
    assert!(
        walk(&laid, &mut buffers).is_empty(),
        "a symbol with nothing to place was reported as having no room"
    );
}

/// And one whose road runs out before its glyphs do still is.
#[test]
fn a_label_longer_than_its_road_has_no_room() {
    let (mut laid, mut buffers) = label(4, None);
    // A road twenty units long, where the label wants eighty.
    let short: [(f32, f32); 2] = [(190.0, 100.0), (210.0, 100.0)];
    laid.line = std::sync::Arc::new(short.to_vec());
    let mut view = ViewSymbols::default();
    let labels = [frame_label(&laid, &short)];
    let without_room = view.write_line_positions(
        &labels,
        |point| point,
        |point| point,
        16.0,
        true,
        1.0,
        &mut buffers,
    );
    assert_eq!(without_room, vec![1], "the label fits a road it cannot");
}
