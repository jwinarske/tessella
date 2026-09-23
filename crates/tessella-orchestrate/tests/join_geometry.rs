// SPDX-License-Identifier: BSD-2-Clause
//! A line's joins and caps, counted against the oracle rather than described.
//!
//! # What the oracle gives
//!
//! `tests/golden/joins_style.dump`: ten polylines whose interior angles straddle every
//! classification boundary the generator has — 2 degrees through 179, a hairpin, and a closed
//! ring — under nine line layers that vary `line-join`, `line-cap`, `line-round-limit`,
//! `line-miter-limit`, `line-gap-width`, `line-offset`, `line-translate` and `line-dasharray`.
//!
//! Each drawable's name carries its vertex count and its `idx=` field the index count, so the
//! totals per layer are the oracle's own answer for how much geometry each join policy produces.
//! A join resolved differently moves them: a miter is two vertices where a fake round is a fan,
//! and mbgl's `roundLimit` and `miterLimit` are what choose between them.
//!
//! # Why this capture exists
//!
//! None of the other ten goldens sets `line-join` at all, so every one of them takes the spec's
//! default of `miter` and the round and bevel arms of the generator were never exercised by a
//! byte-level test. The gap was not theoretical: `line-round-limit` defaults to 1.05 in the style
//! spec and to **1** in mbgl, this build had the spec's number, and the difference is 2,011 joins
//! against 83 on a single coastline ring. Pixels found it eventually — 523 gross on one camera,
//! which read as antialiasing for a long time first. These counts would have named it at once.

use std::collections::BTreeMap;

use tessella_orchestrate::Content;
use tessella_orchestrate::tile::{TileId, build_tile};
use tessella_source::tiling::TilingOptions;
use tessella_style::Style;

const DUMP: &str = include_str!("../../../tests/golden/joins_style.dump");
const STYLE: &str = include_str!("../../tessella-style/tests/joins_style.json");

/// The tiles the capture covers, which is the z13 cover of its camera.
const TILES: [(u32, u32); 6] = [
    (4092, 2723),
    (4092, 2724),
    (4093, 2723),
    (4093, 2724),
    (4094, 2723),
    (4094, 2724),
];

/// Vertex and index totals per layer index, read out of the oracle's drawable names.
fn oracle_totals() -> BTreeMap<usize, (usize, usize)> {
    let mut out: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for line in DUMP.lines() {
        let Some(rest) = line.strip_prefix("drawable L") else {
            continue;
        };
        let layer: usize = rest[..5].parse().expect("a five-digit layer index");
        // `…v00000010#00 … idx=12:<hash>` — the vertex count is in the name, the index count is
        // the first half of `idx=`.
        let verts: usize = rest
            .split(".v")
            .nth(1)
            .and_then(|t| t.split('#').next())
            .expect("a vertex count")
            .parse()
            .expect("digits");
        let indices: usize = line
            .split("idx=")
            .nth(1)
            .and_then(|t| t.split(':').next())
            .expect("an index count")
            .parse()
            .expect("digits");
        let slot = out.entry(layer).or_default();
        slot.0 += verts;
        slot.1 += indices;
    }
    out
}

/// The same totals from this build's own buckets.
fn our_totals() -> (BTreeMap<usize, (usize, usize)>, Vec<String>) {
    let style = Style::parse(STYLE).expect("the style parses");
    let tessella_style::Source::Geojson(source) = style.source("probe").expect("a source") else {
        panic!("the fixture has one geojson source")
    };
    let features = tessella_source::geojson::read(&source.data).expect("features read");

    let mut out: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for (x, y) in TILES {
        let buckets = build_tile(
            &style,
            "probe",
            TileId::new(13, x, y),
            &features,
            TilingOptions::default(),
        )
        .expect("the tile builds");
        for bucket in &buckets {
            let Content::Line(line) = &bucket.content else {
                continue;
            };
            // An empty bucket is not a drawable -- `is_encodable` gates it on `has_data` -- so it
            // must not be counted against an oracle that only records what it drew.
            if line.vertices.is_empty() {
                continue;
            }
            let slot = out.entry(bucket.layer_index).or_default();
            slot.0 += line.vertices.len();
            slot.1 += line.indices.len();
        }
    }
    (out, style.layers.iter().map(|l| l.id.clone()).collect())
}

/// Every line layer produces exactly the geometry mbgl produces for it.
#[test]
fn every_join_policy_matches_the_oracles_geometry() {
    let oracle = oracle_totals();
    let (ours, names) = our_totals();

    assert!(!ours.is_empty(), "the fixture built no line buckets");
    for (&layer, &(verts, indices)) in &ours {
        let name = names.get(layer).map_or("?", String::as_str);
        let &(want_v, want_i) = oracle
            .get(&layer)
            .unwrap_or_else(|| panic!("the oracle drew nothing for layer {layer} ({name})"));
        assert_eq!(
            (verts, indices),
            (want_v, want_i),
            "layer {layer} ({name}): {verts} vertices and {indices} indices against the oracle's \
             {want_v} and {want_i}"
        );
    }
}

/// And the round arm really is exercised — a guard against the fixture quietly going all-miter.
///
/// Without it a change that made every join a miter would leave the test above passing against a
/// re-captured oracle that had done the same thing.
#[test]
fn the_fixture_separates_the_join_policies() {
    let (ours, names) = our_totals();
    let by_name = |want: &str| {
        names
            .iter()
            .position(|n| n == want)
            .and_then(|i| ours.get(&i).copied())
            .unwrap_or_else(|| panic!("no geometry for {want}"))
    };
    let miter = by_name("join-miter");
    let round = by_name("join-round");
    let bevel = by_name("join-bevel");
    let raised = by_name("round-limit-high");

    assert!(
        round.0 > bevel.0 && bevel.0 > miter.0,
        "a fan should cost more than a bevel and a bevel more than a miter: round {round:?}, \
         bevel {bevel:?}, miter {miter:?}"
    );
    assert!(
        raised.0 < round.0,
        "raising line-round-limit turns fans into miters: {raised:?} against {round:?}"
    );
}
