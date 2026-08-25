//! The worker-count budget, taken on the target — R1's last exit criterion.
//!
//! §5.4 fixes the pool at a bounded constant rather than the host's core count, for a reason a
//! workstation cannot check: decode is meant to fit on a device's small cores, and a number
//! derived from the machine that measured it says nothing about the machine that runs it. So the
//! constant needs a number taken on an RK3566, which is what this produces.
//!
//! `#[ignore]`d: it is a measurement, not an assertion. A wall-clock budget baked into CI would
//! fail on whatever runner it landed on, which is the mistake §12.5's trace tests already avoid.
//!
//! ```sh
//! cargo test -p tessella-orchestrate --test worker_budget -- --ignored --nocapture
//! ```
//!
//! # What it measures, and what it deliberately does not
//!
//! Nine real z5 tiles of a Protomaps planet extract — the same bytes `live_parity` diffs against
//! the oracle — served over loopback by the in-repo server. Loopback has no latency, which is
//! the point: with the network removed, what is left is decode and bucket build, and those are
//! what a worker count governs. A run over a real link would measure the link.
//!
//! The nine tiles are *different* tiles, spanning 300 bytes to 146 KB. A benchmark over one
//! fixture repeated would have every worker finish at the same moment and make any scheduling
//! policy look equally good; the spread is what makes the tail visible.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::{BootError, ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_orchestrate::tile::{TileId, build_mvt_tile};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{Coalescing, FileSource};
use tessella_style::Style;
use tessella_tile::cover::ViewTransform;

const STYLE: &str = include_str!("../../tessella-style/tests/live_style.json");
const MANIFEST: &str = include_str!("../../../tests/live-fixtures/world_z7.json");

type Fixture = ((u8, u32, u32), &'static [u8]);

const TILES: &[Fixture] = &[
    (
        (5, 14, 9),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-9.mvt"),
    ),
    (
        (5, 14, 10),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-10.mvt"),
    ),
    (
        (5, 14, 11),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-14-11.mvt"),
    ),
    (
        (5, 15, 9),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-9.mvt"),
    ),
    (
        (5, 15, 10),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-10.mvt"),
    ),
    (
        (5, 15, 11),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-15-11.mvt"),
    ),
    (
        (5, 16, 9),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-9.mvt"),
    ),
    (
        (5, 16, 10),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-10.mvt"),
    ),
    (
        (5, 16, 11),
        include_bytes!("../../../tests/live-fixtures/world_z7-5-16-11.mvt"),
    ),
];

fn fixture_server() -> tile_server::Server {
    let server = tile_server::Server::start(tile_server::Routes::new()).expect("binds");
    let origin = server.origin();
    let mut routes = tile_server::Routes::new().at(
        "/world_z7.json",
        "application/json",
        MANIFEST.replace("ORIGIN", &origin).into_bytes(),
    );
    for ((z, x, y), body) in TILES {
        routes = routes.at(
            &format!("/world_z7/{z}/{x}/{y}.mvt"),
            "application/x-protobuf",
            body.to_vec(),
        );
    }
    server.set_routes(routes);
    server
}

/// Total CPU time this process has used, user plus system.
///
/// Divided by wall time it gives the cores actually occupied, and every table here reports it.
/// Without it a speedup ratio is uninterpretable: it was reporting that the pool scaled at 2.1x
/// on four cores, which looked like a serialization bug and was not one. The "one worker"
/// baseline was already using 1.8 cores, because [`Batch::wait`] makes the submitting thread
/// help rather than idle. Against a baseline that is itself parallel, a genuinely linear pool
/// measures as half of one.
fn cpu_time() -> Duration {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("procfs");
    // Fields after the parenthesised comm: utime is the 14th overall, stime the 15th.
    let tail = &stat[stat.rfind(')').expect("comm") + 2..];
    let fields: Vec<&str> = tail.split_whitespace().collect();
    let ticks: u64 =
        fields[11].parse::<u64>().expect("utime") + fields[12].parse::<u64>().expect("stime");
    Duration::from_secs_f64(ticks as f64 / 100.0)
}

fn view() -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        // The same camera `live_parity` diffs against the oracle, whose z5 cover is exactly the
        // nine tiles vendored here. A view whose cover falls outside them is not a lighter
        // benchmark, it is a different one: the tiles it misses answer 404 and cost nothing, so
        // the table measures however many tiles happened to land inside the fixtures.
        longitude: -0.11,
        latitude: 51.505,
        zoom: 5.0,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

/// One cold start with `workers` workers, from an empty cache, returning the trace.
fn once(origin: &str, workers: Workers) -> (Duration, Duration, usize) {
    let style = STYLE.replace("http://127.0.0.1:8080", origin);
    // A fresh cache and a fresh pool each run: a warm bucket cache would make every count after
    // the first measure nothing at all, which is how a fan-out benchmark accidentally reports
    // that one worker is fastest.
    let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
        10,
    ))));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(256));
    let pool = Pool::new(workers);
    let view = view();

    let boot = ColdStart {
        style: &style,
        view: &view,
        files: Arc::clone(&files),
        cache: Arc::clone(&cache),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    }
    .run()
    .expect("boots");

    (
        boot.trace.first_bucket,
        boot.trace.complete,
        boot.tiles.len(),
    )
}

