//! The response cache and etag revalidation (§12.6).

#![cfg(all(feature = "cache", feature = "http"))]

use std::cell::Cell;

use tessella_storage::cache::{CachingFileSource, SqliteCache};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::{FileSource, Response};

const BODY: &[u8] = b"a body worth caching";

// A clock the tests move, since freshness is the whole of what the cache decides.
//
// Thread-local, not a static: cargo runs the tests in this binary in parallel, and a shared
// clock means one test winding time forward expires another's entries out from under it. That
// is not hypothetical — it is what happened. Each test drives the fetch on its own thread, so a
// thread-local is exactly the right scope.
thread_local! {
    static CLOCK: Cell<i64> = const { Cell::new(1_000_000) };
}
fn clock() -> i64 {
    CLOCK.with(Cell::get)
}
fn set_clock(value: i64) {
    CLOCK.with(|clock| clock.set(value));
}

fn server(cache_control: Option<&'static str>) -> tile_server::Server {
    let mut routes = tile_server::Routes::new().at("/thing", "text/plain", BODY.to_vec());
    if let Some(value) = cache_control {
        routes = routes.cache_control(value);
    }
    tile_server::Server::start(routes).expect("binds")
}

fn caching(server: &tile_server::Server) -> CachingFileSource<HttpFileSource> {
    let _ = server;
    CachingFileSource::with_clock(
        HttpFileSource::default(),
        SqliteCache::in_memory().expect("opens"),
        clock,
    )
}

/// A fresh entry is served without touching the network.
#[test]
fn a_fresh_entry_costs_no_request() {
    set_clock(1_000_000);
    let server = server(Some("max-age=3600"));
    let files = caching(&server);
    let url = format!("{}/thing", server.origin());

    let first = files.fetch(&url).expect("fetches");
    assert_eq!(first.body, BODY);
    assert_eq!(server.requests(), 1);

    let second = files.fetch(&url).expect("hits");
    assert_eq!(second.body, BODY);
    assert_eq!(server.requests(), 1, "the second call went nowhere");
}

/// A stale entry is revalidated, and a matching etag costs a round trip and no body.
#[test]
fn a_stale_entry_revalidates_to_a_304() {
    set_clock(1_000_000);
    let server = server(Some("max-age=10"));
    let files = caching(&server);
    let url = format!("{}/thing", server.origin());

    assert_eq!(files.fetch(&url).expect("fetches").body, BODY);
    assert_eq!(server.requests(), 1);

    // Past the max-age, so the entry is stale and must be checked.
    set_clock(1_000_100);
    let again = files.fetch(&url).expect("revalidates");
    assert_eq!(server.requests(), 2, "the origin was asked");
    assert_eq!(
        again.body, BODY,
        "and answered 304, so the held body was served"
    );

    // The refresh moved the expiry, so the next call is a hit again.
    let third = files.fetch(&url).expect("hits");
    assert_eq!(third.body, BODY);
    assert_eq!(server.requests(), 2, "no third request");
}

/// A stated expiry is absolute once stored, not relative to when it is read.
///
/// `max-age` is relative to the response, so storing it as written would give an entry that is
/// perpetually ten seconds from expiring however long it sat on disk.
#[test]
fn max_age_becomes_an_absolute_expiry() {
    set_clock(1_000_000);
    let server = server(Some("max-age=10"));
    let files = caching(&server);
    let url = format!("{}/thing", server.origin());
    files.fetch(&url).expect("fetches");

    let entry = files
        .cache()
        .get(&url, clock())
        .expect("queries")
        .expect("was stored");
    assert_eq!(
        entry.expires,
        Some(1_000_010),
        "absolute, resolved when stored"
    );
    assert!(entry.is_usable(1_000_009));
    assert!(!entry.is_usable(1_000_011));
}

