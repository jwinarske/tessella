//! Cold start, traced (§12.5) — R1's remaining exit criterion.
//!
//! Runs against the in-repo tile server, so the numbers are reproducible and CI needs no
//! network. They are not *representative* — loopback has no latency and the fixture is one tile
//! repeated — which is the point: what this asserts is the shape of the startup, not a
//! wall-clock budget that would only measure the machine it ran on.
//!
//! `live_pmtiles` reports the same trace against real archives, where the numbers mean
//! something.

use std::sync::Arc;
use std::time::Duration;

use tessella_orchestrate::boot::{Boot, BootError, ColdStart, Workers};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::ViewTransform;

const FIXTURE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

fn server() -> tile_server::Server {
    tile_server::Server::start(tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))))
        .expect("binds")
}

/// Wraps a source so each fetch takes a known minimum time.
///
/// Without one, a loopback fetch is faster than the thread that issues it: a serial cold start
/// finishes before a parallel one has spun its workers up, and the fan-out would measure as a
/// pessimisation. The delay is what turns "did the work overlap" into an observation.
struct Slow {
    inner: HttpFileSource,
    delay: Duration,
}

impl FileSource for Slow {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        std::thread::sleep(self.delay);
        self.inner.fetch(url)
    }
}

fn style(origin: &str) -> String {
    format!(
        r##"{{"version": 8,
             "sources": {{"v": {{"type": "vector",
                 "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"], "minzoom": 0, "maxzoom": 6}}}},
             "layers": [
               {{"id": "bg", "type": "background", "paint": {{"background-color": "#000000"}}}},
               {{"id": "water", "type": "fill", "source": "v", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}},
               {{"id": "admin", "type": "line", "source": "v", "source-layer": "admin",
                 "paint": {{"line-color": "#c04030"}}}}]}}"##
    )
}

fn view(zoom: f64) -> ViewTransform {
    tessella_tile::camera::settled(&ViewTransform {
        longitude: -0.11,
        latitude: 51.505,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    })
}

fn boot<S: FileSource + 'static>(
    style: &str,
    view: &ViewTransform,
    files: &Arc<Coalescing<S>>,
    cache: &Arc<TileCache<BootError>>,
    workers: Workers,
) -> Result<Boot, BootError> {
    // A pool per call, not `Pool::shared`: the serial baseline a trace is compared against is
    // exactly "this start with one worker", and the process pool cannot be resized to give it.
    let pool = Pool::new(workers);
    ColdStart {
        style,
        view,
        files: Arc::clone(files),
        cache: Arc::clone(cache),
        pool: &pool,
        priority: Priority::Foreground,
        style_rev: 1,
    }
    .run()
}

/// A cold start reaches geometry, and reports every stage on the way.
#[test]
fn a_cold_start_reaches_the_first_bucket() {
    let server = server();
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(
        &style(&server.origin()),
        &view(4.0),
        &files,
        &cache,
        Workers::default(),
    )
    .expect("boots");

    assert!(!started.tiles.is_empty(), "the cover produced tiles");
    assert!(started.vertices() > 0, "and something tessellated");

    // The stages are monotonic: each finishes no earlier than the one before. A trace whose
    // stages crossed would be measuring from different clocks.
    let t = started.trace;
    assert!(t.style_parsed <= t.sources_resolved, "{t:?}");
    assert!(t.sources_resolved <= t.cover_computed, "{t:?}");
    assert!(t.cover_computed <= t.first_fetch, "{t:?}");
    assert!(t.first_fetch <= t.first_bucket, "{t:?}");
    assert!(t.first_bucket <= t.complete, "{t:?}");
}

/// The first bucket lands before the last one, which is the whole point of reporting it.
///
/// A cold start that only reported completion would say a map with sixty tiles is sixty times
/// slower to *show something* than a map with one, and it is not.
#[test]
fn the_first_bucket_precedes_completion() {
    let server = server();
    let files = Arc::new(Coalescing::new(Slow {
        inner: HttpFileSource::default(),
        delay: Duration::from_millis(20),
    }));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(
        &style(&server.origin()),
        &view(4.0),
        &files,
        &cache,
        Workers::default(),
    )
    .expect("boots");

    assert!(started.tiles.len() >= 4, "{} tiles", started.tiles.len());
    assert!(
        started.trace.first_bucket < started.trace.complete,
        "first {:?} vs complete {:?}",
        started.trace.first_bucket,
        started.trace.complete
    );
}

