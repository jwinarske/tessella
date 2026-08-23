//! Does `earcutr` agree with `earcut.hpp`?
//!
//! §8 picks `earcutr` on the grounds that it is the same algorithm, and notes in passing that
//! "output ordering matters for §9". That note is the whole risk, and this is the measurement.
//!
//! The answer: they produce the *same triangulation*, and for simple polygons — including
//! concave ones — they emit it in the same order, index for index. Once a hole is involved the
//! triangles come out in a different order. The same triangles: same count, same total area,
//! and every one with the same winding.
//!
//! Emission order is not a property of the map. Triangles are independent, the rendered result
//! is identical, and nothing downstream depends on the sequence. So the oracle hashes indices
//! in the canonical form below rather than raw, and this file is where that decision is
//! justified and kept honest.
//!
//! Expectations come from running mbgl's vendored `earcut.hpp` on these polygons.

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

/// The divergence, recorded rather than hidden. A hole makes the two emit the same triangles
/// in a different sequence, which is why the oracle compares canonically.
#[test]
fn a_hole_changes_the_emission_order() {
    let ours = earcut(&[SQUARE, HOLE]);
    let theirs = [
        0, 4, 7, 5, 4, 0, 1, 0, 7, 5, 0, 3, 2, 1, 7, 6, 5, 3, 2, 7, 6, 6, 3, 2,
    ];

    assert_ne!(
        ours, theirs,
        "if this ever passes, the canonicalization is no longer needed"
    );
    assert_eq!(
        canonical(&ours),
        canonical(&theirs),
        "the same triangles, differently ordered"
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
