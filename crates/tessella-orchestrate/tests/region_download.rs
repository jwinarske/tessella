//! Region downloads across the process pool (§5.4).

#![cfg(all(feature = "offline", feature = "std"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tessella_orchestrate::offline::{Counters, RegionDownload};
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::cache::SqliteCache;
use tessella_storage::download::Download;
use tessella_storage::http::HttpFileSource;
use tessella_storage::offline::Region;
use tessella_storage::source::{FetchError, FileSource, Response};
use tessella_tile::cover::Bounds;

const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");
const NOW: i64 = 1_000_000;

/// Waits for a condition, failing rather than hanging if it never comes.
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

fn style(origin: &str) -> tessella_style::Style {
    serde_json::from_str(&format!(
        r##"{{
          "version": 8,
          "sources": {{
            "base": {{ "type": "vector",
                       "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.mvt"],
                       "minzoom": 0, "maxzoom": 14 }}
          }},
          "layers": []
        }}"##
    ))
    .expect("a style")
}

/// A box covering a handful of tiles per zoom, over several zooms.
fn region(origin: &str, max_zoom: f64) -> Region {
    Region {
        style_url: format!("{origin}/style.json"),
        bounds: Bounds::new(13.0, 52.3, 13.8, 52.7),
        min_zoom: 0.0,
        max_zoom,
        pixel_ratio: 1.0,
        include_ideographs: false,
    }
}

fn routes() -> tile_server::Routes {
    tile_server::Routes::new()
        .at("/style.json", "application/json", b"{}".to_vec())
        .tiles(TILE.to_vec(), Some((0, 14)))
}

/// A source that takes its time, so concurrency is observable rather than inferred.
#[derive(Debug)]
struct Slow<S> {
    inner: S,
    delay: Duration,
    in_flight: AtomicU64,
    peak: AtomicU64,
}

impl<S: FileSource> FileSource for Slow<S> {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let now = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(now, Ordering::AcqRel);
        std::thread::sleep(self.delay);
        let response = self.inner.fetch(url);
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
        response
    }

    fn fetch_conditional(&self, url: &str, etag: Option<&str>) -> Result<Response, FetchError> {
        self.inner.fetch_conditional(url, etag)
    }
}

fn slow(delay: Duration) -> Arc<Slow<HttpFileSource>> {
    Arc::new(Slow {
        inner: HttpFileSource::default(),
        delay,
        in_flight: AtomicU64::new(0),
        peak: AtomicU64::new(0),
    })
}

fn plan_for(
    cache: &Arc<SqliteCache>,
    files: &Arc<Slow<HttpFileSource>>,
    region: &Region,
    id: tessella_storage::cache::RegionId,
    style: &tessella_style::Style,
) -> tessella_storage::offline::Plan {
    Download {
        cache,
        files: &**files,
        region: id,
        definition: region,
        now: NOW,
    }
    .plan(style)
    .expect("plans")
}

/// A download fetches everything, and fetches it in parallel.
#[test]
fn a_region_downloads_across_the_pool() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let pool = Pool::new(tessella_orchestrate::boot::Workers::new(4));
    let cache = Arc::new(SqliteCache::in_memory_with_capacity(4_000_000).expect("opens"));
    let files = slow(Duration::from_millis(15));
    let definition = region(&origin, 8.0);
    let id = cache
        .create_region(&definition, Some("Berlin"), NOW)
        .expect("creates");
    let plan = plan_for(&cache, &files, &definition, id, &style(&origin));

    let counters = Arc::new(Counters::default());
    let outcome = RegionDownload {
        pool: &pool,
        cache: Arc::clone(&cache),
        files: Arc::clone(&files),
        region: id,
        definition: Arc::new(definition),
        cancel: Arc::new(AtomicBool::new(false)),
        now: NOW,
        counters: Arc::clone(&counters),
    }
    .run(&plan)
    .expect("downloads");

    assert!(!outcome.cancelled);
    let expected = plan.len() as u64 + 1;
    assert_eq!(counters.completed.load(Ordering::Acquire), expected);
    assert_eq!(counters.fraction(), Some(1.0));
    assert_eq!(outcome.fetched + outcome.held + outcome.missing, expected);
    assert_eq!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources,
        expected
    );

    assert!(
        files.peak.load(Ordering::Acquire) > 1,
        "fetches overlapped: peak {}",
        files.peak.load(Ordering::Acquire)
    );
}

