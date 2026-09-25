// SPDX-License-Identifier: BSD-2-Clause
//! What the even-odd union does to a ring that crosses, touches or doubles back on itself.
//!
//! # What the oracle gives
//!
//! `tests/golden/selfcross_style.dump`: one fill layer over three polygons, each a single ring and
//! each malformed a different way.
//!
//! | feature | ring |
//! |---|---|
//! | bowtie | a quad whose two diagonals genuinely *cross* |
//! | figure-eight | two lobes that meet at a vertex the ring visits twice, crossing nothing |
//! | spur | a square with a zero-area tail retraced out and back |
//!
//! # Why this capture exists
//!
//! tessella#267 gave the GeoJSON path mbgl's `fixupPolygons`, and that rests on an assumption
//! nothing had checked: that `i_overlay`'s `FillRule::EvenOdd` decides these the same way wagyu's
//! `fill_type_even_odd` does. A union is not a single well-defined answer on degenerate input --
//! two libraries can both be defensible and disagree -- so the question is empirical.
//!
//! It came up concretely. While writing tessella#267 a synthetic call to `fixup_polygons` returned
//! **two** contours for a bowtie, which read as a divergence from an oracle that draws none. It was
//! not one: that call fed raw coordinates straight in, outside the clip and quantization the real
//! path applies, and the fixture in that change had a bowtie too small to settle it either way. This
//! is the same question asked at a size where the answer shows.
//!
//! # They agree, and the asymmetry is the interesting part
//!
//! | feature | oracle | this build |
//! |---|---|---|
//! | bowtie | nothing drawn | nothing drawn |
//! | figure-eight | both lobes filled | both lobes filled |
//! | spur | the square, tail dropped | the square, tail dropped |
//!
//! **1 gross of 786,432** at 51.505/-0.11 z13 over 1024x768.
//!
//! A crossing annihilates the ring and a *touching* does not, which is not what "even-odd" suggests
//! on its own -- both shapes look like an hourglass and only one survives. What separates them is
//! whether the boundary intersects itself at a point the ring actually visits. Recorded as measured
//! on both sides rather than explained from wagyu's internals, which is not something this build
//! reimplements.

use std::collections::BTreeMap;

use tessella_orchestrate::tile::{Content, TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/selfcross_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/selfcross_style.json");

/// The z13 tiles the fixture's shapes fall in, plus the two the bowtie would occupy if it drew.
const TILES: [(u32, u32); 8] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
    (4095, 2723),
    (4095, 2724),
];

/// `FillShader`. `sh0012` is the outline and carries its own geometry, so only this one is summed.
const FILL: &str = "sh0011";

/// Per tile, the oracle's fill vertex and index counts.
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

/// This build's fill geometry, per tile.
fn our_geometry() -> BTreeMap<(u32, u32), (usize, usize)> {
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
                out.insert((x, y), (fill.vertices.len(), fill.indices.len()));
            }
        }
    }
    out
}

/// The three malformed rings resolve exactly as the oracle resolves them.
#[test]
fn a_self_crossing_ring_resolves_like_the_oracles() {
    let oracle = oracle_geometry();
    let ours = our_geometry();

    assert!(!oracle.is_empty(), "the capture drew no fill");
    assert_eq!(
        ours, oracle,
        "fill geometry per tile against the oracle's -- this is what says i_overlay's even-odd \
         and wagyu's agree on degenerate input"
    );
}

/// Four triangles over the whole cover: the figure-eight's two and the spur's square.
///
/// The count is the arithmetic that says *which* of the three drew. A bowtie contributing even one
/// triangle, or the spur's tail contributing one, would not fit in twelve indices.
#[test]
fn the_bowtie_contributes_nothing_and_the_other_two_do() {
    let ours = our_geometry();
    let triangles: usize = ours.values().map(|&(_, indices)| indices).sum::<usize>() / 3;
    let tiles = ours.len();
    assert_eq!(tiles, 4, "the fixture draws on four tiles: {ours:?}");
    assert_eq!(
        triangles / tiles,
        4,
        "four triangles a tile -- two for the figure-eight's lobes and two for the spur's square"
    );

    // And nothing at all on the bowtie's own column, which is west of both survivors.
    for tile in [(4092, 2723), (4092, 2724)] {
        assert!(
            !ours.contains_key(&tile),
            "the bowtie's tiles should hold no fill: {tile:?}"
        );
    }
}

/// The oracle is silent on the bowtie's tiles too, which is what makes the test above a comparison.
#[test]
fn the_oracle_is_silent_where_the_bowtie_is() {
    let oracle = oracle_geometry();
    for tile in [(4092, 2723), (4092, 2724)] {
        assert!(
            !oracle.contains_key(&tile),
            "the capture should hold no fill at {tile:?}"
        );
    }
    assert_eq!(oracle.len(), 4, "and it draws on the same four: {oracle:?}");
}
