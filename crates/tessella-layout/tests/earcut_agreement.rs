//! Does `earcutr` agree with `earcut.hpp`?
//!
//! §8 picks `earcutr` on the grounds that it is the same algorithm, and notes in passing that
//! "output ordering matters for §9". That note is the whole risk, and this is the measurement.
//!
//! The answer: they produce the *same triangulation*, and emit it in the same order, index for
//! index -- holes included. That last part was not always so. The vendored crate followed an
//! older `earcut.js` in how it bridges a hole to its outer ring, and on a polygon with a hole the
//! same triangles came out in another sequence; `vendor/earcutr/PATCH.md` records the port of
//! `earcut.hpp`'s bridging that closed it.
//!
//! Emission order is not a property of the map. Triangles are independent, the rendered result
//! is identical, and nothing downstream depends on the sequence. So the oracle hashes indices
//! in the canonical form below rather than raw, and this file is where that decision is
//! justified and kept honest -- it is what made the old difference harmless, and it is what
//! keeps a future one from being mistaken for a different triangulation.
//!
//! Expectations come from running mbgl's vendored `earcut.hpp` on these polygons.

use tessella_layout::fill::{self, Ring};

/// Canonicalizes a triangle list the way the oracle's dump does.
///
/// Each triangle is rotated to start at its lowest index, then the triangles are sorted.
/// Rotation removes the rotation ambiguity while preserving winding — which *is* a real
/// property, since a reversed triangle is backface-culled — and sorting removes emission
/// order, which is not.
fn canonical(indices: &[usize]) -> Vec<[usize; 3]> {
    let mut triangles: Vec<[usize; 3]> = indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|t| {
            let lowest = (0..3).min_by_key(|&i| t[i]).expect("three vertices");
            [t[lowest], t[(lowest + 1) % 3], t[(lowest + 2) % 3]]
        })
        .collect();
    triangles.sort_unstable();
    triangles
}

fn earcut(rings: &[&[[f64; 2]]]) -> Vec<usize> {
    let mut vertices = Vec::new();
    let mut holes = Vec::new();
    for (index, ring) in rings.iter().enumerate() {
        if index > 0 {
            holes.push(vertices.len() / 2);
        }
        for point in *ring {
            vertices.push(point[0]);
            vertices.push(point[1]);
        }
    }
    earcutr::earcut(&vertices, &holes, 2).expect("tessellates")
}

const SQUARE: &[[f64; 2]] = &[[0.0, 0.0], [0.0, 100.0], [100.0, 100.0], [100.0, 0.0]];
const SQUARE_CW: &[[f64; 2]] = &[[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]];
const CONCAVE: &[[f64; 2]] = &[
    [0.0, 0.0],
    [100.0, 0.0],
    [100.0, 100.0],
    [50.0, 40.0],
    [0.0, 100.0],
];
const HOLE: &[[f64; 2]] = &[[30.0, 30.0], [70.0, 30.0], [70.0, 70.0], [30.0, 70.0]];
const IRREGULAR: &[[f64; 2]] = &[
    [10.0, 0.0],
    [90.0, 12.0],
    [100.0, 60.0],
    [70.0, 100.0],
    [30.0, 95.0],
    [0.0, 55.0],
    [25.0, 45.0],
    [5.0, 20.0],
];

/// Simple polygons agree index for index, with no canonicalization needed. A square is a weak
/// test because almost any correct implementation agrees on it; the concave and irregular
/// cases are where ear-clipping genuinely has choices to make.
#[test]
fn simple_polygons_agree_index_for_index() {
    assert_eq!(earcut(&[SQUARE]), [1, 0, 3, 3, 2, 1]);
    assert_eq!(earcut(&[SQUARE_CW]), [2, 3, 0, 0, 1, 2]);
    assert_eq!(earcut(&[CONCAVE]), [3, 4, 0, 1, 2, 3, 3, 0, 1]);
    assert_eq!(
        earcut(&[IRREGULAR]),
        [6, 7, 0, 0, 1, 2, 2, 3, 4, 4, 5, 6, 6, 0, 2, 2, 4, 6]
    );
}