/// Reports cold-start time against worker count.
///
/// Minimum of N runs rather than the mean: interference is one-sided — a scheduler hiccup or a
/// migration can only make a run slower — so the fastest run is the one least contaminated by
/// something that is not the thing being measured.
#[test]
#[ignore]
fn worker_count_budget() {
    let server = fixture_server();
    let origin = server.origin();

    // Warm the page cache and the server's accept path so the first row is not paying for both.
    let _ = once(&origin, Workers::new(4));

    println!(
        "\n  worker budget: a {}-tile z5 cover of a Protomaps extract, over loopback",
        once(&origin, Workers::new(4)).2
    );
    println!(
        "  {:>7}  {:>12}  {:>12}  {:>12}  {:>16}",
        "workers", "first bucket", "complete", "vs 1 worker", "cores busy"
    );

    let mut serial = None;
    for count in [1usize, 2, 3, 4, 5, 6, 8] {
        let runs = 9;
        let mut best_first = Duration::MAX;
        let mut best_complete = Duration::MAX;
        let mut best_cores = 0.0f64;
        for _ in 0..runs {
            let cpu_before = cpu_time();
            let (first, complete, _) = once(&origin, Workers::new(count));
            best_first = best_first.min(first);
            if complete < best_complete {
                best_complete = complete;
                best_cores = (cpu_time() - cpu_before).as_secs_f64() / complete.as_secs_f64();
            }
        }
        let base = *serial.get_or_insert(best_complete);
        let ratio = base.as_secs_f64() / best_complete.as_secs_f64();
        println!(
            "  {count:>7}  {:>12.3?}  {:>12.3?}  {ratio:>11.2}x  {best_cores:>10.2} cores busy",
            best_first, best_complete
        );
    }
    println!();
}

/// The same shape, but reporting how long the pieces take rather than the whole.
///
/// A worker count is chosen against where the parallel part stops shrinking, and that is only
/// meaningful next to the part that never shrinks — style parse, source resolution and cover are
/// serial however many workers there are.
#[test]
#[ignore]
fn cold_start_trace() {
    let server = fixture_server();
    let origin = server.origin();
    let _ = once(&origin, Workers::new(4));

    let style = STYLE.replace("http://127.0.0.1:8080", &origin);
    let view = view();
    let mut best = None;
    for _ in 0..9 {
        let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
            10,
        ))));
        let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(256));
        let pool = Pool::new(Workers::new(4));
        let started = Instant::now();
        let boot = ColdStart {
            style: &style,
            view: &view,
            files: Arc::clone(&files),
            cache: Arc::clone(&cache),
            pool: &pool,
            priority: Priority::Foreground,
            style_rev: 1,
        }
        .run()
        .expect("boots");
        let total = started.elapsed();
        if best
            .as_ref()
            .is_none_or(|(t, _): &(Duration, _)| total < *t)
        {
            best = Some((total, boot.trace));
        }
    }
    let (_, trace) = best.expect("a run");
    println!("\n  RK3566 cold start, four workers:");
    println!("    parse         {:>10.3?}", trace.style_parsed);
    println!("    sources       {:>10.3?}", trace.sources_resolved);
    println!("    cover         {:>10.3?}", trace.cover_computed);
    println!("    first fetch   {:>10.3?}", trace.first_fetch);
    println!("    first bucket  {:>10.3?}", trace.first_bucket);
    println!("    complete      {:>10.3?}\n", trace.complete);
}

