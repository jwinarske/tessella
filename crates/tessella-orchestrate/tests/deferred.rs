//! The native deferred transport: a blocking source, a pool, and a request that does not wait.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tessella_orchestrate::deferred::PoolBacked;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::deferred::{DeferredFileSource, Ticket};
use tessella_storage::source::{FetchError, FileSource, Response};

/// Spins until `poll` answers, or gives up.
///
/// A deferred source has no way to be told "the answer is here" -- that is what makes it
/// deferred -- so a test that wants one has to ask. The bound is generous because it exists to
/// turn a hang into a failure, not to measure anything.
fn settle(source: &impl DeferredFileSource, ticket: Ticket) -> tessella_storage::source::Fetched {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(outcome) = source.poll(ticket) {
            return outcome;
        }
        assert!(Instant::now() < deadline, "the fetch never landed");
        std::thread::yield_now();
    }
}

/// Answers every URL with its own bytes, and records what it was asked.
#[derive(Default)]
struct Echo {
    calls: AtomicUsize,
    etags: Mutex<Vec<Option<String>>>,
}

impl FileSource for Echo {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(Response {
            status: 200,
            body: url.as_bytes().to_vec(),
            ..Response::default()
        })
    }

    fn fetch_conditional(&self, url: &str, etag: Option<&str>) -> Result<Response, FetchError> {
        self.etags
            .lock()
            .expect("not poisoned")
            .push(etag.map(str::to_string));
        self.fetch(url)
    }
}

/// Unwinds instead of answering.
struct Panics;

impl FileSource for Panics {
    fn fetch(&self, _url: &str) -> Result<Response, FetchError> {
        panic!("this source is meant to come apart");
    }
}

/// Blocks until released, so a test can look at the table mid-flight.
#[derive(Default)]
struct Held {
    released: Mutex<bool>,
    signal: Condvar,
}

impl Held {
    fn release(&self) {
        *self.released.lock().expect("not poisoned") = true;
        self.signal.notify_all();
    }
}

impl FileSource for Held {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        let mut held = self.released.lock().expect("not poisoned");
        while !*held {
            held = self.signal.wait(held).expect("not poisoned");
        }
        Ok(Response {
            status: 200,
            body: url.as_bytes().to_vec(),
            ..Response::default()
        })
    }
}

#[test]
fn a_request_answers_before_the_fetch_does() {
    let held = Arc::new(Held::default());
    let source = PoolBacked::new(Arc::clone(&held), Pool::shared(), Priority::Background);

    // The point of the whole trait: this line returns while the fetch is still blocked.
    let ticket = source.request("https://example.invalid/a.mvt", None);
    assert_eq!(source.outstanding(), 1);
    assert!(source.poll(ticket).is_none());

    held.release();
    let landed = settle(&source, ticket).expect("ok");
    assert_eq!(landed.body, b"https://example.invalid/a.mvt");
    assert_eq!(source.outstanding(), 0);
}

#[test]
fn the_etag_reaches_the_source() {
    let echo = Arc::new(Echo::default());
    let source = PoolBacked::new(Arc::clone(&echo), Pool::shared(), Priority::Background);

    let with = source.request("https://example.invalid/a.mvt", Some("\"abc\""));
    settle(&source, with).expect("ok");
    let without = source.request("https://example.invalid/b.mvt", None);
    settle(&source, without).expect("ok");

    let mut seen = echo.etags.lock().expect("not poisoned").clone();
    seen.sort();
    assert_eq!(seen, vec![None, Some("\"abc\"".to_string())]);
}

#[test]
fn a_source_that_unwinds_fails_its_ticket_rather_than_stranding_it() {
    let source = PoolBacked::new(Arc::new(Panics), Pool::shared(), Priority::Background);
    let ticket = source.request("https://example.invalid/a.mvt", None);

    // Without the drop guard this is where the suite hangs: the pool counts the panic, the job
    // is gone, and the ticket waits for a result nobody will ever post.
    match settle(&source, ticket) {
        Err(FetchError::LeaderLost { url }) => {
            assert_eq!(url, "https://example.invalid/a.mvt");
        }
        other => panic!("expected a lost leader, got {other:?}"),
    }
    assert_eq!(source.outstanding(), 0);
}

#[test]
fn cancelling_in_flight_releases_the_entry() {
    let held = Arc::new(Held::default());
    let source = PoolBacked::new(Arc::clone(&held), Pool::shared(), Priority::Background);

    let ticket = source.request("https://example.invalid/a.mvt", None);
    source.cancel(ticket);
    assert_eq!(source.outstanding(), 0);

    // The job is still running and still going to post. That post must find nothing.
    held.release();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(&held) > 2 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(source.outstanding(), 0);
    assert!(source.poll(ticket).is_none());
}

#[test]
fn many_requests_all_land_and_keep_their_own_answers() {
    let echo = Arc::new(Echo::default());
    let source = PoolBacked::new(Arc::clone(&echo), Pool::shared(), Priority::Background);

    let urls: Vec<String> = (0..64)
        .map(|n| format!("https://example.invalid/{n}"))
        .collect();
    let tickets: Vec<Ticket> = urls.iter().map(|url| source.request(url, None)).collect();

    for (url, ticket) in urls.iter().zip(tickets) {
        let landed = settle(&source, ticket).expect("ok");
        assert_eq!(
            landed.body,
            url.as_bytes(),
            "a ticket answered someone else's fetch"
        );
    }
    assert_eq!(source.outstanding(), 0);
    assert_eq!(echo.calls.load(Ordering::Relaxed), 64);
}

#[test]
fn a_priority_can_be_named_per_request() {
    let echo = Arc::new(Echo::default());
    let source = PoolBacked::new(Arc::clone(&echo), Pool::shared(), Priority::Prefetch);

    let urgent = source.request_at(Priority::Foreground, "https://example.invalid/a.mvt", None);
    assert_eq!(
        settle(&source, urgent).expect("ok").body,
        b"https://example.invalid/a.mvt"
    );
}
