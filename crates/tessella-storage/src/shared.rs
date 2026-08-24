//! Doing a piece of work once when several callers want it at once.
//!
//! §5.5 lists what is process-scoped: fetches, decodes, buckets, shaping, atlases. §9.3's
//! flatness counters assert that none of it scales with view count. A *cache* gets most of the
//! way — the second view to want a tile finds it there — but not the case that matters at
//! startup, where four views begin on the same camera and issue their work before any of them
//! has a result to share. That window is what this closes.
//!
//! # Leader and waiters
//!
//! The first caller for a key becomes its leader: it registers, *releases the table*, and does
//! the work. Everyone else finds the registration and blocks on that entry. Holding the table
//! across the work would serialize every computation in the process behind the slowest one —
//! the opposite of the point, and a deadlock as soon as the work itself needs the table.
//!
//! # A panicking leader must not strand its waiters
//!
//! If the leader unwinds, the entry is registered and its waiters are blocked on a result that
//! will never be posted. The guard below deregisters and posts a failure whether the work
//! returned or panicked. Without it one malformed tile freezes every worker that asked for it,
//! and the symptom is a hung map with no error anywhere.
//!
//! # Not a cache
//!
//! A finished entry is deregistered. Keeping it would make this a cache with no eviction, no
//! revalidation and no bound — and caching has different lifetime rules, which is why it lives
//! in `tessella_tile::store` instead. Compose the two: look in the store, and use this to make
//! sure only one caller computes what is missing.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// The work was not done, because whoever was doing it did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the computation was abandoned by the caller that owned it")]
pub struct Abandoned;

/// How much work sharing saved, for the §9.3 flatness assertion.
#[derive(Debug, Default)]
pub struct ShareStats {
    computed: AtomicU64,
    waited: AtomicU64,
}

impl ShareStats {
    /// Calls that actually did the work.
    #[must_use]
    pub fn computed(&self) -> u64 {
        self.computed.load(Ordering::Relaxed)
    }

    /// Calls that joined work already in flight.
    #[must_use]
    pub fn waited(&self) -> u64 {
        self.waited.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct Pending<V> {
    outcome: Mutex<Option<Result<V, Abandoned>>>,
    ready: Condvar,
}

impl<V> Default for Pending<V> {
    fn default() -> Self {
        Self {
            outcome: Mutex::new(None),
            ready: Condvar::new(),
        }
    }
}

/// A table of work in flight, keyed.
#[derive(Debug)]
pub struct Shared<K, V> {
    inflight: Mutex<HashMap<K, Arc<Pending<V>>>>,
    stats: ShareStats,
}

impl<K, V> Default for Shared<K, V> {
    fn default() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
            stats: ShareStats::default(),
        }
    }
}

/// The leader's obligation to post *something*, honoured even if it unwinds.
struct Leadership<'a, K: Eq + Hash + Clone, V> {
    table: &'a Mutex<HashMap<K, Arc<Pending<V>>>>,
    key: K,
    pending: Arc<Pending<V>>,
    posted: bool,
}

impl<K: Eq + Hash + Clone, V> Leadership<'_, K, V> {
    fn post(&mut self, outcome: Result<V, Abandoned>) {
        let mut slot = self
            .pending
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *slot = Some(outcome);
        self.posted = true;
        drop(slot);
        self.pending.ready.notify_all();
    }
}

impl<K: Eq + Hash + Clone, V> Drop for Leadership<'_, K, V> {
    fn drop(&mut self) {
        // Deregister first: a later caller must be free to become the next leader rather than
        // wait on an entry that is finished.
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        if !self.posted {
            self.post(Err(Abandoned));
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> Shared<K, V> {
    /// A new, empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What sharing saved.
    #[must_use]
    pub fn stats(&self) -> &ShareStats {
        &self.stats
    }

    /// Runs `work` for `key`, or waits for the call already running it.
    ///
    /// # Errors
    ///
    /// [`Abandoned`] when the caller that owned this key unwound without producing a value. The
    /// key is free again, so a retry becomes a new leader.
    pub fn compute(&self, key: K, work: impl FnOnce() -> V) -> Result<V, Abandoned> {
        let mut table = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);

        if let Some(pending) = table.get(&key).cloned() {
            drop(table);
            self.stats.waited.fetch_add(1, Ordering::Relaxed);

            let mut slot = pending
                .outcome
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            while slot.is_none() {
                slot = pending
                    .ready
                    .wait(slot)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            return slot.clone().expect("the loop only exits with a result");
        }

        let pending = Arc::new(Pending::default());
        table.insert(key.clone(), Arc::clone(&pending));
        drop(table);

        self.stats.computed.fetch_add(1, Ordering::Relaxed);
        let mut leadership = Leadership {
            table: &self.inflight,
            key,
            pending,
            posted: false,
        };

        let value = work();
        leadership.post(Ok(value.clone()));
        Ok(value)
    }
}