/// A hole no longer changes the emission order.
///
/// It did: this test used to assert that the sequences differed while the triangles agreed, and
/// said that if it ever passed the canonicalization was no longer needed. It stopped passing when
/// the crate took `earcut.hpp`'s hole bridging, and now the sequence is `earcut.hpp`'s too. The
/// canonicalization stays -- see the module comment -- but it is no longer covering a difference.
#[test]
fn a_hole_leaves_the_emission_order_alone() {
    let ours = earcut(&[SQUARE, HOLE]);
    let theirs = [
        0, 4, 7, 5, 4, 0, 1, 0, 7, 5, 0, 3, 2, 1, 7, 6, 5, 3, 2, 7, 6, 6, 3, 2,
    ];

    assert_eq!(
        ours, theirs,
        "earcut.hpp's triangles, in earcut.hpp's order"
    );
}

/// The property that actually matters: same triangles, same winding, same area. A different
/// triangulation would fail this even though it too would be a valid one.
#[test]
fn the_triangulation_itself_is_identical() {
    let ours = earcut(&[SQUARE, HOLE]);
    let theirs = [
        0, 4, 7, 5, 4, 0, 1, 0, 7, 5, 0, 3, 2, 1, 7, 6, 5, 3, 2, 7, 6, 6, 3, 2,
    ];
    let points: Vec<[f64; 2]> = SQUARE.iter().chain(HOLE).copied().collect();

    let signed = |t: &[usize; 3]| {
        let (a, b, c) = (points[t[0]], points[t[1]], points[t[2]]);
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])) / 2.0
    };
    let total = |ts: &[[usize; 3]]| ts.iter().map(|t| signed(t).abs()).sum::<f64>();

    let ours = canonical(&ours);
    let theirs = canonical(&theirs);

    assert_eq!(ours.len(), 8);
    // A 100x100 square less a 40x40 hole.
    assert!((total(&ours) - 8400.0).abs() < 1e-9, "{}", total(&ours));
    assert!((total(&theirs) - 8400.0).abs() < 1e-9);

    // Winding is a real property: a reversed triangle is backface-culled. Canonicalization
    // preserves it deliberately, so this comparison would catch a flip.
    for (a, b) in ours.iter().zip(&theirs) {
        assert_eq!(a, b);
        assert!(signed(a) > 0.0, "consistent winding: {a:?}");
    }
}

/// Canonicalization must discard order and rotation without discarding winding, or the oracle
/// would stop catching a flipped triangle.
#[test]
fn canonicalization_keeps_winding_and_drops_the_rest() {
    let base = [0, 1, 2, 3, 4, 5];
    // Same triangles, emitted in the other order.
    assert_eq!(canonical(&base), canonical(&[3, 4, 5, 0, 1, 2]));
    // Same triangles, each rotated.
    assert_eq!(canonical(&base), canonical(&[1, 2, 0, 4, 5, 3]));
    // One triangle reversed: a different, culled-differently triangle.
    assert_ne!(canonical(&base), canonical(&[0, 2, 1, 3, 4, 5]));
}

/// A polygon keeps at most five hundred holes, and keeps the largest.
///
/// mbgl's `limitHoles(polygon, 500)` runs before every fill and extrusion triangulation, and it
/// is not an optimization this build is free to skip. earcut eliminates holes by bridging each
/// one to the outer ring through a scan, so the cost is quadratic in the hole count — but the
/// reason it has to be matched is the diff, not the clock: a bucket built without the cap has
/// *more* geometry than the oracle's for the same feature.
#[test]
fn a_polygon_keeps_its_five_hundred_largest_holes() {
    // An exterior big enough to hold everything, then holes of descending size — the smallest
    // first, so a cap that kept the leading holes rather than the largest would be visible.
    let exterior: Ring = vec![[0, 0], [20000, 0], [20000, 20000], [0, 20000]];
    let mut polygon = vec![exterior];
    for index in 0..600i16 {
        let x = (index % 30) * 600 + 10;
        let y = (index / 30) * 600 + 10;
        // Size grows with the index, so the *last* holes are the largest.
        let size = 1 + index / 10;
        polygon.push(vec![
            [x, y],
            [x, y + size],
            [x + size, y + size],
            [x + size, y],
        ]);
    }

    let mut capped = polygon.clone();
    fill::limit_holes(&mut capped);
    assert_eq!(capped.len(), 1 + fill::MAX_HOLES);
    assert_eq!(
        capped[0], polygon[0],
        "the exterior is not ranked with the holes"
    );

    let smallest_kept = capped[1..]
        .iter()
        .map(|ring| fill::signed_area(ring).abs())
        .min()
        .expect("holes");
    let largest_dropped = polygon[1..]
        .iter()
        .filter(|ring| !capped[1..].contains(ring))
        .map(|ring| fill::signed_area(ring).abs())
        .max()
        .expect("something was dropped");
    assert!(
        smallest_kept >= largest_dropped,
        "a dropped hole ({largest_dropped}) is larger than a kept one ({smallest_kept})"
    );
}

