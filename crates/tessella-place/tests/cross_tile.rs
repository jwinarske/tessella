//! Keeping a label's identity across tiles, against mbgl's own expectations.
//!
//! mbgl's `CrossTileSymbolLayerIndex.addBucket` states the exact id every symbol receives
//! through a sequence of four tiles at four zooms. Exact ids are what make it worth
//! transcribing: they pin the rounding, the tolerance, the order identities are handed out in,
//! and what a removed tile releases — none of which a count would notice.

use std::collections::BTreeSet;

use tessella_place::cross_tile::{CrossTileIndex, Symbol};
use tessella_tile::renderables::DataTileId;

fn symbols(entries: &[(&str, f32, f32)]) -> Vec<Symbol> {
    entries
        .iter()
        .map(|(key, x, y)| Symbol::new(*key, (*x, *y)))
        .collect()
}

fn ids(symbols: &[Symbol]) -> Vec<u32> {
    symbols.iter().map(|symbol| symbol.cross_tile_id).collect()
}

/// mbgl `CrossTileSymbolLayerIndex.addBucket`, id for id.
#[test]
fn identities_match_mbgl() {
    let mut index = CrossTileIndex::new();

    // The first tile: everything is new.
    let main = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut main_symbols = symbols(&[("Detroit", 1000.0, 1000.0), ("Toronto", 2000.0, 2000.0)]);
    assert!(index.add_bucket(main, 1, &mut main_symbols));
    assert_eq!(ids(&main_symbols), [1, 2]);

    // A child tile. Two of its labels are the parent's, two are not.
    let child = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut child_symbols = symbols(&[
        ("Detroit", 2000.0, 2000.0), // the parent's Detroit, at the child's coordinates
        ("Windsor", 2000.0, 2000.0), // same place, different text
        ("Toronto", 3000.0, 3000.0), // same text, different place
        ("Toronto", 4001.0, 4001.0), // the parent's Toronto, a shade off
    ]);
    assert!(index.add_bucket(child, 2, &mut child_symbols));
    assert_eq!(
        ids(&child_symbols),
        [1, 3, 4, 2],
        "matched, new by key, new by place, matched despite the offset"
    );

    // A parent tile matches its child's labels the same way.
    let parent = DataTileId::overscaled(5, 0, 5, 4, 4);
    let mut parent_symbols = symbols(&[("Detroit", 500.0, 500.0)]);
    assert!(index.add_bucket(parent, 3, &mut parent_symbols));
    assert_eq!(ids(&parent_symbols), [1]);

    // Everything but the first tile goes away.
    let current: BTreeSet<u32> = [1].into_iter().collect();
    assert!(index.remove_stale_buckets(&current));

    // A grandchild matches what survived and not what did not.
    let grandchild = DataTileId::overscaled(8, 0, 8, 32, 32);
    let mut grandchild_symbols =
        symbols(&[("Detroit", 4000.0, 4000.0), ("Windsor", 4000.0, 4000.0)]);
    assert!(index.add_bucket(grandchild, 4, &mut grandchild_symbols));
    assert_eq!(
        ids(&grandchild_symbols),
        [1, 5],
        "Detroit is still held by the first tile; Windsor's tile was removed"
    );
}

/// Adding the same bucket again changes nothing.
///
/// Placement runs every frame and most frames change no tiles. Re-indexing an unchanged bucket
/// would reassign identities that are already correct, and the fades keyed by them would all
/// restart.
#[test]
fn the_same_bucket_twice_is_not_a_change() {
    let mut index = CrossTileIndex::new();
    let tile = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut first = symbols(&[("Detroit", 1000.0, 1000.0)]);

    assert!(index.add_bucket(tile, 1, &mut first));
    assert_eq!(ids(&first), [1]);

    let mut again = symbols(&[("Detroit", 1000.0, 1000.0)]);
    assert!(
        !index.add_bucket(tile, 1, &mut again),
        "the same bucket is not a change"
    );
    assert_eq!(index.issued(), 1, "and no identity was handed out");
}

