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
//! # The tile count is the result; the sweep's clock is not
//!
//! The two policies differ by tens of microseconds over a sweep whose own repeat-to-repeat
//! spread is larger than that, so the timing columns below cannot separate them and are not
//! asked to. What they are for is the *shape*: which frames are expensive, and whether a bad
//! maximum is one outlier or a fat tail.
//!
//! The result is the tile count, which is exact and deterministic — the same cameras give the
//! same covers every run. Turning it into a time is done separately and honestly, by measuring
//! what a real tile costs and multiplying, rather than by reading a difference out of noise.
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

/// The vendored Protomaps tiles the live parity test runs against, largest first.
///
/// Real geometry, chosen by nobody, which is what turns the sweep's tile *count* into a time.
/// The hermetic style's four features cost almost nothing to build, so a saving measured only
/// against it says the shape and not the size.
///
/// All of them and not one: the nine span three orders of magnitude — 301 bytes of empty ocean
/// to 147 KiB of coastline — and quoting either end as "a real tile" would be picking the
/// answer. The median across them is what a cover entry costs on average, which is the number a
/// count should be multiplied by.
const LIVE_TILES: [(&str, &[u8]); 9] = [
    (
        "16-11",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt"),
    ),
    (
        "16-10",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-10.mvt"),
    ),
    (
        "15-11",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-11.mvt"),
    ),
    (
        "15-10",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-10.mvt"),
    ),
    (
        "16-9",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-9.mvt"),
    ),
    (
        "15-9",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-9.mvt"),
    ),
    (
        "14-11",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-11.mvt"),
    ),
    (
        "14-10",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-10.mvt"),
    ),
    (
        "14-9",
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-9.mvt"),
    ),
];
const LIVE_STYLE: &str = include_str!("../../tessella-style/tests/live_style.json");

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

    real_tile_cost(repeated_minus_one(&counts));

    println!(
        "\n(The clock columns describe the shape of a frame, not the difference between the two \
         rows:\n the policies are tens of microseconds apart over a sweep whose own repeats \
         spread further.)"
    );

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

/// How many tile requests the policy removed over the sweep.
fn repeated_minus_one(counts: &[usize]) -> usize {
    counts[0] - counts[1]
}

/// What one real tile costs to decode and build, and what the sweep's saving is worth in those
/// units.
///
/// The sweep above runs the hermetic style, whose four features cost almost nothing — so its
/// per-frame times say the shape of the curve and not the size of the bill. This measures a real
/// tile once and multiplies, which is the honest way to turn a count into a time: the identity of
/// the tiles a low-zoom cover asks for is not this one, but the work of decoding and bucketing a
/// tile of real data is, and that is the part that scales.
fn real_tile_cost(saved: usize) {
    let style = match Style::parse(LIVE_STYLE) {
        Ok(style) => style,
        Err(error) => {
            println!("\n(skipping the real-tile cost: {error})");
            return;
        }
    };

    // Decode included, because a cover entry the policy removes is a decode as well as a build.
    // Repeated because one sample of a microsecond-scale measurement is noise.
    let mut medians = Vec::with_capacity(LIVE_TILES.len());
    for (name, bytes) in LIVE_TILES {
        let mut samples = Vec::with_capacity(32);
        for _ in 0..32 {
            let started = Instant::now();
            let decoded = tessella_source::mvt::Tile::decode(bytes).expect("the fixture decodes");
            let buckets = tessella_orchestrate::tile::build_mvt_tile(
                &style,
                "world",
                TileId::new(5, 14, 10),
                &decoded,
            )
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            // Kept so the build is not optimized away, and so a fixture that produced nothing
            // would be visible rather than fast.
            std::hint::black_box(&buckets);
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        medians.push((samples[samples.len() / 2], name, bytes.len()));
    }

    medians.sort_unstable();
    let median = medians[medians.len() / 2];
    println!(
        "\nA cover entry, over the nine vendored Protomaps tiles: {:.1?} median, \
         {:.1?} to {:.1?}\n(the cheapest is {} bytes, the dearest {} — and they are not the \
         same order as the byte counts, because\nwhat costs is the geometry the style's three \
         layers read, not the tile's size).",
        median.0,
        medians[0].0,
        medians[medians.len() - 1].0,
        medians[0].2,
        medians[medians.len() - 1].2,
    );
    println!(
        "At the median the {saved} requests the policy removes are {:.1?} of producer work over \
         the\nsweep — and each was also a subdivision and a draw the consumer no longer makes.",
        median.0 * u32::try_from(saved).unwrap_or(u32::MAX)
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
