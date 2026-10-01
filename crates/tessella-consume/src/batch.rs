//! Collapsing a frame's draw order into batches.
//!
//! §11.7 asks a consumer for geometry batching, merged by (layer, shader permutation, texture
//! set). R-9 is why that is not a group-by.
//!
//! # Why adjacency, and not grouping
//!
//! R-9: merging drawables into multi-primitive renderables assumes layer-contiguous draw order and
//! stencil-resolved order within a layer, and translucent layers with cross-tile sort keys —
//! symbol fade, line sort-key — can violate it. A group-by would happily merge the first and third
//! entries of a run and silently move the second behind them.
//!
//! So only *adjacent* entries collapse. A run extends while the merge key holds and breaks the
//! moment it does not, which makes the painter order this produces identical to the order it was
//! given — not approximately, but by construction, because nothing is ever reordered. That is
//! R-9's "collapse only within (layer, pass) groups the order proves contiguous", and the proof is
//! that adjacency is the only thing ever collapsed.
//!
//! Symbols are excluded from collapse entirely, which R-9 also asks for until it is measured:
//! every symbol drawable is its own batch.
//!
//! # What it is not responsible for
//!
//! It does not upload, own or outlive anything. A batch names geometry ids the caller has already
//! seen, and holding the bytes behind them is the caller's on the terms §11.7 states.

use alloc::vec::Vec;

use tessella_capture_abi::BuiltIn;
use tessella_capture_abi::envelope::{GeometryId, OrderEntry, TextureRef};

/// What a drawable must agree on to share a renderable with its neighbor.
///
/// Layer and pass come from the order; the rest describe the program and its bindings. Two
/// drawables differing in any of them need different state between the draws, which is the whole
/// reason not to merge them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// Layer group, from the order entry.
    pub layer_index: u32,
    /// Pass, from the order entry. Part of the key because R-9 scopes collapse to (layer, pass).
    pub pass: u8,
    /// Shader family.
    pub builtin_shader: i32,
    /// Which data-driven variant of that family, which is a different program.
    pub permutation_key: u64,
    /// The textures bound, in the order the shader declares them.
    pub texture_refs: Vec<TextureRef>,
}

/// A contiguous run of drawables issued as one renderable, in painter order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// What every drawable in the run agrees on.
    pub key: Key,
    /// Geometry ids, in draw order.
    pub geometries: Vec<GeometryId>,
    /// The consolidated-buffer slot for each, parallel to `geometries`.
    ///
    /// Parallel rather than folded into the key: `ubo_index` is assigned per pass from the view's
    /// own draw order, so it differs between drawables a renderable merges and is exactly what the
    /// vertex stage needs per draw.
    pub ubo_indexes: Vec<u32>,
}

impl Batch {
    /// Whether this batch holds more than one drawable.
    #[must_use]
    pub fn merged(&self) -> bool {
        self.geometries.len() > 1
    }
}

/// What a drawable contributes to the key, which the caller resolves from its announcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program<'a> {
    /// Shader family.
    pub builtin_shader: i32,
    /// Data-driven variant.
    pub permutation_key: u64,
    /// Textures bound.
    pub texture_refs: &'a [TextureRef],
}

/// Whether a family is excluded from collapse.
///
/// Symbols, per R-9, until the collapse is measured: their cross-tile sort keys are the case that
/// can violate the contiguity merging assumes, and a fade reorders them within a layer.
#[must_use]
pub fn collapsible(builtin_shader: i32) -> bool {
    !matches!(
        BuiltIn::from_repr(builtin_shader),
        Some(
            BuiltIn::SymbolIconShader
                | BuiltIn::SymbolSDFShader
                | BuiltIn::SymbolTextAndIconShader
                | BuiltIn::CustomSymbolIconShader
        )
    )
}

/// Collapses a frame's order into batches.
///
/// `program` resolves an entry's geometry into what decides its program and bindings, and returns
/// `None` for a geometry the consumer has not been given — which is not an error: an order can
/// name geometry whose announcement has not arrived, and the entry is skipped rather than drawn
/// with whatever the previous one bound.
#[must_use]
pub fn collapse<'a>(
    entries: &[OrderEntry],
    program: impl FnMut(GeometryId) -> Option<Program<'a>>,
) -> Vec<Batch> {
    let mut out = Vec::new();
    collapse_into(entries, &mut out, program);
    out
}

/// Collapses into a buffer the caller keeps, which is what makes a steady state allocate nothing.
///
/// `out` is cleared first. Its capacity is retained between frames, so after the first few the
/// only cost is refilling it.
pub fn collapse_into<'a>(
    entries: &[OrderEntry],
    out: &mut Vec<Batch>,
    mut program: impl FnMut(GeometryId) -> Option<Program<'a>>,
) {
    out.clear();
    for entry in entries {
        let Some(program) = program(entry.geometry) else {
            continue;
        };
        let key = Key {
            layer_index: entry.layer_index,
            pass: entry.pass.bits(),
            builtin_shader: program.builtin_shader,
            permutation_key: program.permutation_key,
            texture_refs: program.texture_refs.to_vec(),
        };

        // Extend the open run, or start a new one. Only the last batch is ever considered: an
        // earlier one matching is a batch the order put something else behind, and reaching back
        // to it is exactly the reordering R-9 forbids.
        let extend = out
            .last()
            .is_some_and(|last| last.key == key && collapsible(program.builtin_shader));
        if extend {
            let last = out.last_mut().expect("checked above");
            last.geometries.push(entry.geometry);
            last.ubo_indexes.push(entry.ubo_index);
        } else {
            out.push(Batch {
                key,
                geometries: alloc::vec![entry.geometry],
                ubo_indexes: alloc::vec![entry.ubo_index],
            });
        }
    }
}
