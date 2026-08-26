//! Which part of a tile a *better* tile has already covered — mbgl's `algorithm::updateTileMasks`.
//!
//! # What a mask is for, and what it is not for
//!
//! While a view is loading, its renderable set is not one zoom level. A tile whose children have
//! not arrived is drawn in their place, and as each child lands the parent should stop drawing
//! the part that child now covers. Without that, the parent's low-resolution pixels are drawn
//! *over* or *under* the child's — which for a translucent raster layer is the same picture
//! blended with itself, and for an opaque one is a race between two draws at the same depth.
//!
//! A mask is the set of sub-tiles a tile should still draw, stated **relative** to that tile: the
//! whole tile is `{(0, 0, 0)}` and an entirely covered tile is the empty set. mbgl's own comment
//! works the example — a z2 tile with a z3 and a z4 descendant loaded masks down to five
//! rectangles of three different sizes.
//!
//! # It is a raster mechanism, not a stencil one
//!
//! This is worth stating plainly because the plan said the opposite for a while. `TileMask` is
//! consumed by exactly two things in mbgl — `RasterBucket::setMask` and `HillshadeBucket::setMask`
//! — and both turn it into *geometry*: a quad per entry at `EXTENT >> z`. The clipping-mask path
//! never sees it. `PaintParameters::renderTileClippingMasks` builds one `ClipUBO` per render tile
//! carrying a matrix and a stencil reference, and draws a full-tile quad for each; there is no
//! quadrant anywhere in it.
//!
//! So a mask is not a field the capture stream is missing. It is geometry, and geometry already
//! travels. What it does mean is that a masked raster bucket is *different geometry* from an
//! unmasked one, so it takes a different id and is shared between views that agree on the mask
//! rather than between all views of the tile (§5.3).
//!
//! # Why a settled frame never has one
//!
//! Every tile of a settled cover is at the same zoom, so no tile is an ancestor of another and
//! every mask is the whole tile. That is why no golden capture contains one: the state exists
//! only while tiles are arriving, and a probe that renders until it settles renders past it.

use crate::cover::TileCoord;

/// One rectangle of a tile that is still to be drawn, relative to the tile itself.
///
/// `z` is the *depth below* the tile rather than a zoom, so `(0, 0, 0)` is the whole tile and
/// `(1, 1, 0)` is its top-right quarter. Relative rather than absolute because the mask is used
/// to build geometry in tile-local units, where the tile's own address does not appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaskEntry {
    /// Levels below the tile.
    pub z: u8,
    /// Column at that level, within the tile.
    pub x: u32,
    /// Row at that level, within the tile.
    pub y: u32,
}

/// The whole tile, undivided.
pub const WHOLE_TILE: MaskEntry = MaskEntry { z: 0, x: 0, y: 0 };

/// The deepest a mask may describe.
///
/// A mask entry's extent is `EXTENT >> z`, so past thirteen levels a sub-tile is less than one
/// unit across and its quad is degenerate. The recursion is bounded by the renderable set in
/// practice — it only descends where a descendant actually exists — and this is the backstop for
/// a set that is not one this build produced.
pub const MAX_DEPTH: u8 = 13;

/// Whether `id` sits inside `parent`.
///
/// mbgl's `CanonicalTileID::isChildOf`, including its `parent.z == 0` short-circuit, which is
/// there to avoid a shift by 32 rather than to state that everything is a child of the root.
/// Callers reach it only after ruling out equality, which is what keeps the shortcut honest.
fn is_child_of(id: &TileCoord, parent: &TileCoord) -> bool {
    if id.wrap != parent.wrap {
        return false;
    }
    if parent.z == 0 {
        return true;
    }
    parent.z < id.z
        && parent.x == (id.x >> (id.z - parent.z))
        && parent.y == (id.y >> (id.z - parent.z))
}

