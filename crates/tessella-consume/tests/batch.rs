//! Collapsing a draw order, and the orders it must refuse to collapse.
//!
//! The merge key is §11.7's: layer, shader permutation, texture set, scoped to a pass. What the
//! tests are really about is R-9 — that collapse never reorders, which is a property of adjacency
//! rather than of the key.

use tessella_capture_abi::RenderPass;
use tessella_capture_abi::envelope::{GeometryId, OrderEntry, TextureId, TextureRef};
use tessella_capture_abi::generated::mbgl_enums::BuiltIn;
use tessella_consume::batch::{Program, collapse, collapsible};

/// An order entry; `layer` and `pass` are what scope a collapse.
fn entry(geometry: u64, layer: u32, ubo: u32) -> OrderEntry {
    OrderEntry {
        geometry: GeometryId(geometry),
        draw_priority: 0,
        layer_index: layer,
        sub_layer_index: 0,
        ubo_index: ubo,
        pass: RenderPass::TRANSLUCENT,
        _pad: [0; 3],
    }
}

const FILL: i32 = BuiltIn::FillShader as i32;
const LINE: i32 = BuiltIn::LineShader as i32;
const SYMBOL: i32 = BuiltIn::SymbolSDFShader as i32;

/// Every geometry is the same program with no textures: the most collapsible order there is.
fn one_program(_: GeometryId) -> Option<Program<'static>> {
    Some(Program {
        builtin_shader: FILL,
        permutation_key: 0,
        texture_refs: &[],
    })
}

/// A run of like drawables becomes one batch, in the order given.
#[test]
fn a_contiguous_run_collapses_into_one() {
    let order = [entry(1, 0, 0), entry(2, 0, 1), entry(3, 0, 2)];
    let batches = collapse(&order, one_program);

    assert_eq!(batches.len(), 1, "one renderable");
    assert!(batches[0].merged());
    assert_eq!(
        batches[0].geometries,
        [GeometryId(1), GeometryId(2), GeometryId(3)],
        "in the order given"
    );
    assert_eq!(
        batches[0].ubo_indexes,
        [0, 1, 2],
        "each keeps its own slot in the layer's buffer"
    );
}

/// A different layer breaks the run, because collapse is scoped to one.
#[test]
fn a_layer_change_breaks_the_run() {
    let order = [entry(1, 0, 0), entry(2, 1, 0), entry(3, 0, 1)];
    let batches = collapse(&order, one_program);
    assert_eq!(
        batches.len(),
        3,
        "three layers' worth of state between them"
    );
}

/// So does a different program, and so does a different texture set.
#[test]
fn a_program_or_texture_change_breaks_the_run() {
    let order = [entry(1, 0, 0), entry(2, 0, 1), entry(3, 0, 2)];
    let batches = collapse(&order, |id| {
        Some(Program {
            builtin_shader: if id == GeometryId(2) { LINE } else { FILL },
            permutation_key: 0,
            texture_refs: &[],
        })
    });
    assert_eq!(
        batches.len(),
        3,
        "a different family needs a different pipeline"
    );

    static ATLAS: [TextureRef; 1] = [TextureRef {
        texture: TextureId(7),
        slot: 0,
        filter: 0,
    }];
    let batches = collapse(&order, |id| {
        Some(Program {
            builtin_shader: FILL,
            permutation_key: 0,
            texture_refs: if id == GeometryId(2) { &ATLAS } else { &[] },
        })
    });
    assert_eq!(
        batches.len(),
        3,
        "a different binding needs a different descriptor"
    );
}

/// The permutation is part of the key: one family's data-driven variants are different programs.
#[test]
fn a_permutation_change_breaks_the_run() {
    let order = [entry(1, 0, 0), entry(2, 0, 1)];
    let batches = collapse(&order, |id| {
        Some(Program {
            builtin_shader: FILL,
            permutation_key: u64::from(id == GeometryId(2)),
            texture_refs: &[],
        })
    });
    assert_eq!(batches.len(), 2);
}

