//! The process-scoped worker pool (§5.4, §5.5).
//!
//! # One pool, not one per call
//!
//! The cold start used to spawn a scope of threads, run a cover across them and join. That is
//! correct and it is not what §5.5 asks for: the pool is listed as process-owned, alongside the
//! orchestrator and the deadline wheel, because four views ticking together must not mean four
//! sets of threads. Threads here are started once and live until the process ends, so a view
//! opening costs a queue push rather than four clone(2) calls, and the flatness counters in
//! §9.3 have something to be flat about.
//!
//! # Priority classes
//!
//! §5.4 names three: foreground visible-tile decode, then background view, then prefetch.
//! Selection is strict — a worker looking for work takes the highest class that has any, and
//! only looks lower when the ones above are empty.
//!
//! Strict priority starves the bottom class under sustained load, and that is the intent rather
//! than a defect to be softened. Prefetch is a speculative cover along the camera's velocity:
//! work that exists to be thrown away when something real arrives. Ageing it up would mean a
//! guess the user may never look at competing with the tile under their finger. What must not
//! starve is foreground, and nothing outranks it.
//!
//! # Why jobs are `'static`
//!
//! The ergonomic alternative is a scoped API in the shape of [`std::thread::scope`], which lets
//! a job borrow the caller's stack because the scope cannot return until every job is done.
//! Over a *persistent* pool that needs a lifetime transmute, and this crate is
//! `#![deny(unsafe_code)]` for reasons that outrank the convenience.
//!
//! It is also the wrong shape here. What a decode job borrows is the compiled style, the file
//! sources and the tile store — every one of them listed in §5.5 as process-owned. Holding them
//! in an [`Arc`] is not a workaround for the missing lifetime; it is the ownership the table
//! already describes.
//!
//! # Deadlock
//!
//! A [`Batch`] waits by *running work itself*, so the thread that submitted a batch is never
//! merely blocked. That keeps a full pool from deadlocking when a job waits on a batch of its
//! own, and it means a caller that submits four jobs to four workers is a fifth pair of hands
//! rather than an idle one.

use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

use crate::boot::Workers;

/// What a job is competing for a worker against (§5.4).
///
/// Ordered: [`Priority::Foreground`] is taken before [`Priority::Background`], which is taken
/// before [`Priority::Prefetch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// A tile a view is waiting to draw. Nothing outranks this.
    Foreground,
    /// A tile some other view needs, or work with no one watching it — an offline region
    /// download, which is hours of fetching that must never make the visible map stutter.
    Background,
    /// A speculative cover along the camera's velocity. Correct to starve.
    Prefetch,
}

impl Priority {
    /// Every class, highest first.
    pub const ALL: [Self; 3] = [Self::Foreground, Self::Background, Self::Prefetch];

