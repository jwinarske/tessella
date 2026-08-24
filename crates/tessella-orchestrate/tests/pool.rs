//! The process-scoped worker pool (§5.4, §5.5).

#![cfg(feature = "std")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use tessella_orchestrate::boot::Workers;
use tessella_orchestrate::pool::{Pool, Priority};

/// Waits for a condition, failing rather than hanging if it never comes.
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::yield_now();
    }
}

/// Every job runs exactly once, whatever the worker count.
#[test]
fn a_batch_runs_every_job_once() {
    for workers in [1, 2, 4, 8] {
        let pool = Pool::new(Workers::new(workers));
        let counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![0; 200]));

        let batch = pool.batch(Priority::Foreground);
        for index in 0..200 {
            let counts = Arc::clone(&counts);
            batch.submit(move || {
                counts.lock().expect("not poisoned")[index] += 1;
            });
        }
        batch.wait().expect("no panics");

        assert_eq!(batch.outstanding(), 0);
        assert!(
            counts.lock().expect("not poisoned").iter().all(|&n| n == 1),
            "{workers} workers"
        );
    }
}

/// A batch that has been waited on is done, not nearly done.
///
/// The whole point of the type. A `wait` that returns while a job is still running turns every
/// caller into a race — boot would read a half-filled tile cache and the offline download would
/// report a region complete before its last tile landed.
#[test]
fn wait_returns_only_when_everything_has_finished() {
    let pool = Pool::new(Workers::new(4));
    for _ in 0..50 {
        let finished = Arc::new(AtomicUsize::new(0));
        let batch = pool.batch(Priority::Foreground);
        for _ in 0..16 {
            let finished = Arc::clone(&finished);
            batch.submit(move || {
                std::thread::yield_now();
                finished.fetch_add(1, Ordering::AcqRel);
            });
        }
        batch.wait().expect("no panics");
        assert_eq!(finished.load(Ordering::Acquire), 16);
    }
}

/// Foreground work is taken before background, which is taken before prefetch.
///
/// Queued while every worker is busy, so the ordering under test is the queue's rather than
/// whatever order the submissions happened to be picked up in.
#[test]
fn higher_priority_work_is_taken_first() {
    let pool = Pool::new(Workers::new(1));
    let order: Arc<Mutex<Vec<Priority>>> = Arc::new(Mutex::new(Vec::new()));

    // Occupy the one worker, so everything below queues rather than running as it arrives.
    // Waiting for the job to announce it has started, not merely for the queue to be non-empty:
    // otherwise the ordering under test would be raced by the occupying job still being queued.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(Barrier::new(2));
    let held = Arc::clone(&gate);
    let announce = Arc::clone(&started);
    pool.submit(Priority::Foreground, move || {
        announce.store(true, Ordering::Release);
        held.wait();
    });
    until("the worker to pick the job up", || {
        started.load(Ordering::Acquire)
    });

    // Submitted worst-first, so priority and arrival order disagree.
    let batch = pool.batch(Priority::Prefetch);
    for priority in [
        Priority::Prefetch,
        Priority::Background,
        Priority::Foreground,
    ] {
        let order = Arc::clone(&order);
        let batch = pool.batch(priority);
        batch.submit(move || order.lock().expect("not poisoned").push(priority));
        std::mem::forget(batch);
    }
    drop(batch);

    gate.wait();
    until("every job to run", || {
        order.lock().expect("not poisoned").len() == 3
    });

    assert_eq!(
        *order.lock().expect("not poisoned"),
        vec![
            Priority::Foreground,
            Priority::Background,
            Priority::Prefetch
        ]
    );
}

