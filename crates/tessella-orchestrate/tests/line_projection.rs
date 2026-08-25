//! Placing a line label's glyphs along the projected line, and keeping them upright.
//!
//! The distances layout records are one number per glyph; everything about where that glyph
//! actually goes is here, and it is per view per frame because it depends on the camera.

use tessella_orchestrate::project::{
    LineOffsets, Placement, place_glyph_along_line, place_glyphs_along_line, place_upright,
};

/// A line running left to right at y = 100, four hundred units long.
fn eastward() -> Vec<(f32, f32)> {
    vec![(0.0, 100.0), (100.0, 100.0), (300.0, 100.0), (400.0, 100.0)]
}

/// The same line, given in the other order, so it runs right to left.
fn westward() -> Vec<(f32, f32)> {
    let mut line = eastward();
    line.reverse();
    line
}

/// The distances a five-glyph word records, centered on its anchor.
const WORD: [f32; 5] = [-40.0, -20.0, 0.0, 20.0, 40.0];

/// An angle brought into `-PI..=PI`.
///
/// The placement accumulates half turns rather than normalizing, exactly as mbgl does — a glyph
/// walked backwards along an eastward line comes out at two pi, which is upright. Nothing
/// downstream cares, because the angle is only ever consumed through a sine and a cosine, but a
/// test comparing the number has to.
fn turns(angle: f32) -> f32 {
    let two_pi = core::f32::consts::TAU;
    let wrapped = angle.rem_euclid(two_pi);
    if wrapped > core::f32::consts::PI {
        wrapped - two_pi
    } else {
        wrapped
    }
}

/// A glyph at no offset sits on the anchor.
#[test]
fn a_glyph_at_no_offset_is_at_the_anchor() {
    let placed =
        place_glyph_along_line(&eastward(), (200.0, 100.0), 1, 0.0, &LineOffsets::default())
            .expect("the line is long enough");
    assert!((placed.point.0 - 200.0).abs() < 0.01, "{placed:?}");
    assert!((placed.point.1 - 100.0).abs() < 0.01, "{placed:?}");
}

/// Offsets advance along the line, and the glyphs face the way it runs.
#[test]
fn the_glyphs_advance_along_the_line() {
    let Placement::Placed(glyphs) = place_glyphs_along_line(
        &eastward(),
        (200.0, 100.0),
        1,
        &WORD,
        &LineOffsets::default(),
    ) else {
        panic!("a 400-unit line holds an 80-unit word");
    };

    assert_eq!(glyphs.len(), 5);
    let xs: Vec<f32> = glyphs.iter().map(|glyph| glyph.point.0).collect();
    for pair in xs.windows(2) {
        assert!(pair[1] > pair[0], "{xs:?} does not advance");
    }
    // 40 units each side of the anchor, on a line with no bends.
    assert!((xs[0] - 160.0).abs() < 0.01, "{xs:?}");
    assert!((xs[4] - 240.0).abs() < 0.01, "{xs:?}");
    for glyph in &glyphs {
        assert!(
            turns(glyph.angle).abs() < 0.01,
            "{glyph:?} is not horizontal"
        );
    }
}

/// A label on a line that runs right to left reads backwards, and says so.
///
/// The whole reason this stage exists. Without the check the label is placed perfectly, every
/// glyph in the right position, and the word is mirrored — which no arithmetic assertion
/// anywhere else catches, because the arithmetic is right.
#[test]
fn a_westward_line_needs_flipping() {
    let placement = place_glyphs_along_line(
        &westward(),
        (200.0, 100.0),
        1,
        &WORD,
        &LineOffsets::default(),
    );
    assert_eq!(placement, Placement::NeedsFlipping);
}

/// And an eastward one does not.
#[test]
fn an_eastward_line_does_not() {
    assert!(matches!(
        place_glyphs_along_line(
            &eastward(),
            (200.0, 100.0),
            1,
            &WORD,
            &LineOffsets::default(),
        ),
        Placement::Placed(_)
    ));
}

/// Flipping puts the word back the right way round.
#[test]
fn flipping_makes_it_read_left_to_right() {
    let (placement, flipped) = place_upright(
        &westward(),
        (200.0, 100.0),
        1,
        &WORD,
        &LineOffsets::default(),
    );
    assert!(flipped);
    let Placement::Placed(glyphs) = placement else {
        panic!("the flipped placement fits too");
    };

    // The first glyph of the word is now left of the last, which is what "reads left to right"
    // means on screen.
    let xs: Vec<f32> = glyphs.iter().map(|glyph| glyph.point.0).collect();
    for pair in xs.windows(2) {
        assert!(pair[1] > pair[0], "{xs:?} still reads backwards");
    }

    // And every glyph now stands upright rather than on its head, which is the whole point:
    // unflipped, this line's glyphs face along it, and along it is backwards.
    for glyph in &glyphs {
        assert!(turns(glyph.angle).abs() < 0.01, "{glyph:?} is upside down");
    }
}

/// An eastward line is left alone by the retry.
#[test]
fn an_upright_label_is_not_flipped() {
    let (placement, flipped) = place_upright(
        &eastward(),
        (200.0, 100.0),
        1,
        &WORD,
        &LineOffsets::default(),
    );
    assert!(!flipped);
    assert!(matches!(placement, Placement::Placed(_)));
}