/// The four tiles one level below.
fn children(id: &TileCoord) -> [TileCoord; 4] {
    let (z, x, y) = (id.z + 1, id.x * 2, id.y * 2);
    [
        TileCoord {
            z,
            x,
            y,
            wrap: id.wrap,
        },
        TileCoord {
            z,
            x,
            y: y + 1,
            wrap: id.wrap,
        },
        TileCoord {
            z,
            x: x + 1,
            y,
            wrap: id.wrap,
        },
        TileCoord {
            z,
            x: x + 1,
            y: y + 1,
            wrap: id.wrap,
        },
    ]
}

/// Adds to `mask` the parts of `reference` that no tile in `candidates` covers.
///
/// `candidates` is the tail of the renderable set after `root`, in the set's own order. The
/// recursion descends only where a descendant is actually present, so its depth is bounded by
/// the set rather than by the zoom range — a tile with no descendants terminates on the first
/// pass over the list.
fn compute(
    root: &TileCoord,
    reference: &TileCoord,
    candidates: &[TileCoord],
    mask: &mut Vec<MaskEntry>,
) {
    for (index, id) in candidates.iter().enumerate() {
        // This part of the tile is covered by a better tile, so it contributes nothing.
        if id == reference {
            return;
        }

        if is_child_of(id, reference) {
            // Something below this reference is covered, so split and ask again. The tail
            // starts *at* the item found rather than after it: the same child can cover more
            // than one of the four quarters being asked about.
            if reference.z.saturating_sub(root.z) >= MAX_DEPTH {
                return;
            }
            for child in children(reference) {
                compute(root, &child, &candidates[index..], mask);
            }
            return;
        }
    }

    // Nothing covers it, so the whole of this reference is drawn. Stated relative to the root,
    // which is what makes a mask reusable as tile-local geometry.
    let depth = reference.z - root.z;
    mask.push(MaskEntry {
        z: depth,
        x: reference.x - (root.x << depth),
        y: reference.y - (root.y << depth),
    });
}

/// The mask for each renderable tile, in the order given.
///
/// `renderables` must be sorted the way mbgl sorts its renderable map — by `(wrap, z, x, y)` —
/// because the algorithm relies on it twice: a tile's descendants can only appear *after* it, and
/// the next wrap's tiles are found by a bound rather than by a test. A caller handing over an
/// unsorted set gets masks that are wrong rather than an error, so [`sorted`] is provided to make
/// the requirement cheap to satisfy.
#[must_use]
pub fn update_tile_masks(renderables: &[TileCoord]) -> Vec<Vec<MaskEntry>> {
    let mut masks = Vec::with_capacity(renderables.len());
    for (index, id) in renderables.iter().enumerate() {
        // Only the tail can hold descendants, and only up to the end of this wrap: a tile in
        // another world copy is never inside this one however its numbers compare.
        let tail = &renderables[index + 1..];
        let end = tail
            .iter()
            .position(|other| other.wrap != id.wrap)
            .unwrap_or(tail.len());

        let mut mask = Vec::new();
        compute(id, id, &tail[..end], &mut mask);
        mask.sort_unstable();
        masks.push(mask);
    }
    masks
}

/// A copy of `tiles` in the order [`update_tile_masks`] needs.
#[must_use]
pub fn sorted(tiles: &[TileCoord]) -> Vec<TileCoord> {
    let mut out = tiles.to_vec();
    // `TileCoord`'s derived order is `(z, x, y, wrap)`, and this needs wrap outermost.
    out.sort_unstable_by_key(|tile| (tile.wrap, tile.z, tile.x, tile.y));
    out
}

/// Whether a mask means "draw all of it".
///
/// The case worth naming: mbgl keeps a whole-tile raster bucket on shared full-extent buffers
/// rather than building a one-quad mask, and an empty mask means the tile draws *nothing* rather
/// than everything. The two are easy to confuse in a caller and impossible to confuse here.
#[must_use]
pub fn is_whole_tile(mask: &[MaskEntry]) -> bool {
    mask == [WHOLE_TILE]
}
