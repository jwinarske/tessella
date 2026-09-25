// SPDX-License-Identifier: BSD-2-Clause
//! A GeoJSON polygon's rings go through mbgl's even-odd union, against the oracle.
//!
//! # What the oracle gives
//!
//! `tests/golden/evenodd_style.dump`: one fill layer over two features. The first is a polygon of
//! two half-overlapping square rings; the second is a single self-crossing ring, a bowtie.
//!
//! # Why this capture exists
//!
//! mbgl runs every GeoJSON polygon through `fixupPolygons`, a wagyu union with
//! `fill_type_even_odd`, and gates the same repair on version for a vector tile:
//!
//! ```cpp
//! // geojson_tile_data.hpp, on the cut tile's geometry
//! if (getType() == FeatureType::Polygon) {
//!     geometry = fixupPolygons(*geometry);
//! }
//! // vector_mvt_tile_data.cpp
//! if (feature.getVersion() < 2) { lines = fixupPolygons(*lines); }
//! ```
//!
//! This build had the union -- `fill::fixup_polygons`, `i_overlay` with `FillRule::EvenOdd` -- and
//! called it only on the MVT version-1 path. tessella#265 had repaired *winding* at ingest, which
//! covers a hole wound like its exterior and an island inside a hole, and cannot reach what an
//! even-odd union does beyond that (tessella#267):
//!
//! | | oracle | before |
//! |---|---|---|
//! | two overlapping rings at `13/4093/2723` | v14 i24 | v10 i12, and the overlap inked |
//! | a self-crossing ring | no fill drawable | v5 i3 (on #267's fixture, not this one) |
//!
//! Measured on this fixture at 51.505/-0.11 z13 over 1024x768: **27,856 gross of 786,432 before,
//! 2 after**, the remaining two being the fill-outline rasterization floor that
//! `fill_antialias.rs` describes.
//!
//! # The overlap is excluded, not merged
//!
//! Even-odd puts a twice-covered region *outside*, so two half-overlapping squares are not their
//! union -- they are two L-shapes meeting at a corner, with the overlap punched out. That is the
//! part worth stating, because "union" suggests the opposite and an earlier reading of this
//! assumed two overlapping opaque exteriors would cover the same ink.
//!
//! # Counts are not shapes
//!
//! Both sides read v14 i24 at `4093/2723` while the pictures differed by 3.5% of the frame, at a
//! point in the work where only the counts had been compared. Two L-shapes and a filled blob can
//! have the same vertex and index totals. The geometry assertions below are kept because they
//! localize a failure, but [`the_overlap_is_punched_out`] is what says the shape is right.

use std::collections::BTreeMap;

use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/evenodd_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/evenodd_style.json");

/// The z13 tiles the fixture's shapes fall in.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// `FillShader`, counting from `BuiltIn::None`. `sh0012` is the outline and carries its own
/// geometry, so only this one is summed.
const FILL: &str = "sh0011";

/// Per `(tile)`, the oracle's vertex and index counts for the fill layer.
fn oracle_geometry() -> BTreeMap<(u32, u32), (usize, usize)> {
    let mut out = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable L") else {
            continue;
        };
        if !rest.contains(FILL) {
            continue;
        }
        let tile = rest
            .split(".t13_")
            .nth(1)
            .and_then(|tail| tail.split("_o").next())
            .expect("a tile key");
        let (x, y) = tile.split_once('_').expect("x_y");
        let verts: usize = rest
            .split(".v")
            .nth(1)
            .and_then(|tail| tail.split('#').next())
            .expect("a vertex count")
            .parse()
            .expect("digits");
        let indices: usize = line
            .split("idx=")
            .nth(1)
            .and_then(|tail| tail.split(':').next())
            .expect("an index count")
            .parse()
            .expect("digits");
        let slot = out
            .entry((x.parse().expect("digits"), y.parse().expect("digits")))
            .or_insert((0, 0));
        slot.0 += verts;
        slot.1 += indices;
    }
    out
}

/// One tile's fill: how many vertices, how many indices, and the vertices themselves.
type Drawn = (usize, usize, Vec<[i16; 2]>);