/// Tile work overlaps: more workers finish a cover sooner.
///
/// Asserted as a ratio rather than a wall-clock bound, so it measures the fan-out rather than
/// the machine.
#[test]
fn the_cover_is_fetched_in_parallel() {
    let server = server();
    let delay = Duration::from_millis(25);
    let text = style(&server.origin());

    let run = |workers: Workers| {
        let files = Arc::new(Coalescing::new(Slow {
            inner: HttpFileSource::default(),
            delay,
        }));
        // A fresh cache each time: reusing one would make the second run a cache hit and
        // measure nothing.
        let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
        boot(&text, &view(4.0), &files, &cache, workers)
            .expect("boots")
            .trace
            .complete
    };

    let serial = run(Workers::serial());
    let parallel = run(Workers::default());
    assert!(
        parallel * 2 < serial,
        "four workers took {parallel:?}, one took {serial:?}"
    );
}

/// A style whose source no layer draws from is not fetched.
///
/// A manifest is a round trip on the critical path. Fetching one for a source nothing reads
/// costs a cold start that time for nothing, and on a slow link it is the whole budget.
#[test]
fn an_unused_source_costs_no_round_trip() {
    let server = server();
    let text = format!(
        r##"{{"version": 8,
             "sources": {{
               "used": {{"type": "vector", "tiles": ["{origin}/{{z}}/{{x}}/{{y}}.pbf"],
                         "minzoom": 0, "maxzoom": 6}},
               "unused": {{"type": "vector", "url": "{origin}/no-such-manifest.json"}}}},
             "layers": [{{"id": "w", "type": "fill", "source": "used",
                          "source-layer": "water", "paint": {{"fill-color": "#3050c0"}}}}]}}"##,
        origin = server.origin()
    );

    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    // If the unused source were resolved, its manifest would 404 and this would fail.
    let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
    assert!(started.vertices() > 0);
    assert!(
        !server
            .paths()
            .iter()
            .any(|path| path.contains("no-such-manifest")),
        "{:?}",
        server.paths()
    );
}

/// A failure names the resource rather than surfacing as a panic in a worker.
#[test]
fn a_dead_origin_fails_with_the_url() {
    let origin = {
        let server = server();
        server.origin()
    };
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    match boot(
        &style(&origin),
        &view(4.0),
        &files,
        &cache,
        Workers::default(),
    ) {
        Err(BootError::Fetch { url, .. }) => assert!(url.starts_with(&origin), "{url}"),
        other => panic!("{other:?}"),
    }
}

/// The worker count is a policy with a default, not a number every caller invents.
#[test]
fn the_worker_count_has_a_policy() {
    assert_eq!(Workers::default().get(), Workers::DEFAULT);
    assert_eq!(Workers::serial().get(), 1);

    // Zero means one: a caller asking for no workers wants the work done, and a cold start
    // that silently did nothing would be worse than a slow one.
    assert_eq!(Workers::new(0).get(), 1);

    // Never more threads than tiles. The extras would start, find the queue empty and exit.
    assert_eq!(Workers::new(16).for_jobs(9), 9);
    assert_eq!(Workers::new(4).for_jobs(9), 4);
    assert_eq!(Workers::new(4).for_jobs(0), 0);
}

/// A cover smaller than the pool still completes.
#[test]
fn a_pool_larger_than_the_cover_is_harmless() {
    let server = server();
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(
        &style(&server.origin()),
        &view(0.0),
        &files,
        &cache,
        Workers::new(64),
    )
    .expect("boots");
    assert!(!started.tiles.is_empty());
    assert!(started.vertices() > 0);
}

