//! Times the §13.3 four-view zoom sweep. Run with `cargo bench -p tessella-orchestrate`.
//!
//! # Why this is not criterion
//!
//! §13.3's measurement is not "how fast is this function on average" — it is "was the frame
//! budget held on *every* tick". That is a distribution question with the tail as the answer, and
//! the number that matters is the worst frame, not the mean of the good ones. A frame that
//! misses budget once per sweep is a visible stutter and averages away to nothing.
//!
//! So this reports percentiles and the maximum over the sweep's own frames, in the sweep's own
//! order, with no warm-up discarded — the first frames of a sweep are exactly when a cold store
//! is doing the most work, and discarding them would hide the case the budget is about.
//!
//! Dependency-free for the same reason the toolchain is pinned (DR-17): this has to run on the
//! target, and the target is where an extra build dependency is expensive.
//!
//! # What the numbers here are and are not
//!
//! On a developer's x86 machine these are a smoke measurement and a regression tripwire. The
//! §13.3 criterion is stated on RK3566, and a budget assertion belongs there. What this
//! establishes now is that the harness exists, produces the right shape of number, and drives
//! the real store — so the board contributes a stopwatch rather than a porting exercise.

use std::time::{Duration, Instant};

use tessella_orchestrate::sweep;
use tessella_orchestrate::tile::{TileBuilder, TileId};
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

fn main() {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features");

    let views = sweep::four_views();
    let zooms = sweep::sweep_zooms(33);
    let plan = sweep::run(&views, &zooms, 48).expect("the sweep runs");
    println!("sweep: {}", plan.describe());

    // Sized from the peak union, which is the floor below which a frame evicts what it still
    // needs. Timing a thrashing store would measure the store's capacity, not the pipeline.
    let mut builder = TileBuilder::new(plan.peak_union() * 4, 1);
    let mut frame_times = Vec::with_capacity(zooms.len());

    for &zoom in &zooms {
        let started = Instant::now();
        for view in views {
            let at_zoom = ViewTransform { zoom, ..view };
            for tile in cover::cover(&at_zoom).expect("covers") {
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
        frame_times.push(started.elapsed());
    }

    report("frame (4 views)", &mut frame_times);
    // Sharing within a frame and sharing across frames are different problems, and the gap
    // between these two numbers is the second one. `peak_union` sizes a store so that no frame
    // evicts a tile it still needs; a zoom sweep also *revisits* tiles on the way back down, and
    // an LRU sized for one frame drops them in between. The rebuilds are not a flatness failure
    // — no tile is built twice for one frame — but they are real work, and they are the measure
    // of how much a sweep would gain from retain hysteresis across the turn (§5.4, R-10).
    let rebuilt = builder.builds() as usize - plan.distinct_tiles;
    println!(
        "builds: {} for {} distinct tiles over {} requests ({rebuilt} rebuilt after eviction)",
        builder.builds(),
        plan.distinct_tiles,
        plan.tile_requests
    );
}

/// Percentiles and the worst frame. The maximum is the §13.3 number; the percentiles are there
/// to say whether a bad maximum is one outlier or a fat tail, which is the difference between a
/// stutter and a frame rate.
fn report(label: &str, times: &mut [Duration]) {
    let worst = times.iter().copied().max().unwrap_or_default();
    let total: Duration = times.iter().sum();
    times.sort_unstable();
    let at = |fraction: f64| {
        let index = ((times.len() as f64 - 1.0) * fraction).round() as usize;
        times[index]
    };
    println!(
        "{label}: n={} p50 {:?} p95 {:?} p99 {:?} max {:?} total {:?}",
        times.len(),
        at(0.50),
        at(0.95),
        at(0.99),
        worst,
        total
    );
}
