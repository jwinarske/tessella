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
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::BootError;
use tessella_orchestrate::boot::Workers;
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::deferred::TileTransport;
use tessella_orchestrate::map::Tiles;
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::source::{Readiness, TileSource};
use tessella_orchestrate::tile::TileId;
use tessella_storage::deferred::{DeferredFileSource, Ticket, Tickets};
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

/// Builds a source over a counted HTTP file source, on a named pool.
fn source_over_pool(
    document: String,
    pool: &'static Pool,
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
    let source = TileSource::new(document, Arc::clone(&files), cache, pool, 1);
    (source, fetches, files)
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

/// Ticks until `done` or the deadline, without holding a lock while it waits.
///
/// Drains on every pass, because that is what a tick does. The transport answers a request with
/// a ticket rather than with bytes, so nothing lands until someone comes back for it -- a loop
/// that only polled `done` would wait out its full deadline on tiles whose bodies had arrived
/// long before.
fn settle<S: FileSource + 'static, D: TileTransport + 'static>(
    source: &Arc<TileSource<S, D>>,
    mut done: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        source.drain();
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

    source.want(&view(0.0), &cover, &[]);

    // Resolution runs on a worker, so this thread carried on. `Idle` is the one answer that
    // would mean the call did nothing at all.
    assert_ne!(
        source.readiness(),
        Readiness::Idle,
        "want() left the source idle"
    );

    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
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
    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(0.0), &cover, &[]);

    assert!(
        settle(&source, || source.buckets(tile).is_some()),
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
        source.want(&view(0.0), &cover, &[]);
    }
    std::thread::sleep(Duration::from_millis(200));

    assert_eq!(
        fetches.load(Ordering::Acquire),
        after_landing,
        "wanting a tile that had already landed fetched it again"
    );
}

/// Nothing lands without a drain, and one drain is enough.
///
/// The contract the split introduced, stated so it cannot be lost: `want` asks the transport for
/// bytes and returns, and the tile appears only when someone comes back for the answer. A
/// consumer that forgets to drain gets a map that resolves, reports work outstanding, and never
/// draws a tile -- which looks exactly like a dead origin and is not one.
#[test]
fn a_tile_lands_on_the_drain_and_not_before() {
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
    let tile = TileId::new(0, 0, 0);

    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "never resolved"
    );
    source.want(&view(0.0), &cover, &[]);

    // Long enough for the fetch to have finished several times over. Without a drain the bytes
    // sit in the ticket table and the tile is not built.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        source.buckets(tile).is_none(),
        "a tile landed without anything draining for it"
    );
    assert_eq!(
        source.outstanding(),
        1,
        "the tile stopped counting as in flight"
    );

    // One drain starts the build; the build itself still runs on a worker.
    source.drain();
    let deadline = Instant::now() + Duration::from_secs(20);
    while source.buckets(tile).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        source.buckets(tile).is_some(),
        "the tile never landed after its drain"
    );
}

/// A tile whose fetch failed is asked for again, rather than counting as in flight for ever.
///
/// The failure path through the drain. A tile that is planned goes into the in-flight set so the
/// next tick does not ask twice, and only something clearing it lets the tick after that retry --
/// so a transport error that forgot to clear would turn one refused connection into a permanently
/// blank coordinate. The origin here is a port nothing is listening on, while the source itself
/// is declared inline so that resolution still succeeds and the *tile* is the thing that fails.
#[test]
fn a_failed_fetch_is_counted_and_the_tile_is_asked_for_again() {
    let (source, fetches, _files) = source_over("http://127.0.0.1:1");
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];

    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "an inline source never resolved"
    );

    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || source.failures().0 > 0),
        "a refused connection was never counted as a failure"
    );
    let after_first = fetches.load(Ordering::Acquire);
    assert!(after_first > 0, "nothing was ever fetched");

    // Nothing is in flight any more, so the next tick may ask again -- and does.
    assert!(
        settle(&source, || source.outstanding() == 0),
        "the failed tile is still counted as in flight"
    );
    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || fetches.load(Ordering::Acquire) > after_first),
        "a tile whose fetch failed was never asked for again"
    );
}

