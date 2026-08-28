//! The index answers what a brute-force scan answers.
//!
//! # What this can and cannot check
//!
//! That the *set* is right, which is what a spatial index is for. What it cannot check on its
//! own is the *order*, and the order matters: clustering marks points visited as it walks a
//! zoom level, so which neighbour a query reaches first decides which cluster absorbs it. There
//! is no independent way to say what that order should be — it is whatever the tree layout
//! makes it — so it is pinned from the other end, by supercluster's own expectations over the
//! `places.json` fixture. A layout that differed would answer these tests and fail those.

use std::collections::BTreeSet;

use tessella_source::kdbush::KdBush;

/// A spread of points that is neither sorted nor uniform, and large enough to be several nodes
/// deep — a set under the leaf size never splits and would test only the linear scan.
fn points(count: usize) -> Vec<(f64, f64)> {
    let mut out = Vec::with_capacity(count);
    // A cheap deterministic scatter: an irrational-ish stride so nothing lines up on a grid.
    let (mut x, mut y) = (0.311_f64, 0.737_f64);
    for _ in 0..count {
        x = (x + 0.618_033_988_75).fract();
        y = (y + 0.414_213_562_37).fract();
        out.push((x * 100.0, y * 100.0));
    }
    out
}

fn brute_range(points: &[(f64, f64)], min: (f64, f64), max: (f64, f64)) -> BTreeSet<u32> {
    points
        .iter()
        .enumerate()
        .filter(|(_, (x, y))| *x >= min.0 && *x <= max.0 && *y >= min.1 && *y <= max.1)
        .map(|(index, _)| u32::try_from(index).expect("small"))
        .collect()
}

fn brute_within(points: &[(f64, f64)], at: (f64, f64), r: f64) -> BTreeSet<u32> {
    points
        .iter()
        .enumerate()
        .filter(|(_, (x, y))| (x - at.0).powi(2) + (y - at.1).powi(2) <= r * r)
        .map(|(index, _)| u32::try_from(index).expect("small"))
        .collect()
}

fn found(query: impl FnOnce(&mut dyn FnMut(u32))) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    query(&mut |id| {
        assert!(out.insert(id), "id {id} was visited twice");
    });
    out
}

/// Every rectangle, at several sizes and in several places.
#[test]
fn a_range_query_finds_exactly_what_is_in_the_rectangle() {
    let points = points(1000);
    let bush = KdBush::new(&points);

    for (min, max) in [
        ((0.0, 0.0), (100.0, 100.0)),
        ((0.0, 0.0), (50.0, 50.0)),
        ((25.0, 25.0), (75.0, 75.0)),
        ((90.0, 90.0), (100.0, 100.0)),
        ((40.0, 0.0), (41.0, 100.0)),
        ((101.0, 101.0), (102.0, 102.0)),
    ] {
        let expected = brute_range(&points, min, max);
        let actual = found(|visit| bush.range(min.0, min.1, max.0, max.1, visit));
        assert_eq!(actual, expected, "rectangle {min:?}..{max:?}");
    }
}

/// And every disc.
#[test]
fn a_radius_query_finds_exactly_what_is_within_it() {
    let points = points(1000);
    let bush = KdBush::new(&points);

    for (at, r) in [
        ((50.0, 50.0), 10.0),
        ((0.0, 0.0), 5.0),
        ((50.0, 50.0), 0.0),
        ((50.0, 50.0), 200.0),
        ((100.0, 100.0), 3.0),
    ] {
        let expected = brute_within(&points, at, r);
        let actual = found(|visit| bush.within(at.0, at.1, r, visit));
        assert_eq!(actual, expected, "disc at {at:?} radius {r}");
    }
}

/// A set below the leaf size never splits, and one point is the degenerate case of that.
#[test]
fn small_sets_are_a_single_leaf() {
    for count in [0usize, 1, 2, 63, 64, 65] {
        let points = points(count);
        let bush = KdBush::new(&points);
        assert_eq!(bush.len(), count);
        assert_eq!(bush.is_empty(), count == 0);

        let all = found(|visit| bush.range(-1.0, -1.0, 200.0, 200.0, visit));
        assert_eq!(all.len(), count, "{count} points should all be in range");
    }
}

/// Duplicated coordinates do not lose a point.
///
/// The partition compares on one axis and swaps equal elements around; a select that dropped
/// one would show here and nowhere else, since the scatter above has no repeats.
#[test]
fn coincident_points_are_all_found() {
    let mut points = points(300);
    for point in points.iter_mut().take(150) {
        *point = (42.0, 17.0);
    }
    let bush = KdBush::new(&points);

    let at_the_pile = found(|visit| bush.within(42.0, 17.0, 0.0, visit));
    assert_eq!(
        at_the_pile.len(),
        150,
        "every copy of the repeated point should answer"
    );
    let everything = found(|visit| bush.range(-1.0, -1.0, 200.0, 200.0, visit));
    assert_eq!(everything.len(), 300);
}
