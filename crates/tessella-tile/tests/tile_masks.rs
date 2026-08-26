//! Clip masks, against `test/algorithm/update_tile_masks.test.cpp` case for case.
//!
//! The expectations are mbgl's own, tile for tile. They are worth having exactly because the
//! algorithm is one where a plausible implementation is wrong in a way no picture reveals: a mask
//! that is one rectangle too large draws a strip of the parent's low-resolution pixels over the
//! child's, which reads as a slightly soft seam rather than as a bug in a set operation.

use tessella_tile::cover::TileCoord;
use tessella_tile::mask::{MaskEntry, is_whole_tile, sorted, update_tile_masks};

/// A tile at wrap zero.
fn tile(z: u8, x: u32, y: u32) -> TileCoord {
    TileCoord { z, x, y, wrap: 0 }
}

/// A mask entry, in mbgl's `CanonicalTileID{z, x, y}` order.
fn entry(z: u8, x: u32, y: u32) -> MaskEntry {
    MaskEntry { z, x, y }
}

/// Runs the algorithm over a renderable set and checks each tile's mask.
///
/// The input is sorted here rather than assumed, because that is what the caller has to do and a
/// test that hand-sorted its fixtures would not exercise it.
fn check(expected: &[(TileCoord, Vec<MaskEntry>)]) {
    let tiles: Vec<TileCoord> = expected.iter().map(|(tile, _)| *tile).collect();
    let ordered = sorted(&tiles);
    let masks = update_tile_masks(&ordered);

    for (tile, mask) in ordered.iter().zip(&masks) {
        let want = expected
            .iter()
            .find(|(candidate, _)| candidate == tile)
            .map(|(_, want)| want)
            .unwrap_or_else(|| panic!("{tile:?} is not in the fixture"));
        assert_eq!(mask, want, "mask for {tile:?}");
    }
}

/// mbgl `UpdateTileMasks.NoChildren`: a tile with nothing below it draws all of itself.
///
/// The case every settled frame is, which is why no golden capture contains anything else: the
/// tiles of a settled cover are all at one zoom, so none is an ancestor of another.
#[test]
fn a_tile_with_no_children_draws_whole() {
    check(&[(tile(0, 0, 0), vec![entry(0, 0, 0)])]);
    check(&[(tile(4, 3, 8), vec![entry(0, 0, 0)])]);
    check(&[
        (tile(1, 0, 0), vec![entry(0, 0, 0)]),
        (tile(1, 1, 1), vec![entry(0, 0, 0)]),
    ]);
    // A deeper tile that is *not* a descendant changes nothing: 2/2/3 is not inside 1/0/0.
    check(&[
        (tile(1, 0, 0), vec![entry(0, 0, 0)]),
        (tile(2, 2, 3), vec![entry(0, 0, 0)]),
    ]);
}

/// mbgl `UpdateTileMasks.ParentAndFourChildren`: a fully covered parent draws nothing.
///
/// An *empty* mask, which is the opposite of the whole-tile mask and the pair most easily
/// confused in a caller. A build that read empty as "no restriction" would draw the parent at
/// full size underneath four children that already cover it — every pixel rendered twice, and
/// for a translucent raster layer visibly darker.
#[test]
fn a_parent_covered_by_four_children_draws_nothing() {
    check(&[
        (tile(0, 0, 0), vec![]),
        (tile(1, 0, 0), vec![entry(0, 0, 0)]),
        (tile(1, 0, 1), vec![entry(0, 0, 0)]),
        (tile(1, 1, 0), vec![entry(0, 0, 0)]),
        (tile(1, 1, 1), vec![entry(0, 0, 0)]),
    ]);
}

/// mbgl `UpdateTileMasks.OneChild`: one child loaded, three quarters of the parent still drawn.
///
/// The case the whole mechanism exists for. It is what a view looks like for the fraction of a
/// second after the first child of a zoom-in arrives, and it is the state a probe that renders
/// until it settles renders straight past.
#[test]
fn a_parent_with_one_child_draws_the_other_three_quarters() {
    check(&[
        (
            tile(0, 0, 0),
            vec![entry(1, 0, 1), entry(1, 1, 0), entry(1, 1, 1)],
        ),
        (tile(1, 0, 0), vec![entry(0, 0, 0)]),
    ]);
}

