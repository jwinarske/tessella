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

/// Eviction drops the least recently used first.
#[test]
fn eviction_is_least_recently_used() {
    let cache = SqliteCache::in_memory().expect("opens");
    let response = Response {
        status: 200,
        body: BODY.to_vec(),
        ..Response::default()
    };
    for (index, url) in ["a", "b", "c", "d"].into_iter().enumerate() {
        cache
            .put(url, &response, 1_000 + index as i64)
            .expect("stores");
    }
    // Touch the oldest, so recency and insertion order disagree.
    cache.get("a", 9_999).expect("queries");

    assert_eq!(cache.evict_to(2).expect("evicts"), 2);
    assert_eq!(cache.len().expect("counts"), 2);
    assert!(
        cache.get("a", 10_000).expect("queries").is_some(),
        "touched"
    );
    assert!(cache.get("d", 10_000).expect("queries").is_some(), "newest");
    assert!(cache.get("b", 10_000).expect("queries").is_none());
    assert!(cache.get("c", 10_000).expect("queries").is_none());
}