    const fn index(self) -> usize {
        match self {
            Self::Foreground => 0,
            Self::Background => 1,
            Self::Prefetch => 2,
        }
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A job and whatever batch is counting it.
///
/// The bookkeeping is carried beside the closure rather than wrapped around it in a drop guard.
/// A guard runs *during* the unwind, which puts the batch's "one fewer outstanding" before the
/// pool has finished accounting for the panic — so a [`Batch::wait`] could return, and its
/// caller read [`Pool::panics`], in the window before the count moved. It showed up as one
/// stress run in ten reading two panics where three had happened.
struct Task {
    job: Job,
    pending: Option<Arc<Pending>>,
}

#[derive(Default)]
struct Queues {
    classes: [VecDeque<Task>; 3],
    /// Set once, never cleared. A worker that wakes to find this takes no more work.
    stopping: bool,
}

impl Queues {
    /// The highest-priority job available, if any.
    fn take(&mut self) -> Option<Task> {
        self.classes.iter_mut().find_map(VecDeque::pop_front)
    }
}

struct Inner {
    queues: Mutex<Queues>,
    /// Signalled when a job is pushed or the pool is stopping.
    available: Condvar,
    /// Jobs taken and not yet finished, for [`Pool::is_idle`].
    running: AtomicUsize,
    /// Jobs that panicked, ever.
    panics: AtomicUsize,
}

impl Inner {
    /// Runs jobs until the pool stops.
    fn work(&self) {
        loop {
            let task = {
                let mut queues = self
                    .queues
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if let Some(task) = queues.take() {
                        break task;
                    }
                    if queues.stopping {
                        return;
                    }
                    queues = self
                        .available
                        .wait(queues)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let _ = self.run(task);
        }
    }

    /// Runs one job, counting it as in flight for as long as it takes.
    ///
    /// Returns whether it panicked.
    ///
    /// # Why the unwind is caught
    ///
    /// Letting it past this point costs the worker. The thread unwinds out of [`Self::work`]
    /// and dies, and the pool is quietly one thread smaller for the rest of the process — so a
    /// malformed tile that panics one decode does not fail one tile, it permanently shrinks the
    /// pool that was going to draw the rest of the map. Enough of them and there is nothing
    /// left to decode on, with no error anywhere saying so.
    ///
    /// It is worse on a waiting thread. [`Batch::wait`] runs queued work while it waits, so an
    /// uncaught panic would unwind into a caller that merely happened to be helping — a boot
    /// dying of a panic in somebody else's prefetch.
    ///
    /// The panic is not silenced: the default hook still prints it, and the count comes back
    /// through [`Batch::wait`] and [`Pool::panics`] so it is reported rather than absorbed.
    fn run(&self, task: Task) -> bool {
        // A guard rather than a decrement after the call, so the count is right however the job
        // ends -- `catch_unwind` returns normally, but a `panic = "abort"` build does not, and
        // the guard costs nothing.
        self.running.fetch_add(1, Ordering::AcqRel);
        let _running = Running(self);
        // `AssertUnwindSafe` because the job owns whatever it captured; anything it shares is
        // behind a lock or an atomic, and a lock this poisons is recovered at every use.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task.job)).is_err();
        if panicked {
            self.panics.fetch_add(1, Ordering::AcqRel);
        }
        // After both counters, so a `wait` that returns on this decrement is looking at figures
        // that are already final.
        if let Some(pending) = task.pending {
            if panicked {
                pending.panicked.fetch_add(1, Ordering::AcqRel);
            }
            pending.finish();
        }
        panicked
    }

    /// Takes one job if there is one, without blocking.
    fn try_take(&self) -> Option<Task> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Decrements the in-flight count however the job ends.
struct Running<'a>(&'a Inner);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A fixed set of threads taking work by priority class.
#[derive(Debug)]
pub struct Pool {
    inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Inner")
            .field("running", &self.running.load(Ordering::Relaxed))
            .field("panics", &self.panics.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Starts `workers` threads.
    ///
    /// The count is [`Workers`]' business, and deliberately not the host's core count — see
    /// that type for why a workstation measurement must not decide what an RK3566 does.
    #[must_use]
    pub fn new(workers: Workers) -> Self {
        let inner = Arc::new(Inner {
            queues: Mutex::new(Queues::default()),
            available: Condvar::new(),
            running: AtomicUsize::new(0),
            panics: AtomicUsize::new(0),
        });
        let threads = (0..workers.get())
            .map(|index| {
                let inner = Arc::clone(&inner);
                std::thread::Builder::new()
                    // Named so a profile or a core-affinity policy has something to match on.
                    // §5.4 wants these on the little cores; naming them is the part that does
                    // not need the RK3566 lane to land first.
                    .name(format!("tessella-decode-{index}"))
                    .spawn(move || inner.work())
                    .expect("a worker thread")
            })
            .collect();
        Self { inner, threads }
    }

    /// The one pool for this process (§5.5).
    ///
    /// Started on first use with [`Workers::default`], and never stopped: it outlives every
    /// view, which is the whole point of it being process-scoped.
    pub fn shared() -> &'static Self {
        static SHARED: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
        SHARED.get_or_init(|| Self::new(Workers::default()))
    }

    /// How many threads are running.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.threads.len()
    }

    /// Queues one job.
    pub fn submit(&self, priority: Priority, job: impl FnOnce() + Send + 'static) {
        self.push(
            priority,
            Task {
                job: Box::new(job),
                pending: None,
            },
        );
    }

    fn push(&self, priority: Priority, task: Task) {
        self.inner
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .classes[priority.index()]
        .push_back(task);
        // One waiter, because one job feeds one worker. Waking all of them to have all but one
        // find an empty queue is the thundering herd this exists to avoid.
        self.inner.available.notify_one();
    }

    /// A group of jobs that can be waited on together.
    #[must_use]
    pub fn batch(&self, priority: Priority) -> Batch<'_> {
        Batch {
            pool: self,
            priority,
            pending: Arc::new(Pending {
                count: AtomicUsize::new(0),
                panicked: AtomicUsize::new(0),
                done: Condvar::new(),
                lock: Mutex::new(()),
            }),
        }
    }

    /// How many jobs have panicked, ever.
    ///
    /// A panic is contained rather than silenced — the default hook still prints it, and the
    /// worker survives — but a caller that fires
    /// work and never waits on it, a prefetch, has nowhere else to learn that it went wrong.
    #[must_use]
    pub fn panics(&self) -> usize {
        self.inner.panics.load(Ordering::Acquire)
    }

    /// True when nothing is queued and nothing is running.
    ///
    /// For tests and for the §9.3 counters. Not a synchronisation primitive: by the time it
    /// answers, a job may have arrived.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        let queued = self
            .inner
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .classes
            .iter()
            .all(VecDeque::is_empty);
        queued && self.inner.running.load(Ordering::Acquire) == 0
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        {
            let mut queues = self
                .inner
                .queues
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queues.stopping = true;
        }
        // Every worker, since every one of them has to see the flag to exit.
        self.inner.available.notify_all();
        for thread in self.threads.drain(..) {
            // A worker that panicked has already unwound; there is nothing useful to do about
            // it here, and refusing to finish dropping would be worse than continuing.
            let _ = thread.join();
        }
    }
}

