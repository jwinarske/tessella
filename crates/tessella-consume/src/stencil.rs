//! How a frame's clip masks share one stencil byte.
//!
//! mbgl repaints the stencil before each layer group, so at any moment the buffer holds one
//! group's tiles and nothing else. A consumer that draws the frame in one pass cannot: every
//! group's masks are in the buffer together. That is harmless while the groups agree on a grid and
//! it is not when they do not — one style clips a basemap against a single z14 tile drawn at 16
//! and its own source against six z16 tiles inside it, and a single value per pixel hands the
//! whole z14 tile to the wrong one.
//!
//! So the byte is shared out by canonical zoom: a field of bits for each zoom, a mask writing only
//! its own field, and geometry comparing only its own. Two zooms then coexist, and a finer tile
//! still replaces the ancestor standing in for it, by clearing that ancestor's field — but only
//! where one layer group asked for both. Across groups a finer tile is a different source, not a
//! replacement.
//!
//! # The budget is the caller's
//!
//! [`partition`] takes the number of bits it may spend, where the C++ this is ported from has it
//! as a constant. A consumer that draws in one pass may need to keep bits back for something else
//! — the Filament mirror reserves the top one for a 3D layer's draw-once mask, because it has no
//! depth attachment in its main pass and nothing else can bound how many times a translucent
//! surface blends. A consumer that owns a depth attachment has no such need and can spend all
//! eight. Baking one answer in would make this crate the wrong shape for the second caller.
//!
//! Pure, so it is tested without a GPU: the caller paints what this returns.

use alloc::collections::{BTreeMap, BTreeSet};

use tessella_capture_abi::envelope::TileId;

/// Every bit of the stencil byte, which is what a consumer with its own depth attachment can
/// spend on clipping.
pub const ALL_BITS: u32 = 8;

/// How one tile's mask is painted and how its geometry tests against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Assignment {
    /// Written by the mask, compared by the geometry.
    pub value: u8,
    /// The bits the geometry compares: its own zoom's field.
    pub read_mask: u8,
    /// The bits the mask writes: its own field, and those of any ancestor it replaces.
    pub write_mask: u8,
}

/// Every tile's assignment for one frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partition {
    /// Whether the fields fit in the budget.
    ///
    /// When they do not, each tile takes a value of its own in the whole byte — the scheme this
    /// replaces, and the right one for a single zoom. A caller holding bits back for its own use
    /// must read this before trusting them: the fallback spends all eight whatever the budget
    /// said, because a value per tile has nowhere else to go.
    pub partitioned: bool,
    /// A tile absent here has no mask; its geometry is left unclipped.
    pub tiles: BTreeMap<TileId, Assignment>,
}

/// The bits needed to count to `n`, since zero means "no mask". Eight at most.
fn bits_for(n: usize) -> u32 {
    let mut width = 0;
    while width < ALL_BITS && (1_usize << width) <= n {
        width += 1;
    }
    width
}

/// One place at one canonical zoom: the same ground however many drawn zooms name it.
type Cell = (i16, u32, u32);

/// Shares `bits` of the stencil byte among `tiles`.
///
/// `groups` is each layer group's own tile set, as the producer named it; a finer tile clears an
/// ancestor's field only when some group holds both. Every tile in a group is expected to be in
/// `tiles`; one that is not is ignored rather than panicking, since the stream is another
/// process's and a consumer that aborts on it is a consumer that can be crashed.
///
/// `bits` is clamped to [`ALL_BITS`]. Zero is legal and means everything falls back.
#[must_use]
pub fn partition(
    tiles: &BTreeSet<TileId>,
    groups: &BTreeMap<i32, BTreeSet<TileId>>,
    bits: u32,
) -> Partition {
    let budget = bits.min(ALL_BITS);
    let mut out = Partition {
        partitioned: true,
        tiles: BTreeMap::new(),
    };

    // One code per place at each zoom. The overscaled zoom is not part of it: two sources can name
    // the same canonical tile at different drawn zooms and they cover the same ground.
    let mut codes: BTreeMap<u8, BTreeMap<Cell, u8>> = BTreeMap::new();
    for tile in tiles {
        codes
            .entry(tile.z)
            .or_default()
            .insert((tile.wrap, tile.x, tile.y), 0);
    }

    // Coarsest zoom in the lowest bits. The order is a choice, not a requirement: the fields are
    // disjoint whichever way round they go.
    let mut fields: BTreeMap<u8, (u32, u8)> = BTreeMap::new();
    let mut offset = 0_u32;
    for (zoom, cells) in &mut codes {
        let width = bits_for(cells.len());
        if cells.len() > 255 || offset + width > budget {
            out.partitioned = false;
            break;
        }
        let mut code = 0_u8;
        for value in cells.values_mut() {
            code += 1;
            *value = code;
        }
        let mask = (((1_u32 << width) - 1) << offset) as u8;
        fields.insert(*zoom, (offset, mask));
        offset += width;
    }

    if !out.partitioned {
        // A value per tile in the whole byte, stopping short of 255 as the renderer always has.
        // This spends every bit, including any the caller meant to keep: `partitioned` is what
        // says so.
        // Values 1 through 254: zero means "no mask", and the renderer has always stopped short
        // of 255. Zipping against the range rather than counting says the same thing and ends the
        // walk at whichever runs out first, so a frame with more tiles than values leaves the
        // rest unassigned -- which is the existing behavior: unassigned means unclipped.
        for (tile, value) in tiles.iter().zip(1_u8..255) {
            out.tiles.insert(
                *tile,
                Assignment {
                    value,
                    read_mask: 0xFF,
                    write_mask: 0xFF,
                },
            );
        }
        return out;
    }

    for tile in tiles {
        let Some((shift, mask)) = fields.get(&tile.z).copied() else {
            continue;
        };

        // The fields of the coarser tiles this one lies inside, within any one group holding both.
        // Clearing them is what makes a child replace the parent standing in for it.
        let mut cleared = 0_u8;
        for members in groups.values() {
            if !members.contains(tile) {
                continue;
            }
            for other in members {
                if other.z >= tile.z || other.wrap != tile.wrap {
                    continue;
                }
                let down = u32::from(tile.z - other.z);
                if (tile.x >> down) == other.x
                    && (tile.y >> down) == other.y
                    && let Some((_, ancestor)) = fields.get(&other.z)
                {
                    cleared |= *ancestor;
                }
            }
        }

        let code = codes
            .get(&tile.z)
            .and_then(|cells| cells.get(&(tile.wrap, tile.x, tile.y)))
            .copied()
            .unwrap_or(0);
        out.tiles.insert(
            *tile,
            Assignment {
                value: code << shift,
                read_mask: mask,
                write_mask: mask | cleared,
            },
        );
    }
    out
}
