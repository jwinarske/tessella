//! The pole of inaccessibility, against the oracle's own answers.
//!
//! Every expected number here was read off `mbgl-render`'s frame: the scene `arealabel_p` draws
//! one point-placed label per shape over a plain ground, and the center of each label's ink is
//! the anchor mbgl chose. The shapes below are that scene's, in the frame's own pixels, so these
//! are measurements rather than this port's own output written down.

use tessella_layout::polylabel::{Point, polylabel};

/// One pixel of precision, which is what mbgl passes in tile units: `EXTENT / tileSize`. The
/// shapes here are in screen pixels, where the same precision is 1.
const PRECISION: f64 = 1.0;

fn closed(points: &[(f64, f64)]) -> Vec<Point> {
    let mut out: Vec<Point> = points.iter().map(|&(x, y)| [x, y]).collect();
    out.push(out[0]);
    out
}

fn pole(rings: &[Vec<Point>]) -> Option<Point> {
    let borrowed: Vec<&[Point]> = rings.iter().map(Vec::as_slice).collect();
    polylabel(&borrowed, PRECISION)
}

/// Whether a point is inside the polygon, by the crossing rule.
fn inside(point: Point, rings: &[Vec<Point>]) -> bool {
    let mut inside = false;
    for ring in rings {
        let len = ring.len();
        let mut j = len - 1;
        for i in 0..len {
            let (a, b) = (ring[i], ring[j]);
            if (a[1] > point[1]) != (b[1] > point[1])
                && (point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0])
            {
                inside = !inside;
            }
            j = i;
        }
    }
    inside
}

fn ell() -> Vec<Point> {
    closed(&[
        (132.0, 284.0),
        (362.0, 284.0),
        (362.0, 154.0),
        (262.0, 154.0),
        (262.0, 94.0),
        (132.0, 94.0),
    ])
}

fn square() -> Vec<Point> {
    closed(&[
        (662.0, 674.0),
        (902.0, 674.0),
        (902.0, 484.0),
        (662.0, 484.0),
    ])
}

fn near(got: Option<Point>, want: Point) {
    let at = got.expect("a pole");
    assert!(
        (at[0] - want[0]).abs() < 0.5 && (at[1] - want[1]).abs() < 0.5,
        "expected {want:?}, got {at:?}"
    );
}

#[test]
fn a_rectangle_poles_at_its_center() {
    near(pole(&[square()]), [782.0, 579.0]);
}

#[test]
fn an_ell_poles_inside_its_own_arm() {
    near(pole(&[ell()]), [208.07, 207.93]);
}

#[test]
fn a_hole_pushes_the_pole_out_of_itself() {
    let outer = closed(&[(642.0, 284.0), (902.0, 284.0), (902.0, 94.0), (642.0, 94.0)]);
    let hole = closed(&[
        (712.0, 244.0),
        (712.0, 134.0),
        (832.0, 134.0),
        (832.0, 244.0),
    ]);
    let rings = vec![outer, hole];
    near(pole(&rings), [866.88, 130.37]);
    assert!(
        inside(pole(&rings).expect("a pole"), &rings),
        "and it is in the ring rather than in the hole"
    );
}

/// The reason the pole and not the centroid: a shape can have its own centroid outside it, and a
/// label there is off the thing it names.
#[test]
fn a_crescent_poles_inside_itself_where_its_centroid_is_not() {
    let crescent = closed(&[
        (0.0, 0.0),
        (300.0, 0.0),
        (300.0, 40.0),
        (40.0, 40.0),
        (40.0, 260.0),
        (300.0, 260.0),
        (300.0, 300.0),
        (0.0, 300.0),
    ]);
    let rings = vec![crescent];
    let centroid = [
        rings[0].iter().map(|at| at[0]).sum::<f64>() / rings[0].len() as f64,
        rings[0].iter().map(|at| at[1]).sum::<f64>() / rings[0].len() as f64,
    ];
    assert!(!inside(centroid, &rings), "the centroid is in the notch");
    assert!(
        inside(pole(&rings).expect("a pole"), &rings),
        "and the pole is not"
    );
}

/// The one shape the search cannot start on: a cell size of zero has no grid to lay down, and
/// mbgl returns the envelope's corner rather than dividing by it.
#[test]
fn a_ring_of_no_width_poles_at_its_corner() {
    let flat = closed(&[(10.0, 20.0), (300.0, 20.0)]);
    assert_eq!(pole(&[flat]), Some([10.0, 20.0]));
}

#[test]
fn nothing_to_measure_is_no_pole() {
    assert_eq!(pole(&[]), None);
    assert_eq!(pole(&[Vec::new()]), None);
}

/// A shape whose pole the queue's order decides.
///
/// Two cells tie closely enough that which one the queue hands over first changes the answer, and
/// the two candidates are 37 tile units apart -- 2.3 pixels, which is a label visibly off its
/// shape. This is tile (8802, 5373)'s own units and the shape `arealabel_p` draws there: the
/// oracle puts that label's ink at screen (763.5, 504.5), which is this point and not the other.
/// Answered with a `BinaryHeap`, or with the queue sorted instead of sifted, the same scene reads
/// 242 gross pixels.
#[test]
fn the_queues_order_decides_which_of_two_poles() {
    let star = closed(&[
        (7096.0, 4096.0),
        (5966.0, 6441.0),
        (3428.0, 7021.0),
        (2294.0, 4964.0),
        (2294.0, 3228.0),
        (3206.0, 196.0),
        (6590.0, 969.0),
    ]);
    let at = polylabel(&[&star], 16.0).expect("a pole");
    assert!(
        (at[0] - 4648.1).abs() < 0.1 && (at[1] - 3713.1).abs() < 0.1,
        "expected (4648.1, 3713.1), got {at:?}"
    );
}
