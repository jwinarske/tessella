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

/// A label whose road runs out fades, rather than switching off on the frame it happens.
///
/// Placement and the walk are two different refusals and a label needs both to draw. Only the
/// first reached the fades, so a road that ran out hid the text outright -- measured on one
/// orbiting pane, 33 labels went from full opacity to none in a single frame over 100 seconds,
/// every one on the frame its walk lost room, and all of them road names, because a road's shape
/// on screen is what a rotation changes.
///
/// The sequence here is the frame's own: place, walk, step. `write_line_positions` runs before
/// the step that reads its verdict, so the fade this asserts is this frame's rather than the
/// next one's.
#[test]
fn a_label_whose_road_runs_out_fades_rather_than_switching_off() {
    use tessella_orchestrate::symbols::FrameOptions;
    use tessella_place::feature::Padding;

    let (mut laid, mut buffers) = label(4, None);
    let road: [(f32, f32); 2] = [(190.0, 100.0), (210.0, 100.0)];
    laid.line = std::sync::Arc::new(road.to_vec());
    let labels = [frame_label(&laid, &road)];

    // A fifth of a fade a step, so the trajectory is readable rather than a single jump.
    let options = FrameOptions {
        padding: Padding::uniform(2.0),
        increment: 0.2,
        viewport: (1024.0, 768.0),
        ..FrameOptions::default()
    };

    let mut view = ViewSymbols::default();
    // Placed, and taken to full opacity, with a road long enough to walk.
    let long: Vec<(f32, f32)> = (0..40).map(|at| (at as f32 * 20.0, 100.0)).collect();
    let mut wide = laid.clone();
    wide.line = std::sync::Arc::new(long.clone());
    let placeable = [frame_label(&wide, &long)];
    for _ in 0..8 {
        // `frame` places and steps; the walk and the step after it are the rest of the
        // frame's own order. Two steps an iteration rather than one, which only makes the
        // trajectory below shorter.
        view.frame(&placeable, |point| point, &options);
        view.write_line_positions(
            &placeable,
            |point| point,
            |point| point,
            16.0,
            true,
            1.0,
            &mut buffers,
        );
        view.step(options.increment);
    }
    let full = view.opacity(1).expect("the label was placed").text.opacity;
    assert!(
        full > 0.99,
        "the label never reached full opacity, got {full}"
    );

    // Now the same label on a road that cannot hold it. Placement still wants it; the walk does
    // not find room.
    let mut trajectory = Vec::new();
    for _ in 0..4 {
        view.frame(&placeable, |point| point, &options);
        let without_room = view.write_line_positions(
            &labels,
            |point| point,
            |point| point,
            16.0,
            true,
            1.0,
            &mut buffers,
        );
        assert_eq!(without_room, vec![1], "the walk should have found no room");
        view.step(options.increment);
        trajectory.push(view.opacity(1).map_or(0.0, |joint| joint.text.opacity));
    }

    assert!(
        trajectory.windows(2).all(|pair| pair[1] <= pair[0]),
        "the opacity went back up: {trajectory:?}"
    );
    // The whole of it. A label that switches off reads `[0, 0, 0, 0]` here and one the fades
    // never hear about reads `[1, 1, 1, 1]`; a fade is the only thing that lands in between.
    assert!(
        trajectory.iter().any(|&at| at > 0.05 && at < 0.95),
        "the label switched off rather than fading: {trajectory:?}"
    );
    assert!(
        trajectory.last().is_some_and(|&at| at < 0.05),
        "the fade never finished: {trajectory:?}"
    );
}