/// mbgl `UpdateTileMasks.Complex`, first case: descendants three levels deep.
///
/// Six rectangles of three different sizes for one tile, which is what says the recursion
/// descends rather than stopping at quadrants. An implementation that only ever split one level
/// would produce four entries here and cover the wrong ground with three of them.
#[test]
fn a_mask_descends_as_far_as_the_descendants_go() {
    check(&[
        (
            tile(0, 0, 0),
            vec![
                entry(1, 0, 1),
                entry(1, 1, 0),
                entry(2, 2, 3),
                entry(2, 3, 2),
                entry(3, 6, 7),
                entry(3, 7, 6),
            ],
        ),
        (tile(1, 0, 0), vec![entry(0, 0, 0)]),
        (tile(2, 2, 2), vec![entry(0, 0, 0)]),
        (tile(3, 7, 7), vec![entry(0, 0, 0)]),
        (tile(3, 6, 6), vec![entry(0, 0, 0)]),
    ]);
}

/// mbgl `UpdateTileMasks.Complex`, second case: one very deep descendant.
///
/// A single z4 tile under a z0 one, and the mask is twelve rectangles — three at each of four
/// levels, the classic quadtree difference. It is also the case that pins the *relative*
/// addressing: every entry is stated against the root, so `4/4/5` under `0/0/0` is `(4, 4, 5)`
/// and not the absolute tile it came from.
#[test]
fn one_deep_descendant_masks_three_rectangles_per_level() {
    check(&[
        (
            tile(0, 0, 0),
            vec![
                entry(1, 0, 1),
                entry(1, 1, 0),
                entry(1, 1, 1),
                entry(2, 0, 0),
                entry(2, 0, 1),
                entry(2, 1, 0),
                entry(3, 2, 3),
                entry(3, 3, 2),
                entry(3, 3, 3),
                entry(4, 4, 5),
                entry(4, 5, 4),
                entry(4, 5, 5),
            ],
        ),
        (tile(4, 4, 4), vec![entry(0, 0, 0)]),
    ]);
}

/// mbgl `UpdateTileMasks.Complex`, third case: real tile numbers at street zoom.
///
/// The same shapes at z12 to z14 with the six- and seven-digit indices a real view produces.
/// Worth having beside the small cases because the relative subtraction is `x - (root.x << depth)`,
/// and at these magnitudes an implementation that shifted the wrong operand or used the absolute
/// index produces a plausible-looking mask that is nowhere near the tile.
#[test]
fn the_relative_subtraction_holds_at_real_tile_numbers() {
    check(&[
        (
            tile(12, 1028, 1456),
            vec![entry(1, 1, 1), entry(2, 3, 0), entry(2, 3, 1)],
        ),
        (
            tile(13, 2056, 2912),
            vec![entry(1, 0, 1), entry(1, 1, 0), entry(1, 1, 1)],
        ),
        (
            tile(13, 2056, 2913),
            vec![entry(1, 0, 0), entry(1, 1, 0), entry(1, 1, 1)],
        ),
        (tile(14, 4112, 5824), vec![entry(0, 0, 0)]),
        (tile(14, 4112, 5827), vec![entry(0, 0, 0)]),
        (tile(14, 4114, 5824), vec![entry(0, 0, 0)]),
        (tile(14, 4114, 5825), vec![entry(0, 0, 0)]),
    ]);
}

/// A tile in another world copy is never inside this one.
///
/// mbgl bounds the child search at the next wrap rather than testing each candidate, and the two
/// agree only because the set is sorted with wrap outermost. A viewport straddling the
/// antimeridian holds the same canonical tile in two copies, and a mask that treated one as a
/// descendant of the other would blank a tile that is fully visible.
#[test]
fn a_tile_in_another_world_copy_is_not_a_descendant() {
    let tiles = [
        TileCoord {
            z: 0,
            x: 0,
            y: 0,
            wrap: 0,
        },
        TileCoord {
            z: 1,
            x: 0,
            y: 0,
            wrap: 1,
        },
    ];
    let ordered = sorted(&tiles);
    let masks = update_tile_masks(&ordered);

    for mask in &masks {
        assert!(is_whole_tile(mask), "{masks:?}");
    }

    // And within one wrap the same pair *is* a parent and child, so the guard above is the wrap
    // and not something else.
    let same_wrap = sorted(&[tile(0, 0, 0), tile(1, 0, 0)]);
    let masks = update_tile_masks(&same_wrap);
    assert_eq!(masks[0].len(), 3, "{masks:?}");
}

/// The whole-tile mask and the empty mask are distinguishable.
#[test]
fn a_whole_tile_is_not_an_empty_mask() {
    assert!(is_whole_tile(&[entry(0, 0, 0)]));
    assert!(!is_whole_tile(&[]), "an empty mask draws nothing");
    assert!(!is_whole_tile(&[entry(1, 0, 0)]));
}

/// An empty renderable set produces no masks rather than panicking on the tail slice.
#[test]
fn an_empty_set_masks_nothing() {
    assert!(update_tile_masks(&[]).is_empty());
}
