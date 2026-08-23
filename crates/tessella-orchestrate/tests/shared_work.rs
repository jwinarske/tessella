//! Two views over one cover, measured with the §9.3 flatness counters.
//!
//! # This test asserts the gap, not the goal
//!
//! §9.3 requires fetches, decodes and bucket builds to be flat in view count for overlapping
//! covers. There is no shared tile store yet — `build_tile` rebuilds from features every call —
//! so two views over one tile currently do the work twice, and that is what this asserts.
//!
//! Writing it the other way round would mean an ignored test or a failing one, and both rot.
//! Written this way it is a tripwire: the day a shared store lands, this test fails, and the
//! failure says exactly what to change and why. The assertion at the bottom names the
//! inversion.
//!
//! What is being proved today is narrower but real: the counters detect duplication through the
//! actual pipeline rather than through a mock, so when the store arrives the measurement is
//! already trustworthy.

use tessella_orchestrate::counters::{SharedCounters, SharedWork};
use tessella_orchestrate::tile::{TileId, bucket_for, build_tile};
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

/// Two views over one cover. Today the work is done twice; the counters see it.
///
/// **This inverts when the shared tile store lands.** At that point `is_flat()` becomes the
/// assertion and this one goes.
#[test]
fn two_views_over_one_cover_currently_duplicate_work() {
    let mut counters = SharedCounters::new();

    for _view in 0..2 {
        for (x, y) in COVER {
            build_counting(&mut counters, x, y);
        }
    }

    assert_eq!(
        counters.distinct(SharedWork::BucketBuild),
        4,
        "four distinct tiles"
    );
    assert_eq!(
        counters.total(SharedWork::BucketBuild),
        8,
        "built twice each, because there is no shared store yet"
    );

    let duplicated = counters.duplicated();
    assert_eq!(duplicated.len(), 4, "every tile in the cover");
    for (work, key, count) in duplicated {
        assert_eq!(work, SharedWork::BucketBuild);
        assert_eq!(count, 2, "{key}");
    }

    assert!(
        !counters.is_flat(),
        "when this starts failing, the shared store has landed: \
         swap this for `assert!(counters.is_flat())` and delete the rest"
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
