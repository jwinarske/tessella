//! The dash atlas, against what a distance field has to mean.
//!
//! mbgl's own `LineAtlas` test builds a hundred random patterns and asserts that nothing crashes,
//! so there is no recorded ground truth to compare against. What can be asserted without one is
//! the meaning: inside a dash the field is positive and grows toward the middle, inside a gap it
//! is negative, the boundaries sit where the pattern puts them, and a pattern that repeats has no
//! seam. Those are the properties the shader relies on, and each of them fails differently.

use tessella_glyph::dash::{Atlas, Cap, WIDTH};

/// The byte at a column of the first row.
fn at(atlas: &Atlas, x: u32) -> i32 {
    i32::from(atlas.data[x as usize]) - 128
}

/// An even dasharray is dash then gap, each taking its share of the row.
///
/// `[2, 2]` stretches to 256 pixels, so the dash runs 0..128 and the gap 128..256. The field is
/// the distance to the nearest boundary, signed by which side it is on -- so it peaks in the
/// middle of each and crosses zero where they meet.
#[test]
fn a_dash_is_positive_and_a_gap_is_negative() {
    let (atlas, position) = Atlas::single(&[2.0, 2.0], Cap::Butt).expect("two entries");
    assert!((position.width - 4.0).abs() < 1e-6, "the length is the sum");

    assert!(at(&atlas, 64) > 0, "the middle of the dash");
    assert!(at(&atlas, 192) < 0, "the middle of the gap");
    // 64 pixels from either boundary, on opposite sides.
    assert_eq!(at(&atlas, 64), 64);
    assert_eq!(at(&atlas, 192), -64);
    // And zero where they meet.
    assert_eq!(at(&atlas, 128), 0);
}

/// The field grows away from a boundary rather than jumping.
///
/// A shader antialiases a dash end by thresholding this, so a field that stepped would give a
/// hard edge -- which is the thing a distance field exists to avoid.
#[test]
fn the_field_ramps_rather_than_steps() {
    let (atlas, _) = Atlas::single(&[2.0, 2.0], Cap::Butt).expect("two entries");
    for x in 1..64 {
        assert!(
            at(&atlas, x) >= at(&atlas, x - 1),
            "rising toward the middle of the dash at {x}"
        );
    }
}

/// An odd dasharray begins and ends with a dash, and they are one dash across the seam.
///
/// `[1, 2, 1]` is dash, gap, dash -- and the last dash runs into the first when the pattern
/// repeats. Both ends have to be inside it with no boundary between them, or a dashed line shows
/// a notch every time the pattern comes round.
#[test]
fn an_odd_pattern_joins_at_the_seam() {
    let (atlas, position) = Atlas::single(&[1.0, 2.0, 1.0], Cap::Butt).expect("three entries");
    assert!((position.width - 4.0).abs() < 1e-6);
    assert!(at(&atlas, 0) > 0, "the seam is inside a dash");
    assert!(at(&atlas, WIDTH - 1) > 0, "and so is the other end of it");
    // No boundary in the gap's own middle, which is where the sign must be most negative.
    assert!(at(&atlas, WIDTH / 2) < 0);
}

/// A zero-length entry is removed, and the dashes either side of it become one.
///
/// Leaving it in would put a boundary -- and so a distance of zero, and so a visible seam -- in
/// the middle of what the style asked to be a continuous dash.
#[test]
fn a_zero_length_entry_does_not_split_a_dash() {
    let (atlas, _) = Atlas::single(&[4.0, 0.0, 4.0], Cap::Butt).expect("three entries");
    // The whole row is one dash, so nothing in it is negative.
    assert!(
        (0..WIDTH).all(|x| at(&atlas, x) >= 0),
        "a pattern of dash, nothing, dash has no gap in it"
    );
}

/// Round caps need the distance across the line as well as along it.
#[test]
fn a_round_cap_fills_the_lines_width() {
    let (butt, _) = Atlas::single(&[2.0, 2.0], Cap::Butt).expect("two entries");
    let (round, position) = Atlas::single(&[2.0, 2.0], Cap::Round).expect("two entries");
    assert_eq!(butt.height, 1, "one row is enough for a square end");
    assert_eq!(
        round.height, 16,
        "fifteen rows, rounded up to a power of two"
    );
    assert!(
        position.height > 0.9,
        "the pattern spans almost all of them"
    );
    // The rows differ: that difference *is* the cap, and a round atlas whose rows were identical
    // would draw the square one.
    let row = |n: u32| &round.data[(WIDTH * n) as usize..(WIDTH * (n + 1)) as usize];
    assert_ne!(row(7), row(0), "the middle of the line is not its edge");
}

/// The height is a power of two, because the texture repeats horizontally.
///
/// GL ES 2.0 refuses to repeat a non-power-of-two texture and samples black instead, which is a
/// dashed line that draws as nothing at all.
#[test]
fn the_atlas_is_a_power_of_two() {
    for cap in [Cap::Butt, Cap::Round] {
        for pattern in [vec![2.0, 2.0], vec![1.0, 2.0, 3.0, 4.0]] {
            let (atlas, _) = Atlas::single(&pattern, cap).expect("two or more entries");
            assert!(atlas.height.is_power_of_two(), "{cap:?} {pattern:?}");
            assert_eq!(atlas.data.len(), (WIDTH * atlas.height) as usize);
        }
    }
    assert!(WIDTH.is_power_of_two(), "and so is the width");
}

/// Two patterns share a row when they are the same one.
///
/// The pair exists for the cross-fade between zoom levels, and a dasharray that does not vary
/// with zoom is the same pattern on both sides of it -- so giving it two identical rows would
/// double the texture for nothing.
#[test]
fn an_unchanging_pattern_is_not_stored_twice() {
    let (same, from, to) = Atlas::pair(&[2.0, 2.0], &[2.0, 2.0], Cap::Butt).expect("two entries");
    assert_eq!(from, to, "one position, because it is one pattern");
    assert_eq!(same.height, 1);

    let (differing, from, to) =
        Atlas::pair(&[2.0, 2.0], &[1.0, 3.0], Cap::Butt).expect("two entries");
    assert_ne!(from.y, to.y, "two patterns, two rows");
    assert_eq!(differing.height, 2);
}

/// Fewer than two entries is not a pattern.
///
/// One entry is a dash with no gap after it. mbgl warns and draws a plain line; this answers
/// `None` and leaves that decision to the caller, which is the one that knows whether it has a
/// plain line to fall back to.
#[test]
fn a_pattern_needs_a_dash_and_a_gap() {
    assert!(Atlas::single(&[], Cap::Butt).is_none());
    assert!(Atlas::single(&[2.0], Cap::Butt).is_none());
    assert!(Atlas::single(&[2.0, 2.0], Cap::Butt).is_some());
    // And a pattern of nothing at all has no length to stretch to the row.
    assert!(Atlas::single(&[0.0, 0.0], Cap::Butt).is_none());
}
