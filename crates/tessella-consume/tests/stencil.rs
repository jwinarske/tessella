//! `partition`, checked against a stencil buffer painted the way the GPU paints it.
//!
//! Each mask writes `(old & !write_mask) | (value & write_mask)` over its tile, coarsest first, and
//! a drawable passes where `(stencil & read_mask) == (value & read_mask)` — Vulkan's `REPLACE`
//! under a write mask and `EQUAL` under a compare mask. So the questions here are asked of pixels
//! rather than of the numbers: does geometry survive where it should, and only there.
//!
//! Ported from `stencil_partition_probe.cc` in the Filament mirror, including its simulation, so
//! the two implementations answer the same questions and a disagreement is visible as one.

use std::collections::{BTreeMap, BTreeSet};

use tessella_capture_abi::envelope::TileId;
use tessella_consume::stencil::{ALL_BITS, Partition, partition};

/// Every tile is painted at this zoom's resolution, one cell a pixel.
const FINE: u8 = 6;

/// A tile, at its canonical zoom unless it is drawn at a coarser one.
fn tile(z: u8, x: u32, y: u32) -> TileId {
    TileId {
        x,
        y,
        z,
        overscaled_z: z,
        wrap: 0,
    }
}

/// The same ground, stood in for at a finer drawn zoom.
fn overscaled(z: u8, x: u32, y: u32, drawn: u8) -> TileId {
    TileId {
        x,
        y,
        z,
        overscaled_z: drawn,
        wrap: 0,
    }
}

/// The painted stencil, one cell per pixel at [`FINE`].
type Buffer = BTreeMap<(u32, u32), u8>;

fn for_cells(t: &TileId, mut f: impl FnMut(u32, u32)) {
    let shift = u32::from(FINE - t.z);
    for x in (t.x << shift)..((t.x + 1) << shift) {
        for y in (t.y << shift)..((t.y + 1) << shift) {
            f(x, y);
        }
    }
}

/// Paints every mask, in the order the renderer does.
fn paint(p: &Partition) -> Buffer {
    let mut buffer = Buffer::new();
    for (t, a) in &p.tiles {
        for_cells(t, |x, y| {
            let cell = buffer.entry((x, y)).or_default();
            *cell = (*cell & !a.write_mask) | (a.value & a.write_mask);
        });
    }
    buffer
}

/// Whether `t`'s geometry passes the stencil at every cell of `where_`.
fn passes_over(p: &Partition, buffer: &Buffer, t: &TileId, where_: &TileId) -> bool {
    let Some(a) = p.tiles.get(t) else {
        return false;
    };
    let mut all = true;
    for_cells(where_, |x, y| {
        let value = buffer.get(&(x, y)).copied().unwrap_or(0);
        all = all && (value & a.read_mask) == (a.value & a.read_mask);
    });
    all
}

/// Whether `t`'s geometry fails the stencil at every cell of `where_`.
fn fails_over(p: &Partition, buffer: &Buffer, t: &TileId, where_: &TileId) -> bool {
    let a = &p.tiles[t];
    let mut none = true;
    for_cells(where_, |x, y| {
        let value = buffer.get(&(x, y)).copied().unwrap_or(0);
        none = none && (value & a.read_mask) != (a.value & a.read_mask);
    });
    none
}

fn group(members: &[TileId]) -> BTreeMap<i32, BTreeSet<TileId>> {
    BTreeMap::from([(1, members.iter().copied().collect())])
}

