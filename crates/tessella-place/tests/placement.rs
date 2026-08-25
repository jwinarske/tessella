//! Which labels get drawn, and which are pushed off the map.
//!
//! The decision itself is a handful of booleans, and every one of them is a style property a
//! cartographer set deliberately. What is asserted here is the whole truth table rather than a
//! few points of it, because the interesting failures are the combinations: a rule that works
//! for text alone and inverts when an icon is present looks correct on most styles.

use tessella_place::feature::{Extent, Padding, collision_box};
use tessella_place::grid::GridIndex;
use tessella_place::placement::{Candidate, Placed, Rules, Shape, place};

/// A label 40 by 20, anchored where asked.
fn label(anchor: (f32, f32)) -> tessella_place::feature::CollisionBox {
    collision_box(
        Extent {
            top: -10.0,
            bottom: 10.0,
            left: -20.0,
            right: 20.0,
        },
        anchor,
        1.0,
        Padding::default(),
        0.0,
    )
    .expect("a label with extent")
}

fn text_only(id: u32, anchor: (f32, f32)) -> Candidate {
    Candidate {
        cross_tile_id: id,
        text: Some(Shape::Box(label(anchor))),
        icon: None,
    }
}

fn grid() -> GridIndex<u32> {
    GridIndex::new(1000.0, 1000.0, 32)
}

/// Labels that do not overlap are all placed.
#[test]
fn labels_that_fit_are_all_placed() {
    let candidates = [
        text_only(1, (100.0, 100.0)),
        text_only(2, (300.0, 100.0)),
        text_only(3, (500.0, 100.0)),
    ];
    let placed = place(&candidates, &Rules::default(), &mut grid());

    assert!(placed.iter().all(|symbol| symbol.text));
    assert_eq!(placed.len(), 3);
}

/// When two labels overlap, the first one offered wins.
///
/// Not the larger, not the more important — the first. The order is the style's, by
/// `symbol-sort-key` and then feature order, so a cartographer decides rather than an algorithm.
#[test]
fn the_first_of_two_overlapping_labels_wins() {
    let candidates = [text_only(1, (100.0, 100.0)), text_only(2, (110.0, 100.0))];
    let placed = place(&candidates, &Rules::default(), &mut grid());

    assert_eq!(
        placed,
        [
            Placed {
                cross_tile_id: 1,
                text: true,
                icon: false
            },
            Placed {
                cross_tile_id: 2,
                text: false,
                icon: false
            },
        ]
    );
}

/// Reversing the order reverses the winner, and nothing else changes.
///
/// The assertion that placement is decided by order rather than by geometry: the same two
/// labels, the same overlap, the other one drawn.
#[test]
fn order_decides_which_label_survives() {
    let forwards = place(
        &[text_only(1, (100.0, 100.0)), text_only(2, (110.0, 100.0))],
        &Rules::default(),
        &mut grid(),
    );
    let backwards = place(
        &[text_only(2, (110.0, 100.0)), text_only(1, (100.0, 100.0))],
        &Rules::default(),
        &mut grid(),
    );

    assert!(forwards[0].text && !forwards[1].text);
    assert!(backwards[0].text && !backwards[1].text);
    assert_eq!(forwards[0].cross_tile_id, 1);
    assert_eq!(backwards[0].cross_tile_id, 2);
}

/// A rejected label does not block the ones after it.
///
/// It was never placed, so it reserves nothing. A loop that inserted every candidate rather than
/// every winner would let a label that is not drawn push a third one off the map.
#[test]
fn a_rejected_label_blocks_nothing() {
    // The labels are 40 wide. The first covers 80..120 and the second 90..130, so the second is
    // rejected. The third covers 125..165: clear of the first — and clear by a margin, since
    // boxes that merely touch collide — but overlapping the rejected second.
    let candidates = [
        text_only(1, (100.0, 100.0)),
        text_only(2, (110.0, 100.0)),
        text_only(3, (145.0, 100.0)),
    ];
    let placed = place(&candidates, &Rules::default(), &mut grid());

    assert!(placed[0].text);
    assert!(!placed[1].text);
    assert!(
        placed[2].text,
        "the third overlaps only a label that was never drawn"
    );
}

/// `allow-overlap` skips the test: the label is always drawn.
#[test]
fn allow_overlap_draws_regardless() {
    let rules = Rules {
        text_allow_overlap: true,
        ..Rules::default()
    };
    let candidates = [text_only(1, (100.0, 100.0)), text_only(2, (100.0, 100.0))];
    let placed = place(&candidates, &rules, &mut grid());

    assert!(placed.iter().all(|symbol| symbol.text), "both drawn");
}

/// `ignore-placement` skips the insert: the label does not block others.
///
/// A different permission from `allow-overlap`, and the pair is what pins a label that must be
/// drawn and must never move anything else.
#[test]
fn ignore_placement_blocks_nothing() {
    let rules = Rules {
        text_ignore_placement: true,
        ..Rules::default()
    };
    let candidates = [text_only(1, (100.0, 100.0)), text_only(2, (105.0, 100.0))];
    let placed = place(&candidates, &rules, &mut grid());

    assert!(placed[0].text);
    assert!(
        placed[1].text,
        "the first was drawn but reserves nothing, so the second fits"
    );

    // And without it, the second loses — which is what makes the assertion above mean something.
    let plain = place(&candidates, &Rules::default(), &mut grid());
    assert!(!plain[1].text);
}