/// A tile replaced by a new bucket keeps its labels' identities.
///
/// The tile was rebuilt — a style change, a re-decode — but the labels are the same labels.
/// Failing to release the old bucket's claims first would leave the new one unable to take back
/// the ids its own labels had a moment ago, and every label in the tile would re-fade.
#[test]
fn a_replaced_bucket_keeps_its_identities() {
    let mut index = CrossTileIndex::new();
    let tile = DataTileId::overscaled(6, 0, 6, 8, 8);

    let mut first = symbols(&[("Detroit", 1000.0, 1000.0), ("Toronto", 2000.0, 2000.0)]);
    index.add_bucket(tile, 1, &mut first);
    assert_eq!(ids(&first), [1, 2]);

    let mut replaced = symbols(&[("Detroit", 1000.0, 1000.0), ("Toronto", 2000.0, 2000.0)]);
    assert!(
        index.add_bucket(tile, 2, &mut replaced),
        "a new bucket is a change"
    );
    assert_eq!(ids(&replaced), [1, 2], "the same labels keep the same ids");
    assert_eq!(index.issued(), 2, "nothing new was handed out");
}

/// One parent lends each label to exactly one child.
///
/// Four children of a parent all match against it. Without the guard they would all claim the
/// same label's identity, and four separate labels would share one fade — mbgl's issue #10844.
#[test]
fn a_parent_lends_each_label_once() {
    let mut index = CrossTileIndex::new();

    let parent = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut parent_symbols = symbols(&[("Main St", 4096.0, 4096.0)]);
    index.add_bucket(parent, 1, &mut parent_symbols);
    let lent = parent_symbols[0].cross_tile_id;

    // Two children whose labels both round onto the parent's position.
    let mut taken = Vec::new();
    for (index_of, (x, y)) in [(16u32, 16u32), (17, 16)].iter().enumerate() {
        let child = DataTileId::overscaled(7, 0, 7, *x, *y);
        let mut child_symbols = symbols(&[("Main St", 8192.0, 8192.0)]);
        index.add_bucket(child, 2 + index_of as u32, &mut child_symbols);
        taken.push(child_symbols[0].cross_tile_id);
    }

    assert!(
        taken.iter().filter(|id| **id == lent).count() <= 1,
        "the parent's label was lent to {taken:?}"
    );
    assert_ne!(taken[0], taken[1], "two labels, two identities");
}

/// Labels far apart with the same text are different labels.
#[test]
fn distance_separates_labels_with_the_same_text() {
    let mut index = CrossTileIndex::new();
    let tile = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut first = symbols(&[("Springfield", 1000.0, 1000.0)]);
    index.add_bucket(tile, 1, &mut first);

    let child = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut child_symbols = symbols(&[("Springfield", 7000.0, 7000.0)]);
    index.add_bucket(child, 2, &mut child_symbols);

    assert_ne!(
        child_symbols[0].cross_tile_id, first[0].cross_tile_id,
        "these are half a tile apart"
    );
}

/// Removing nothing is not a change, and identities survive.
#[test]
fn nothing_stale_is_not_a_change() {
    let mut index = CrossTileIndex::new();
    let tile = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut first = symbols(&[("Detroit", 1000.0, 1000.0)]);
    index.add_bucket(tile, 1, &mut first);

    let current: BTreeSet<u32> = [1].into_iter().collect();
    assert!(!index.remove_stale_buckets(&current));
    assert_eq!(index.len(), 1);
}

// The tests above reproduce mbgl's fixture exactly, and four separate mutations of the index
// survived all of them. The fixture is degenerate in ways it never had to care about: its tiles
// are perfectly aligned, its "slightly different location" differs by one tile unit, and its two
// children never contend for the same parent label. The tests below are built to discriminate.

/// A label's position includes the tile it is in, not just where it sits inside it.
///
/// Two children of one parent hold a label at the same *local* anchor and different ground
/// positions. Only the one that actually coincides with the parent's label is that label. Drop
/// the tile's origin from the calculation and both coincide, so whichever is offered first wins
/// — which is why this adds the non-matching child first.
#[test]
fn a_position_is_where_the_label_is_on_the_ground() {
    let mut index = CrossTileIndex::new();

    let parent = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut parent_symbols = symbols(&[("Detroit", 1000.0, 1000.0)]);
    index.add_bucket(parent, 1, &mut parent_symbols);
    let parent_id = parent_symbols[0].cross_tile_id;

    // The eastern child, holding a label at the same local anchor as the western one. On the
    // ground it is a whole child-tile away from the parent's label.
    let east = DataTileId::overscaled(7, 0, 7, 17, 16);
    let mut east_symbols = symbols(&[("Detroit", 2000.0, 2000.0)]);
    index.add_bucket(east, 2, &mut east_symbols);

    // The western child, whose label *is* the parent's.
    let west = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut west_symbols = symbols(&[("Detroit", 2000.0, 2000.0)]);
    index.add_bucket(west, 3, &mut west_symbols);

    assert_ne!(
        east_symbols[0].cross_tile_id, parent_id,
        "the eastern label is a different label and must not have taken the parent's identity"
    );
    assert_eq!(
        west_symbols[0].cross_tile_id, parent_id,
        "the western label is the parent's"
    );
}