/// Four views over one cover build each tile once — §9.3's flatness claim, past the fetch.
///
/// Coalescing already made the *bytes* arrive once. This is the rest of it: decoding and
/// bucket building are process-scoped work (§5.5), and a build that ran per view would be a bug
/// whether or not the fetch was shared.
///
/// The views start together on a barrier, so they all miss the cache — which is the case a
/// cache alone cannot cover and the shared-work table exists for.
#[test]
fn four_views_over_one_cover_build_each_tile_once() {
    const VIEWS: usize = 4;
    let server = server();
    let text = style(&server.origin());
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache = Arc::new(TileCache::<BootError>::new(64));

    let start = Arc::new(std::sync::Barrier::new(VIEWS));
    let handles: Vec<_> = (0..VIEWS)
        .map(|_| {
            let files = Arc::clone(&files);
            let cache = Arc::clone(&cache);
            let start = Arc::clone(&start);
            let text = text.clone();
            std::thread::spawn(move || {
                start.wait();
                let started =
                    boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
                (started.tiles.len(), started.vertices())
            })
        })
        .collect();

    let results: Vec<(usize, usize)> = handles
        .into_iter()
        .map(|handle| handle.join().expect("no panic"))
        .collect();
    assert!(
        results.iter().all(|result| *result == results[0]),
        "every view saw the same map: {results:?}"
    );

    let tiles = results[0].0;
    assert!(tiles >= 4, "{tiles} tiles");
    assert_eq!(
        cache.builds() as usize,
        tiles,
        "one build per tile, not per view"
    );
    // Builds, joins *and* hits: a view that arrives after another finished hits rather than
    // joining, and which of the three happens depends on scheduling. What must hold in every
    // interleaving is that each tile is built once and every caller is answered.
    assert_eq!(
        cache.lookups(),
        (VIEWS * tiles) as u64,
        "every view is accounted for: {} built, {} joined, {} hit",
        cache.builds(),
        cache.joins(),
        cache.hits()
    );
    // The cache is consulted before the network, so a tile whose buckets exist costs no
    // request. Coalescing alone would not give this: it dedupes requests *in flight* and
    // deliberately is not a cache, so a view arriving after another finished would fetch
    // again. Until the byte cache of §12.6 exists, this is what keeps fetches flat.
    assert!(
        server.requests() as usize <= tiles,
        "{} requests for {tiles} tiles",
        server.requests()
    );
}

/// A second view arriving later finds the tiles built, and builds nothing.
#[test]
fn a_later_view_finds_the_cache_warm() {
    let server = server();
    let text = style(&server.origin());
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));

    let first = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
    let builds = cache.builds();
    assert_eq!(builds as usize, first.tiles.len());

    let requests = server.requests();
    let second = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
    assert_eq!(second.tiles.len(), first.tiles.len());
    assert_eq!(
        server.requests(),
        requests,
        "a warm view costs no network at all"
    );
    assert_eq!(cache.builds(), builds, "the second view built nothing");
    assert_eq!(cache.joins(), 0, "and waited on nothing, having hit");
    assert_eq!(cache.hits() as usize, second.tiles.len(), "it hit for each");
}

/// A style with two sources fetches both, and each layer draws from its own.
///
/// This did not work and could not: `boot` took `sets.first()` and built one source, while the
/// tile builder matched layers on `source-layer` alone — so the second source was never
/// fetched and its layers were filled from the first source's tiles. Both halves are fixed, and
/// this is what says so: two origins, two request streams, and the layers landing on the right
/// side of the line.
#[test]
fn a_two_source_style_fetches_both() {
    let world = server();
    let local = server();
    let text = format!(
        r##"{{"version": 8,
             "sources": {{
               "world": {{"type": "vector", "tiles": ["{w}/{{z}}/{{x}}/{{y}}.pbf"],
                          "minzoom": 0, "maxzoom": 6}},
               "local": {{"type": "vector", "tiles": ["{l}/{{z}}/{{x}}/{{y}}.pbf"],
                          "minzoom": 0, "maxzoom": 6}}}},
             "layers": [
               {{"id": "bg", "type": "background",
                 "paint": {{"background-color": "#000000"}}}},
               {{"id": "w-water", "type": "fill", "source": "world", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}},
               {{"id": "l-water", "type": "fill", "source": "local", "source-layer": "water",
                 "paint": {{"fill-color": "#c04030"}}}}]}}"##,
        w = world.origin(),
        l = local.origin()
    );

    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");

    // Both origins were asked, and for the same number of tiles.
    assert!(world.requests() > 0, "the world source was fetched");
    assert!(local.requests() > 0, "and so was the local one");
    assert_eq!(world.requests(), local.requests());

    // One entry per (tile, source), so twice the tiles of a single-source cover.
    let cover: std::collections::BTreeSet<_> =
        started.tiles.iter().map(|built| built.tile).collect();
    assert_eq!(started.tiles.len(), cover.len() * 2, "a tile per source");

    // And each entry carries only its own source's layer.
    for built in &started.tiles {
        let ids: Vec<&str> = built
            .buckets
            .iter()
            .map(|bucket| bucket.layer_id.as_str())
            .collect();
        match built.source.as_str() {
            "world" => assert_eq!(ids, ["w-water"], "{:?}", built.tile),
            "local" => assert_eq!(ids, ["l-water"], "{:?}", built.tile),
            other => panic!("unexpected source {other}"),
        }
    }

    // The background belongs to neither, and appears once per tile rather than once per source.
    assert_eq!(started.sourceless.len(), cover.len());
    for (_, buckets) in &started.sourceless {
        let ids: Vec<&str> = buckets
            .iter()
            .map(|bucket| bucket.layer_id.as_str())
            .collect();
        assert_eq!(ids, ["bg"]);
    }
}