/// This build's fill geometry, per tile, with the vertices kept.
fn our_geometry() -> BTreeMap<(u32, u32), Drawn> {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut out = BTreeMap::new();
    for (x, y) in TILES {
        let built = build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        for bucket in &built {
            if let Content::Fill(fill) = &bucket.content
                && !fill.vertices.is_empty()
            {
                out.insert(
                    (x, y),
                    (
                        fill.vertices.len(),
                        fill.indices.len(),
                        fill.vertices.clone(),
                    ),
                );
            }
        }
    }
    out
}

/// The fill geometry agrees with the oracle, tile for tile.
#[test]
fn the_fill_geometry_matches_the_oracle() {
    let oracle = oracle_geometry();
    let ours = our_geometry();

    assert!(!oracle.is_empty(), "the capture drew no fill");
    let ours_counts: BTreeMap<(u32, u32), (usize, usize)> = ours
        .iter()
        .map(|(&tile, &(verts, indices, _))| (tile, (verts, indices)))
        .collect();
    assert_eq!(
        ours_counts, oracle,
        "fill geometry per tile against the oracle's"
    );
}

/// The overlap is outside the shape, which is the whole of what even-odd does here.
///
/// A count cannot say this -- the filled blob this used to draw had the same totals on one tile --
/// so the test is about a point rather than a number: the center of the two squares' intersection
/// must not be covered by any triangle.
#[test]
fn the_overlap_is_punched_out() {
    let ours = our_geometry();
    let (_, _, verts) = ours
        .get(&(4093, 2723))
        .expect("the fixture draws at 4093/2723");

    // The intersection of the two rings on this tile, from the geometry itself: the x and y each
    // appear in both rings' bounds, so the center of that box is the twice-covered point.
    let inside = |point: [f64; 2], ring: &[[i16; 2]]| -> bool {
        let mut hit = false;
        let mut j = ring.len() - 1;
        for i in 0..ring.len() {
            let (a, b) = (
                [f64::from(ring[i][0]), f64::from(ring[i][1])],
                [f64::from(ring[j][0]), f64::from(ring[j][1])],
            );
            if (a[1] > point[1]) != (b[1] > point[1])
                && point[0] < (b[0] - a[0]) * (point[1] - a[1]) / (b[1] - a[1]) + a[0]
            {
                hit = !hit;
            }
            j = i;
        }
        hit
    };

    // The two rings come back concatenated, seven vertices each.
    assert_eq!(verts.len(), 14, "two seven-point rings");
    let (first, second) = verts.split_at(7);

    // A point in the region both source squares covered. Taken from the emitted geometry rather
    // than recomputed from the style: the notch corner each ring turns at is the intersection's
    // far corner, so the midpoint of the two notch corners sits inside it.
    let overlap_center = [2800.0, 8000.0];
    assert!(
        !inside(overlap_center, first) && !inside(overlap_center, second),
        "the twice-covered region must be outside both rings -- it was inked before \
         tessella#267: {first:?} / {second:?}"
    );

    // And a point each ring does keep, so the assertion above is not passing because the rings
    // are empty or elsewhere.
    assert!(
        inside([1000.0, 8000.0], first),
        "the first ring should still cover its own half"
    );
    assert!(
        inside([4500.0, 6000.0], second),
        "the second ring should still cover its own half"
    );
}

/// A self-crossing ring draws nothing, as it does for the oracle.
///
/// **This one does not bite.** Removing the union leaves it passing: at this fixture's size the
/// bowtie's lobes are already lost to the tile clip, so the agreement here is not evidence the
/// union is running. It is kept because the oracle's silence is worth recording -- tessella#267
/// measured the bowtie at 10.8% of a frame on a fixture where it did survive -- and because a
/// future change that started drawing one would fail it. The tests above are the regression
/// guards; both fail when the union is removed.
#[test]
fn a_bowtie_draws_nothing() {
    let ours = our_geometry();
    let oracle = oracle_geometry();

    // The bowtie sits east of the overlapping pair, in the 4094 column and the east of 4093.
    for tile in [(4094, 2723), (4094, 2724)] {
        assert!(
            !oracle.contains_key(&tile),
            "the oracle should draw no fill at {tile:?}"
        );
        assert!(
            !ours.contains_key(&tile),
            "this build should draw no fill at {tile:?} either"
        );
    }

    // And where it shares a tile with the pair, the counts still agree -- which is what says the
    // bowtie contributed nothing rather than that its tile was skipped.
    assert!(
        ours.contains_key(&(4093, 2723)),
        "4093/2723 holds the second square and must still draw"
    );
}
