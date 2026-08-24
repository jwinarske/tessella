//! Cold start, traced (§12.5) — R1's remaining exit criterion.
//!
//! Runs against the in-repo tile server, so the numbers are reproducible and CI needs no
//! network. They are not *representative* — loopback has no latency and the fixture is one
//! tile repeated — which is the point: what this asserts is the shape of the startup, not a
//! wall-clock budget that would only measure the machine it ran on.
//!
//! `live_pmtiles` reports the same trace against real archives, where the numbers mean
//! something.

use std::time::Duration;

use tessella_orchestrate::boot::{BootError, cold_start};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
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

impl tessella_storage::source::FileSource for Slow {
    fn fetch(
        &self,
        url: &str,
    ) -> Result<tessella_storage::source::Response, tessella_storage::source::FetchError> {
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

/// A cold start reaches geometry, and reports every stage on the way.
#[test]
fn a_cold_start_reaches_the_first_bucket() {
    let server = server();
    let files = Coalescing::new(HttpFileSource::default());
    let boot = cold_start(&style(&server.origin()), &view(4.0), &files, 4).expect("boots");

    assert!(!boot.tiles.is_empty(), "the cover produced tiles");
    assert!(boot.vertices() > 0, "and something tessellated");

    // The stages are monotonic: each finishes no earlier than the one before it. A trace whose
    // stages crossed would be measuring from different clocks.
    let t = boot.trace;
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
    let files = Coalescing::new(Slow {
        inner: HttpFileSource::default(),
        delay: Duration::from_millis(20),
    });
    let boot = cold_start(&style(&server.origin()), &view(4.0), &files, 4).expect("boots");

    assert!(boot.tiles.len() >= 4, "{} tiles", boot.tiles.len());
    assert!(
        boot.trace.first_bucket < boot.trace.complete,
        "first {:?} vs complete {:?}",
        boot.trace.first_bucket,
        boot.trace.complete
    );
}

/// Tile work overlaps: more workers finish a cover sooner.
///
/// Asserted as a ratio rather than a wall-clock bound, so it measures the fan-out rather than
/// the machine. With a 20 ms delay per fetch and at least four tiles, a serial start pays at
/// least four delays and a four-way one pays about one.
#[test]
fn the_cover_is_fetched_in_parallel() {
    let server = server();
    let delay = Duration::from_millis(25);
    let text = style(&server.origin());

    let run = |workers: usize| {
        let files = Coalescing::new(Slow {
            inner: HttpFileSource::default(),
            delay,
        });
        cold_start(&text, &view(4.0), &files, workers)
            .expect("boots")
            .trace
            .complete
    };

    let serial = run(1);
    let parallel = run(4);
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

    let files = Coalescing::new(HttpFileSource::default());
    // If the unused source were resolved, its manifest would 404 and this would fail.
    let boot = cold_start(&text, &view(4.0), &files, 2).expect("boots");
    assert!(boot.vertices() > 0);
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
    let files = Coalescing::new(HttpFileSource::default());
    match cold_start(&style(&origin), &view(4.0), &files, 4) {
        Err(BootError::Fetch { url, .. }) => assert!(url.starts_with(&origin), "{url}"),
        other => panic!("{other:?}"),
    }
}