/// The whole producer on one thread, with one call site per tick.
///
/// DR-24's claim about what native gains from the wasm work, stated as a test. A pool with no
/// workers has nobody to run its jobs but the thread that ticks it, so this drives the exact loop
/// the FFI drives in that mode -- run what is queued, drain the transport, run what that queued.
/// Nothing else in the producer needs to know which mode it is in, which is the property that
/// makes the trace worth having.
#[test]
fn the_producer_runs_on_one_thread_when_the_pool_has_no_workers() {
    let server = tile_server::Server::start(
        tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))),
    )
    .expect("binds");

    // Leaked because a source holds its pool for `'static`, and a pool with no threads has
    // nothing to join, so never dropping it costs the allocation and nothing else.
    let pool: &'static Pool = Box::leak(Box::new(Pool::new(Workers::none())));
    let (source, fetches, _files) = source_over_pool(style(&server.origin()), pool);
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];
    let tile = TileId::new(0, 0, 0);

    let tick = || {
        pool.drain(usize::MAX);
        source.drain();
        pool.drain(usize::MAX);
    };

    source.want(&view(0.0), &cover, &[]);
    // No worker exists, so nothing has happened yet -- not even the style resolution, which is
    // the first thing `want` queues.
    assert_eq!(source.readiness(), Readiness::Resolving);
    assert_eq!(fetches.load(Ordering::Acquire), 0);

    let deadline = Instant::now() + Duration::from_secs(20);
    while source.buckets(tile).is_none() && Instant::now() < deadline {
        tick();
        source.want(&view(0.0), &cover, &[]);
    }

    assert_eq!(source.readiness(), Readiness::Ready, "never resolved");
    assert!(
        source.buckets(tile).is_some(),
        "the tile never landed on a pool nobody but this thread was running"
    );
    assert_eq!(source.outstanding(), 0);
    assert!(
        pool.is_idle(),
        "work was left queued on a pool with no workers"
    );
}

/// A transport the test drives by hand, answering nothing until it is told to.
///
/// Not a mock of `PoolBacked` -- it shares no code with it, which is the point. If the tile path
/// works over this, it works over anything that answers the trait, and a browser's `fetch` is
/// another such thing.
#[derive(Default)]
struct Manual {
    tickets: Tickets,
    asked: Mutex<Vec<(Ticket, String)>>,
}

impl Manual {
    /// The URLs asked for so far, in order.
    fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, url)| url.clone())
            .collect()
    }

    /// Answers the first outstanding request with `body`.
    fn answer(&self, body: &[u8]) {
        let held = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
        let (ticket, _) = *held.first().expect("something was asked for");
        drop(held);
        self.tickets.post(
            ticket,
            Ok(Arc::new(Response {
                status: 200,
                body: body.to_vec(),
                ..Response::default()
            })),
        );
    }
}

impl DeferredFileSource for Manual {
    fn request(&self, url: &str, _etag: Option<&str>) -> Ticket {
        let ticket = self.tickets.issue();
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((ticket, url.to_string()));
        ticket
    }

    fn poll(&self, ticket: Ticket) -> Option<tessella_storage::source::Fetched> {
        self.tickets.take(ticket)
    }

    fn cancel(&self, ticket: Ticket) {
        self.tickets.cancel(ticket);
    }

    fn outstanding(&self) -> usize {
        self.tickets.open()
    }
}

// The default: one queue, and no class to put a request in. Exactly the shape a browser has.
impl TileTransport for Manual {}

/// A source that would fail if the tile path ever reached for it.
struct Unused;

impl FileSource for Unused {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        Err(FetchError::Transport {
            url: url.to_string(),
            message: "the blocking source should not have been asked".to_string(),
        })
    }
}