/// An origin asking for revalidation is never served unasked, however fresh the copy.
#[test]
fn must_revalidate_is_never_served_unasked() {
    set_clock(1_000_000);
    let server = server(Some("max-age=3600, must-revalidate"));
    let files = caching(&server);
    let url = format!("{}/thing", server.origin());

    files.fetch(&url).expect("fetches");
    let entry = files
        .cache()
        .get(&url, clock())
        .expect("queries")
        .expect("was stored");
    assert!(entry.response.must_revalidate);
    assert!(
        entry.expires.is_none_or(|expires| expires > clock()),
        "fresh by the clock, and still not usable"
    );
    assert!(!entry.is_usable(clock()));

    files.fetch(&url).expect("revalidates");
    assert_eq!(server.requests(), 2, "asked again despite being fresh");
}

/// No stated expiry is fresh, not stale.
///
/// mbgl's rule, and the difference between a cache that works and one that revalidates
/// everything on every start.
#[test]
fn silence_about_expiry_is_freshness() {
    set_clock(1_000_000);
    let server = server(None);
    let files = caching(&server);
    let url = format!("{}/thing", server.origin());

    files.fetch(&url).expect("fetches");
    assert_eq!(server.requests(), 1);

    set_clock(2_000_000_000);
    files.fetch(&url).expect("hits");
    assert_eq!(server.requests(), 1, "still held, years later");
}

/// A stale entry survives a dead origin.
///
/// A map that goes blank when the link drops is worse than one showing tiles from a minute ago,
/// which is why mbgl's own schema says expired tiles can still be rendered.
#[test]
fn a_stale_entry_outlives_its_origin() {
    set_clock(1_000_000);
    let cache = SqliteCache::in_memory().expect("opens");
    let url;
    {
        let server = server(Some("max-age=10"));
        url = format!("{}/thing", server.origin());
        let files = CachingFileSource::with_clock(HttpFileSource::default(), cache, clock);
        files.fetch(&url).expect("fetches");

        // Past its expiry, and the origin is about to go away.
        set_clock(1_000_100);
        let served = {
            drop(server);
            files.fetch(&url)
        };
        assert_eq!(
            served.expect("served from the cache").body,
            BODY,
            "stale beats blank"
        );
    }
}

/// Unless the origin asked for revalidation, in which case the failure is reported.
#[test]
fn a_must_revalidate_entry_does_not_outlive_its_origin() {
    set_clock(1_000_000);
    let cache = SqliteCache::in_memory().expect("opens");
    let server = server(Some("max-age=10, must-revalidate"));
    let url = format!("{}/thing", server.origin());
    let files = CachingFileSource::with_clock(HttpFileSource::default(), cache, clock);
    files.fetch(&url).expect("fetches");

    drop(server);
    assert!(
        files.fetch(&url).is_err(),
        "the origin said it must be asked, and it could not be"
    );
}

/// A failure is not cached: a source's coverage would otherwise become permanent.
#[test]
fn a_404_is_not_stored() {
    set_clock(1_000_000);
    let server = server(Some("max-age=3600"));
    let files = caching(&server);
    let url = format!("{}/absent", server.origin());

    let response = files.fetch(&url).expect("no error");
    assert_eq!(response.status, 404);
    assert!(
        files.cache().get(&url, clock()).expect("queries").is_none(),
        "not stored"
    );

    files.fetch(&url).expect("no error");
    assert_eq!(server.requests(), 2, "asked again");
}

/// The cache outlives the process that wrote it.
#[test]
fn a_file_backed_cache_persists() {
    set_clock(1_000_000);
    let directory = tempfile::tempdir().expect("a temp dir");
    let path = directory.path().join("cache.sqlite");

    let response = Response {
        status: 200,
        body: BODY.to_vec(),
        etag: Some("\"tag\"".into()),
        max_age: Some(10),
        expires_at: None,
        must_revalidate: false,
    };
    {
        let cache = SqliteCache::open(&path).expect("opens");
        cache
            .put("https://host/thing", &response, clock())
            .expect("stores");
        assert_eq!(cache.len().expect("counts"), 1);
    }

    let cache = SqliteCache::open(&path).expect("reopens");
    let entry = cache
        .get("https://host/thing", clock())
        .expect("queries")
        .expect("survived");
    assert_eq!(entry.response.body, response.body);
    assert_eq!(entry.response.etag, response.etag);
    assert_eq!(
        entry.expires,
        Some(1_000_010),
        "the resolved expiry survived"
    );
}