/// A panicking job is contained, counted, and does not cost the pool a thread.
///
/// Two failures avoided. A `wait` that never returns because one job in a hundred hit an edge
/// case would stop the map with no error to show for it. And a panic allowed past the worker
/// kills the thread, so a malformed tile does not fail one tile — it permanently shrinks the
/// pool that was going to draw the rest of the map.
#[test]
fn a_panicking_job_is_contained_and_reported() {
    let pool = Pool::new(Workers::new(2));
    let ran = Arc::new(AtomicUsize::new(0));

    let batch = pool.batch(Priority::Foreground);
    for index in 0..9 {
        let ran = Arc::clone(&ran);
        batch.submit(move || {
            ran.fetch_add(1, Ordering::AcqRel);
            assert!(index % 3 != 0, "deliberate");
        });
    }
    let panicked = batch.wait().expect_err("the panics are reported");

    assert_eq!(panicked.jobs, 3, "0, 3 and 6");
    assert_eq!(batch.outstanding(), 0, "the batch still completed");
    assert_eq!(ran.load(Ordering::Acquire), 9, "every job was attempted");
    assert_eq!(pool.panics(), 3);

    // The pool still has both its threads and still works.
    assert_eq!(pool.workers(), 2);
    let after = Arc::new(AtomicUsize::new(0));
    let batch = pool.batch(Priority::Foreground);
    for _ in 0..64 {
        let counted = Arc::clone(&after);
        batch.submit(move || {
            counted.fetch_add(1, Ordering::AcqRel);
        });
    }
    batch.wait().expect("no panics");
    assert_eq!(after.load(Ordering::Acquire), 64);
}

/// The waiting thread runs work rather than idling.
///
/// With one worker occupied and a batch of one job, a caller that merely slept would wait for
/// the occupied worker to come free. Running the work itself is what stops a full pool from
/// deadlocking on a job that waits on a batch of its own.
#[test]
fn a_waiter_helps_instead_of_blocking() {
    let pool = Pool::new(Workers::new(1));

    // The occupying job announces that it has *started*, not merely that it was queued. Waiting
    // on `!is_idle` would pass while the job was still in the queue, and the waiting thread
    // below would then help by running the blocking job itself -- and block forever, since this
    // thread is the one that has to release it.
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(Barrier::new(2));
    let held = Arc::clone(&gate);
    let announce = Arc::clone(&started);
    pool.submit(Priority::Foreground, move || {
        announce.store(true, Ordering::Release);
        held.wait();
    });
    until("the worker to pick the job up", || {
        started.load(Ordering::Acquire)
    });

    // The only worker is blocked, so this can only run on the waiting thread.
    let who = Arc::new(Mutex::new(None));
    let batch = pool.batch(Priority::Foreground);
    let recorded = Arc::clone(&who);
    batch.submit(move || {
        *recorded.lock().expect("not poisoned") = Some(std::thread::current().id());
    });
    batch.wait().expect("no panics");

    assert_eq!(
        *who.lock().expect("not poisoned"),
        Some(std::thread::current().id()),
        "the waiter ran it"
    );
    gate.wait();
}

/// A job that waits on a batch of its own does not deadlock the pool.
#[test]
fn a_nested_batch_completes() {
    let pool = Arc::new(Pool::new(Workers::new(2)));
    let inner_ran = Arc::new(AtomicUsize::new(0));

    let outer = pool.batch(Priority::Foreground);
    for _ in 0..4 {
        let pool = Arc::clone(&pool);
        let inner_ran = Arc::clone(&inner_ran);
        outer.submit(move || {
            let inner = pool.batch(Priority::Background);
            for _ in 0..4 {
                let inner_ran = Arc::clone(&inner_ran);
                inner.submit(move || {
                    inner_ran.fetch_add(1, Ordering::AcqRel);
                });
            }
            inner.wait().expect("no panics");
        });
    }
    outer.wait().expect("no panics");

    assert_eq!(inner_ran.load(Ordering::Acquire), 16);
}

/// Work submitted from several threads at once is all accounted for.
#[test]
fn concurrent_submitters_lose_nothing() {
    let pool = Arc::new(Pool::new(Workers::new(4)));
    let ran = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..8 {
            let pool = Arc::clone(&pool);
            let ran = Arc::clone(&ran);
            scope.spawn(move || {
                let batch = pool.batch(Priority::Background);
                for _ in 0..64 {
                    let ran = Arc::clone(&ran);
                    batch.submit(move || {
                        ran.fetch_add(1, Ordering::AcqRel);
                    });
                }
                batch.wait().expect("no panics");
            });
        }
    });

    assert_eq!(ran.load(Ordering::Acquire), 8 * 64);
    until("the pool to settle", || pool.is_idle());
}

/// An empty batch is finished the moment it is asked.
#[test]
fn an_empty_batch_waits_for_nothing() {
    let pool = Pool::new(Workers::new(2));
    let batch = pool.batch(Priority::Foreground);
    batch.wait().expect("no panics");
    assert_eq!(batch.outstanding(), 0);
}