/// With `text-keep-upright` off, a backwards label is placed backwards.
///
/// The property exists for scripts and symbols that are meant to follow the line whichever way
/// it runs, so turning it off has to actually place rather than refuse.
#[test]
fn keep_upright_off_places_it_mirrored() {
    let Placement::Placed(glyphs) = place_glyphs_along_line(
        &westward(),
        (200.0, 100.0),
        1,
        &WORD,
        &LineOffsets {
            keep_upright: false,
            ..LineOffsets::default()
        },
    ) else {
        panic!("with the check off it is placed as it lies");
    };

    let xs: Vec<f32> = glyphs.iter().map(|glyph| glyph.point.0).collect();
    for pair in xs.windows(2) {
        assert!(pair[1] < pair[0], "{xs:?} was flipped anyway");
    }
}

/// A word longer than its line is not placed at all.
///
/// Half a road name is worse than none: it reads as a different road.
#[test]
fn a_word_longer_than_its_line_is_not_placed() {
    let stub = vec![(190.0, 100.0), (210.0, 100.0)];
    assert_eq!(
        place_glyphs_along_line(&stub, (200.0, 100.0), 0, &WORD, &LineOffsets::default()),
        Placement::NotEnoughRoom
    );
}

/// A short line refuses before it reports a flip.
///
/// Both ends are tested for the upright check, so a line too short to hold the word cannot
/// answer `NeedsFlipping` and send the caller round the loop to discover the same thing.
#[test]
fn a_short_westward_line_refuses_rather_than_flips() {
    let mut stub = vec![(190.0, 100.0), (210.0, 100.0)];
    stub.reverse();
    assert_eq!(
        place_glyphs_along_line(&stub, (200.0, 100.0), 0, &WORD, &LineOffsets::default()),
        Placement::NotEnoughRoom
    );
}

/// The across offset lifts the label off the line it names.
#[test]
fn the_across_offset_moves_it_perpendicular() {
    let offsets = LineOffsets {
        across: 12.0,
        ..LineOffsets::default()
    };
    let placed = place_glyph_along_line(&eastward(), (200.0, 100.0), 1, 20.0, &offsets)
        .expect("the line is long enough");

    // The line runs along x, so a perpendicular offset is purely in y, and does not move the
    // glyph along the line.
    assert!((placed.point.0 - 220.0).abs() < 0.01, "{placed:?}");
    assert!((placed.point.1 - 112.0).abs() < 0.01, "{placed:?}");
}

/// A bend turns the glyphs that sit on it, and only those.
#[test]
fn a_bend_turns_the_glyphs_on_it() {
    // Straight to (200, 100), then a right angle downwards. The anchor sits twenty units short
    // of the corner, so the word's near half is on the horizontal run and its far half is not.
    let corner = vec![(0.0, 100.0), (200.0, 100.0), (200.0, 400.0)];
    let Placement::Placed(glyphs) = place_glyphs_along_line(
        &corner,
        (180.0, 100.0),
        0,
        &WORD,
        &LineOffsets {
            // The bend is to the right, so the label as a whole still reads left to right; the
            // check would otherwise fire on the glyphs past the corner.
            keep_upright: false,
            ..LineOffsets::default()
        },
    ) else {
        panic!("the corner is long enough either side");
    };

    // The first glyphs are on the horizontal run and the last on the vertical one.
    assert!(turns(glyphs[0].angle).abs() < 0.01, "{:?}", glyphs[0]);
    let quarter = core::f32::consts::FRAC_PI_2;
    assert!(
        (turns(glyphs[4].angle) - quarter).abs() < 0.01,
        "{:?} is not turned onto the descending run",
        glyphs[4]
    );
}

/// The font size scales the distances, so the same layout draws at any zoom.
#[test]
fn the_font_scale_stretches_the_word() {
    let placed = |scale: f32| {
        let Placement::Placed(glyphs) = place_glyphs_along_line(
            &eastward(),
            (200.0, 100.0),
            1,
            &WORD,
            &LineOffsets {
                font_scale: scale,
                ..LineOffsets::default()
            },
        ) else {
            panic!("both fit on a 400-unit line");
        };
        glyphs[4].point.0 - glyphs[0].point.0
    };

    let single = placed(1.0);
    let double = placed(2.0);
    assert!(
        (double - single * 2.0).abs() < 0.01,
        "{single} then {double}"
    );
}

/// A single-glyph label is placed without an upright test.
///
/// One glyph has no reading direction, so there is nothing to compare and mbgl skips the check.
/// The bound is easy to write as `>= 1` and then the first and last are the same glyph, which
/// never flips and is never wrong — until a one-letter label on a westward road draws mirrored.
#[test]
fn a_single_glyph_needs_no_upright_test() {
    let Placement::Placed(glyphs) = place_glyphs_along_line(
        &westward(),
        (200.0, 100.0),
        1,
        &[0.0],
        &LineOffsets::default(),
    ) else {
        panic!("one glyph fits anywhere on the line");
    };
    assert_eq!(glyphs.len(), 1);
}