/// A style mixing a vector source and a GeoJSON one builds both.
///
/// The two have different lifecycles and that is the point: the vector source is fetched once
/// per tile because the server cut it up, and the GeoJSON source is fetched *once in total*
/// because this side does the cutting. A cold start has to do both and report one trace.
#[test]
fn a_style_mixing_source_kinds_builds_both() {
    const DOCUMENT: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {"type": "Feature", "properties": {},
         "geometry": {"type": "Polygon",
           "coordinates": [[[-1.0,51.0],[-1.0,52.0],[0.5,52.0],[0.5,51.0],[-1.0,51.0]]]}}
      ]
    }"#;

    let tiles = server();
    let docs = tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        DOCUMENT.as_bytes().to_vec(),
    ))
    .expect("binds");

    let text = format!(
        r##"{{"version": 8,
             "sources": {{
               "v": {{"type": "vector", "tiles": ["{t}/{{z}}/{{x}}/{{y}}.pbf"],
                      "minzoom": 0, "maxzoom": 6}},
               "g": {{"type": "geojson", "data": "{d}/features.geojson"}}}},
             "layers": [
               {{"id": "bg", "type": "background",
                 "paint": {{"background-color": "#000000"}}}},
               {{"id": "v-water", "type": "fill", "source": "v", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}},
               {{"id": "g-area", "type": "fill", "source": "g",
                 "paint": {{"fill-color": "#c04030"}}}}]}}"##,
        t = tiles.origin(),
        d = docs.origin()
    );

    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");

    // The document was asked for exactly once, however many tiles were cut from it.
    assert_eq!(docs.paths(), ["/features.geojson"], "one request in total");
    assert!(tiles.requests() > 1, "and the vector source once per tile");

    let cover: std::collections::BTreeSet<_> =
        started.tiles.iter().map(|built| built.tile).collect();
    assert_eq!(started.tiles.len(), cover.len() * 2, "a tile per source");

    let mut vector_vertices = 0usize;
    let mut geojson_vertices = 0usize;
    for built in &started.tiles {
        for bucket in built.buckets.iter() {
            let n = bucket.content.as_fill().map_or(0, |f| f.vertices.len());
            match built.source.as_str() {
                "v" => {
                    assert_eq!(bucket.layer_id, "v-water");
                    vector_vertices += n;
                }
                "g" => {
                    assert_eq!(bucket.layer_id, "g-area");
                    geojson_vertices += n;
                }
                other => panic!("unexpected source {other}"),
            }
        }
    }
    assert!(vector_vertices > 0, "the vector source tessellated");
    assert!(geojson_vertices > 0, "and so did the GeoJSON one");
}

/// A GeoJSON-only style needs no per-tile fetch at all.
///
/// Its trace has no per-tile fetch to report, so `first_fetch` falls back to the moment the
/// cover was known — which is honest: the only fetch was the document, and it happened during
/// source resolution.
#[test]
fn a_geojson_only_style_fetches_once() {
    const DOCUMENT: &str = r#"{
      "type": "FeatureCollection",
      "features": [
        {"type": "Feature", "properties": {},
         "geometry": {"type": "Polygon",
           "coordinates": [[[-1.0,51.0],[-1.0,52.0],[0.5,52.0],[0.5,51.0],[-1.0,51.0]]]}}
      ]
    }"#;

    let docs = tile_server::Server::start(tile_server::Routes::new().at(
        "/features.geojson",
        "application/json",
        DOCUMENT.as_bytes().to_vec(),
    ))
    .expect("binds");

    let text = format!(
        r##"{{"version": 8,
             "sources": {{"g": {{"type": "geojson", "data": "{d}/features.geojson"}}}},
             "layers": [{{"id": "g-area", "type": "fill", "source": "g",
                          "paint": {{"fill-color": "#c04030"}}}}]}}"##,
        d = docs.origin()
    );

    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");

    assert_eq!(docs.requests(), 1, "the document, once");
    assert!(started.vertices() > 0);
    assert_eq!(
        started.trace.first_fetch, started.trace.cover_computed,
        "no per-tile fetch to report"
    );
    assert_eq!(started.bytes, 0, "no tile bodies were fetched");
}

