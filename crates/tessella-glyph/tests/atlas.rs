//! Packing glyphs into the shared atlas.
//!
//! The packer has no mbgl unit test to diff against, so what is asserted here are its
//! invariants — nothing overlaps, nothing leaves the texture, a repeat ask is free — and the
//! two properties the rest of the pipeline depends on: that the reported rectangle keeps a
//! pixel of the glyph's own padding, and that a full packer refuses rather than overwriting.
//!
//! The packer refusing is not the atlas refusing: an `Atlas` grows and re-packs, which is what
//! mbgl does by discarding the texture and retrying at double the size.

use tessella_glyph::atlas::{Atlas, MAX_SIZE, PADDING, Rect, ShelfPack};
use tessella_glyph::pbf::{BORDER, Glyph, Metrics};

/// A glyph with a distinguishable bitmap: every pixel is `fill`.
fn glyph(id: u32, width: u32, height: u32, fill: u8) -> Glyph {
    let (w, h) = (width + 2 * BORDER, height + 2 * BORDER);
    Glyph {
        id,
        metrics: Metrics {
            width,
            height,
            left: 0,
            top: 0,
            advance: width,
        },
        bitmap: vec![fill; (w * h) as usize],
    }
}

/// Whether two rectangles share any pixel.
fn overlaps(one: Rect, other: Rect) -> bool {
    one.x < other.x + other.width
        && other.x < one.x + one.width
        && one.y < other.y + other.height
        && other.y < one.y + one.height
}

/// Nothing packed ever overlaps anything else, and nothing leaves the texture.
///
/// The invariant everything else rests on: two glyphs sharing a pixel is a letter with another
/// letter's ink in it, and it is not an error anywhere — it just draws wrongly.
#[test]
fn packed_rectangles_never_overlap_or_escape() {
    let mut pack = ShelfPack::new(256, 256);
    let mut placed: Vec<Rect> = Vec::new();

    // Sizes chosen to be awkward: several heights, so shelves of different heights open and
    // the best-fit search actually has choices to make.
    for id in 0..120u32 {
        let width = 4 + (id * 7) % 29;
        let height = 4 + (id * 5) % 17;
        let Some(rect) = pack.pack(id, width, height) else {
            continue;
        };
        assert_eq!((rect.width, rect.height), (width, height));
        assert!(
            rect.x + rect.width <= 256 && rect.y + rect.height <= 256,
            "{rect:?} leaves the texture"
        );
        for earlier in &placed {
            assert!(!overlaps(rect, *earlier), "{rect:?} overlaps {earlier:?}");
        }
        placed.push(rect);
    }
    assert!(placed.len() > 50, "only packed {}", placed.len());
}

/// Asking for an id already packed returns the same slot and costs nothing.
///
/// The reason the packer takes an id at all: a glyph wanted by two tiles is one rectangle. A
/// packer that allocated again would fill the atlas with duplicates of the alphabet.
#[test]
fn a_repeated_id_returns_the_same_slot() {
    let mut pack = ShelfPack::new(64, 64);
    let first = pack.pack(7, 10, 10).expect("packs");
    let again = pack.pack(7, 10, 10).expect("packs");

    assert_eq!(first, again);
    assert_eq!(pack.len(), 1, "one slot, not two");
}

/// A freed slot is reused, and reused exactly when the sizes match.
#[test]
fn a_freed_slot_is_reused() {
    let mut pack = ShelfPack::new(64, 64);
    let first = pack.pack(1, 10, 10).expect("packs");
    pack.pack(2, 10, 10).expect("packs");

    pack.unref(1);
    let reused = pack.pack(3, 10, 10).expect("packs");
    assert_eq!(reused, first, "the freed slot should have been taken");
}

/// A slot is only freed when the last reference goes.
///
/// Two tiles drawing the same label take two references. Freeing on the first release would
/// hand the slot to another glyph while the second tile is still drawing from it.
#[test]
fn a_slot_survives_until_the_last_reference() {
    let mut pack = ShelfPack::new(64, 64);
    let held = pack.pack(1, 10, 10).expect("packs");
    pack.pack(1, 10, 10).expect("packs"); // a second reference
    pack.pack(2, 10, 10).expect("packs");

    pack.unref(1);
    let other = pack.pack(3, 10, 10).expect("packs");
    assert_ne!(
        other, held,
        "still referenced, so its slot is not available"
    );

    pack.unref(1);
    let now = pack.pack(4, 10, 10).expect("packs");
    assert_eq!(now, held, "the last reference went, so the slot is free");
}