/// Eviction drops the least recently used first, and runs without being asked.
#[test]
fn eviction_is_least_recently_used() {
    // Room for three bodies of ten bytes, so the fourth write must drop something.
    let cache = SqliteCache::in_memory_with_capacity(35).expect("opens");
    let body = |n: u8| Response {
        status: 200,
        body: vec![n; 10],
        ..Response::default()
    };

    for (index, url) in ["a", "b", "c"].into_iter().enumerate() {
        cache
            .put(url, &body(index as u8), 1_000 + index as i64)
            .expect("stores");
    }
    assert_eq!(cache.len().expect("counts"), 3);
    assert_eq!(cache.size().expect("sizes"), 30);

    // Touch the oldest so recency and insertion order disagree, then overflow.
    cache.get("a", 9_000).expect("queries");
    cache.put("d", &body(3), 9_001).expect("stores");

    assert!(cache.size().expect("sizes") <= 35, "back inside the bound");
    assert!(
        cache.get("a", 10_000).expect("queries").is_some(),
        "touched"
    );
    assert!(cache.get("d", 10_000).expect("queries").is_some(), "newest");
    assert!(
        cache.get("b", 10_000).expect("queries").is_none(),
        "least recently used went first"
    );
}

/// A cache is bounded by bytes, not by entries.
///
/// A count would be meaningless: one tile can be worth thousands of manifests, and a limit that
/// counts them the same bounds nothing that matters on a device with a storage budget.
#[test]
fn the_bound_is_bytes() {
    let cache = SqliteCache::in_memory_with_capacity(1_000).expect("opens");
    assert_eq!(cache.capacity(), 1_000);

    let small = Response {
        status: 200,
        body: vec![0; 10],
        ..Response::default()
    };
    for index in 0..50 {
        cache
            .put(&format!("small{index}"), &small, 1_000 + index)
            .expect("stores");
    }
    assert_eq!(cache.len().expect("counts"), 50, "fifty small ones fit");
    assert_eq!(cache.size().expect("sizes"), 500);

    // One big one is worth many small ones, and eviction is measured in bytes.
    let big = Response {
        status: 200,
        body: vec![0; 600],
        ..Response::default()
    };
    cache.put("big", &big, 2_000).expect("stores");
    assert!(cache.size().expect("sizes") <= 1_000);
    assert!(cache.len().expect("counts") < 51, "small ones made way");
    assert!(cache.get("big", 3_000).expect("queries").is_some());
}

/// An entry larger than the whole cache is not stored.
///
/// Storing it would evict everything to make room for something that cannot help, and on the
/// next write it would go itself — having thrown away the tiles that were in use.
#[test]
fn an_entry_larger_than_the_cache_is_refused() {
    let cache = SqliteCache::in_memory_with_capacity(100).expect("opens");
    let keep = Response {
        status: 200,
        body: vec![0; 50],
        ..Response::default()
    };
    cache.put("keep", &keep, 1_000).expect("stores");

    let huge = Response {
        status: 200,
        body: vec![0; 1_000],
        ..Response::default()
    };
    cache.put("huge", &huge, 2_000).expect("does not error");

    assert!(
        cache.get("huge", 3_000).expect("queries").is_none(),
        "refused"
    );
    assert!(
        cache.get("keep", 3_000).expect("queries").is_some(),
        "and it did not take the useful entry with it"
    );
}

/// The default bound is mbgl's.
#[test]
fn the_default_capacity_matches_mbgl() {
    assert_eq!(SqliteCache::DEFAULT_CAPACITY, 50 * 1024 * 1024);
    let cache = SqliteCache::in_memory().expect("opens");
    assert_eq!(cache.capacity(), SqliteCache::DEFAULT_CAPACITY);
}