/// The process pool is one pool, however many times it is asked for (§5.5).
#[test]
fn the_shared_pool_is_shared() {
    let first = std::ptr::from_ref(Pool::shared());
    let second = std::ptr::from_ref(Pool::shared());
    assert!(std::ptr::eq(first, second));
    assert_eq!(Pool::shared().workers(), Workers::default().get());
}

/// Dropping a pool stops its threads rather than leaking them.
///
/// Four views opening and closing must not accumulate threads; this is the half of §5.5 that a
/// per-call pool got right for free and a persistent one has to be told.
#[test]
fn dropping_a_pool_stops_its_threads() {
    let ran = Arc::new(AtomicUsize::new(0));
    for _ in 0..20 {
        let pool = Pool::new(Workers::new(4));
        let batch = pool.batch(Priority::Foreground);
        let ran = Arc::clone(&ran);
        batch.submit(move || {
            ran.fetch_add(1, Ordering::AcqRel);
        });
        batch.wait().expect("no panics");
    }
    assert_eq!(ran.load(Ordering::Acquire), 20);
}

/// A waiter helps with its own class and above, never below it.
///
/// The hazard this closes: a foreground start whose own jobs are all in flight on workers looks
/// for more work to do. Without a floor it finds a queued region-download fetch, runs it, and
/// blocks first-tile behind a network round trip nobody asked it to make.
///
/// Constructing that needs the batch's jobs to be on *workers* rather than in the queue — a
/// waiter is greedy about its own class first, so while anything of its own is queued it never
/// looks lower. Hence the two-stage gate: both foreground jobs are confirmed running before the
/// background job is queued, which leaves no free worker to take it.
///
/// Without the floor this thread takes that background job and blocks on a barrier nothing
/// releases until after the assertion — the test hangs rather than failing an assert, which is
/// what "the waiter went and did something else" looks like from the outside.
#[test]
fn a_waiter_does_not_help_with_lower_priority_work() {
    let pool = Pool::new(Workers::new(2));

    let started = Arc::new(AtomicUsize::new(0));
    let release_foreground = Arc::new(Barrier::new(3));
    let batch = pool.batch(Priority::Foreground);
    for _ in 0..2 {
        let started = Arc::clone(&started);
        let gate = Arc::clone(&release_foreground);
        batch.submit(move || {
            started.fetch_add(1, Ordering::AcqRel);
            gate.wait();
        });
    }
    until("both foreground jobs to be on workers", || {
        started.load(Ordering::Acquire) == 2
    });

    // Every worker is inside a foreground job, so this queues rather than running.
    let background_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let release_background = Arc::new(Barrier::new(2));
    {
        let flag = Arc::clone(&background_started);
        let gate = Arc::clone(&release_background);
        pool.submit(Priority::Background, move || {
            flag.store(true, Ordering::Release);
            gate.wait();
        });
    }

    // From another thread, so this one is free to wait — and to be tempted.
    let releaser = {
        let gate = Arc::clone(&release_foreground);
        std::thread::spawn(move || gate.wait())
    };

    batch.wait().expect("no panics");
    releaser.join().expect("the releaser");

    // A worker picks the background job up in its own time, now that they are free.
    until("a worker to take the background job", || {
        background_started.load(Ordering::Acquire)
    });
    release_background.wait();
}

/// A background waiter still helps with foreground work.
///
/// Helping upward is not a hazard: that work outranks the waiter's own, so running it is what
/// the priority order asks for anyway.
#[test]
fn a_waiter_helps_with_higher_priority_work() {
    let pool = Pool::new(Workers::new(1));

    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(Barrier::new(2));
    let held = Arc::clone(&gate);
    let announce = Arc::clone(&started);
    pool.submit(Priority::Foreground, move || {
        announce.store(true, Ordering::Release);
        held.wait();
    });
    until("the worker to pick the job up", || {
        started.load(Ordering::Acquire)
    });

    let urgent = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&urgent);
    pool.submit(Priority::Foreground, move || {
        flag.store(true, Ordering::Release);
    });

    let batch = pool.batch(Priority::Prefetch);
    batch.submit(|| {});
    batch.wait().expect("no panics");

    assert!(
        urgent.load(Ordering::Acquire),
        "the prefetch waiter ran the foreground job"
    );
    gate.wait();
}