/// A full atlas refuses rather than overwriting.
#[test]
fn a_full_packer_refuses() {
    let mut pack = ShelfPack::new(16, 16);
    assert!(pack.pack(1, 16, 16).is_some());
    assert!(pack.pack(2, 16, 16).is_none(), "there is no room");
    assert!(pack.pack(3, 1, 1).is_none());
}

/// Something larger than the texture is refused rather than clipped.
#[test]
fn an_oversized_rectangle_is_refused() {
    let mut pack = ShelfPack::new(32, 32);
    assert!(pack.pack(1, 64, 8).is_none());
    assert!(pack.pack(2, 8, 64).is_none());
}

/// A glyph's pixels land where its rectangle says, with padding around them.
///
/// The rectangle handed back covers the distance field plus one pixel, so the pixel just inside
/// its corner is padding and the one after that is the glyph. Getting this off by one puts a
/// row of a neighbouring glyph along every label's edge.
#[test]
fn a_glyph_lands_inside_its_rectangle() {
    let mut atlas = Atlas::new(128, 128);
    let rect = atlas.add(65, &glyph(65, 4, 4, 200)).expect("packs");

    let (width, _) = atlas.size();
    let at = |x: u32, y: u32| atlas.pixels()[(y * width + x) as usize];

    // The reported rectangle is the bitmap plus one pixel on each side.
    assert_eq!(rect.width, 4 + 2 * BORDER + 2);
    assert_eq!(rect.height, 4 + 2 * BORDER + 2);

    // Its outermost ring is padding, and one pixel further in is the glyph.
    assert_eq!(at(rect.x, rect.y), 0, "the reported edge is padding");
    assert_eq!(at(rect.x + 1, rect.y + 1), 200, "and then the glyph");

    // The pixel outside the reported rectangle is the separator, and is untouched.
    assert_eq!(at(rect.x - 1, rect.y - 1), 0);
}

/// Two glyphs do not bleed into one another.
///
/// With linear filtering a sampler reads past the edge of a glyph, so the padding between two
/// of them has to actually be blank. This is the assertion that catches a blit that wrote its
/// full slot rather than its bitmap.
#[test]
fn glyphs_do_not_bleed_into_each_other() {
    let mut atlas = Atlas::new(128, 128);
    let first = atlas.add(65, &glyph(65, 4, 4, 200)).expect("packs");
    let second = atlas.add(66, &glyph(66, 4, 4, 100)).expect("packs");

    assert!(!overlaps(first, second));

    let (width, _) = atlas.size();
    let at = |x: u32, y: u32| atlas.pixels()[(y * width + x) as usize];

    // The column between them belongs to neither.
    let gap = first.x + first.width;
    for y in first.y..first.y + first.height {
        assert_eq!(at(gap, y), 0, "the gap at ({gap}, {y}) is not blank");
    }
}

/// A glyph with no pixels takes no atlas space.
///
/// A space has an advance and nothing to draw. Reserving a rectangle for it would fill the
/// texture with blanks and, worse, hand the quad builder a rectangle to sample.
#[test]
fn a_blank_glyph_is_not_packed() {
    let mut atlas = Atlas::new(64, 64);
    let space = Glyph {
        id: 32,
        metrics: Metrics {
            width: 0,
            height: 0,
            left: 0,
            top: 0,
            advance: 8,
        },
        bitmap: Vec::new(),
    };

    assert!(atlas.add(32, &space).is_none());
    assert!(atlas.is_empty());
}

