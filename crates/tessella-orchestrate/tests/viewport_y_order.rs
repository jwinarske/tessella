// SPDX-License-Identifier: BSD-2-Clause
//! Which of two overlapping symbols is drawn last, and why the camera decides it.
//!
//! mbgl draws a layer's symbols lowest-on-screen last wherever overlap is allowed, so a label in
//! front of another covers it rather than being covered. `SymbolBucket::sortFeatures` rewrites the
//! index buffer to reference the unchanged vertices in that order, and `Placement` calls it every
//! pass with the bearing.
//!
//! Nothing in this build did, which is tessella#428: a scene of two overlapping icons read 4,776
//! of 786,432 against the oracle, and 0 at the one bearing where the sorted order happens to be
//! the source's. These are the arithmetic that fixes it — the pixels are `zorder_p` in the sweep.

use tessella_layout::symbol_bucket::{LaidOut, Run, SizeRange, SymbolBuffers};
use tessella_orchestrate::symbols::sort_by_viewport_y;

/// One quad at `anchor`, for a label that is `pending` in the layout's own order.
fn instance(buffers: &mut SymbolBuffers, anchor: (f32, f32), pending: usize) -> LaidOut {
    let before = buffers.vertices.len();
    buffers.add_quad(
        anchor,
        [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)],
        (0.0, 0.0),
        (0, 0, 10, 10),
        SizeRange { min: 0.0, max: 0.0 },
        false,
        1.0,
    );
    LaidOut {
        pending,
        anchor,
        extent: (-5.0, 5.0, -5.0, 5.0),
        glyphs: 1,
        content_margins: None,
        segment: 0,
        line: std::sync::Arc::new(Vec::new()),
        vertical: None,
        vertices: before..buffers.vertices.len(),
    }
}

/// The first vertex each of a buffer's quads is drawn from, in index order.
///
/// What the sort rearranges, read back the way a draw reads it: two triangles per quad and six
/// indices, so the first index of each group of six names the quad.
fn drawn_order(buffers: &SymbolBuffers) -> Vec<u16> {
    buffers.indices.chunks(6).map(|group| group[0]).collect()
}

/// A single run, which is what every single-stack bucket has.
fn one_run(buffers: &mut SymbolBuffers) {
    buffers.runs = vec![Run {
        fonts: Vec::new(),
        indices: 0..buffers.indices.len(),
    }];
}

/// North up, the symbol lower on the screen is drawn last.
///
/// Tile y grows downward, so "lower on the screen" is the larger y — and mbgl sorts *ascending* by
/// the rotated anchor, which puts it last. The instances are built in the opposite order so that
/// creation order and screen order disagree: a build that did not sort would answer `[0, 4]`.
#[test]
fn the_lower_symbol_is_drawn_last() {
    let mut buffers = SymbolBuffers::default();
    let laid = vec![
        instance(&mut buffers, (100.0, 900.0), 0),
        instance(&mut buffers, (100.0, 100.0), 1),
    ];
    one_run(&mut buffers);
    assert_eq!(
        drawn_order(&buffers),
        [0, 4],
        "creation order, before the sort"
    );

    sort_by_viewport_y(&mut buffers, &laid, 0.0);
    assert_eq!(
        drawn_order(&buffers),
        [4, 0],
        "the instance at y 900 is the lower one and has to be drawn last"
    );
    assert_eq!(
        buffers.vertices.len(),
        8,
        "the vertices do not move: what is rewritten is the index buffer"
    );
    assert_eq!(
        buffers.runs[0].indices,
        0..12,
        "and the run still covers it"
    );
}

/// The order is the bearing's, not the tile's.
///
/// At a bearing of half a turn the screen is upside down, so the anchor with the larger tile y is
/// the *higher* one and is drawn first. This is the case that says the sort reads the camera:
/// a build that sorted by tile y alone passes the test above and fails this one.
#[test]
fn turning_the_map_turns_the_order() {
    let mut buffers = SymbolBuffers::default();
    let laid = vec![
        instance(&mut buffers, (100.0, 900.0), 0),
        instance(&mut buffers, (100.0, 100.0), 1),
    ];
    one_run(&mut buffers);

    sort_by_viewport_y(&mut buffers, &laid, core::f64::consts::PI);
    assert_eq!(
        drawn_order(&buffers),
        [0, 4],
        "upside down, the anchor at y 900 is the higher one"
    );
}

/// Anchors that round to the same screen row are decided by the feature, in reverse.
///
/// mbgl's tie-break is `a.getDataFeatureIndex() > b.getDataFeatureIndex()` — the *later* feature
/// first — and the rounding is `lround`, so anchors less than half a tile unit apart tie rather
/// than being ordered by the last bits of a float. Both halves are read here: the anchors differ
/// by a third of a unit, which rounds together, and the answer is the feature order reversed.
#[test]
fn a_tie_is_decided_by_the_feature_in_reverse() {
    let mut buffers = SymbolBuffers::default();
    let laid = vec![
        instance(&mut buffers, (100.0, 500.0), 0),
        instance(&mut buffers, (100.0, 500.3), 1),
        instance(&mut buffers, (100.0, 499.8), 2),
    ];
    one_run(&mut buffers);

    sort_by_viewport_y(&mut buffers, &laid, 0.0);
    assert_eq!(
        drawn_order(&buffers),
        [8, 4, 0],
        "three anchors that round to one row, drawn latest feature first"
    );
}

/// A bucket whose indices are in more than one run is left alone.
///
/// mbgl declines the same case in its own words — its sort "doesn't sort across text and icon
/// segments so don't try" — and the shape here is the same: a run is the stretch of indices
/// belonging to one font stack, each drawn by its own drawable against its own atlas, so an order
/// across them is an order no draw can honor.
#[test]
fn more_than_one_run_declines_the_sort() {
    let mut buffers = SymbolBuffers::default();
    let laid = vec![
        instance(&mut buffers, (100.0, 900.0), 0),
        instance(&mut buffers, (100.0, 100.0), 1),
    ];
    buffers.runs = vec![
        Run {
            fonts: vec!["one".to_string()],
            indices: 0..6,
        },
        Run {
            fonts: vec!["two".to_string()],
            indices: 6..12,
        },
    ];

    sort_by_viewport_y(&mut buffers, &laid, 0.0);
    assert_eq!(
        drawn_order(&buffers),
        [0, 4],
        "two stacks, so the order is left as it was"
    );
}

/// A buffer holding quads no instance claims is left alone.
///
/// The sort rebuilds the index list from the instances' own ranges, so a quad outside all of them
/// would be dropped — and a drawable that drew fewer quads than the frame before is a worse answer
/// than an unsorted one. Nothing in the tree builds such a buffer; this is the guard that says so
/// if something starts to.
#[test]
fn an_unclaimed_quad_declines_the_sort() {
    let mut buffers = SymbolBuffers::default();
    let laid = vec![instance(&mut buffers, (100.0, 900.0), 0)];
    // A second quad belonging to no instance.
    let orphan = instance(&mut buffers, (100.0, 100.0), 1);
    drop(orphan);
    one_run(&mut buffers);

    sort_by_viewport_y(&mut buffers, &laid, 0.0);
    assert_eq!(
        drawn_order(&buffers),
        [0, 4],
        "one claimed quad of two, so nothing is reordered"
    );
}
