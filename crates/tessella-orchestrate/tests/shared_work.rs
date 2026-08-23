//! Two views over one cover, measured with the §9.3 flatness counters.
//!
//! # The assertion §9.3 asks for
//!
//! Fetches, decodes and bucket builds must be flat in view count for overlapping covers. Two
//! views looking at the same place do one build, not two, and four views do the same four
//! builds as two views do.
//!
//! These tests began as the inverse — asserting the duplication that existed before there was a
//! shared store, as a tripwire. The store landed and they inverted, which is what the tripwire
//! was for.
//!
//! What makes the measurement honest is that only a *miss* counts as work. Counting every call
//! would count cache hits and assert nothing at all.

use tessella_orchestrate::counters::{SharedCounters, SharedWork};
use tessella_orchestrate::tile::{TileBuilder, TileId, bucket_for, build_tile};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

/// The tiles two views would share if they looked at the same place.
const COVER: [(u32, u32); 4] = [(4092, 2723), (4092, 2724), (4093, 2723), (4093, 2724)];

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features() -> Vec<tessella_source::GeoJsonFeature> {
    let style = style();
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    geojson::read(&source.data).expect("features")
}

/// Builds one tile, recording the shared work it did.
fn build_counting(counters: &mut SharedCounters, x: u32, y: u32) -> usize {
    let key = format!("13/{x}/{y}");
    let buckets = build_tile(
        &style(),
        TileId::new(13, x, y),
        &features(),
        TilingOptions::default(),
    )
    .expect("tile builds");

    // One bucket build per fill layer per call. With a shared store this would happen once for
    // the tile however many views wanted it.
    counters.record(SharedWork::BucketBuild, key);

    bucket_for(&buckets, "fill-constant")
        .and_then(|b| b.content.as_fill())
        .map_or(0, |fill| fill.vertices.len())
}

/// The flatness assertion. Two views over one cover build each tile **once**.
///
/// This test used to assert the opposite, as a tripwire for the absence of a shared store. The
/// store landed and it inverted, which is what the tripwire was for.
#[test]
fn two_views_over_one_cover_share_the_work() {
    let mut counters = SharedCounters::new();
    let mut builder = TileBuilder::new(64, 1);

    for _view in 0..2 {
        for (x, y) in COVER {
            let (_, lookup) = builder
                .build(
                    &style(),
                    "probe",
                    TileId::new(13, x, y),
                    &features(),
                    TilingOptions::default(),
                )
                .expect("builds");
            // Only a miss is work. Counting every call would count cache hits as work and
            // assert nothing.
            if lookup.did_work() {
                counters.record(SharedWork::BucketBuild, format!("13/{x}/{y}"));
            }
        }
    }

    assert!(counters.is_flat(), "{:?}", counters.duplicated());
    assert_eq!(
        counters.total(SharedWork::BucketBuild),
        4,
        "four tiles, not eight"
    );
    assert_eq!(builder.builds(), 4, "and the builder agrees");
}

/// Flatness at four views, which is the number §13 budgets against. The claim is that the work
/// is independent of view count, so the same four builds serve four views as served two.
#[test]
fn flatness_holds_at_four_views() {
    let mut builder = TileBuilder::new(64, 1);

    for _view in 0..4 {
        for (x, y) in COVER {
            builder
                .build(
                    &style(),
                    "probe",
                    TileId::new(13, x, y),
                    &features(),
                    TilingOptions::default(),
                )
                .expect("builds");
        }
    }

    assert_eq!(builder.builds(), 4, "four tiles at four views");
}

/// Views get the same object, not equal copies. Sharing the value is what lets one GPU buffer
/// serve every view (§5.3); sharing only the recipe would not.
#[test]
fn views_receive_the_same_tile_object() {
    let mut builder = TileBuilder::new(64, 1);
    let build = |builder: &mut TileBuilder| {
        builder
            .build(
                &style(),
                "probe",
                TileId::new(13, 4093, 2723),
                &features(),
                TilingOptions::default(),
            )
            .expect("builds")
            .0
    };

    let first = build(&mut builder);
    let second = build(&mut builder);
    assert!(std::sync::Arc::ptr_eq(&first, &second));
}

/// A restyle is a new revision, and buckets built against the old one are not reused. A changed
/// filter admits different features, so silently reusing them would draw the old style.
#[test]
fn a_new_style_revision_rebuilds() {
    let mut first = TileBuilder::new(64, 1);
    let mut second = TileBuilder::new(64, 2);

    for builder in [&mut first, &mut second] {
        builder
            .build(
                &style(),
                "probe",
                TileId::new(13, 4093, 2723),
                &features(),
                TilingOptions::default(),
            )
            .expect("builds");
    }

    assert_eq!(first.builds(), 1);
    assert_eq!(
        second.builds(),
        1,
        "a different revision is a different key"
    );
}

/// The work being duplicated is identical, which is what makes sharing it correct rather than
/// merely possible.
///
/// A bucket is a function of `(tile, layer, tile zoom)` and camera-free (§5.1). If two views
/// building one tile produced different buckets, the store could not be shared at all and §5's
/// whole model would be wrong. They do not, and this checks it through the real pipeline rather
/// than by rereading the claim.
#[test]
fn both_views_build_identical_buckets() {
    for (x, y) in COVER {
        let first = build_tile(
            &style(),
            TileId::new(13, x, y),
            &features(),
            TilingOptions::default(),
        )
        .expect("builds");
        let second = build_tile(
            &style(),
            TileId::new(13, x, y),
            &features(),
            TilingOptions::default(),
        )
        .expect("builds");

        let fill_of = |buckets: &[tessella_orchestrate::LayerBucket]| {
            bucket_for(buckets, "fill-constant")
                .and_then(|b| b.content.as_fill())
                .cloned()
                .expect("a fill bucket")
        };
        assert_eq!(fill_of(&first), fill_of(&second), "at 13/{x}/{y}");
    }
}

/// Building is deterministic across tiles too: the same tile always gives the same vertex
/// count, so a shared store returning a cached bucket cannot differ from a fresh build.
#[test]
fn a_tiles_bucket_does_not_depend_on_when_it_was_built() {
    let mut counters = SharedCounters::new();
    let first: Vec<usize> = COVER
        .iter()
        .map(|(x, y)| build_counting(&mut counters, *x, *y))
        .collect();
    let second: Vec<usize> = COVER
        .iter()
        .map(|(x, y)| build_counting(&mut counters, *x, *y))
        .collect();

    assert_eq!(first, second);
    assert_eq!(
        first,
        [5, 5, 10, 10],
        "matching the oracle's per-tile counts"
    );
}
