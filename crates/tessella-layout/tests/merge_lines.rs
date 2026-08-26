//! Joining a road's segments before it is labelled — mbgl's `util::mergeLines`.
//!
//! The expectations are mbgl's own `MergeLines.*`, coordinate for coordinate. They are worth
//! having exactly rather than in spirit: the merge is order-dependent, and an implementation that
//! joined the same set of lines in a different order produces different *lines* — the same
//! points distributed between features differently — which changes where every anchor lands.
//!
//! mbgl leaves a merged-away feature with an empty geometry and skips it later. This drops it,
//! so the expectation is stated as "these lines survive" rather than "these slots are empty".

use tessella_layout::symbol_bucket::{IconOptions, LineOptions, SymbolOptions};
use tessella_layout::symbol_layout::{Anchoring, Pending, Placement, SymbolLayout};

/// A layout holding the given `(text, line)` features, line-placed.
fn layout(features: &[(&str, &[(i32, i32)])]) -> SymbolLayout {
    let pending = features
        .iter()
        .map(|(text, points)| Pending {
            text: (*text).to_string(),
            icon: None,
            fonts: vec!["TestFont".to_string()],
            anchoring: Anchoring::Line(
                points
                    .iter()
                    .map(|(x, y)| {
                        #[allow(clippy::cast_precision_loss)]
                        (*x as f32, *y as f32)
                    })
                    .collect(),
            ),
            symbol: SymbolOptions::default(),
            icon_options: IconOptions::default(),
        })
        .collect();

    SymbolLayout {
        pending,
        symbol: SymbolOptions::default(),
        line: LineOptions::default(),
        placement: Placement::Line,
    }
}

/// The lines a layout holds, as integer coordinates.
fn lines(layout: &SymbolLayout) -> Vec<Vec<(i32, i32)>> {
    layout
        .pending
        .iter()
        .filter_map(|pending| match &pending.anchoring {
            #[allow(clippy::cast_possible_truncation)]
            Anchoring::Line(line) => {
                Some(line.iter().map(|(x, y)| (*x as i32, *y as i32)).collect())
            }
            Anchoring::Point(_) => None,
        })
        .collect()
}

/// mbgl `MergeLines.SameText`.
///
/// Six features, three names' worth of geometry. The two `aaa` runs join into one line each and
/// the `bbb` stub is untouched even though it touches both — text is part of the key, and a
/// merge that ignored it would splice two different streets into one road.
#[test]
fn lines_with_the_same_text_join() {
    let mut layout = layout(&[
        ("aaa", &[(0, 0), (1, 0), (2, 0)]),
        ("bbb", &[(4, 0), (5, 0), (6, 0)]),
        ("aaa", &[(8, 0), (9, 0)]),
        ("aaa", &[(2, 0), (3, 0), (4, 0)]),
        ("aaa", &[(6, 0), (7, 0), (8, 0)]),
        ("aaa", &[(5, 0), (6, 0)]),
    ]);
    layout.merge_lines();

    assert_eq!(
        lines(&layout),
        vec![
            vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)],
            vec![(4, 0), (5, 0), (6, 0)],
            vec![(5, 0), (6, 0), (7, 0), (8, 0), (9, 0)],
        ]
    );
}

/// mbgl `MergeLines.BothEnds`: a line with a neighbour at each end takes both.
///
/// The three-way case, and the one an implementation is most likely to get half right — joining
/// one side and leaving the other, which looks correct on any fixture where only one side
/// touches.
#[test]
fn a_line_joins_neighbours_at_both_ends() {
    let mut layout = layout(&[
        ("aaa", &[(0, 0), (1, 0), (2, 0)]),
        ("aaa", &[(4, 0), (5, 0), (6, 0)]),
        ("aaa", &[(2, 0), (3, 0), (4, 0)]),
    ]);
    layout.merge_lines();

    assert_eq!(
        lines(&layout),
        vec![vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0), (6, 0)]]
    );
}

/// mbgl `MergeLines.CircularLines`: a ring closes and is not merged into itself.
///
/// The guard that keeps the three-way case from eating a closed loop. Without it the line whose
/// two ends are the same feature joins to itself, and what is left is either a doubled ring or
/// nothing at all.
#[test]
fn a_circular_line_closes_without_eating_itself() {
    let mut layout = layout(&[
        ("aaa", &[(0, 0), (1, 0), (2, 0)]),
        ("aaa", &[(2, 0), (3, 0), (4, 0)]),
        ("aaa", &[(4, 0), (0, 0)]),
    ]);
    layout.merge_lines();

    assert_eq!(
        lines(&layout),
        vec![vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (0, 0)]]
    );
}

/// mbgl `MergeLines.EmptyOuterGeometry` and `EmptyInnerGeometry`: an empty line is left alone.
#[test]
fn an_empty_line_is_not_merged() {
    let mut layout = layout(&[("aaa", &[])]);
    layout.merge_lines();
    assert!(lines(&layout).is_empty(), "an empty line became a road");
}

/// The joint appears once, not twice.
///
/// The shared point is the last of one line and the first of the next. Keeping both puts a
/// zero-length segment in the middle of the road, which the anchor walk divides by — so a
/// duplicate here is a division by zero several stages away.
#[test]
fn the_joint_is_not_duplicated() {
    let mut layout = layout(&[("aaa", &[(0, 0), (10, 0)]), ("aaa", &[(10, 0), (20, 0)])]);
    layout.merge_lines();

    let joined = &lines(&layout)[0];
    assert_eq!(joined, &vec![(0, 0), (10, 0), (20, 0)]);
    for pair in joined.windows(2) {
        assert_ne!(pair[0], pair[1], "a zero-length segment survived the join");
    }
}

/// Only line placement merges.
///
/// mbgl guards the call with `symbol-placement == line`, and the reason is not efficiency: a
/// point-placed layer labels each feature where it is, so joining two features would delete one
/// of its labels.
#[test]
fn point_placement_does_not_merge() {
    let mut layout = layout(&[("aaa", &[(0, 0), (10, 0)]), ("aaa", &[(10, 0), (20, 0)])]);
    layout.placement = Placement::Point;
    layout.merge_lines();
    assert_eq!(lines(&layout).len(), 2, "a point layer merged its features");
}

/// Lines that touch but say different things stay apart.
#[test]
fn different_text_does_not_join() {
    let mut layout = layout(&[
        ("High Street", &[(0, 0), (10, 0)]),
        ("Low Street", &[(10, 0), (20, 0)]),
    ]);
    layout.merge_lines();
    assert_eq!(lines(&layout).len(), 2);
}

/// Lines that share a name but not an endpoint stay apart.
#[test]
fn a_gap_is_not_bridged() {
    let mut layout = layout(&[("aaa", &[(0, 0), (10, 0)]), ("aaa", &[(11, 0), (20, 0)])]);
    layout.merge_lines();
    assert_eq!(lines(&layout).len(), 2, "a one-unit gap was bridged");
}
