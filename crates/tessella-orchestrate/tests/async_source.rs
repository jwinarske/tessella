//! The non-blocking tile source (§16): what `create` no longer costs, and what a tick starts.
//!
//! # What this is checking
//!
//! §16 closed on Fluorite's answer that no blocking call is acceptable on the thread that will
//! make it. The property that follows is not "tiles arrive" — a cold start already showed that —
//! but *when* they are asked for: nothing before the first `want`, and nothing on the caller's
//! thread at any point.
//!
//! So the assertions are about the file source's own counter. A source that is constructed and
//! never wanted must not have fetched, however long it is left alone; and a `want` must return
//! before the fetches it schedules have finished, which is the whole difference between this and
//! `cold_start`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::BootError;
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::map::Tiles;
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::source::{Readiness, TileSource};
use tessella_orchestrate::tile::TileId;
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::{TileCoord, ViewTransform};

const FIXTURE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/real-world-0-0-0.mvt");

/// Counts fetches, so "did anything go out" is an observation rather than an inference.
struct Counted {
    inner: HttpFileSource,
    fetches: Arc<AtomicUsize>,
}

impl FileSource for Counted {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.fetches.fetch_add(1, Ordering::AcqRel);
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
                 "paint": {{"fill-color": "#3050c0"}}}}]}}"##
    )
}

/// The same style, but with its source behind a TileJSON `url` rather than an inline `tiles`
/// array.
///
/// The distinction matters and cost this test a first draft: an inline `tiles` array is resolved
/// without fetching anything, so a source declared that way cannot fail to resolve however dead
/// the origin is -- its tiles just never arrive, which is a hole rather than a failure. Only a
/// manifest that has to be fetched can fail, which is what makes this the shape that exercises
/// `Failed`.
///
/// The fill layer is load-bearing for the same reason, and cost the draft after that one:
/// resolution only fetches manifests for sources a layer actually draws from, so a style whose
/// only layer is a background resolves successfully however dead its sources are.
fn style_via_manifest(origin: &str) -> String {
    format!(
        r##"{{"version": 8,
             "sources": {{"v": {{"type": "vector", "url": "{origin}/tiles.json"}}}},
             "layers": [
               {{"id": "bg", "type": "background", "paint": {{"background-color": "#000000"}}}},
               {{"id": "water", "type": "fill", "source": "v", "source-layer": "water",
                 "paint": {{"fill-color": "#3050c0"}}}}]}}"##
    )
}

fn view(zoom: f64) -> ViewTransform {
    ViewTransform {
        longitude: 0.0,
        latitude: 0.0,
        zoom,
        width: 1024.0,
        height: 768.0,
        bearing: 0.0,
        pitch: 0.0,
    }
}

/// Builds a source over a counted HTTP file source.
fn source_over_with(
    document: String,
) -> (
    Arc<TileSource<Counted>>,
    Arc<AtomicUsize>,
    Arc<Coalescing<Counted>>,
) {
    let fetches = Arc::new(AtomicUsize::new(0));
    let files = Arc::new(Coalescing::new(Counted {
        inner: HttpFileSource::new(Duration::from_secs(30)),
        fetches: Arc::clone(&fetches),
    }));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(document, Arc::clone(&files), cache, Pool::shared(), 1);
    (source, fetches, files)
}

/// The common case: a source whose tiles are declared inline.
fn source_over(
    origin: &str,
) -> (
    Arc<TileSource<Counted>>,
    Arc<AtomicUsize>,
    Arc<Coalescing<Counted>>,
) {
    source_over_with(style(origin))
}

/// Spins until `done` or the deadline, without holding a lock while it waits.
fn settle(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// A source that is built and left alone fetches nothing.
///
/// This is §16's decision stated as a test. `cold_start` cannot pass it — resolving the sources
/// *is* what it does — and that is precisely the property that made it unusable from a thread
/// with a frame budget.
#[test]
fn construction_touches_the_network_not_at_all() {
    let server = tile_server::Server::start(
        tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))),
    )
    .expect("binds");

    let (source, fetches, _files) = source_over(&server.origin());

    // Long enough that a fetch issued from the constructor would have landed several times over.
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        fetches.load(Ordering::Acquire),
        0,
        "a source that was never wanted fetched something"
    );
    assert_eq!(
        source.readiness(),
        Readiness::Idle,
        "and it has not started resolving"
    );
}

/// The first `want` starts resolution and returns before it finishes.
#[test]
fn wanting_schedules_rather_than_waits() {
    let server = tile_server::Server::start(
        tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))),
    )
    .expect("binds");

    let (source, _fetches, _files) = source_over(&server.origin());
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];

    source.want(&view(0.0), &cover);

    // Resolution runs on a worker, so this thread carried on. `Idle` is the one answer that
    // would mean the call did nothing at all.
    assert_ne!(
        source.readiness(),
        Readiness::Idle,
        "want() left the source idle"
    );

    assert!(
        settle(|| source.readiness() == Readiness::Ready),
        "the source never became ready: {:?}",
        source.readiness()
    );
}

/// Tiles land, and a later `want` finds them without asking again.
#[test]
fn tiles_arrive_and_are_not_refetched() {
    let server = tile_server::Server::start(
        tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))),
    )
    .expect("binds");

    let (source, fetches, _files) = source_over(&server.origin());
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];
    let tile = TileId::new(0, 0, 0);

    // The first want resolves; the second, once resolved, plans and submits.
    source.want(&view(0.0), &cover);
    assert!(
        settle(|| source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(0.0), &cover);

    assert!(
        settle(|| source.buckets(tile).is_some()),
        "the tile never landed"
    );
    assert!(
        source.sourceless(tile).is_some(),
        "the background layer never landed, so the first frame would have nothing behind its tiles"
    );

    let after_landing = fetches.load(Ordering::Acquire);

    // Ten more ticks over the same cover. Every one plans the same job, and every one must find
    // it already built -- this is what stops a settled camera from re-fetching its own view.
    for _ in 0..10 {
        source.want(&view(0.0), &cover);
    }
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        fetches.load(Ordering::Acquire),
        after_landing,
        "wanting a tile that had already landed fetched it again"
    );
}

/// A style whose sources cannot resolve says so, rather than staying blank and quiet.
///
/// The hazard §16 names: a consumer holding a handle, looking at an empty map, with no way to
/// learn why. `Failed` carries the reason, so there is something to report.
#[test]
fn a_source_that_cannot_resolve_reports_it() {
    // A port nothing is listening on, and a source that must fetch a manifest to resolve.
    let (source, _fetches, _files) = source_over_with(style_via_manifest("http://127.0.0.1:1"));
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];

    source.want(&view(0.0), &cover);

    assert!(
        settle(|| matches!(source.readiness(), Readiness::Failed(_))),
        "a source pointed at nothing never reported a failure: {:?}",
        source.readiness()
    );
    let Readiness::Failed(reason) = source.readiness() else {
        unreachable!("just asserted")
    };
    assert!(!reason.is_empty(), "the failure carried no reason");
}