/// The shared state of a [`Batch`].
#[derive(Debug)]
struct Pending {
    count: AtomicUsize,
    panicked: AtomicUsize,
    done: Condvar,
    lock: Mutex<()>,
}

impl Pending {
    /// Records a job finishing, waking the waiter when it was the last.
    fn finish(&self) {
        // The lock is taken around the decrement, not just the notify. Without it the waiter
        // can read a non-zero count, and only then start waiting — after the notify has already
        // gone out to nobody. That is a hang, not a slow path, and it needs a job to finish in
        // exactly the window between the waiter's check and its wait.
        let _held = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.done.notify_all();
        }
    }
}

/// Jobs submitted together and waited on together.
///
/// Dropping a batch does *not* wait: a caller that wants the results calls [`Batch::wait`], and
/// one that does not — a prefetch fired and forgotten — should not pay for a join it never
/// asked for.
#[derive(Debug)]
pub struct Batch<'a> {
    pool: &'a Pool,
    priority: Priority,
    pending: Arc<Pending>,
}

impl Batch<'_> {
    /// Queues one job as part of this batch.
    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        self.pending.count.fetch_add(1, Ordering::AcqRel);
        self.pool.push(
            self.priority,
            Task {
                job: Box::new(job),
                // Counted by the runner once the job has finished however it finished, so a
                // `wait` that never returns because one job in a hundred hit an edge case is
                // not a thing that can happen.
                pending: Some(Arc::clone(&self.pending)),
            },
        );
    }

    /// Blocks until every job submitted so far has finished.
    ///
    /// The waiting thread runs queued work while it waits rather than idling. That is what
    /// stops a full pool from deadlocking when a job waits on a batch of its own, and it means
    /// the submitting thread is a spare pair of hands instead of a blocked one. The work it
    /// picks up is whatever is queued, not necessarily this batch's — the pool's work is the
    /// pool's work, and clearing the queue in front of this batch still shortens its wait.
    ///
    /// # Errors
    ///
    /// [`Panicked`] when any job in the batch panicked. The batch still completed — every job
    /// ran and the pool is intact — but a caller that treated the result as whole would be
    /// acting on a tile that was never built.
    pub fn wait(&self) -> Result<(), Panicked> {
        loop {
            if self.pending.count.load(Ordering::Acquire) == 0 {
                return self.outcome();
            }
            // Help, rather than sleep.
            if let Some(task) = self.pool.inner.try_take() {
                let _ = self.pool.inner.run(task);
                continue;
            }
            // Nothing left to help with: this batch's remaining jobs are in other workers'
            // hands, so there is genuinely nothing to do but wait to be told.
            let held = self
                .pending
                .lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.pending.count.load(Ordering::Acquire) == 0 {
                drop(held);
                return self.outcome();
            }
            let _held = self
                .pending
                .done
                .wait(held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn outcome(&self) -> Result<(), Panicked> {
        match self.pending.panicked.load(Ordering::Acquire) {
            0 => Ok(()),
            jobs => Err(Panicked { jobs }),
        }
    }

    /// How many of this batch's jobs have not finished.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.pending.count.load(Ordering::Acquire)
    }
}

/// Some of a batch's jobs panicked.
///
/// The batch itself completed: every job ran, and the pool is intact. What did not happen is
/// whatever those jobs were for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{jobs} job(s) panicked")]
pub struct Panicked {
    /// How many.
    pub jobs: usize,
}
