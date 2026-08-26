//! The box a label reserves.
//!
//! mbgl has no unit test for `CollisionFeature`, so what is pinned here is the arithmetic from
//! its source — scale before padding, the envelope of a rotated box, and the empty case — plus
//! the properties placement depends on.

use tessella_place::feature::{Extent, Padding, collision_box};

fn label() -> Extent {
    // A shaped label 40 wide and 20 tall, centred on its anchor.
    Extent {
        top: -10.0,
        bottom: 10.0,
        left: -20.0,
        right: 20.0,
    }
}

/// Unscaled and unpadded, the box is the label.
#[test]
fn the_box_starts_as_the_label() {
    let placed = collision_box(label(), (100.0, 200.0), 1.0, Padding::default(), 0.0)
        .expect("a label with extent has a box");

    assert_eq!(placed.size(), (40.0, 20.0));
    let bounds = placed.bounds();
    assert_eq!(bounds.min, (80.0, 190.0));
    assert_eq!(bounds.max, (120.0, 210.0));
}

/// Scaling happens before padding, so padding stays a constant number of screen pixels.
///
/// The order is the whole point. Padded first and then scaled, `text-padding` would grow with
/// the label, and the gap between two labels would widen as the map zoomed in — which is the
/// opposite of what a constant screen-space padding is for.
#[test]
fn the_label_scales_but_the_padding_does_not() {
    let unscaled =
        collision_box(label(), (0.0, 0.0), 1.0, Padding::uniform(2.0), 0.0).expect("box");
    let scaled = collision_box(label(), (0.0, 0.0), 2.0, Padding::uniform(2.0), 0.0).expect("box");

    // The label doubles: 40 wide becomes 80. The padding stays 2 a side either way.
    assert_eq!(unscaled.size(), (40.0 + 4.0, 20.0 + 4.0));
    assert_eq!(scaled.size(), (80.0 + 4.0, 40.0 + 4.0));
}

/// Padding is applied per side, outward.
#[test]
fn padding_pushes_each_edge_outward() {
    let padding = Padding {
        top: 1.0,
        right: 2.0,
        bottom: 3.0,
        left: 4.0,
    };
    let placed = collision_box(label(), (0.0, 0.0), 1.0, padding, 0.0).expect("box");

    assert_eq!(placed.x1, -20.0 - 4.0);
    assert_eq!(placed.x2, 20.0 + 2.0);
    assert_eq!(placed.y1, -10.0 - 1.0);
    assert_eq!(placed.y2, 10.0 + 3.0);
}

/// A label that occupies nothing reserves nothing.
///
/// Not an empty box at the anchor — that would still collide with anything covering the point,
/// so an invisible label would push a visible one off the map. A label still waiting for its
/// glyphs is exactly this case.
#[test]
fn an_empty_label_has_no_box() {
    assert!(
        collision_box(
            Extent::default(),
            (5.0, 5.0),
            1.0,
            Padding::uniform(4.0),
            0.0
        )
        .is_none()
    );
}

/// A quarter turn swaps the box's width and height.
///
/// Exact in floating point, so the envelope can be asserted rather than approximated.
#[test]
fn a_quarter_turn_swaps_the_extents() {
    let upright = collision_box(label(), (0.0, 0.0), 1.0, Padding::default(), 0.0).expect("box");
    let turned = collision_box(label(), (0.0, 0.0), 1.0, Padding::default(), 90.0).expect("box");

    let (width, height) = upright.size();
    let (turned_width, turned_height) = turned.size();
    assert!(
        (turned_width - height).abs() < 1e-3,
        "{turned_width} vs {height}"
    );
    assert!(
        (turned_height - width).abs() < 1e-3,
        "{turned_height} vs {width}"
    );
}

/// A rotated label reserves the upright box that contains it, which is larger.
///
/// The cost of an axis-aligned index, and mbgl says so in as many words. A wide label on a
/// diagonal reserves close to a square — much more than it draws. A test that only checked the
/// quarter turn would miss this entirely, since a quarter turn is the one angle where the
/// envelope is no larger than the box.
#[test]
fn a_diagonal_label_reserves_more_than_it_draws() {
    let upright = collision_box(label(), (0.0, 0.0), 1.0, Padding::default(), 0.0).expect("box");
    let diagonal = collision_box(label(), (0.0, 0.0), 1.0, Padding::default(), 45.0).expect("box");

    let (width, height) = upright.size();
    let (diagonal_width, diagonal_height) = diagonal.size();

    // 40 by 20 at 45 degrees has an envelope of about 42.4 on each axis.
    assert!(diagonal_width > width * 1.05, "{diagonal_width} vs {width}");
    assert!(
        diagonal_height > height * 2.0,
        "{diagonal_height} vs {height}"
    );
    assert!(
        (diagonal_width - diagonal_height).abs() < 1e-3,
        "and it is square"
    );
}