/// Two groups at two zooms, the finer tiles inside the coarser one: both draw everywhere.
///
/// The case a single value per pixel gets wrong — a basemap clipped against one tile two levels
/// coarser than it is drawn, and another source against tiles at the drawn zoom.
#[test]
fn two_sources_at_two_zooms_both_draw() {
    let basemap = overscaled(2, 1, 1, 4);
    let geojson: BTreeSet<TileId> = (4..7)
        .flat_map(|x| (4..6).map(move |y| tile(4, x, y)))
        .collect();
    let mut all = geojson.clone();
    all.insert(basemap);
    let groups = BTreeMap::from([(10, BTreeSet::from([basemap])), (119, geojson.clone())]);
    let p = partition(&all, &groups, ALL_BITS);
    let buffer = paint(&p);

    assert!(p.partitioned, "two zooms fit in the byte");
    assert!(
        passes_over(&p, &buffer, &basemap, &basemap),
        "the basemap draws over its whole tile, under the other source's masks too"
    );
    for t in &geojson {
        assert!(
            passes_over(&p, &buffer, t, t),
            "each tile draws over itself"
        );
    }
    assert!(
        fails_over(&p, &buffer, &tile(4, 4, 4), &tile(4, 5, 4)),
        "a tile is still clipped against its neighbor"
    );
}

/// A parent standing in for a child, in one group: the child's ground is the child's.
#[test]
fn a_child_replaces_its_parent() {
    let parent = tile(2, 1, 1);
    let child = tile(3, 2, 2);
    let tiles = BTreeSet::from([parent, child]);
    let p = partition(&tiles, &group(&[parent, child]), ALL_BITS);
    let buffer = paint(&p);

    assert!(p.partitioned, "a parent and child fit in the byte");
    assert!(
        passes_over(&p, &buffer, &child, &child),
        "the child draws over itself"
    );
    assert!(
        fails_over(&p, &buffer, &parent, &child),
        "and the parent does not draw where the child replaced it"
    );
}

/// Across groups, a finer tile is a different source rather than a replacement.
#[test]
fn a_finer_tile_of_another_group_does_not_replace() {
    let coarse = tile(2, 1, 1);
    let fine = tile(3, 2, 2);
    let tiles = BTreeSet::from([coarse, fine]);
    let groups = BTreeMap::from([(1, BTreeSet::from([coarse])), (2, BTreeSet::from([fine]))]);
    let p = partition(&tiles, &groups, ALL_BITS);
    let buffer = paint(&p);

    assert!(
        passes_over(&p, &buffer, &coarse, &fine),
        "the coarse tile still draws where the other group's finer tile is"
    );
    assert!(
        passes_over(&p, &buffer, &fine, &fine),
        "and so does the finer one"
    );
}

/// Neighbors at one zoom clip each other, which is what the masks are for.
#[test]
fn neighbors_at_one_zoom_clip_each_other() {
    let left = tile(4, 4, 4);
    let right = tile(4, 5, 4);
    let tiles = BTreeSet::from([left, right]);
    let p = partition(&tiles, &group(&[left, right]), ALL_BITS);
    let buffer = paint(&p);

    assert!(passes_over(&p, &buffer, &left, &left));
    assert!(fails_over(&p, &buffer, &left, &right));
    assert!(fails_over(&p, &buffer, &right, &left));
}

/// One canonical tile named at two drawn zooms is one place, and gets one code.
#[test]
fn one_place_at_two_drawn_zooms_shares_a_code() {
    let stood = overscaled(3, 1, 1, 3);
    let over = overscaled(3, 1, 1, 4);
    let tiles = BTreeSet::from([stood, over]);
    let groups = BTreeMap::from([(1, BTreeSet::from([stood])), (2, BTreeSet::from([over]))]);
    let p = partition(&tiles, &groups, ALL_BITS);
    assert_eq!(
        p.tiles[&stood].value, p.tiles[&over].value,
        "one canonical tile, one value"
    );
}

/// Too many tiles at one zoom fall back to a value each.
#[test]
fn too_many_tiles_fall_back() {
    let tiles: BTreeSet<TileId> = (0..20)
        .flat_map(|x| (0..16).map(move |y| tile(6, x, y)))
        .collect();
    let p = partition(&tiles, &group(&[]), ALL_BITS);
    assert!(!p.partitioned, "three hundred tiles do not partition");
    for a in p.tiles.values() {
        assert_eq!(
            (a.read_mask, a.write_mask),
            (0xFF, 0xFF),
            "the fallback compares the whole byte"
        );
    }
}