/// Assets are finished before any tile starts.
///
/// A download stopped halfway is far more useful with a style and no tiles than with tiles and
/// nothing to draw them with. Fanning both out together would interleave them into no order.
#[test]
fn assets_finish_before_tiles_begin() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let pool = Pool::new(tessella_orchestrate::boot::Workers::new(4));
    let cache = Arc::new(SqliteCache::in_memory_with_capacity(4_000_000).expect("opens"));
    let files = slow(Duration::from_millis(5));
    let definition = region(&origin, 6.0);
    let id = cache
        .create_region(&definition, None, NOW)
        .expect("creates");
    let plan = plan_for(&cache, &files, &definition, id, &style(&origin));

    RegionDownload {
        pool: &pool,
        cache: Arc::clone(&cache),
        files: Arc::clone(&files),
        region: id,
        definition: Arc::new(definition),
        cancel: Arc::new(AtomicBool::new(false)),
        now: NOW,
        counters: Arc::new(Counters::default()),
    }
    .run(&plan)
    .expect("downloads");

    let paths = server.paths();
    let first_tile = paths
        .iter()
        .position(|path| path.ends_with(".mvt"))
        .expect("a tile");
    let last_asset = paths
        .iter()
        .rposition(|path| !path.ends_with(".mvt"))
        .expect("an asset");
    assert!(last_asset < first_tile, "{paths:?}");
}

/// Cancelling stops the download and keeps what it got.
#[test]
fn cancelling_stops_and_keeps() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let pool = Pool::new(tessella_orchestrate::boot::Workers::new(2));
    let cache = Arc::new(SqliteCache::in_memory_with_capacity(4_000_000).expect("opens"));
    let files = slow(Duration::from_millis(10));
    let definition = region(&origin, 11.0);
    let id = cache
        .create_region(&definition, None, NOW)
        .expect("creates");
    let plan = plan_for(&cache, &files, &definition, id, &style(&origin));
    assert!(plan.tiles.len() > 40, "enough to cancel part way through");

    let cancel = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Counters::default());

    let watcher = {
        let cancel = Arc::clone(&cancel);
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            until("some progress", || {
                counters.completed.load(Ordering::Acquire) >= 5
            });
            cancel.store(true, Ordering::Release);
        })
    };

    let outcome = RegionDownload {
        pool: &pool,
        cache: Arc::clone(&cache),
        files,
        region: id,
        definition: Arc::new(definition),
        cancel,
        now: NOW,
        counters: Arc::clone(&counters),
    }
    .run(&plan)
    .expect("stops cleanly");
    watcher.join().expect("the watcher");

    assert!(outcome.cancelled);
    let done = counters.completed.load(Ordering::Acquire);
    assert!(done >= 5, "kept what it got: {done}");
    assert!(done < plan.len() as u64 + 1, "and stopped early: {done}");
    assert_eq!(
        cache
            .region_progress(id)
            .expect("reads")
            .completed_resources,
        counters.fetched.load(Ordering::Acquire) + counters.held.load(Ordering::Acquire)
    );
}

/// Foreground work still runs promptly while a region download is in flight.
///
/// An end-to-end check that the classes compose, not an isolation of any one rule — the
/// waiter's priority floor is pinned down directly in `tests/pool.rs`, where removing it hangs
/// the test. What this rules out is the gross failure: submitting hours of background fetching
/// and finding that a view can no longer draw.
#[test]
fn a_download_does_not_block_foreground_work() {
    let server = tile_server::Server::start(routes()).expect("binds");
    let origin = server.origin();
    let pool = Arc::new(Pool::new(tessella_orchestrate::boot::Workers::new(2)));
    let cache = Arc::new(SqliteCache::in_memory_with_capacity(4_000_000).expect("opens"));
    // Slow enough that a foreground batch waiting behind it would be obvious.
    let files = slow(Duration::from_millis(40));
    let definition = region(&origin, 11.0);
    let id = cache
        .create_region(&definition, None, NOW)
        .expect("creates");
    let plan = plan_for(&cache, &files, &definition, id, &style(&origin));
    assert!(plan.tiles.len() > 40);

    let counters = Arc::new(Counters::default());
    let cancel = Arc::new(AtomicBool::new(false));

    let downloading = {
        let pool = Arc::clone(&pool);
        let cache = Arc::clone(&cache);
        let counters = Arc::clone(&counters);
        let cancel = Arc::clone(&cancel);
        let definition = Arc::new(definition);
        std::thread::spawn(move || {
            RegionDownload {
                pool: &pool,
                cache,
                files,
                region: id,
                definition,
                cancel,
                now: NOW,
                counters,
            }
            .run(&plan)
        })
    };

    // Wait until the download is genuinely occupying the pool.
    until("the download to be under way", || {
        counters.completed.load(Ordering::Acquire) >= 2
    });

    // A foreground batch of pure computation. It must not wait on the download's fetches.
    let started = Instant::now();
    let batch = pool.batch(Priority::Foreground);
    for _ in 0..8 {
        batch.submit(|| {});
    }
    batch.wait().expect("no panics");
    let waited = started.elapsed();

    cancel.store(true, Ordering::Release);
    let outcome = downloading.join().expect("the download thread");
    assert!(outcome.expect("stops cleanly").cancelled);

    // Two workers are inside 40 ms fetches, so the waiter runs all eight itself. Generous by an
    // order of magnitude: what is being ruled out is waiting on the fetches, not a tight bound.
    assert!(
        waited < Duration::from_millis(200),
        "foreground work waited {waited:?} behind a background download"
    );
}
