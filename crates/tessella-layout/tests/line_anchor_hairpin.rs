// SPDX-License-Identifier: BSD-2-Clause
//! A road that doubles back on itself, and where a label may sit on it.
//!
//! Bunker Road in tile 14/2617/6329 of OpenFreeMap's `bright`, which is the `add-a-geojson-line`
//! example's own frame. The road climbs, hairpins twice and comes back, and the question is how
//! many times its name may be written along it.
//!
//! The numbers are the oracle's, taken from the same print placed at the end of `getAnchors` in
//! both renderers: one line of 44 points, 4120 tile units long, `symbol-spacing` of 2000 after
//! the tile ratio, an offset of 1014, a label 598 long, an angle window of 62.4 and the spec's
//! 45 degrees. mbgl answers with ONE anchor and this used to answer with two.

use tessella_layout::anchors::{Anchor, check_max_angle, get_anchors};

/// The road, in tile units, exactly as the tile delivers it.
const BUNKER_ROAD: &[(f32, f32)] = &[
    (4348.0, 540.0),
    (4432.0, 566.0),
    (4592.0, 592.0),
    (4614.0, 512.0),
    (4636.0, 478.0),
    (4650.0, 468.0),
    (4668.0, 462.0),
    (4690.0, 460.0),
    (4740.0, 476.0),
    (4882.0, 576.0),
    (5164.0, 792.0),
    (5360.0, 972.0),
    (5690.0, 1244.0),
    (5780.0, 1338.0),
    (5814.0, 1384.0),
    (5846.0, 1438.0),
    (5884.0, 1490.0),
    (6034.0, 1638.0),
    (6080.0, 1690.0),
    (6118.0, 1750.0),
    (6192.0, 1914.0),
    (6204.0, 1960.0),
    (6210.0, 2006.0),
    (6206.0, 2054.0),
    (6194.0, 2098.0),
    (6174.0, 2140.0),
    (6138.0, 2184.0),
    (6092.0, 2228.0),
    (5990.0, 2314.0),
    (5952.0, 2358.0),
    (5932.0, 2410.0),
    (5932.0, 2464.0),
    (5950.0, 2512.0),
    (6006.0, 2568.0),
    (6064.0, 2602.0),
    (6118.0, 2622.0),
    (6176.0, 2630.0),
    (6240.0, 2612.0),
    (6296.0, 2574.0),
    (6494.0, 2394.0),
    (6534.0, 2370.0),
    (6592.0, 2356.0),
    (6634.0, 2372.0),
    (6660.0, 2408.0),
];

/// `getAnchors`' own arguments for this line, printed from mbgl.
const SPACING: f32 = 2000.0;
/// `text-max-angle`'s spec default, in radians, exactly as the layout computes it.
const MAX_ANGLE: f32 = core::f32::consts::PI / 4.0;
const SHAPED_LEFT: f32 = -69.0;
const SHAPED_RIGHT: f32 = 69.0;
const GLYPH_SIZE: f32 = 24.0;
/// `tilePixelRatio * textMaxSize / glyphSize` -- 8192/(512*2) by 13 over 24, which prints as
/// 4.333 and is not that number.
const BOX_SCALE: f32 = 8.0 * (13.0 / 24.0);
const OVERSCALING: f32 = 2.0;

/// The label is written once, where the oracle writes it.
///
/// The second anchor this used to find sits at 3014 along the line, in the middle of the
/// hairpin: the road turns through more than a right angle inside the window the label covers,
/// and a name laid across that is unreadable. mbgl refuses it and the extra label went on to
/// crowd out the transit stop beside the road, which is most of that example's gap.
#[test]
fn a_road_that_doubles_back_is_named_once() {
    let anchors = get_anchors(
        BUNKER_ROAD,
        SPACING,
        MAX_ANGLE,
        SHAPED_LEFT,
        SHAPED_RIGHT,
        0.0,
        0.0,
        GLYPH_SIZE,
        BOX_SCALE,
        OVERSCALING,
    );
    let points: Vec<(f32, f32)> = anchors.iter().map(|anchor| anchor.point).collect();
    assert_eq!(
        points,
        vec![(5165.0, 793.0)],
        "the oracle finds this one only"
    );
}

/// And the bend check is what refuses the second one, asked directly.
#[test]
fn the_hairpin_refuses_a_label_across_it() {
    // `resample` would have offered this anchor: 3014 along the line, on segment 30.
    let anchor = Anchor {
        point: (6010.0, 2297.0),
        angle: 0.0,
        segment: 30,
    };
    let label_length = (SHAPED_RIGHT - SHAPED_LEFT) * BOX_SCALE;
    let window = 3.0 / 5.0 * GLYPH_SIZE * BOX_SCALE;
    assert!(
        !check_max_angle(BUNKER_ROAD, &anchor, label_length, window, MAX_ANGLE),
        "a label laid across the hairpin was allowed"
    );
}