/// `ignore-placement` does not also grant `allow-overlap`.
///
/// The two are different permissions and it is easy to collapse them: one skips the insert, the
/// other skips the test. A label that ignores placement is still *subject* to it — it may be
/// pushed off the map by an earlier layer, it simply does not push anything itself. The test
/// above cannot see the difference, because its label is offered first and nothing is there to
/// block it.
#[test]
fn ignore_placement_does_not_grant_overlap() {
    let mut grid = grid();
    // An earlier layer's label, which does block.
    place(
        &[text_only(1, (100.0, 100.0))],
        &Rules::default(),
        &mut grid,
    );

    let rules = Rules {
        text_ignore_placement: true,
        ..Rules::default()
    };
    let placed = place(&[text_only(2, (105.0, 100.0))], &rules, &mut grid);
    assert!(
        !placed[0].text,
        "ignoring placement is not permission to overlap"
    );

    // Whereas allowing overlap is.
    let overlapping = Rules {
        text_allow_overlap: true,
        ..Rules::default()
    };
    let placed = place(&[text_only(3, (105.0, 100.0))], &overlapping, &mut grid);
    assert!(placed[0].text);
}

/// A grid carried between calls places later layers against earlier ones.
#[test]
fn placement_accumulates_across_calls() {
    let mut grid = grid();
    let first = place(
        &[text_only(1, (100.0, 100.0))],
        &Rules::default(),
        &mut grid,
    );
    assert!(first[0].text);

    let second = place(
        &[text_only(2, (110.0, 100.0))],
        &Rules::default(),
        &mut grid,
    );
    assert!(!second[0].text, "the earlier layer still holds that space");
}

/// The full truth table for how a symbol's two halves combine.
///
/// Four combinations of `text-optional` and `icon-optional`, against four combinations of what
/// each half would have placed on its own. The interesting failures live here: a rule that is
/// right for text alone and inverts when an icon is present looks correct on most styles.
#[test]
fn the_two_halves_combine_by_the_optional_rules() {
    // (text_optional, icon_optional, text fits, icon fits) -> (text drawn, icon drawn)
    let cases: [(bool, bool, bool, bool, bool, bool); 16] = [
        // Neither optional: both or nothing.
        (false, false, true, true, true, true),
        (false, false, true, false, false, false),
        (false, false, false, true, false, false),
        (false, false, false, false, false, false),
        // Text optional: the icon may stand alone, so the text follows the icon.
        (true, false, true, true, true, true),
        (true, false, true, false, false, false),
        (true, false, false, true, false, true),
        (true, false, false, false, false, false),
        // Icon optional: the text may stand alone, so the icon follows the text.
        (false, true, true, true, true, true),
        (false, true, true, false, true, false),
        (false, true, false, true, false, false),
        (false, true, false, false, false, false),
        // Both optional: independent.
        (true, true, true, true, true, true),
        (true, true, true, false, true, false),
        (true, true, false, true, false, true),
        (true, true, false, false, false, false),
    ];

    for (text_optional, icon_optional, text_fits, icon_fits, want_text, want_icon) in cases {
        let mut grid = grid();
        // Block whichever half is meant not to fit, by placing an immovable label over it.
        let blocker_rules = Rules::default();
        if !text_fits {
            place(&[text_only(99, (100.0, 100.0))], &blocker_rules, &mut grid);
        }
        if !icon_fits {
            place(&[text_only(98, (300.0, 100.0))], &blocker_rules, &mut grid);
        }

        let candidate = Candidate {
            cross_tile_id: 1,
            text: Some(Shape::Box(label((100.0, 100.0)))),
            icon: Some(Shape::Box(label((300.0, 100.0)))),
        };
        let rules = Rules {
            text_optional,
            icon_optional,
            ..Rules::default()
        };
        let placed = place(&[candidate], &rules, &mut grid);

        assert_eq!(
            (placed[0].text, placed[0].icon),
            (want_text, want_icon),
            "text_optional={text_optional} icon_optional={icon_optional} \
             text_fits={text_fits} icon_fits={icon_fits}"
        );
    }
}

/// A symbol with only text is not held back by an icon it does not have.
///
/// `icon-optional` defaults to false, which would mean "the text needs the icon" — so a
/// text-only label would never place at all if the absent icon counted as a failure.
#[test]
fn a_text_only_symbol_places_without_an_icon() {
    let placed = place(
        &[text_only(1, (100.0, 100.0))],
        &Rules::default(),
        &mut grid(),
    );
    assert!(placed[0].text);
    assert!(!placed[0].icon, "there is no icon to draw");
}

/// And an icon-only symbol likewise.
#[test]
fn an_icon_only_symbol_places_without_text() {
    let candidate = Candidate {
        cross_tile_id: 1,
        text: None,
        icon: Some(Shape::Box(label((100.0, 100.0)))),
    };
    let placed = place(&[candidate], &Rules::default(), &mut grid());
    assert!(placed[0].icon);
    assert!(!placed[0].text);
}

/// A symbol with nothing to draw places nothing and reserves nothing.
#[test]
fn a_symbol_with_no_boxes_reserves_nothing() {
    let empty = Candidate {
        cross_tile_id: 1,
        text: None,
        icon: None,
    };
    let mut grid = grid();
    let placed = place(&[empty], &Rules::default(), &mut grid);

    assert!(!placed[0].any());
    assert!(grid.is_empty(), "nothing was reserved");
}