/// What each tile of the cover costs to decode and build, on its own.
///
/// The shape of the worker-count table is only interpretable next to this. A cover whose tiles
/// are wildly uneven has a completion time bounded by its largest tile no matter how many
/// workers there are, and no amount of scheduling changes that — which looks exactly like a
/// decoder that does not parallelize.
#[test]
#[ignore]
fn per_tile_cost() {
    let server = fixture_server();
    let origin = server.origin();
    let style = STYLE.replace("http://127.0.0.1:8080", &origin);
    let view = view();

    // One serial start tells us which tiles the cover holds and what each cost.
    let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
        10,
    ))));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(256));
    let pool = Pool::new(Workers::serial());
    let boot = ColdStart {
        style: &style,
        view: &view,
        files: Arc::clone(&files),
        cache: Arc::clone(&cache),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    }
    .run()
    .expect("boots");

    println!("\n  the cover, {} tiles:", boot.tiles.len());
    for built in &boot.tiles {
        println!(
            "    {}/{}/{}  {:>3} buckets",
            built.tile.z,
            built.tile.x,
            built.tile.y,
            built.buckets.len()
        );
    }
    println!();
}

/// Decode and build alone, with the network taken out entirely.
///
/// The worker-count table above mixes fetch with build, and the two scale differently. This
/// runs the same nine tiles from bytes already in hand, so what it measures is only the part a
/// worker pool exists to spread — and the difference between the two tables says where a
/// ceiling actually is.
#[test]
#[ignore]
fn build_only_scaling() {
    // Fetch once, outside the timing, so every run below starts from bytes.
    let bodies: Vec<(TileId, Arc<Vec<u8>>)> = {
        let server = fixture_server();
        let origin = server.origin();
        let files = HttpFileSource::new(Duration::from_secs(10));
        TILES
            .iter()
            .map(|((z, x, y), _)| {
                let url = format!("{origin}/world_z7/{z}/{x}/{y}.mvt");
                let body = files.fetch(&url).expect("fetch").body;
                (TileId::new(*z, *x, *y), Arc::new(body))
            })
            .collect()
    };
    let style =
        Arc::new(Style::parse(&STYLE.replace("http://127.0.0.1:8080", "http://x")).expect("style"));

    println!("\n  build only, nine tiles, no network:");
    println!(
        "  {:>7}  {:>12}  {:>12}  {:>16}",
        "workers", "complete", "vs inline", "cores busy"
    );

    // The genuinely serial baseline, with no pool involved at all.
    let inline = {
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            for (tile, body) in &bodies {
                let decoded = tessella_source::mvt::Tile::decode(body).expect("decodes");
                std::hint::black_box(
                    build_mvt_tile(&style, "world", *tile, &decoded)
                        .expect("builds")
                        .len(),
                );
            }
            best = best.min(started.elapsed());
        }
        best
    };
    println!(
        "  {:>7}  {:>12.3?}  {:>11.2}x  {:>10.2} cores busy",
        "inline", inline, 1.0, 1.0
    );

    let mut serial = None;
    for count in [1usize, 2, 3, 4, 6, 8] {
        let mut best = Duration::MAX;
        let mut best_cores = 0.0f64;
        for _ in 0..9 {
            let pool = Pool::new(Workers::new(count));
            let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let cpu_before = cpu_time();
            let started = Instant::now();
            let batch = pool.batch(Priority::Foreground);
            for (tile, body) in &bodies {
                // The pool takes `'static` jobs, so each carries its own handles. Cloning an
                // `Arc` per tile is nine atomic increments against tens of milliseconds of
                // decode, which does not move the number being measured.
                let (style, body, tile, done) = (
                    Arc::clone(&style),
                    Arc::clone(body),
                    *tile,
                    Arc::clone(&done),
                );
                batch.submit(move || {
                    let decoded =
                        tessella_source::mvt::Tile::decode(&body).expect("the fixture decodes");
                    let built = build_mvt_tile(&style, "world", tile, &decoded).expect("builds");
                    done.fetch_add(built.len(), std::sync::atomic::Ordering::Relaxed);
                });
            }
            batch.wait().expect("no worker panicked");
            let wall = started.elapsed();
            if wall < best {
                best = wall;
                best_cores = (cpu_time() - cpu_before).as_secs_f64() / wall.as_secs_f64();
            }
            assert!(done.load(std::sync::atomic::Ordering::Relaxed) > 0);
        }
        let base = *serial.get_or_insert(inline);
        println!(
            "  {count:>7}  {:>12.3?}  {:>11.2}x  {best_cores:>10.2} cores busy",
            best,
            base.as_secs_f64() / best.as_secs_f64()
        );
    }
    println!();
}

