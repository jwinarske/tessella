//! mbgl's own `TileCover` expectations, ported verbatim.
//!
//! The rectangle tests in `polygon_cover.rs` check this port against something already verified,
//! but a rectangle exercises none of what makes the scanline hard: concavity, holes, disjoint
//! parts, edges that cross a row without touching either of its corners. These are mbgl's
//! numbers for exactly those, taken from `test/util/tile_cover.test.cpp`.

use std::collections::BTreeSet;

use tessella_tile::polygon::{Cover, Polygon};

fn tiles(parts: &[Polygon], z: u8) -> BTreeSet<(u32, u32)> {
    Cover::shape(parts, z)
        .map(|tile| (tile.x, tile.y))
        .collect()
}

/// The concave outline mbgl's `GeomPolygon` and `GeomMultiPolygon` share.
fn europe() -> Vec<[f64; 2]> {
    vec![
        [5.097_656_25, 53.067_626_642_387_374],
        [2.373_046_875, 43.389_081_939_117_496],
        [-4.746_093_75, 48.458_351_882_808_66],
        [-1.494_140_625, 37.090_239_803_072_08],
        [22.587_890_625, 36.244_273_184_939_09],
        [31.640_625, 46.134_170_046_243_26],
        [17.841_796_875, 54.724_620_194_924_5],
        [5.097_656_25, 53.067_626_642_387_374],
    ]
}

/// mbgl `TileCover.GeomPolygon`: a concave ring with a hole in it.
///
/// The hole is the point. Tile 136/87 sits inside it and must not be covered, while 134/87 and
/// 139/87 are on either side and must be — which is the non-zero rule doing its job rather than
/// the fill simply reaching everywhere between the outermost edges.
#[test]
fn a_concave_ring_with_a_hole() {
    let polygon = Polygon::new(europe()).with_hole(vec![
        [19.687_5, 49.667_627_822_621_94],
        [22.851_562_5, 43.516_688_535_029_06],
        [13.623_046_875, 45.089_035_564_831_036],
        [16.347_656_25, 39.095_962_936_305_476],
        [5.185_546_875, 41.244_772_343_082_076],
        [8.701_171_874_999_998, 50.233_151_832_472_245],
        [19.687_5, 49.667_627_822_621_94],
    ]);
    let covered = tiles(&[polygon], 8);

    assert!(covered.contains(&(134, 87)), "west of the hole");
    assert!(covered.contains(&(139, 87)), "east of the hole");
    assert!(!covered.contains(&(136, 87)), "the hole itself");
}

/// mbgl `TileCover.GeomMultiPolygon`: two disjoint parts, 424 tiles.
///
/// An exact count, which is the strongest form this can take — it catches a fill that runs one
/// tile wide, a row skipped at a lobe boundary, and a merge that bridges the gap between the
/// parts. The same shape as above without its hole, so 136/87 is now covered where it was not.
#[test]
fn two_disjoint_parts() {
    let parts = [
        Polygon::new(europe()),
        Polygon::new(vec![
            [59.150_390_625, 45.460_130_637_921_004],
            [65.126_953_125, 41.112_468_789_180_88],
            [69.169_921_875, 47.457_808_530_750_31],
            [63.896_484_375, 50.064_191_736_659_104],
            [59.150_390_625, 45.460_130_637_921_004],
        ]),
    ];
    let covered = tiles(&parts, 8);

    assert_eq!(covered.len(), 424);
    assert!(covered.contains(&(139, 87)));
    assert!(covered.contains(&(136, 87)), "no hole this time");
    assert!(covered.contains(&(174, 94)), "the eastern part");
}

/// mbgl `TileCover.GeomSanFranciscoPoly`: an exact set, not just a count.
///
/// A real city outline with many short edges, at a zoom where it covers six tiles. Every tile
/// named and no others, so a fill that leaked by one would show.
#[test]
fn a_city_outline() {
    let san_francisco = Polygon::new(vec![
        [-122.514_381_408_691_4, 37.779_127_216_982_424],
        [-122.508_115_768_432_62, 37.727_212_390_567_09],
        [-122.503_137_588_500_99, 37.708_201_780_639_29],
        [-122.393_875_122_070_3, 37.707_454_835_665_274],
        [-122.375_679_016_113_28, 37.706_639_978_016_84],
        [-122.362_976_074_218_74, 37.713_430_184_662_85],
        [-122.354_736_328_125, 37.727_280_276_860_036],
        [-122.364_692_687_988_28, 37.738_684_290_657_97],
        [-122.380_142_211_914_08, 37.754_429_802_955_71],
        [-122.383_918_762_207_02, 37.787_538_738_205_29],
        [-122.359_199_523_925_78, 37.806_528_974_172_5],
        [-122.356_796_264_648_44, 37.820_632_846_207_864],
        [-122.371_215_820_312_5, 37.835_276_322_922_695],
        [-122.381_858_825_683_6, 37.829_581_982_839_02],
        [-122.371_902_465_820_31, 37.807_885_232_791_69],
        [-122.387_351_989_746_08, 37.791_337_175_930_686],
        [-122.409_667_968_749_99, 37.812_767_557_570_204],
        [-122.464_256_286_621_08, 37.807_071_480_609_274],
        [-122.468_032_836_914_05, 37.810_326_435_534_755],
        [-122.479_019_165_039_06, 37.811_682_624_407_36],
        [-122.489_662_170_410_16, 37.789_166_663_996_49],
        [-122.505_798_339_843_75, 37.787_810_061_660_96],
        [-122.514_381_408_691_4, 37.779_127_216_982_424],
    ]);

    let covered = tiles(&[san_francisco], 12);
    let expected: BTreeSet<(u32, u32)> = [
        (654, 1582),
        (655, 1582),
        (654, 1583),
        (655, 1583),
        (654, 1584),
        (655, 1584),
    ]
    .into_iter()
    .collect();
    assert_eq!(covered, expected);
}

/// mbgl `TileCover.GeomInvalid`: an open triangle is closed rather than refused.
///
/// A ring the user has not closed is the normal case coming out of a drawing tool, not an
/// error, and mbgl states the exact four tiles it should produce.
#[test]
fn an_open_triangle_is_closed() {
    let triangle = Polygon::new(vec![[1.0, 34.2], [1.0, 34.4], [0.5, 34.3]]);
    let covered = tiles(&[triangle], 10);
    let expected: BTreeSet<(u32, u32)> = [(513, 407), (514, 407), (513, 408), (514, 408)]
        .into_iter()
        .collect();
    assert_eq!(covered, expected);
}

/// mbgl `TileCover.GeomInvalid`: a ring of one point covers nothing.
#[test]
fn a_single_point_ring_covers_nothing() {
    assert!(tiles(&[Polygon::new(vec![[1.0, 35.0]])], 16).is_empty());
}