/// Positions are rounded before they are compared, not compared exactly.
///
/// A label does not land on the same coordinate in a parent and a child: the anchor is quantised
/// differently and the geometry was simplified at a different zoom. mbgl's own fixture offsets
/// its label by one tile unit, which survives either way — this offsets it by fifty, which only
/// matches once both sides are rounded onto the shared grid.
#[test]
fn positions_are_rounded_before_they_are_compared() {
    let mut index = CrossTileIndex::new();

    let parent = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut parent_symbols = symbols(&[("Detroit", 1000.0, 1000.0)]);
    index.add_bucket(parent, 1, &mut parent_symbols);

    // 1950 rather than the exact 2000: well inside the four-pixel grid cell, well outside an
    // exact comparison.
    let child = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut child_symbols = symbols(&[("Detroit", 1950.0, 1950.0)]);
    index.add_bucket(child, 2, &mut child_symbols);

    assert_eq!(
        child_symbols[0].cross_tile_id, parent_symbols[0].cross_tile_id,
        "a label fifty units off is still the same label"
    );
}

/// A parent label straddling two children is lent to one of them, not both.
///
/// mbgl's issue #10844. Its own fixture never reaches this: two children of a parent can only
/// contend for one label when that label sits on the edge between them, which takes arranging.
/// Without the guard both children take the same identity and two separate labels share one
/// fade — they brighten and dim together, in different places.
#[test]
fn two_children_cannot_share_one_parent_label() {
    let mut index = CrossTileIndex::new();

    // The parent's label sits exactly on the seam between its western and eastern children.
    let parent = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut parent_symbols = symbols(&[("Seam", 4096.0, 1000.0)]);
    index.add_bucket(parent, 1, &mut parent_symbols);
    let parent_id = parent_symbols[0].cross_tile_id;

    // The western child holds it at its far edge; the eastern child at its near edge. Both round
    // onto the parent's grid cell.
    let west = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut west_symbols = symbols(&[("Seam", 8191.0, 2000.0)]);
    index.add_bucket(west, 2, &mut west_symbols);

    let east = DataTileId::overscaled(7, 0, 7, 17, 16);
    let mut east_symbols = symbols(&[("Seam", 0.0, 2000.0)]);
    index.add_bucket(east, 3, &mut east_symbols);

    let taken = [west_symbols[0].cross_tile_id, east_symbols[0].cross_tile_id];
    assert!(
        taken.contains(&parent_id),
        "one of them should have the parent's identity: {taken:?}"
    );
    assert_ne!(
        taken[0], taken[1],
        "two labels in two places must not share one identity"
    );
}

/// A removed tile releases the identities it had claimed.
///
/// Claims are struck off per zoom so a parent lends each label once. When the tile holding a
/// claim goes away, the claim has to go with it — otherwise the identity is reserved for a tile
/// that no longer exists, and the next label to legitimately want it gets a new one and re-fades.
#[test]
fn a_removed_tile_gives_its_identities_back() {
    let mut index = CrossTileIndex::new();

    let parent = DataTileId::overscaled(6, 0, 6, 8, 8);
    let mut parent_symbols = symbols(&[("Seam", 4096.0, 1000.0)]);
    index.add_bucket(parent, 1, &mut parent_symbols);
    let parent_id = parent_symbols[0].cross_tile_id;

    // The eastern child takes the parent's identity.
    let east = DataTileId::overscaled(7, 0, 7, 17, 16);
    let mut east_symbols = symbols(&[("Seam", 0.0, 2000.0)]);
    index.add_bucket(east, 2, &mut east_symbols);
    assert_eq!(east_symbols[0].cross_tile_id, parent_id);

    // It scrolls off and its bucket is dropped.
    let current: BTreeSet<u32> = [1].into_iter().collect();
    assert!(index.remove_stale_buckets(&current));

    // The western child now legitimately wants that identity.
    let west = DataTileId::overscaled(7, 0, 7, 16, 16);
    let mut west_symbols = symbols(&[("Seam", 8191.0, 2000.0)]);
    index.add_bucket(west, 3, &mut west_symbols);

    assert_eq!(
        west_symbols[0].cross_tile_id, parent_id,
        "the identity was reserved for a tile that no longer exists"
    );
}