/// The same tile nine times, so every worker has identical work.
///
/// The nine real tiles span three hundred bytes to a hundred and forty-six kilobytes, and a
/// completion time is bounded below by the largest of them however many workers there are. That
/// alone could explain a disappointing table, so this removes it: nine copies of one mid-sized
/// tile, perfectly divisible. What is left is whatever the cores contend on — the allocator, and
/// the memory system a quad A55 shares.
#[test]
#[ignore]
fn uniform_scaling() {
    let body = Arc::new({
        let server = fixture_server();
        let origin = server.origin();
        let files = HttpFileSource::new(Duration::from_secs(10));
        files
            .fetch(&format!("{origin}/world_z7/5/15/10.mvt"))
            .expect("fetch")
            .body
    });
    let style =
        Arc::new(Style::parse(&STYLE.replace("http://127.0.0.1:8080", "http://x")).expect("style"));

    println!("\n  build only, one 47 KB tile nine times, identical work per job:");
    println!(
        "  {:>7}  {:>12}  {:>12}  {:>16}",
        "workers", "complete", "vs inline", "cores busy"
    );

    let inline = {
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let started = Instant::now();
            for index in 0..9u32 {
                let decoded = tessella_source::mvt::Tile::decode(&body).expect("decodes");
                std::hint::black_box(
                    build_mvt_tile(&style, "world", TileId::new(5, 15 + index, 10), &decoded)
                        .expect("builds")
                        .len(),
                );
            }
            best = best.min(started.elapsed());
        }
        best
    };
    println!(
        "  {:>7}  {:>12.3?}  {:>11.2}x  {:>10.2} cores busy",
        "inline", inline, 1.0, 1.0
    );

    let mut serial = None;
    for count in [1usize, 2, 3, 4, 6, 8] {
        let mut best = Duration::MAX;
        let mut best_cores = 0.0f64;
        for _ in 0..9 {
            let pool = Pool::new(Workers::new(count));
            let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let cpu_before = cpu_time();
            let started = Instant::now();
            let batch = pool.batch(Priority::Foreground);
            for index in 0..9u32 {
                let (style, body, done) =
                    (Arc::clone(&style), Arc::clone(&body), Arc::clone(&done));
                batch.submit(move || {
                    let decoded =
                        tessella_source::mvt::Tile::decode(&body).expect("the fixture decodes");
                    let built =
                        build_mvt_tile(&style, "world", TileId::new(5, 15 + index, 10), &decoded)
                            .expect("builds");
                    done.fetch_add(built.len(), std::sync::atomic::Ordering::Relaxed);
                });
            }
            batch.wait().expect("no worker panicked");
            let wall = started.elapsed();
            if wall < best {
                best = wall;
                best_cores = (cpu_time() - cpu_before).as_secs_f64() / wall.as_secs_f64();
            }
            assert!(done.load(std::sync::atomic::Ordering::Relaxed) > 0);
        }
        let base = *serial.get_or_insert(inline);
        println!(
            "  {count:>7}  {:>12.3?}  {:>11.2}x  {best_cores:>10.2} cores busy",
            best,
            base.as_secs_f64() / best.as_secs_f64()
        );
    }
    println!();
}

