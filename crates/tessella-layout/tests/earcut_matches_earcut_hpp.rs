// SPDX-License-Identifier: Apache-2.0

//! The triangulation is `earcut.hpp`'s, not merely a valid one.
//!
//! §9.1 diffs this crate's frames against mbgl's pixel for pixel, and mbgl triangulates fills with
//! `earcut.hpp`. Two triangulations of the same rings can both be correct and still paint
//! differently -- on geometry that has no valid triangulation at all, which a real vector tile
//! carries plenty of, they paint *very* differently. So the dependency is vendored and this pins
//! the case that made us look; see `vendor/earcutr/PATCH.md`.

use tessella_layout::fill::{self, Ring};

/// One group of a Shanghai water feature at z12, tile 12/3430/1674, in the tile's own 4096 units.
///
/// The `water` layer carries rivers as LineStrings, and a fill layer closes them into lassos --
/// which mbgl does too, so this is geometry both renderers really are asked to fill. Five open
/// lines, forty-five points: above `earcutr`'s hashing threshold and below `earcut.hpp`'s, which
/// is the whole of the bug.
fn lasso() -> Vec<Ring> {
    alloc_rings(&[
        &[
            [2538, 1654],
            [2396, 1346],
            [2382, 1291],
            [2309, 1166],
            [2230, 989],
            [2183, 894],
            [2157, 828],
            [2079, 679],
        ],
        &[
            [2230, 989],
            [1867, 1145],
            [1830, 1158],
            [1822, 1156],
            [1820, 1100],
            [1783, 1059],
            [1768, 974],
            [1771, 957],
            [1789, 943],
            [1787, 933],
            [1735, 844],
        ],
        &[
            [2404, 539],
            [2504, 778],
            [2537, 840],
            [2541, 858],
            [2583, 904],
            [2620, 973],
            [2636, 989],
            [2748, 1049],
            [2812, 1113],
            [2848, 1218],
            [2937, 1334],
            [2997, 1551],
        ],
        &[
            [3961, 264],
            [3940, 232],
            [3890, 174],
            [3878, 115],
            [3852, 60],
            [3840, -10],
            [3812, -64],
        ],
        &[
            [4160, 2754],
            [3984, 2066],
            [3880, 1682],
            [3820, 1321],
            [3735, 905],
            [3731, 871],
            [3735, 801],
        ],
    ])
}

fn alloc_rings(rings: &[&[[i16; 2]]]) -> Vec<Ring> {
    rings.iter().map(|ring| ring.to_vec()).collect()
}

/// Twelve triangles covering 3,092,779 doubled units, which is what `earcut.hpp` answers.
///
/// Both numbers were read off mbgl's own vendored `earcut.hpp` run on these exact rings. The
/// thirteenth triangle the unpatched dependency adds spans ground no ring covers, and it reached
/// the screen as a wedge of water lying across land -- 6,282 gross pixels at that camera against
/// the oracle, and 1 with it gone.
#[test]
fn a_lasso_triangulates_the_way_mbgl_triangulates_it() {
    let rings = lasso();
    let groups = fill::classify_rings(&rings);
    assert_eq!(groups.len(), 1, "these five rings are one polygon");

    let feature: Vec<&[Ring]> = alloc::vec![rings.as_slice()];
    let (bucket, _) = fill::build_features_tracked(&feature);

    assert_eq!(
        bucket.indices.len() / 3,
        12,
        "earcut.hpp answers twelve triangles for this polygon"
    );

    let doubled: i64 = bucket
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .map(|corner| {
            let point = |index: u16| bucket.vertices[index as usize];
            let (a, b, c) = (point(corner[0]), point(corner[1]), point(corner[2]));
            ((i64::from(b[0]) - i64::from(a[0])) * (i64::from(c[1]) - i64::from(a[1]))
                - (i64::from(c[0]) - i64::from(a[0])) * (i64::from(b[1]) - i64::from(a[1])))
            .abs()
        })
        .sum();
    assert_eq!(doubled, 3_092_779, "and covers this much doubled area");
}

/// `earcutr::earcut`, on a thread, with a limit: a regression here is a hang, and a hang should
/// fail this test rather than hold the whole suite until the runner gives up.
fn earcut_within_a_second(flat: &[f64], holes: &[usize]) -> Vec<usize> {
    let (flat, holes) = (flat.to_vec(), holes.to_vec());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(earcutr::earcut(&flat, &holes, 2)));
    rx.recv_timeout(std::time::Duration::from_secs(1))
        .expect("earcut did not return")
        .expect("earcut refused the rings")
}

/// A hole no segment of the outer ring lies to the left of has no bridge, and is left out.
///
/// `find_hole_bridge` answers NULL for it. `earcut.hpp` checks for that and skips the hole; the
/// port spliced the NULL node in regardless, linking the list's sentinel into the rings. On most
/// input that corrupts only the hole's own list, which nothing reads again -- this square is one,
/// and it triangulated correctly by luck. What it answers is what `earcut.hpp` answers.
#[test]
fn a_hole_with_no_bridge_is_left_out() {
    let flat = [
        1000.0, 0.0, 2000.0, 0.0, 2000.0, 1000.0, 1000.0, 1000.0, // the outer square
        100.0, 200.0, 100.0, 800.0, 700.0, 800.0, 700.0, 200.0, // a hole wholly to its left
    ];
    assert_eq!(earcut_within_a_second(&flat, &[4]), [3, 0, 1, 1, 2, 3]);
}

/// And where the corruption was not harmless, it was a hang.
///
/// Holes that collapse to runs of one repeated point, on a 512-unit grid: the sentinel spliced in
/// ahead of them leaves `filter_points` a cycle that never reaches the node it is walking to, and
/// the original never returned. `earcut.hpp` answers the outer ring's two triangles -- these six
/// indices, read off mbgl's vendored copy on these rings -- and so does this now.
///
/// The fill layer does not hand earcut these rings: `classify_rings` drops the zero-area holes
/// first. This is about the triangulator's own contract, which is to return.
#[test]
fn collapsed_holes_with_no_bridge_do_not_hang() {
    let flat = [
        5632.0, 5632.0, 6144.0, 6656.0, 5632.0, 1536.0, 5120.0, 3072.0, // the outer ring
        3072.0, 4096.0, 3072.0, 4096.0, 3072.0, 4096.0, 3072.0,
        4096.0, // one point, four times
        5120.0, 4608.0, 5120.0, 4608.0, 5120.0, 4608.0, 4608.0, 4608.0, 5120.0, 4096.0, 5120.0,
        4096.0, 5120.0, 4096.0, 5120.0, 4096.0, 5120.0, 4096.0, //
        4096.0, 5120.0, 4096.0, 5120.0, 4096.0, 5120.0, 4096.0, 4608.0, 4096.0, 5120.0, 4608.0,
        5120.0, 4096.0, 5120.0, 4096.0, 5120.0, //
        3072.0, 4608.0, 3072.0, 4608.0, 3072.0, 4608.0, 3072.0, 4608.0, 3072.0, 4608.0, 3072.0,
        4608.0, 3072.0, 4096.0, 3072.0, 4096.0, 3072.0, 4096.0,
    ];
    assert_eq!(
        earcut_within_a_second(&flat, &[4, 8, 17, 25]),
        [1, 0, 3, 3, 2, 1]
    );
}

extern crate alloc;