/// The envelope stays centred on the anchor when the label is.
///
/// Rotation is about the anchor, so a centred label stays centred however far it turns. A
/// rotation about a corner would drift the label away from its point as the angle changed.
#[test]
fn rotation_turns_about_the_anchor() {
    for degrees in [0.0f32, 15.0, 45.0, 90.0, 180.0, 270.0] {
        let placed =
            collision_box(label(), (50.0, 60.0), 1.0, Padding::default(), degrees).expect("box");
        let bounds = placed.bounds();
        let centre = (
            (bounds.min.0 + bounds.max.0) / 2.0,
            (bounds.min.1 + bounds.max.1) / 2.0,
        );
        assert!(
            (centre.0 - 50.0).abs() < 1e-3 && (centre.1 - 60.0).abs() < 1e-3,
            "at {degrees} degrees the centre moved to {centre:?}"
        );
    }
}

/// The anchor moves the box and nothing else.
#[test]
fn the_anchor_moves_the_box() {
    let here = collision_box(label(), (0.0, 0.0), 1.0, Padding::uniform(3.0), 0.0).expect("box");
    let there = collision_box(label(), (7.0, -4.0), 1.0, Padding::uniform(3.0), 0.0).expect("box");

    assert_eq!(here.size(), there.size());
    assert_eq!(there.bounds().min.0 - here.bounds().min.0, 7.0);
    assert_eq!(there.bounds().min.1 - here.bounds().min.1, -4.0);
}

/// A padded box collides where an unpadded one would not.
///
/// The reason padding exists, stated against the grid rather than against the arithmetic: two
/// labels that merely fail to overlap still read as crowded.
#[test]
fn padding_is_what_keeps_labels_apart() {
    use tessella_place::grid::GridIndex;

    let mut grid: GridIndex<i16> = GridIndex::new(1000.0, 1000.0, 32);
    let first =
        collision_box(label(), (100.0, 100.0), 1.0, Padding::uniform(5.0), 0.0).expect("box");
    grid.insert_box(0, first.bounds());

    // Anchored 45 to the right: the labels are 5 apart, closer than the 10 units of padding
    // between them, so with padding they collide and without it they do not.
    let close =
        collision_box(label(), (145.0, 100.0), 1.0, Padding::uniform(5.0), 0.0).expect("box");
    assert!(
        grid.hit_test_box(close.bounds()),
        "padded boxes should collide"
    );

    let unpadded_first =
        collision_box(label(), (100.0, 100.0), 1.0, Padding::default(), 0.0).expect("box");
    let unpadded_close =
        collision_box(label(), (145.0, 100.0), 1.0, Padding::default(), 0.0).expect("box");
    let mut bare: GridIndex<i16> = GridIndex::new(1000.0, 1000.0, 32);
    bare.insert_box(0, unpadded_first.bounds());
    assert!(
        !bare.hit_test_box(unpadded_close.bounds()),
        "without padding they clear each other"
    );
}

/// An icon's content margins grow its collision box, and scale with it.
///
/// The half of `icon-text-fit` that placement sees. Once a shield has been stretched around its
/// label, the extent is the shield's *content* area — the box the number sits in — and the drawn
/// picture reaches further out by its border. Collision has to reserve the picture, or two
/// shields overlap by their borders and look crowded while the numbers do not touch.
///
/// Two paddings with different behaviours, which is why they are separate arguments: `text-padding`
/// is a number of screen pixels and stays put under zoom, while the margins are part of the
/// drawing and scale with it.
#[test]
fn an_icons_content_margins_grow_its_box() {
    use tessella_place::feature::collision_box_with;

    let extent = Extent {
        top: -8.0,
        bottom: 8.0,
        left: -8.0,
        right: 8.0,
    };

    let bare =
        collision_box_with(extent, (0.0, 0.0), 1.0, Padding::default(), None, 0.0).expect("a box");
    assert_eq!(bare.size(), (16.0, 16.0));

    // A border of two on every side: the picture is twenty across where its content is sixteen.
    let margins = Some((2.0, 2.0, 2.0, 2.0));
    let bordered = collision_box_with(extent, (0.0, 0.0), 1.0, Padding::default(), margins, 0.0)
        .expect("a box");
    assert_eq!(bordered.size(), (20.0, 20.0));

    // The margins scale with the box; `text-padding` does not.
    let scaled = collision_box_with(extent, (0.0, 0.0), 2.0, Padding::uniform(3.0), margins, 0.0)
        .expect("a box");
    assert_eq!(
        scaled.size(),
        (
            // extent 16 doubled, plus 3 of screen padding each side, plus 2 of margin doubled
            32.0 + 6.0 + 8.0,
            32.0 + 6.0 + 8.0
        ),
        "the two paddings did not scale differently"
    );

    // No margins is the same as zero margins, so an icon without a content box is unaffected.
    let zeroed = collision_box_with(
        extent,
        (0.0, 0.0),
        1.0,
        Padding::default(),
        Some((0.0, 0.0, 0.0, 0.0)),
        0.0,
    )
    .expect("a box");
    assert_eq!(zeroed.size(), bare.size());
}