/// Under the cap, nothing is touched.
#[test]
fn a_polygon_under_the_cap_is_unchanged() {
    let mut polygon = vec![vec![[0i16, 0], [1000, 0], [1000, 1000], [0, 1000]]];
    for index in 0..10i16 {
        let x = index * 50 + 10;
        polygon.push(vec![[x, 10], [x, 30], [x + 20, 30], [x + 20, 10]]);
    }
    let before = polygon.clone();
    fill::limit_holes(&mut polygon);
    assert_eq!(polygon, before);
}

/// Where holes tie, the cap keeps the ones libstdc++'s `std::nth_element` keeps, in its order.
///
/// Six hundred holes in five sizes, so almost every comparison is a tie. The standard leaves the
/// outcome unspecified; mbgl's oracle is built against libstdc++, and its introselect keeps a
/// definite set in a definite order -- the numbers below are what it answered for these areas.
/// The order is not cosmetic: it is the order the kept holes' vertices are numbered in.
#[test]
fn tied_holes_are_kept_as_libstdcxx_keeps_them() {
    let mut polygon = vec![vec![[0i16, 0], [4000, 0], [4000, 4000], [0, 4000]]];
    for index in 0..600i16 {
        let side = 1 + (index * 7) % 5;
        let (x, y) = ((index % 30) * 100 + 10, (index / 30) * 100 + 10);
        polygon.push(vec![
            [x, y],
            [x, y + side],
            [x + side, y + side],
            [x + side, y],
        ]);
    }
    let holes = polygon[1..].to_vec();

    fill::limit_holes(&mut polygon);
    let kept: Vec<usize> = polygon[1..]
        .iter()
        .map(|ring| {
            holes
                .iter()
                .position(|hole| hole == ring)
                .expect("a kept hole")
        })
        .collect();

    assert_eq!(kept.len(), fill::MAX_HOLES);
    assert_eq!(
        kept[..12],
        [1, 599, 2, 597, 4, 596, 594, 7, 592, 9, 591, 589]
    );
    assert_eq!(kept[kept.len() - 4..], [560, 585, 580, 545]);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for index in &kept {
        for byte in (*index as u64).to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    assert_eq!(
        hash, 0x52ff_e102_fb78_1e44,
        "the whole kept order, as libstdc++ left it"
    );
}

/// The cap is stable: the same rings in, the same holes out.
///
/// mbgl selects with `std::nth_element`, which partitions rather than sorts and promises nothing
/// about ties. A producer that chose differently between two builds of the same tile would emit
/// a geometry the consumer had to re-upload for no reason a viewer could see.
#[test]
fn the_cap_is_stable_across_runs() {
    let mut polygon = vec![vec![[0i16, 0], [30000, 0], [30000, 30000], [0, 30000]]];
    for index in 0..700i16 {
        let x = (index % 26) * 1000 + 10;
        let y = (index / 26) * 1000 + 10;
        // Every hole the same size, so every choice is a tie.
        polygon.push(vec![[x, y], [x, y + 100], [x + 100, y + 100], [x + 100, y]]);
    }
    let mut once = polygon.clone();
    let mut twice = polygon.clone();
    fill::limit_holes(&mut once);
    fill::limit_holes(&mut twice);
    assert_eq!(once, twice);
}
