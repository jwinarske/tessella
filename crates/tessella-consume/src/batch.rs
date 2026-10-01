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
use core::ops::Range;

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
    /// Pass, from the order entry. Collapse is scoped to (layer, pass).
    pub pass: u8,
    /// Shader family.
    pub builtin_shader: i32,
    /// Which data-driven variant of that family, which is a different program.
    pub permutation_key: u64,
    /// The textures bound, as a range into the flat array.
    textures: Range<u32>,
}

impl Key {
    /// Whether two keys name the same program and bindings.
    fn same(&self, other: &Self, textures: &[TextureRef]) -> bool {
        let mine = &textures[self.textures.start as usize..self.textures.end as usize];
        let theirs = &textures[other.textures.start as usize..other.textures.end as usize];
        self.layer_index == other.layer_index
            && self.pass == other.pass
            && self.builtin_shader == other.builtin_shader
            && self.permutation_key == other.permutation_key
            && mine == theirs
    }
}

/// A frame's batches, held flat.
///
/// One `Vec` per field rather than a `Vec` of batches each owning its own. That is a measurement
/// rather than a preference: `Vec::clear` keeps the outer capacity while dropping every element,
/// so a batch owning two `Vec`s returns both buffers on every clear and takes them again on every
/// refill. At the quad's entry count that was 15,000 allocations a frame. Flat, the same frame
/// allocates nothing once the buffers have grown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batches {
    keys: Vec<Key>,
    /// Each batch's run, as a range into `geometries` and `ubo_indexes`.
    runs: Vec<Range<u32>>,
    geometries: Vec<GeometryId>,
    ubo_indexes: Vec<u32>,
    textures: Vec<TextureRef>,
}

/// One batch, borrowed from the flat arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Batch<'a> {
    /// What every drawable in the run agrees on.
    pub key: &'a Key,
    /// The textures bound, in the order the shader declares them.
    pub textures: &'a [TextureRef],
    /// Geometry ids, in draw order.
    pub geometries: &'a [GeometryId],
    /// The consolidated-buffer slot for each, parallel to `geometries`.
    ///
    /// Parallel rather than folded into the key: `ubo_index` is assigned per pass from the view's
    /// own draw order, so it differs between drawables a renderable merges.
    pub ubo_indexes: &'a [u32],
}

impl Batch<'_> {
    /// Whether this batch holds more than one drawable.
    #[must_use]
    pub fn merged(&self) -> bool {
        self.geometries.len() > 1
    }
}

impl Batches {
    /// No batches.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many batches a frame collapsed to.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the frame draws nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// One batch.
    #[must_use]
    pub fn get(&self, at: usize) -> Option<Batch<'_>> {
        let key = self.keys.get(at)?;
        let run = self.runs.get(at)?;
        let (from, to) = (run.start as usize, run.end as usize);
        Some(Batch {
            key,
            textures: &self.textures[key.textures.start as usize..key.textures.end as usize],
            geometries: &self.geometries[from..to],
            ubo_indexes: &self.ubo_indexes[from..to],
        })
    }

    /// Every batch, in painter order.
    pub fn iter(&self) -> impl Iterator<Item = Batch<'_>> + '_ {
        (0..self.len()).filter_map(|at| self.get(at))
    }

    /// Empties it, keeping every buffer's capacity.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.runs.clear();
        self.geometries.clear();
        self.ubo_indexes.clear();
        self.textures.clear();
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

/// Collapses a frame's draw order into `out`.
///
/// `out` is cleared first and its capacity kept, so a steady state allocates nothing.
///
/// `program` resolves an entry's geometry into what decides its program and bindings, and returns
/// `None` for a geometry the consumer has not been given -- which is not an error: an order can
/// name geometry whose announcement has not arrived, and the entry is skipped rather than drawn
/// with whatever the previous one bound.
pub fn collapse_into<'a>(
    entries: &[OrderEntry],
    out: &mut Batches,
    mut program: impl FnMut(GeometryId) -> Option<Program<'a>>,
) {
    out.clear();
    for entry in entries {
        let Some(program) = program(entry.geometry) else {
            continue;
        };

        // The textures go in first so the key can name them by range, and are rolled back when
        // this entry extends the open run instead of starting one.
        let from = out.textures.len() as u32;
        out.textures.extend_from_slice(program.texture_refs);
        let key = Key {
            layer_index: entry.layer_index,
            pass: entry.pass.bits(),
            builtin_shader: program.builtin_shader,
            permutation_key: program.permutation_key,
            textures: from..out.textures.len() as u32,
        };

        // Only the last batch is ever considered: an earlier one matching is a batch the order put
        // something else behind, and reaching back to it is the reordering R-9 forbids.
        let extend = collapsible(program.builtin_shader)
            && out
                .keys
                .last()
                .is_some_and(|last| last.same(&key, &out.textures));
        if extend {
            out.textures.truncate(from as usize);
            out.geometries.push(entry.geometry);
            out.ubo_indexes.push(entry.ubo_index);
            let run = out.runs.last_mut().expect("a run beside the key");
            run.end += 1;
        } else {
            let at = out.geometries.len() as u32;
            out.geometries.push(entry.geometry);
            out.ubo_indexes.push(entry.ubo_index);
            out.keys.push(key);
            out.runs.push(at..at + 1);
        }
    }
}
