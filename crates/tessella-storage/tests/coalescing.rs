//! Request coalescing: N views wanting one tile cost one fetch (§5.1, §9.3).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};

/// A source that counts calls and can be held open, so a test can arrange overlap deliberately
/// rather than hoping for it.
struct Controlled {
    calls: AtomicUsize,
    gate: Option<Arc<Barrier>>,
    fail: bool,
    panics: bool,
}

impl Controlled {
    fn counting() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            gate: None,
            fail: false,
            panics: false,
        }
    }
}

impl FileSource for Controlled {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.wait();
        }
        assert!(!self.panics, "the leader fell over");
        if self.fail {
            return Err(FetchError::Transport {
                url: url.to_string(),
                message: "refused".into(),
            });
        }
        Ok(Response {
            status: 200,
            body: url.as_bytes().to_vec(),
            etag: None,
        })
    }
}

/// One in-flight fetch, however many callers ask for it at once.
///
/// The barrier is what makes this a real test rather than a race: the leader cannot finish
/// until every waiter has arrived, so a build without coalescing fetches four times and this
/// fails deterministically rather than occasionally.
#[test]
fn concurrent_callers_share_one_fetch() {
    const VIEWS: usize = 4;
    let gate = Arc::new(Barrier::new(2));
    let source = Controlled {
        gate: Some(Arc::clone(&gate)),
        ..Controlled::counting()
    };
    let coalescing = Arc::new(Coalescing::new(source));

    let start = Arc::new(Barrier::new(VIEWS + 1));
    let mut handles = Vec::new();
    for _ in 0..VIEWS {
        let coalescing = Arc::clone(&coalescing);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            coalescing.fetch("https://host/13/4093/2724.pbf")
        }));
    }

    start.wait();
    // Let the threads pile up on the URL before the leader is allowed to return.
    while coalescing.stats().waited() < (VIEWS - 1) as u64 {
        std::thread::sleep(Duration::from_millis(1));
    }
    gate.wait();

    let bodies: Vec<Vec<u8>> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("no panic")
                .expect("fetched")
                .body
                .clone()
        })
        .collect();

    assert_eq!(
        coalescing.inner().calls.load(Ordering::SeqCst),
        1,
        "one fetch"
    );
    assert_eq!(coalescing.stats().computed(), 1);
    assert_eq!(coalescing.stats().waited(), (VIEWS - 1) as u64);
    assert!(bodies.iter().all(|body| body == &bodies[0]), "one answer");
}

/// Different URLs do not coalesce with each other.
#[test]
fn distinct_urls_each_fetch() {
    let coalescing = Coalescing::new(Controlled::counting());
    for tile in 0..3 {
        coalescing.fetch(&format!("host/{tile}")).expect("fetched");
    }
    assert_eq!(coalescing.stats().computed(), 3);
    assert_eq!(coalescing.stats().waited(), 0);
}

/// A finished request does not keep coalescing: the next caller is a new leader.
///
/// The registration is in-flight state, not a cache. Leaving it would serve stale bytes forever
/// and make revalidation impossible — caching is the store's job and its lifetime rules differ.
#[test]
fn a_finished_request_is_not_a_cache() {
    let coalescing = Coalescing::new(Controlled::counting());
    coalescing.fetch("host/a").expect("fetched");
    coalescing.fetch("host/a").expect("fetched");
    assert_eq!(
        coalescing.stats().computed(),
        2,
        "not deduped after the fact"
    );
    assert_eq!(coalescing.stats().waited(), 0);
}

/// A failure is shared with the waiters rather than retried per caller.
#[test]
fn a_failure_reaches_every_waiter() {
    let coalescing = Coalescing::new(Controlled {
        fail: true,
        ..Controlled::counting()
    });
    assert!(matches!(
        coalescing.fetch("host/a"),
        Err(FetchError::Transport { .. })
    ));
    assert_eq!(coalescing.stats().computed(), 1);
}

/// A leader that panics wakes its waiters with an error instead of stranding them.
///
/// Without the drop guard this test *hangs* rather than fails, which is the failure mode worth
/// having a test for: a single malformed response would otherwise block every worker that
/// asked for the same tile, and the symptom would be a frozen map with no error anywhere.
#[test]
fn a_panicking_leader_does_not_strand_waiters() {
    let gate = Arc::new(Barrier::new(2));
    let coalescing = Arc::new(Coalescing::new(Controlled {
        gate: Some(Arc::clone(&gate)),
        panics: true,
        ..Controlled::counting()
    }));

    let waiter_result: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(None));

    let leader = {
        let coalescing = Arc::clone(&coalescing);
        std::thread::spawn(move || {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                coalescing.fetch("host/a")
            }));
            std::panic::set_hook(previous);
            assert!(outcome.is_err(), "the leader was supposed to panic");
        })
    };

    while coalescing.stats().computed() == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let waiter = {
        let coalescing = Arc::clone(&coalescing);
        let waiter_result = Arc::clone(&waiter_result);
        std::thread::spawn(move || {
            let outcome = coalescing.fetch("host/a");
            *waiter_result.lock().expect("not poisoned") =
                Some(matches!(outcome, Err(FetchError::LeaderLost { .. })));
        })
    };

    while coalescing.stats().waited() == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    gate.wait();

    leader.join().expect("the leader thread itself survives");
    waiter
        .join()
        .expect("the waiter is woken rather than stranded");
    assert_eq!(
        *waiter_result.lock().expect("not poisoned"),
        Some(true),
        "the waiter is told the leader was lost"
    );

    // And the URL is free again, so a later caller is not stuck behind the dead entry.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let gate = Arc::clone(&gate);
        std::thread::spawn(move || gate.wait());
        coalescing.fetch("host/a")
    }));
    std::panic::set_hook(previous);
    assert!(again.is_err(), "still panicking, but reachable");
    assert_eq!(coalescing.stats().computed(), 2, "a new leader was allowed");
}
