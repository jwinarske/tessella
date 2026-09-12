//! A symbol bucket whose labels span two font stacks becomes two drawables.
//!
//! # What a drawable can say
//!
//! A glyph's quad carries its rectangle in the atlas its stack was packed into, and a drawable
//! binds one texture. A layer whose `text-font` is data-driven puts labels from two stacks in one
//! bucket -- a place label in medium above a population and regular below it, which is how the
//! Protomaps style writes one -- and binding either atlas draws the other stack's labels with the
//! wrong rectangles. Letters from the wrong script, which is what it looks like on a map.
//!
//! mbgl splits the bucket. This splits the *indices*: the vertices are one buffer laid out in runs
//! that each belong to one stack, and a drawable takes the runs that share its atlas.

use tessella_layout::symbol_bucket::{Run, SizeRange, SymbolBuffers};

fn stack(name: &str) -> Vec<String> {
    vec![name.to_string()]
}

/// A quad's worth of vertices, so `append` has something to shift indices against.
fn piece(glyphs: usize) -> SymbolBuffers {
    let mut buffers = SymbolBuffers::default();
    for index in 0..glyphs {
        let at = index as i16 * 4;
        buffers.add_quad(
            (f32::from(at), 0.0),
            [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)],
            (0.0, 0.0),
            (0, 0, 4, 4),
            SizeRange::constant(16.0),
            true,
            1.0,
        );
    }
    buffers
}

/// Consecutive runs of one stack merge; a different stack opens a new one.
#[test]
fn runs_record_the_stack_their_indices_belong_to() {
    let mut buffers = SymbolBuffers::default();
    buffers.append_run(&piece(2), &stack("Regular"));
    buffers.append_run(&piece(1), &stack("Regular"));
    buffers.append_run(&piece(3), &stack("Medium"));
    buffers.append_run(&piece(1), &stack("Regular"));

    assert_eq!(
        buffers.runs,
        vec![
            Run {
                fonts: stack("Regular"),
                indices: 0..18
            },
            Run {
                fonts: stack("Medium"),
                indices: 18..36
            },
            Run {
                fonts: stack("Regular"),
                indices: 36..42
            },
        ],
        "adjacent runs of one stack merge, and a stack that returns opens a third"
    );
    assert_eq!(
        buffers.indices.len(),
        42,
        "six indices a glyph, seven glyphs"
    );
    // Every index is covered exactly once: a drawable per stack draws the whole buffer between
    // them, and a gap would be glyphs nothing draws.
    let covered: usize = buffers.runs.iter().map(|run| run.indices.len()).sum();
    assert_eq!(covered, buffers.indices.len());
}

/// A buffer nobody recorded runs for reports none, and the whole-buffer encoder still draws it.
#[test]
fn a_plain_append_records_nothing() {
    let mut buffers = SymbolBuffers::default();
    buffers.append(&piece(2));
    assert!(buffers.runs.is_empty());
    assert_eq!(buffers.indices.len(), 12);
}
