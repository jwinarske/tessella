//! What a globe costs the producer over a zoom sweep. Run with
//! `cargo bench -p tessella-orchestrate --bench globe_sweep`.
//!
//! # Why this sweep and not §13.3's
//!
//! §13.3 sweeps z8 to z16, which is where a car's map lives and where the frame budget is
//! stated. It is the wrong range for this question: a globe and a plane ask for *identical*
//! tiles from z3 upwards, so a z8–z16 sweep measures the globe by measuring nothing about it.
//!
//! Everything that distinguishes the two is below z3. So this sweeps z0 to z8 and back, which
//! is the range a globe view actually spends its distinctive time in — a spin-up from the whole
//! planet to a city — and is where `globe_cover` found the difference.
//!
//! # What is being compared
//!
//! Two covers over the same cameras, the same store and the same style. `WorldCopies::Repeated`
//! is what a flat map asks for; `WorldCopies::One` is §13.4's policy, which is the producer's
//! entire part in the globe. The horizon is not here on purpose — it is the consumer's, and
//! `globe_cover` measured why.
//!
//! The saving is not a saving in the ordinary sense. A globe drawing a repeated cover is
//! *wrong*: every copy bends to the same patch, so the surface z-fights and the subdivision is
//! paid twice at the zooms where an edge splits into ninety segments. What this measures is the
//! producer-side part of that bill.
//!
//! # Not criterion, and no warm-up discarded
//!
//! For the reasons `four_view_sweep` gives: the question is the tail rather than the mean, and
//! the first frames of a sweep are when a cold store is doing the most work. Dependency-free
//! because this has to run on the target (DR-17).

use std::time::{Duration, Instant};

use tessella_orchestrate::sweep;
use tessella_orchestrate::tile::{TileBuilder, TileId};
use tessella_source::tiling::TilingOptions;
use tessella_style::{Source, Style};
use tessella_tile::cover::{self, ViewTransform, WorldCopies};

const HERMETIC: &str = include_str!("../../tessella-style/tests/hermetic_style.json");

/// The zooms a globe sweep visits: the whole planet up to a city and back.
///
/// `sweep::sweep_zooms` spans z8–z16 by construction, which is §13.3's range and not this one.
fn globe_zooms(steps: usize) -> Vec<f64> {
    let steps = steps.max(2);
    let up: Vec<f64> = (0..steps)
        .map(|i| 8.0 * (i as f64) / ((steps - 1) as f64))
        .collect();
    let mut all = up.clone();
    all.extend(up.into_iter().rev().skip(1));
    all
}

fn main() {
    let style = Style::parse(HERMETIC).expect("style parses");
    let Some(Source::Geojson(source)) = style.source("probe") else {
        panic!("a geojson source");
    };
    let features = tessella_source::geojson::read(&source.data).expect("features");

    let views = sweep::four_views();
    let zooms = globe_zooms(33);

    println!(
        "globe sweep: z0 to z8 and back, {} frames, 4 views\n",
        zooms.len()
    );
    println!(" copies    | tiles | p50      | p95      | p99      | max      | total");
    println!("-----------+-------+----------+----------+----------+----------+---------");

    let mut counts = Vec::new();
    let mut per_frame = Vec::new();
    for copies in [WorldCopies::Repeated, WorldCopies::One] {
        let (times, tiles, frames) = run(&style, &features, &views, &zooms, copies);
        counts.push(tiles);
        per_frame.push(frames);
        report(copies, tiles, times);
    }

    let (repeated, one) = (counts[0], counts[1]);
    println!(
        "\n{} of {repeated} tile requests over the sweep are copies of a patch already asked \
         for ({:.0}%).",
        repeated - one,
        100.0 * (repeated - one) as f64 / repeated as f64
    );

    // The average understates it badly, because the copies are not spread across the sweep —
    // they are all in the handful of frames below z2, where they are most of the cover. A
    // per-frame worst case is the number a budget is set against.
    let worst = zooms
        .iter()
        .zip(per_frame[0].iter().zip(&per_frame[1]))
        .map(|(zoom, (repeated, one))| {
            let share = if *repeated == 0 {
                0.0
            } else {
                100.0 * (repeated - one) as f64 / *repeated as f64
            };
            (share, *zoom, *repeated, *one)
        })
        .fold(
            (0.0, 0.0, 0, 0),
            |best, at| if at.0 > best.0 { at } else { best },
        );
    println!(
        "The worst frame is z{:.2}, where {} of {} are copies ({:.0}%) — the average is spread \
         over a sweep\nthat spends most of its frames above z3, where a globe and a plane ask \
         for the same tiles.",
        worst.1,
        worst.2 - worst.3,
        worst.2,
        worst.0
    );
    println!(
        "\nA flat map draws the copies and is right to; a globe bends every one of them to the \
         same\npatch. Per-frame time is small here because the hermetic style is four features \
         — the shape\nis what this says, and the tile count is the part that scales with real \
         data."
    );
}

/// One sweep, returning each frame's time, the total tiles asked for, and the count per frame.
fn run(
    style: &Style,
    features: &[tessella_source::geojson::GeoJsonFeature],
    views: &[ViewTransform],
    zooms: &[f64],
    copies: WorldCopies,
) -> (Vec<Duration>, usize, Vec<usize>) {
    // Sized well past the peak so the measurement is the pipeline rather than the store's
    // capacity, as `four_view_sweep` sizes its own.
    let mut builder = TileBuilder::new(512, 1);
    let mut times = Vec::with_capacity(zooms.len());
    let mut per_frame = Vec::with_capacity(zooms.len());
    let mut tiles = 0;

    for &zoom in zooms {
        let before = tiles;
        let started = Instant::now();
        for view in views {
            let at_zoom = ViewTransform { zoom, ..*view };
            for tile in cover::cover_with(&at_zoom, copies).expect("covers") {
                tiles += 1;
                builder
                    .build(
                        style,
                        "probe",
                        TileId::new(tile.z, tile.x, tile.y),
                        features,
                        TilingOptions::default(),
                    )
                    .expect("builds");
            }
        }
        times.push(started.elapsed());
        per_frame.push(tiles - before);
    }
    (times, tiles, per_frame)
}

/// Percentiles and the worst frame, as §13.3 asks for them: the tail is the answer.
fn report(copies: WorldCopies, tiles: usize, mut times: Vec<Duration>) {
    let worst = times.iter().copied().max().unwrap_or_default();
    let total: Duration = times.iter().sum();
    times.sort_unstable();
    let at = |fraction: f64| {
        let index = ((times.len() as f64 - 1.0) * fraction).round() as usize;
        times[index]
    };
    let label = match copies {
        WorldCopies::Repeated => "repeated ",
        WorldCopies::One => "one      ",
    };
    println!(
        " {label} | {tiles:5} | {:8.1?} | {:8.1?} | {:8.1?} | {:8.1?} | {total:8.1?}",
        at(0.50),
        at(0.95),
        at(0.99),
        worst,
    );
}