/// A second process starts warm: same disk cache, fresh in-memory state, no network.
///
/// This is §12.5's warm start, and it needs both caches to be understood as different things.
/// The bucket cache makes a second *view* free within a process. The response cache makes a
/// second *process* free of the network — which is the case a restart is, and the one an
/// in-memory cache cannot help with.
///
/// A fresh `TileCache` is what makes this a restart rather than a repeat: the buckets are gone,
/// so every tile is decoded and tessellated again, and the only thing carried over is the
/// bytes.
#[test]
fn a_second_process_starts_warm() {
    use tessella_storage::cache::{CachingFileSource, SqliteCache};

    let server = server();
    let text = style(&server.origin());
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");

    // First start: cold in every sense.
    let cold_tiles;
    let cold_vertices;
    {
        let files = Arc::new(Coalescing::new(CachingFileSource::new(
            HttpFileSource::default(),
            SqliteCache::open(&path).expect("opens"),
        )));
        let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
        let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
        cold_tiles = started.tiles.len();
        cold_vertices = started.vertices();

        assert!(cold_tiles > 0);
        assert!(
            files.inner().stats().fetched() > 0,
            "it went to the network"
        );
        assert_eq!(files.inner().stats().hits(), 0, "nothing was there yet");
    }
    let requests_after_cold = server.requests();

    // Second start: a new process would have a new bucket cache and the same file on disk.
    let files = Arc::new(Coalescing::new(CachingFileSource::new(
        HttpFileSource::default(),
        SqliteCache::open(&path).expect("reopens"),
    )));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let started = boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");

    assert_eq!(started.tiles.len(), cold_tiles, "the same map");
    assert_eq!(started.vertices(), cold_vertices);
    assert_eq!(
        server.requests(),
        requests_after_cold,
        "and not one request to get it"
    );
    assert_eq!(files.inner().stats().round_trips(), 0, "no round trips");
    assert!(files.inner().stats().hits() > 0, "served from disk");

    // The buckets really were rebuilt: this is a restart, not a repeat.
    assert_eq!(cache.builds() as usize, started.tiles.len());
}

/// The response cache also covers what a cold start needs before it has a cover.
///
/// A TileJSON manifest is a round trip on the critical path, and §12.5 calls it out as the
/// thing serialising startup. It goes through the same file source as the tiles, so it is
/// cached by the same mechanism — which is worth asserting rather than assuming, because the
/// manifest is fetched on a different code path from the tiles.
#[test]
fn a_warm_start_does_not_refetch_the_manifest() {
    use tessella_storage::cache::{CachingFileSource, SqliteCache};

    let tiles = server();
    let manifest = format!(
        r#"{{"tilejson":"3.0.0","tiles":["{t}/{{z}}/{{x}}/{{y}}.pbf"],"minzoom":0,"maxzoom":6}}"#,
        t = tiles.origin()
    );
    let manifests = tile_server::Server::start(tile_server::Routes::new().at(
        "/tiles.json",
        "application/json",
        manifest.into_bytes(),
    ))
    .expect("binds");

    let text = format!(
        r##"{{"version": 8,
             "sources": {{"v": {{"type": "vector", "url": "{m}/tiles.json"}}}},
             "layers": [{{"id": "water", "type": "fill", "source": "v",
                          "source-layer": "water", "paint": {{"fill-color": "#3050c0"}}}}]}}"##,
        m = manifests.origin()
    );

    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");

    for round in 0..2 {
        let files = Arc::new(Coalescing::new(CachingFileSource::new(
            HttpFileSource::default(),
            SqliteCache::open(&path).expect("opens"),
        )));
        let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
        boot(&text, &view(4.0), &files, &cache, Workers::default()).expect("boots");
        assert_eq!(
            manifests.requests(),
            1,
            "the manifest was fetched once, on round {round}"
        );
    }
}

/// A start runs on the process pool, which is the shape production uses (§5.5).
///
/// Every other test here builds a pool of its own so it can pin the worker count. This one
/// exercises the path that actually ships: no threads spawned per view, work queued at
/// foreground onto threads that were already running.
#[test]
fn a_start_runs_on_the_process_pool() {
    let server = server();
    let files = Arc::new(Coalescing::new(HttpFileSource::default()));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let text = style(&server.origin());

    let started = ColdStart {
        style: &text,
        view: &view(4.0),
        files: Arc::clone(&files),
        cache: Arc::clone(&cache),
        pool: Pool::shared(),
        priority: Priority::Foreground,
        style_rev: 1,
    }
    .run()
    .expect("boots");

    assert!(!started.tiles.is_empty());
    assert!(started.tiles.iter().any(|tile| !tile.buckets.is_empty()));

    // A second view over the same cover joins the shared pool rather than starting one, and
    // finds the buckets already built.
    let again = ColdStart {
        style: &text,
        view: &view(4.0),
        files,
        cache,
        pool: Pool::shared(),
        priority: Priority::Foreground,
        style_rev: 1,
    }
    .run()
    .expect("boots again");
    assert_eq!(again.bytes, 0, "no network the second time");
}
