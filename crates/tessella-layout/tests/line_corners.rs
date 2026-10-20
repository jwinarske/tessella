// SPDX-License-Identifier: BSD-2-Clause
//! Two of mbgl's line-bucket constants, and the geometry that makes each one visible.
//!
//! Both were found by mutation rather than by reading. `SHARP_CORNER_OFFSET` 15 → 14 and
//! `DEG_PER_TRIANGLE` 20 → 21 each left **all 2,433 tests passing**, the golden-oracle diff
//! included — and that diff is byte-exact on the line layer over six tiles. What that says is
//! not that the oracle is weak but what those six tiles hold: no corner sharper than 75 degrees,
//! and no join that resolves to fake round. A byte-exact comparison over features that never
//! reach a branch says nothing about the branch, and a constant with no data behind it is live
//! code free to drift.
//!
//! So each test carries geometry chosen to reach its branch, and a control that does not reach
//! it — because an assertion that holds either way pins nothing.

use tessella_layout::line::{LineBucket, LineCap, LineJoin, LineOptions};

/// The centerline points the bucket emitted, in order, without repeats.
///
/// `pos_normal` is the point doubled with the side and cap flags in its low bits, so the point
/// is the arithmetic shift back. Two vertices share each point and a join emits several, which
/// is why the repeats are dropped here and counted directly where a count is the subject.
fn centers(bucket: &LineBucket) -> Vec<(i16, i16)> {
    let mut out: Vec<(i16, i16)> = Vec::new();
    for v in &bucket.vertices {
        let at = (v.pos_normal[0] >> 1, v.pos_normal[1] >> 1);
        if out.last() != Some(&at) {
            out.push(at);
        }
    }
    out
}

/// How many vertices sit exactly on `point`.
fn vertices_at(bucket: &LineBucket, point: [i16; 2]) -> usize {
    bucket
        .vertices
        .iter()
        .filter(|v| (v.pos_normal[0] >> 1, v.pos_normal[1] >> 1) == (point[0], point[1]))
        .count()
}

/// A corner: `arm` tile units east, then `arm` more at `turn` degrees off that.
fn corner(arm: f64, turn: f64) -> [[i16; 2]; 3] {
    let radians = turn.to_radians();
    #[allow(clippy::cast_possible_truncation)]
    let end = [
        (arm + arm * radians.cos()) as i16,
        (arm * radians.sin()) as i16,
    ];
    #[allow(clippy::cast_possible_truncation)]
    [[0, 0], [arm as i16, 0], end]
}

fn build(points: &[[i16; 2]], join: LineJoin) -> LineBucket {
    let mut bucket = LineBucket::default();
    let options = LineOptions {
        join,
        begin_cap: LineCap::Butt,
        end_cap: LineCap::Butt,
        ..LineOptions::default()
    };
    bucket.add_geometry(points, &options);
    bucket
}

/// mbgl's `SHARP_CORNER_OFFSET` is **15 screen pixels**, taken in tile units: a tile is 8192
/// units over 512 pixels, so fifteen pixels is 240 units at the default overscaling of one.
///
/// Restated from mbgl's own two numbers rather than read from the implementation, so that moving
/// the implementation's constant disagrees with this one.
const EXPECTED_OFFSET: i16 = 15 * (8192 / 512);

/// A sharp corner gets an extra point on each arm, fifteen pixels from the corner.
///
/// A 120-degree turn is past the 75-degree threshold, and arms of 1200 units clear the
/// `2 * offset` guard that skips the insertion where there is nowhere to put the point.
#[test]
fn a_sharp_corner_puts_its_extra_points_fifteen_pixels_out() {
    let points = corner(1200.0, 120.0);
    let at = centers(&build(&points, LineJoin::Miter));
    let [start, elbow, end] = points;

    assert_eq!(
        at.len(),
        5,
        "three given points and one extra on each arm: {at:?}"
    );
    assert_eq!((at[0].0, at[0].1), (start[0], start[1]));
    assert_eq!((at[2].0, at[2].1), (elbow[0], elbow[1]));
    assert_eq!((at[4].0, at[4].1), (end[0], end[1]));

    // The near arm runs due east, so the offset is the whole of the x difference.
    assert_eq!(
        (elbow[0] - at[1].0, at[1].1),
        (EXPECTED_OFFSET, 0),
        "the point before the corner sits {EXPECTED_OFFSET} units back"
    );
    // The far arm leaves at 120 degrees, so the offset is spread over both axes and its length
    // is what the constant fixes. One unit of slack for the i16 rounding of each component.
    let dx = f64::from(at[3].0 - elbow[0]);
    let dy = f64::from(at[3].1 - elbow[1]);
    let length = dx.hypot(dy);
    assert!(
        (length - f64::from(EXPECTED_OFFSET)).abs() <= 1.0,
        "the point after the corner is {length} units out, not {EXPECTED_OFFSET}"
    );
}

/// The control: a corner inside the threshold gets no extra points.
///
/// Without it, the test above would pass on a build that inserted points at every corner.
#[test]
fn a_shallow_corner_gets_no_extra_points() {
    let at = centers(&build(&corner(1200.0, 20.0), LineJoin::Miter));
    assert_eq!(
        at.len(),
        3,
        "a 20-degree corner emits what it was given: {at:?}"
    );
}

/// The other control: a sharp corner on arms too short to hold the offset gets none either.
///
/// mbgl inserts only where the segment is longer than twice the offset — 480 units — because a
/// shorter one would put the new point past the far end of the arm.
#[test]
fn a_sharp_corner_on_short_arms_gets_no_extra_points() {
    let at = centers(&build(&corner(300.0, 120.0), LineJoin::Miter));
    assert_eq!(at.len(), 3, "arms of 300 units are inside 2 * 240: {at:?}");
}

/// A fake-round join fans one triangle per twenty degrees of arc.
///
/// `DEG_PER_TRIANGLE` is mbgl's 20. This corner's arc measures about 90 degrees, so the fan is
/// five triangles and eight vertices land on the corner; at 21 degrees a triangle it is four and
/// seven. **That is why the turn is 92.5 degrees and not 60**: at 60 degrees the fan is three
/// triangles either way, so a count there would pin nothing — which is the shape of the gap this
/// test closes.
///
/// Reaching the branch at all takes `LineJoin::Round` with a miter length between the round limit
/// and two: below the limit the join flattens to a miter, above two it becomes a bevel.
#[test]
fn a_fake_round_join_fans_one_triangle_each_twenty_degrees() {
    let points = corner(400.0, 92.5);
    let round = build(&points, LineJoin::Round);
    assert_eq!(
        vertices_at(&round, points[1]),
        8,
        "a 90-degree arc at 20 degrees a triangle is a five-triangle fan"
    );

    // The control: the same corner mitered has no fan, so the count above is the fan's and not
    // the corner's.
    let mitered = build(&points, LineJoin::Miter);
    assert_eq!(
        vertices_at(&mitered, points[1]),
        2,
        "a mitered corner is two vertices, fan or no fan"
    );
}
