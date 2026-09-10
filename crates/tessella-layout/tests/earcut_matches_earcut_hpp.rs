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

extern crate alloc;
