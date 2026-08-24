//! File sources, and the coalescing that makes N views cost one fetch.
//!
//! # What coalescing is for
//!
//! §5.1 puts tile ownership at the process. Two views looking at the same place want the same
//! bytes, and the point of a shared store is that they do not fetch them twice. The store
//! already dedupes what has *arrived*; this dedupes what is *in flight*, which is the case a
//! cache cannot help with — four views starting together on the same camera issue their
//! requests before any of them has an answer to share.
//!
//! §9.3's flatness counters are the assertion: over an identical cover, four views must produce
//! one fetch and three waits, not four fetches. [`Stats`] is what those counters read.
//!
//! # The leader/waiter split, and why the leader cannot simply hold the lock
//!
//! The first caller for a URL becomes its leader: it registers the request, *releases the
//! table*, and fetches. Everyone else finds the registration and blocks on that entry's
//! condition variable. Holding the table across the fetch would serialize every request in the
//! process behind the slowest one — the exact opposite of what coalescing is for, and a
//! deadlock the moment a source's fetch needs the table itself.
//!
//! # A panicking leader must not strand its waiters
//!
//! If the leader unwinds, the entry is still registered and its waiters are still blocked on a
//! result that will never be posted. The leader holds a drop guard: it posts a failure and
//! wakes everyone whether the fetch returned, returned an error, or panicked. Without it a
//! single malformed response could hang every worker that asked for the same tile.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// What a fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status, or 200 for a source with no notion of one.
    pub status: u16,
    /// The body. Empty is legitimate — a tile with no features is a valid, empty tile.
    pub body: Vec<u8>,
    /// Entity tag, for revalidation.
    pub etag: Option<String>,
}

impl Response {
    /// Whether the status is a success.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Whether the resource is absent rather than broken.
    ///
    /// A 404 for a tile is ordinary: a source's coverage is not a rectangle, and asking for a
    /// tile outside it is how you find the edge. It is not an error to log or retry.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        self.status == 404 || self.status == 204
    }
}

/// Why a fetch did not produce a response.
///
/// Cloneable because one failure is handed to every waiter on the same URL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The transport failed.
    #[error("fetching `{url}`: {message}")]
    Transport {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The leader unwound without posting a result.
    #[error("the fetch of `{url}` panicked")]
    LeaderLost {
        /// What was asked for.
        url: String,
    },
}

/// A fetch's outcome, shared between a leader and its waiters.
pub type Fetched = Result<Arc<Response>, FetchError>;

/// Somewhere bytes come from.
///
/// Blocking, and called on a worker: §12.6's connection reuse and the §5.1 coalescing above it
/// both assume a small pool of threads that can afford to wait, rather than an async runtime
/// this build does not otherwise need.
pub trait FileSource: Send + Sync {
    /// Fetches one resource.
    ///
    /// # Errors
    ///
    /// [`FetchError`] when the transport failed. A 404 is a *response*, not an error.
    fn fetch(&self, url: &str) -> Result<Response, FetchError>;
}

/// How much work coalescing saved, for the §9.3 flatness assertion.
#[derive(Debug, Default)]
pub struct Stats {
    fetches: AtomicU64,
    waits: AtomicU64,
}

impl Stats {
    /// Requests that actually went to the source.
    #[must_use]
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::Relaxed)
    }

    /// Requests that joined one already in flight.
    #[must_use]
    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
struct Pending {
    outcome: Mutex<Option<Fetched>>,
    ready: Condvar,
}

/// Wraps a source so concurrent requests for one URL become one request.
#[derive(Debug)]
pub struct Coalescing<S> {
    source: S,
    inflight: Mutex<HashMap<String, Arc<Pending>>>,
    stats: Stats,
}

/// The leader's obligation to post *something*.
///
/// Held across the fetch. On drop it deregisters the URL and, if no result was posted, posts a
/// failure — so an unwinding leader wakes its waiters with an error instead of leaving them
/// blocked forever.
struct Leadership<'a, S> {
    owner: &'a Coalescing<S>,
    url: &'a str,
    pending: Arc<Pending>,
    posted: bool,
}

impl<S> Leadership<'_, S> {
    fn post(&mut self, outcome: Fetched) {
        let mut slot = self
            .pending
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(outcome);
        self.posted = true;
        drop(slot);
        self.pending.ready.notify_all();
    }
}

impl<S> Drop for Leadership<'_, S> {
    fn drop(&mut self) {
        // Deregister first: a later caller must be free to become the next leader rather than
        // wait on an entry that is finished.
        self.owner
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(self.url);

        if !self.posted {
            self.post(Err(FetchError::LeaderLost {
                url: self.url.to_string(),
            }));
        }
    }
}

impl<S: FileSource> Coalescing<S> {
    /// Wraps a source.
    pub fn new(source: S) -> Self {
        Self {
            source,
            inflight: Mutex::new(HashMap::new()),
            stats: Stats::default(),
        }
    }

    /// What coalescing saved.
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The wrapped source.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.source
    }

    /// Fetches, joining a request already in flight for the same URL.
    ///
    /// # Errors
    ///
    /// [`FetchError`] from the underlying source, or if the leader for this URL was lost.
    pub fn fetch(&self, url: &str) -> Fetched {
        let mut table = self
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(pending) = table.get(url).cloned() {
            drop(table);
            self.stats.waits.fetch_add(1, Ordering::Relaxed);

            let mut slot = pending
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while slot.is_none() {
                slot = pending
                    .ready
                    .wait(slot)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            return slot.clone().expect("the loop only exits with a result");
        }

        let pending = Arc::new(Pending::default());
        table.insert(url.to_string(), Arc::clone(&pending));
        drop(table);

        self.stats.fetches.fetch_add(1, Ordering::Relaxed);
        let mut leadership = Leadership {
            owner: self,
            url,
            pending,
            posted: false,
        };

        let outcome = self.source.fetch(url).map(Arc::new);
        leadership.post(outcome.clone());
        outcome
    }
}