/// Two zooms whose fields together overflow the byte also fall back.
#[test]
fn fields_that_overflow_fall_back() {
    let mut tiles: BTreeSet<TileId> = (0..16)
        .flat_map(|x| (0..8).map(move |y| tile(8, x, y)))
        .collect();
    tiles.insert(tile(2, 0, 0));
    let p = partition(&tiles, &group(&[]), ALL_BITS);
    assert!(!p.partitioned, "nine bits of fields do not partition");
}

/// A caller keeping a bit back gets fields that leave it alone.
///
/// The Filament mirror reserves the top bit for a 3D layer's draw-once mask, because it has no
/// depth attachment in its main pass. A field sharing that bit would be written by the mask and
/// read by the clip, and a building would blend once, twice or not at all depending on which tile
/// it stood on.
#[test]
fn a_reserved_bit_is_left_alone() {
    // Seven bits exactly: 32 places at one zoom is six — `bits_for` counts to n past zero, so 32
    // wants 33 codes — and a second zoom's single tile takes the seventh.
    let mut tiles: BTreeSet<TileId> = (0..8)
        .flat_map(|x| (0..4).map(move |y| tile(6, x, y)))
        .collect();
    tiles.insert(tile(2, 0, 0));

    let p = partition(&tiles, &group(&[]), ALL_BITS - 1);
    assert!(p.partitioned, "seven bits of fields still partition");
    for a in p.tiles.values() {
        assert_eq!(a.value & 0x80, 0, "no value takes the reserved bit");
        assert_eq!(a.read_mask & 0x80, 0, "no read mask takes it");
        assert_eq!(a.write_mask & 0x80, 0, "no write mask takes it");
    }

    // And the same tiles with the whole byte to spend still partition, so the case above is the
    // budget biting rather than the tiles being unreasonable.
    assert!(partition(&tiles, &group(&[]), ALL_BITS).partitioned);
}

/// A budget one bit tighter refuses what seven bits accepted.
///
/// Pins the arithmetic in both directions: with six bits the same tiles fall back, so the seven in
/// the test above is the boundary rather than a number that happens to pass.
#[test]
fn a_tighter_budget_falls_back() {
    let mut tiles: BTreeSet<TileId> = (0..8)
        .flat_map(|x| (0..4).map(move |y| tile(6, x, y)))
        .collect();
    tiles.insert(tile(2, 0, 0));
    assert!(!partition(&tiles, &group(&[]), ALL_BITS - 2).partitioned);
}

/// And the fallback spends every bit, whatever the budget said.
///
/// The other half of the contract a caller holding bits back relies on: it must read
/// `partitioned` before trusting its reservation, because a value per tile has nowhere else to go.
#[test]
fn the_fallback_spends_the_whole_byte() {
    let tiles: BTreeSet<TileId> = (0..16)
        .flat_map(|x| (0..8).map(move |y| tile(8, x, y)))
        .collect();
    let p = partition(&tiles, &group(&[]), ALL_BITS - 1);
    assert!(!p.partitioned, "a zoom wanting eight bits falls back");
    for a in p.tiles.values() {
        assert_eq!(
            (a.read_mask, a.write_mask),
            (0xFF, 0xFF),
            "the fallback spends the whole byte, reserved bit included"
        );
    }
}

/// A group naming a tile the frame does not have is ignored, not a panic.
///
/// The stream is another process's. A consumer that aborts on a malformed one is a consumer that
/// can be crashed by the thing it is meant to survive.
#[test]
fn a_group_naming_an_absent_tile_is_survivable() {
    let present = tile(4, 4, 4);
    let absent = tile(4, 9, 9);
    let tiles = BTreeSet::from([present]);
    let p = partition(&tiles, &group(&[present, absent]), ALL_BITS);
    assert!(p.partitioned);
    assert!(p.tiles.contains_key(&present));
    assert!(
        !p.tiles.contains_key(&absent),
        "an absent tile gets no assignment"
    );
}
