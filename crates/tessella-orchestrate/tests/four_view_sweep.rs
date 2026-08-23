//! The four-view zoom sweep of §13.3, running the half of it that needs no board.
//!
//! Coverage completeness and §9.3 flatness are correctness properties, not performance ones: a
//! sweep that leaves holes or duplicates work is wrong on any machine. Both are checked here, on
//! x86, in CI. Frame budget, ring occupancy and symbol pops need the target and R2 symbols, and
//! they will be measured over this same sweep rather than over one written later.

use tessella_orchestrate::sweep;
use tessella_orchestrate::tile::{TileBuilder, TileId};
use tessella_source::geojson;
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

/// The sweep as §13.3 specifies it: 33 frames up, 32 back down, z8→z16→z8.
fn report() -> sweep::SweepReport {
    sweep::run(&sweep::four_views(), &sweep::sweep_zooms(33), 48).expect("the sweep runs")
}

fn style() -> Style {
    Style::parse(HERMETIC).expect("style parses")
}

fn features(style: &Style) -> Vec<tessella_source::GeoJsonFeature> {
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    geojson::read(&source.data).expect("features")
}

/// §13.3's coverage completeness: zero uncovered frames.
///
/// Every frame of the sweep, for every one of the four views, fully tile-covered. A hole here is
/// a patch of background where the map should be, and the frames that produce one are the frames
/// that straddle a zoom boundary — which is most of them, in a sweep whose entire purpose is to
/// cross every boundary from z8 to z16 and back.
#[test]
fn no_frame_of_the_sweep_is_uncovered() {
    let report = report();
    let uncovered = report.uncovered();
    assert!(
        uncovered.is_empty(),
        "{} uncovered frames, first at zoom {}",
        uncovered.len(),
        uncovered.first().map_or(0.0, |f| f.zoom)
    );
    assert_eq!(report.frames.len(), 65, "33 up, 32 down, z16 visited once");
}

/// The sweep's zooms are the range §13.3 names, and the turn at the top is not double-counted.
#[test]
fn the_sweep_goes_up_and_comes_back() {
    let zooms = sweep::sweep_zooms(33);
    assert_eq!(zooms.first(), Some(&sweep::SWEEP_LOW));
    assert_eq!(zooms.last(), Some(&sweep::SWEEP_LOW));
    assert_eq!(
        zooms.iter().filter(|z| **z == sweep::SWEEP_HIGH).count(),
        1,
        "the top is one frame, not two"
    );
    assert_eq!(zooms.iter().copied().fold(f64::MIN, f64::max), 16.0);
}

/// Both sharing regimes are in the sweep, which is what makes the flatness assertion mean
/// something.
///
/// At the bottom of the sweep the four views are far closer together than a tile, so their
/// covers coincide and sharing is total — the easy case, and the one a store that shares only
/// identical keys would still pass. At the top they have separated and the covers overlap
/// partially, which is the case such a store gets wrong while continuing to look correct.
///
/// No frame is disjoint. That is a property of this arrangement rather than of covers in
/// general, and it is deliberate: four views that shared nothing would make every flatness
/// number trivially correct.
#[test]
fn the_views_both_coincide_and_diverge_during_the_sweep() {
    let report = report();

    let bottom = &report.frames[0];
    assert_eq!(bottom.zoom, sweep::SWEEP_LOW);
    assert_eq!(
        bottom.union * 4,
        bottom.total,
        "at z8 all four covers are the same"
    );

    let top = report
        .frames
        .iter()
        .find(|f| f.zoom == sweep::SWEEP_HIGH)
        .expect("the top of the sweep");
    assert!(top.shared() > 0, "at z16 the views still overlap");
    assert!(
        top.union * 4 > top.total,
        "but no longer identically: union {} of {}",
        top.union,
        top.total
    );

    assert!(
        report.frames.iter().all(|f| f.shared() > 0),
        "and no frame is disjoint"
    );
}

/// §9.3 flatness over the whole sweep: the work equals the union of the covers.
///
/// Stated against the union rather than against a view count, because "four views do what one
/// view does" is only true when all four look at the same place. The union form holds for any
/// arrangement and still says something when the covers differ — nothing is built twice, and
/// nothing needed goes unbuilt.
///
/// The contrast is the mbgl model, where each view owns its own pyramid: that does one build per
/// request, and the request count is on the other side of this assertion.
#[test]
fn the_sweep_builds_each_tile_once() {
    let report = report();
    let style = style();
    let features = features(&style);

    // Sized above the sweep's whole distinct set, so nothing is evicted and the count measures
    // duplication rather than capacity. The capacity a real view needs is `peak_union`, which
    // the test below is about.
    let mut builder = TileBuilder::new(report.distinct_tiles * 2, 1);
    let mut requests = 0;

    for zoom in sweep::sweep_zooms(33) {
        for view in sweep::four_views() {
            let at_zoom = tessella_tile::cover::ViewTransform { zoom, ..view };
            for tile in tessella_tile::cover::cover(&at_zoom).expect("covers") {
                requests += 1;
                builder
                    .build(
                        &style,
                        "probe",
                        TileId::new(tile.z, tile.x, tile.y),
                        &features,
                        TilingOptions::default(),
                    )
                    .expect("builds");
            }
        }
    }

    assert_eq!(
        requests, report.tile_requests,
        "the sweep is the same sweep"
    );
    assert_eq!(
        builder.builds() as usize,
        report.distinct_tiles,
        "one build per distinct tile, not one per request"
    );
    assert!(
        requests > report.distinct_tiles * 10,
        "and the sharing is substantial: {requests} requests for {} tiles",
        report.distinct_tiles
    );
}

/// A store smaller than a single frame's union thrashes, which is why `peak_union` is reported.
///
/// This is the failure that looks like flatness holding: the counts stay low per frame while the
/// same tiles are evicted and rebuilt every frame. Sizing the store from the peak union rather
/// than from a guess is what avoids it, and this test is the evidence that the floor is real
/// rather than cautious.
#[test]
fn a_store_below_the_peak_union_rebuilds_what_it_just_built() {
    let report = report();
    let style = style();
    let features = features(&style);
    let peak = report.peak_union();
    assert!(peak > 4, "the sweep has a frame worth thrashing: {peak}");

    let run = |capacity: usize| {
        let mut builder = TileBuilder::new(capacity, 1);
        for _pass in 0..3 {
            for view in sweep::four_views() {
                let at_zoom = tessella_tile::cover::ViewTransform { zoom: 15.0, ..view };
                for tile in tessella_tile::cover::cover(&at_zoom).expect("covers") {
                    builder
                        .build(
                            &style,
                            "probe",
                            TileId::new(tile.z, tile.x, tile.y),
                            &features,
                            TilingOptions::default(),
                        )
                        .expect("builds");
                }
            }
        }
        builder.builds()
    };

    let roomy = run(peak * 2);
    let cramped = run(peak / 2);
    assert!(
        cramped > roomy,
        "a store below the peak union rebuilds: {cramped} vs {roomy}"
    );
}