/// Tiles arrive over whatever answers the transport trait, and only when it answers.
///
/// The seam DR-23 exists for, tested without a network. The style declares its tiles inline, so
/// resolution needs no fetch and the only thing that reaches for bytes is the tile path -- which
/// here reaches a transport the test controls completely.
#[test]
fn tiles_arrive_over_a_transport_that_is_not_the_pool() {
    let manual = Arc::new(Manual::default());
    let files = Arc::new(Coalescing::new(Unused));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::with_transport(
        style("http://tiles.invalid"),
        files,
        Arc::clone(&manual),
        cache,
        Pool::shared(),
        1,
    );
    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];
    let tile = TileId::new(0, 0, 0);

    source.want(&view(0.0), &cover, &[]);
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "an inline source never resolved"
    );
    source.want(&view(0.0), &cover, &[]);

    assert_eq!(
        manual.asked(),
        vec!["http://tiles.invalid/0/0/0.pbf".to_string()],
        "the tile path did not go through the supplied transport"
    );

    // Nothing has answered, so draining finds nothing however often it runs.
    for _ in 0..5 {
        source.drain();
    }
    assert!(source.buckets(tile).is_none());
    assert_eq!(source.outstanding(), 1);

    // The test decides when the bytes exist. No timing, no network, no sleep.
    manual.answer(FIXTURE);
    source.drain();

    let deadline = Instant::now() + Duration::from_secs(20);
    while source.buckets(tile).is_none() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        source.buckets(tile).is_some(),
        "the tile never landed from bytes the transport supplied"
    );
    assert_eq!(source.outstanding(), 0);
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

    source.want(&view(0.0), &cover, &[]);

    assert!(
        settle(&source, || matches!(
            source.readiness(),
            Readiness::Failed(_)
        )),
        "a source pointed at nothing never reported a failure: {:?}",
        source.readiness()
    );
    let Readiness::Failed(reason) = source.readiness() else {
        unreachable!("just asserted")
    };
    assert!(!reason.is_empty(), "the failure carried no reason");
}

/// A source resolving its style is not "nothing further is coming".
///
/// `outstanding` is what a caller waiting on a settled frame reads, and it counted `inflight` --
/// tiles *submitted* and not landed. While `readiness` is `Resolving` nothing has been submitted,
/// because a tile's URL comes from a manifest that has not arrived, so the number was zero for the
/// whole of that round trip. A settle loop stops there, on a frame with no tiles in it.
///
/// Held in `Resolving` deliberately rather than raced for: the window is a network round trip, so
/// a test that merely looked quickly would pass on a slow day and prove nothing on a fast one.
#[test]
fn resolving_a_style_counts_as_outstanding() {
    /// Blocks the manifest fetch until the test says otherwise.
    struct Gated {
        inner: HttpFileSource,
        gate: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl FileSource for Gated {
        fn fetch(&self, url: &str) -> Result<Response, FetchError> {
            if url.ends_with("/tiles.json") {
                let (lock, cv) = &*self.gate;
                let mut open = lock.lock().unwrap_or_else(PoisonError::into_inner);
                while !*open {
                    open = cv.wait(open).unwrap_or_else(PoisonError::into_inner);
                }
            }
            self.inner.fetch(url)
        }
    }

    let server = tile_server::Server::start(
        tile_server::Routes::new().tiles(FIXTURE.to_vec(), Some((0, 14))),
    )
    .expect("binds");
    // The manifest has to name the server, and the server's port is only known once it is bound.
    let manifest = format!(
        r##"{{"tilejson":"3.0.0","tiles":["{origin}/{{z}}/{{x}}/{{y}}.mvt"],
             "minzoom":0,"maxzoom":14}}"##,
        origin = server.origin()
    );
    server.set_routes(
        tile_server::Routes::new()
            .tiles(FIXTURE.to_vec(), Some((0, 14)))
            .at("/tiles.json", "application/json", manifest.into_bytes()),
    );

    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let files = Arc::new(Coalescing::new(Gated {
        inner: HttpFileSource::new(Duration::from_secs(30)),
        gate: Arc::clone(&gate),
    }));
    let cache: Arc<TileCache<BootError>> = Arc::new(TileCache::new(64));
    let source = TileSource::new(
        style_via_manifest(&server.origin()),
        files,
        cache,
        Pool::shared(),
        1,
    );

    let cover = [TileCoord {
        z: 0,
        x: 0,
        y: 0,
        wrap: 0,
    }];
    source.want(&view(0.0), &cover, &[]);

    assert!(
        settle(&source, || source.readiness() == Readiness::Resolving),
        "the source never started resolving: {:?}",
        source.readiness()
    );
    assert!(
        source.outstanding() > 0,
        "a source resolving its style reported nothing outstanding, which is what a settle loop \
         reads as done"
    );

    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        cv.notify_all();
    }
    assert!(
        settle(&source, || source.readiness() == Readiness::Ready),
        "the source never became ready: {:?}",
        source.readiness()
    );
}
