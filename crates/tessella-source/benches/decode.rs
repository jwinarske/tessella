//! Decode cost on the tile maplibre-native benchmarks, reported so it can be compared with it.
//!
//! Run with `cargo bench -p tessella-source --bench decode`.
//!
//! # The comparison
//!
//! `benchmark/parse/vector_tile.benchmark.cpp` in maplibre-native decodes
//! `test/fixtures/api/assets/streets/10-163-395.vector.pbf` and, for every feature of every
//! layer, sums `getGeometries().size()` and `getProperties().size()`. Its tile data is lazy, so
//! touching both is what forces a full decode; this does the same accounting on the same bytes,
//! which is the only way the two numbers are about the same work.
//!
//! To run the other side:
//!
//! ```text
//! ninja -C <maplibre-native>/build-capture mbgl-benchmark-runner
//! ./build-capture/mbgl-benchmark-runner --benchmark_filter=Parse_VectorTile \
//!     --benchmark_repetitions=12
//! ```
//!
//! Better still, compile that benchmark's *body* as a standalone program over the same fixture
//! and count instructions on both sides with `valgrind --tool=callgrind`. That answers the
//! question the wall clock cannot on a shared machine: it is deterministic, and it showed mbgl
//! to be roughly 1.8x more sensitive to load than this decoder is, which flatters the ratio a
//! stopwatch reports under contention. Both programs print the same total — the sum of every
//! feature's ring count and property count — so a mismatch there means they are not doing the
//! same work and no timing of them means anything.
//!
//! # Why the minimum, and why alternating
//!
//! Interference is one-sided: another process can take time from a run and cannot give any back.
//! So the distribution has a floor at the true cost and a tail above it, the *minimum* is the
//! best estimator of that floor, and the mean is biased upward by exactly the contamination it
//! is supposed to average away. Measured here: under load average 25, mbgl's own harness
//! reported mean 414 us against min 345 and median 370, with one repetition at 574 — and the
//! same benchmark on a quiet machine runs at 302. More samples would have converged on 414,
//! which is not the number anyone wants.
//!
//! What does work is running the two implementations *alternately*, so both eat the same
//! interference, and comparing per-round. Done that way the ratio held at 1.40 across every
//! statistic — minima, medians, means, and the median of paired ratios — while the absolutes
//! were still moving.
//!
//! The coefficient of variation is reported for the same reason Google Benchmark reports it: it
//! says whether to believe the run at all. Below a few per cent the machine was quiet; at twenty
//! the numbers are about the machine.

use std::hint::black_box;
use std::time::{Duration, Instant};

use tessella_source::mvt;

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/streets-10-163-395.mvt");

/// Repetitions, matching what the C++ side is asked for.
const ROUNDS: usize = 12;
/// Iterations per repetition. Google Benchmark chose 1835 for this tile; matching it keeps the
/// per-repetition duration, and so the sensitivity to a passing scheduler decision, similar.
const ITERATIONS: u32 = 1835;

/// The same accounting `Parse_VectorTile` does.
fn once() -> usize {
    let tile = mvt::Tile::decode(TILE).expect("decodes");
    let mut length = 0usize;
    for layer in &tile.layers {
        for feature in layer.features() {
            length += feature.ring_count();
            length += feature.properties().len();
        }
    }
    length
}

fn main() {
    let _ = once();

    let mut samples: Vec<Duration> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(once());
        }
        samples.push(started.elapsed() / ITERATIONS);
    }
    samples.sort_unstable();

    #[allow(clippy::cast_precision_loss)]
    let nanos: Vec<f64> = samples.iter().map(|s| s.as_nanos() as f64).collect();
    #[allow(clippy::cast_precision_loss)]
    let count = nanos.len() as f64;
    let mean = nanos.iter().sum::<f64>() / count;
    let variance = nanos.iter().map(|n| (n - mean).powi(2)).sum::<f64>() / (count - 1.0);
    let cv = 100.0 * variance.sqrt() / mean;

    println!(
        "decode 10-163-395  min {:.1}us  median {:.1}us  mean {:.1}us  max {:.1}us  cv {cv:.1}%",
        nanos[0] / 1000.0,
        nanos[nanos.len() / 2] / 1000.0,
        mean / 1000.0,
        nanos[nanos.len() - 1] / 1000.0,
    );
    if cv > 5.0 {
        println!("  cv above 5%: the machine was busy, compare ratios rather than these numbers");
    }
}