/// A control: nine jobs of pure arithmetic, touching almost no memory.
///
/// If this scales with the cores and the decode above does not, the pool is not the ceiling and
/// the difference is what decoding does that arithmetic does not — allocate, and stream bytes
/// through a memory system four in-order cores share. If this does *not* scale either, the pool
/// itself is the problem and every other number here is measuring it.
#[test]
#[ignore]
fn pool_control_scaling() {
    println!("\n  control: nine jobs of arithmetic, no allocation:");
    // A real serial baseline: the jobs run inline, on one thread, with no pool at all. The
    // `Workers::new(1)` row is *not* this -- it is one worker plus a helping submitter.
    let serial_wall = {
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            for seed in 0..9u64 {
                let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ seed;
                for _ in 0..60_000_000u32 {
                    hash ^= hash >> 33;
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
                std::hint::black_box(hash);
            }
            best = best.min(started.elapsed());
        }
        best
    };
    println!(
        "  {:>7}  {:>12.3?}  {:>11.2}x  {:>10.2} cores busy",
        "inline", serial_wall, 1.0, 1.0
    );
    println!(
        "  {:>7}  {:>12}  {:>12}",
        "workers", "complete", "vs serial"
    );

    let mut serial = None;
    for count in [1usize, 2, 3, 4, 6, 8] {
        let mut best = Duration::MAX;
        let mut best_cores = 0.0f64;
        for _ in 0..5 {
            let pool = Pool::new(Workers::new(count));
            let sink = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let cpu_before = cpu_time();
            let started = Instant::now();
            let batch = pool.batch(Priority::Foreground);
            for seed in 0..9u64 {
                let sink = Arc::clone(&sink);
                batch.submit(move || {
                    // Long enough that a governor ramp is a small fraction of it. At six
                    // million iterations each job was thirteen milliseconds, and on a board
                    // running `ondemand` the cores start every burst at 408 MHz and take time
                    // to reach 1800 -- which penalises the parallel rows most, because their
                    // bursts are shortest. That is a property of the measurement, not of the
                    // pool.
                    let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ seed;
                    for _ in 0..60_000_000u32 {
                        hash ^= hash >> 33;
                        hash = hash.wrapping_mul(0x1000_0000_01b3);
                    }
                    sink.fetch_add(hash, std::sync::atomic::Ordering::Relaxed);
                });
            }
            batch.wait().expect("no worker panicked");
            let wall = started.elapsed();
            if wall < best {
                best = wall;
                best_cores = (cpu_time() - cpu_before).as_secs_f64() / wall.as_secs_f64();
            }
            std::hint::black_box(sink.load(std::sync::atomic::Ordering::Relaxed));
        }
        let base = *serial.get_or_insert(serial_wall);
        println!(
            "  {count:>7}  {:>12.3?}  {:>11.2}x  {best_cores:>10.2} cores busy",
            best,
            base.as_secs_f64() / best.as_secs_f64()
        );
    }
    println!();
}

/// How many jobs are actually in flight at once.
///
/// The tables above are ratios, and a ratio cannot say whether four workers ran four jobs
/// slowly or two jobs at a time. This counts: each job raises a gauge on entry and lowers it on
/// exit, and the high-water mark is what the pool actually achieved.
#[test]
#[ignore]
fn observed_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    println!("\n  jobs in flight at once, nine jobs of arithmetic:");
    for count in [1usize, 2, 4, 8] {
        let pool = Pool::new(Workers::new(count));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let batch = pool.batch(Priority::Foreground);
        for seed in 0..9u64 {
            let (live, peak) = (Arc::clone(&live), Arc::clone(&peak));
            batch.submit(move || {
                let now = live.fetch_add(1, Ordering::AcqRel) + 1;
                peak.fetch_max(now, Ordering::AcqRel);
                let mut hash = 0xcbf2_9ce4_8422_2325u64 ^ seed;
                for _ in 0..6_000_000u32 {
                    hash ^= hash >> 33;
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
                std::hint::black_box(hash);
                live.fetch_sub(1, Ordering::AcqRel);
            });
        }
        batch.wait().expect("no worker panicked");
        println!(
            "    workers {count:>2}  threads spawned {:>2}  peak in flight {:>2}",
            pool.workers(),
            peak.load(Ordering::Acquire)
        );
    }
    println!();
}