/// Every addition reports a dirty rectangle, and taking them clears the list.
///
/// §6.4: the consumer uploads what changed. A list that never cleared would re-upload the whole
/// atlas every frame, and one that never filled would upload nothing and draw stale glyphs.
#[test]
fn additions_report_dirty_rectangles() {
    let mut atlas = Atlas::new(128, 128);
    assert!(atlas.take_dirty().is_empty(), "nothing has changed yet");

    atlas.add(65, &glyph(65, 4, 4, 200)).expect("packs");
    atlas.add(66, &glyph(66, 4, 4, 100)).expect("packs");

    let dirty = atlas.take_dirty();
    assert_eq!(dirty.len(), 2, "one per glyph: {dirty:?}");
    assert!(atlas.take_dirty().is_empty(), "taking clears them");

    // A repeat of a glyph already present changes nothing.
    atlas.add(65, &glyph(65, 4, 4, 200)).expect("already here");
    assert!(atlas.take_dirty().is_empty(), "a repeat is not a change");
}

/// The padding is what the sampler needs and what the reported rectangle assumes.
#[test]
fn the_padding_is_two() {
    assert_eq!(PADDING, 2);
}

/// A full atlas doubles rather than dropping the glyph.
///
/// The defect this pins: a fixed 512 square holds a few hundred glyphs, so a frame with
/// thousands of distinct CJK characters in it lost every one past the fill. The label still
/// spent the advance, so it drew with holes in it -- visible as `八重洲一丁目` rendering as
/// `八　　一丁`.
#[test]
fn a_full_atlas_grows() {
    let mut atlas = Atlas::new(32, 32);
    assert_eq!(atlas.size(), (32, 32));

    // A 2x2 glyph takes a 12-square slot, so four of them fill a 32-square atlas.
    for id in 0..4 {
        atlas.add(id, &glyph(id, 2, 2, 200)).expect("packs at 32");
    }
    let grown = atlas
        .add(4, &glyph(4, 2, 2, 100))
        .expect("grows rather than dropping");

    assert_eq!(atlas.size(), (64, 64), "doubled in both dimensions");
    assert_eq!(atlas.len(), 5);
    assert!(grown.x + grown.width <= 64 && grown.y + grown.height <= 64);
}

/// Growing keeps every glyph already packed, pixels and position both.
///
/// The rectangles handed out before the growth are still live in the shaper's quads, so a grow
/// that moved them would draw the wrong character. Only the texture they are relative to
/// changes.
#[test]
fn growing_keeps_what_was_packed() {
    let mut atlas = Atlas::new(32, 32);
    let first = atlas.add(65, &glyph(65, 2, 2, 200)).expect("packs");
    let before = atlas.pixels()[(first.y * 32 + first.x + 1) as usize];

    for id in 1..5 {
        atlas.add(id, &glyph(id, 2, 2, 100));
    }
    assert_eq!(atlas.size(), (64, 64), "it grew");

    assert_eq!(atlas.get(65), Some(first), "the rectangle did not move");
    let (width, _) = atlas.size();
    assert_eq!(
        atlas.pixels()[(first.y * width + first.x + 1) as usize],
        before,
        "and its pixels came with it",
    );
}

/// A grown atlas asks for the whole texture to be re-uploaded.
///
/// Every rectangle is now relative to a texture twice the size, so a consumer holding the old
/// upload has every glyph at the wrong texture coordinate. One rect covering everything is the
/// only honest answer.
#[test]
fn growing_dirties_the_whole_texture() {
    let mut atlas = Atlas::new(32, 32);
    for id in 0..4 {
        atlas.add(id, &glyph(id, 2, 2, 200));
    }
    atlas.take_dirty();

    atlas.add(4, &glyph(4, 2, 2, 100)).expect("grows");

    let dirty = atlas.take_dirty();
    assert!(
        dirty
            .iter()
            .any(|rect| rect.x == 0 && rect.y == 0 && rect.width == 64 && rect.height == 64),
        "the whole 64-square texture is dirty: {dirty:?}",
    );
}

/// Growth stops at the cap rather than doubling forever.
#[test]
fn growth_is_bounded() {
    let mut atlas = Atlas::new(MAX_SIZE, MAX_SIZE);
    // One glyph wider than the atlas: it cannot be packed and cannot be grown into.
    assert!(atlas.add(1, &glyph(1, MAX_SIZE, 2, 200)).is_none());
    assert_eq!(atlas.size(), (MAX_SIZE, MAX_SIZE), "did not grow past the cap");
}