/// Eviction keeps what is in use, even when the cache holds fewer entries than a batch.
///
/// The case that found the flaw in the batched policy: with four entries and a batch of fifty,
/// "the timestamp of the fiftieth-oldest" is the *newest* entry's, so deleting everything at or
/// before it takes the whole cache — including the entry just touched, which is the one thing
/// an LRU exists to keep.
#[test]
fn a_small_cache_does_not_evict_everything() {
    let cache = SqliteCache::in_memory_with_capacity(25).expect("opens");
    let body = |n: u8| Response {
        status: 200,
        body: vec![n; 10],
        ..Response::default()
    };

    cache.put("old", &body(0), 1_000).expect("stores");
    cache.put("new", &body(1), 2_000).expect("stores");
    assert_eq!(cache.len().expect("counts"), 2);

    // A third does not fit, so exactly one must go — not both.
    cache.put("newest", &body(2), 3_000).expect("stores");
    assert_eq!(cache.len().expect("counts"), 2, "one dropped, not all");
    assert!(cache.get("old", 4_000).expect("queries").is_none());
    assert!(cache.get("new", 4_000).expect("queries").is_some());
    assert!(cache.get("newest", 4_000).expect("queries").is_some());
}

/// Eviction drops no more than it must.
#[test]
fn eviction_stops_at_the_bound() {
    let cache = SqliteCache::in_memory_with_capacity(100).expect("opens");
    let body = Response {
        status: 200,
        body: vec![0; 10],
        ..Response::default()
    };
    for index in 0..10 {
        cache
            .put(&format!("u{index}"), &body, 1_000 + index)
            .expect("stores");
    }
    assert_eq!(cache.len().expect("counts"), 10, "exactly full");

    cache.put("one-more", &body, 2_000).expect("stores");
    assert_eq!(
        cache.len().expect("counts"),
        10,
        "one in, one out — not a batch of fifty"
    );
    assert_eq!(cache.size().expect("sizes"), 100);
}

/// A read does not rewrite the access timestamp unless it has meaningfully moved.
///
/// Eviction is least-recently-used, so a read has to record that it happened — which makes
/// every read a write. Measured, that was 71% of the cost of a lookup, and on flash it is write
/// amplification as much as latency. Minute resolution answers "which entries are cold" as well
/// as second resolution does.
#[test]
fn a_read_does_not_rewrite_the_timestamp_every_time() {
    let cache = SqliteCache::in_memory().expect("opens");
    let response = Response {
        status: 200,
        body: BODY.to_vec(),
        ..Response::default()
    };
    cache.put("u", &response, 1_000).expect("stores");

    // Reads inside the granularity leave it alone.
    for offset in [0, 1, 30, SqliteCache::ACCESS_GRANULARITY] {
        let entry = cache
            .get("u", 1_000 + offset)
            .expect("queries")
            .expect("present");
        assert_eq!(entry.accessed, 1_000, "not rewritten at +{offset}");
    }

    // Past it, the timestamp moves, or eviction would forget the entry is in use.
    let after = 1_000 + SqliteCache::ACCESS_GRANULARITY + 1;
    cache.get("u", after).expect("queries");
    let entry = cache.get("u", after).expect("queries").expect("present");
    assert_eq!(entry.accessed, after, "rewritten once past the granularity");
}

/// Coarsening the timestamp does not break the eviction order.
///
/// The risk of writing less often is that a hot entry looks cold. It does not: entries read
/// within a minute of each other are all recent, and eviction compares them against entries
/// that have not been read for much longer.
#[test]
fn coarsened_timestamps_still_order_eviction() {
    let cache = SqliteCache::in_memory_with_capacity(25).expect("opens");
    let body = |n: u8| Response {
        status: 200,
        body: vec![n; 10],
        ..Response::default()
    };
    cache.put("cold", &body(0), 1_000).expect("stores");
    cache.put("hot", &body(1), 1_000).expect("stores");

    // Read "hot" repeatedly over several minutes; "cold" is never touched again.
    for minute in 1..5 {
        cache.get("hot", 1_000 + minute * 120).expect("queries");
    }

    cache.put("new", &body(2), 1_600).expect("stores");
    assert!(cache.get("hot", 2_000).expect("queries").is_some(), "kept");
    assert!(
        cache.get("cold", 2_000).expect("queries").is_none(),
        "the one nobody read went first"
    );
}