/// **R-9.** A matching entry on the far side of a different one does not reach back to it.
///
/// This is the whole of why collapse is adjacency and not a group-by. Grouping would put 1 and 3
/// in one renderable and issue 2 after both, which moves 2 in front of 3 in the painter order the
/// producer specified. The picture that produces is a translucent layer composited in the wrong
/// sequence, which is not an error anywhere — it just looks wrong.
#[test]
fn a_match_across_a_break_does_not_reach_back() {
    let order = [entry(1, 0, 0), entry(2, 1, 0), entry(3, 0, 1)];
    let batches = collapse(&order, one_program);

    assert_eq!(batches.len(), 3, "not two");
    let order_out: Vec<GeometryId> = batches
        .iter()
        .flat_map(|batch| batch.geometries.iter().copied())
        .collect();
    assert_eq!(
        order_out,
        [GeometryId(1), GeometryId(2), GeometryId(3)],
        "the order out is the order in"
    );
}

/// Whatever the order, collapsing never changes the sequence of drawables.
///
/// The property R-9 actually asks for, asserted over a shape with every kind of break in it
/// rather than inferred from the cases above.
#[test]
fn collapse_never_reorders() {
    let order = [
        entry(1, 0, 0),
        entry(2, 0, 1),
        entry(3, 1, 0),
        entry(4, 1, 1),
        entry(5, 0, 2),
        entry(6, 2, 0),
        entry(7, 2, 1),
    ];
    let batches = collapse(&order, |id| {
        Some(Program {
            builtin_shader: if id.0 % 3 == 0 { LINE } else { FILL },
            permutation_key: id.0 % 2,
            texture_refs: &[],
        })
    });
    let order_out: Vec<GeometryId> = batches
        .iter()
        .flat_map(|batch| batch.geometries.iter().copied())
        .collect();
    let given: Vec<GeometryId> = order.iter().map(|entry| entry.geometry).collect();
    assert_eq!(order_out, given, "collapse is a grouping, never a sort");
}

/// Symbols are excluded from collapse, which R-9 asks for until it is measured.
///
/// Their cross-tile sort keys are the case that can violate the contiguity merging assumes: a fade
/// reorders them within a layer, so a run that looked contiguous when it was built is not when it
/// is drawn.
#[test]
fn symbols_do_not_collapse() {
    let order = [entry(1, 0, 0), entry(2, 0, 1), entry(3, 0, 2)];
    let batches = collapse(&order, |_| {
        Some(Program {
            builtin_shader: SYMBOL,
            permutation_key: 0,
            texture_refs: &[],
        })
    });
    assert_eq!(batches.len(), 3, "every symbol drawable is its own batch");
    assert!(batches.iter().all(|batch| !batch.merged()));
}

/// Named rather than left to the key, so the exclusion is checkable on its own.
#[test]
fn every_symbol_family_is_excluded() {
    for family in [
        BuiltIn::SymbolIconShader,
        BuiltIn::SymbolSDFShader,
        BuiltIn::SymbolTextAndIconShader,
        BuiltIn::CustomSymbolIconShader,
    ] {
        assert!(!collapsible(family as i32), "{family:?} collapses");
    }
    for family in [
        BuiltIn::FillShader,
        BuiltIn::LineShader,
        BuiltIn::CircleShader,
    ] {
        assert!(collapsible(family as i32), "{family:?} does not collapse");
    }
}

/// An entry whose geometry the consumer has not been given is skipped, not drawn.
///
/// An order can name geometry whose announcement has not arrived. Drawing it would bind whatever
/// the previous entry left bound, which is one drawable wearing another's program.
#[test]
fn an_unknown_geometry_is_skipped() {
    let order = [entry(1, 0, 0), entry(2, 0, 1), entry(3, 0, 2)];
    let batches = collapse(&order, |id| {
        (id != GeometryId(2)).then_some(Program {
            builtin_shader: FILL,
            permutation_key: 0,
            texture_refs: &[],
        })
    });

    assert_eq!(batches.len(), 1, "the two known entries still collapse");
    assert_eq!(
        batches[0].geometries,
        [GeometryId(1), GeometryId(3)],
        "and the unknown one is absent rather than guessed at"
    );
}

/// An empty order is an empty frame, not a panic.
#[test]
fn an_empty_order_collapses_to_nothing() {
    assert!(collapse(&[], one_program).is_empty());
}
