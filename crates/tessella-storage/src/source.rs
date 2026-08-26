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
//! one fetch and three waits, not four fetches. [`ShareStats`] is what those counters read.
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

use std::sync::Arc;

use crate::shared::{ShareStats, Shared};

/// What a fetch produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    /// HTTP status, or 200 for a source with no notion of one.
    pub status: u16,
    /// The body. Empty is legitimate — a tile with no features is a valid, empty tile.
    pub body: Vec<u8>,
    /// Entity tag, for revalidation.
    pub etag: Option<String>,
    /// `Cache-Control: max-age`, in seconds, exactly as the origin stated it.
    ///
    /// Relative, and left relative. Resolving it here would need a clock, and a transport with
    /// a clock is a transport that disagrees with whatever else has one — which is what
    /// happened: the cache resolved freshness against an injected clock while the transport had
    /// already resolved the expiry against the system one, and every stored entry expired a
    /// lifetime away from when the cache thought it would. [`Self::expires`] resolves it, and
    /// its caller owns the clock.
    pub max_age: Option<i64>,
    /// `Expires`, in seconds since the Unix epoch.
    ///
    /// Absolute as sent, and the fallback when there is no `max-age` — mbgl reads both and
    /// prefers `Cache-Control`.
    pub expires_at: Option<i64>,
    /// The origin said `Cache-Control: must-revalidate`.
    ///
    /// The difference between "stale" and "unusable". A stale tile may still be drawn while a
    /// fresh copy is fetched — mbgl's schema says so in as many words — but one whose origin
    /// asked for revalidation may not be drawn until the origin has been asked.
    pub must_revalidate: bool,
}

impl Response {
    /// Whether the status is a success.
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// Whether the origin said the cached copy is still good.
    #[must_use]
    pub const fn is_not_modified(&self) -> bool {
        self.status == 304
    }

    /// When this stops being fresh, given when it was received.
    ///
    /// `None` means the origin said nothing about it, which mbgl treats as *fresh* rather than
    /// as stale — `isFresh()` is `expires ? *expires > now : !error`. Treating silence as
    /// immediately stale would revalidate every tile on every start.
    #[must_use]
    pub fn expires(&self, received_at: i64) -> Option<i64> {
        self.max_age
            .map(|seconds| received_at.saturating_add(seconds))
            .or(self.expires_at)
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

/// The largest a single fetched resource may be, decompressed.
///
/// Every byte this crate parses came off a network, and a source is not a trusted party — an
/// origin can be hostile, compromised, or merely wrong, and a plain-HTTP one can be anybody on
/// the path. The parsers below are all `forbid(unsafe_code)`, so the risk is not memory
/// corruption; it is *allocation*. A few hundred bytes of gzip expand to gigabytes, and on a
/// device-class target that is an out-of-memory rather than a slow frame.
///
/// Ten mebibytes is generous against what a real resource is: a vector tile is tens to hundreds
/// of kilobytes, a glyph range under a hundred, a sprite sheet a few. It is stated here rather
/// than inherited from a dependency's default — `ureq` happens to cap `read_to_vec` at the same
/// figure today, and a version that raised it would remove this bound without anything saying so.
pub const MAX_RESOURCE_BYTES: u64 = 10 * 1024 * 1024;

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

    /// Fetches, telling the origin which copy is already held.
    ///
    /// The origin may answer `304 Not Modified`, which means the held copy is still good and
    /// costs a round trip rather than a body. A source with no notion of conditional requests
    /// may ignore `etag` and fetch normally: that is always correct and merely slower, which is
    /// why the default does exactly that rather than refusing.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    fn fetch_conditional(&self, url: &str, etag: Option<&str>) -> Result<Response, FetchError> {
        let _ = etag;
        self.fetch(url)
    }
}

/// Wraps a source so concurrent requests for one URL become one request.
///
/// # Not a cache, and what that costs
///
/// A finished request is deregistered, so two views wanting a tile *one after the other* fetch
/// it twice. That is deliberate — caching has lifetime rules of its own, and revalidation is
/// impossible against a table that never forgets — but it means coalescing alone does not make
/// fetches flat in view count (§9.3). Only concurrent ones. Flatness across time needs
/// something that remembers: the byte cache of §12.6, or a caller that consults its own cache
/// before reaching for the network, which is what `tessella_orchestrate::boot` does.
///
/// The leader/waiter machinery — including the guard that keeps a panicking leader from
/// stranding its waiters — lives in [`crate::shared::Shared`], because §5.5 shares decodes and
/// buckets the same way and the subtle part should exist once.
#[derive(Debug)]
pub struct Coalescing<S> {
    source: S,
    inflight: Shared<String, Fetched>,
}

impl<S: FileSource> Coalescing<S> {
    /// Wraps a source.
    pub fn new(source: S) -> Self {
        Self {
            source,
            inflight: Shared::new(),
        }
    }

    /// What coalescing saved.
    #[must_use]
    pub fn stats(&self) -> &ShareStats {
        self.inflight.stats()
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
        // The value shared between leader and waiters is the *outcome*, not the response: a
        // failure is one caller's answer for all of them, not something each retries.
        self.inflight
            .compute(url.to_string(), || self.source.fetch(url).map(Arc::new))
            .unwrap_or_else(|_| {
                Err(FetchError::LeaderLost {
                    url: url.to_string(),
                })
            })
    }
}

/// Whether a route claims a URL.
type Accepts = Box<dyn Fn(&str) -> bool + Send + Sync>;

/// A source that dispatches by URL, so one style can name several kinds of origin.
///
/// mbgl's `MainResourceLoader` holds a list of file sources and asks each `canRequest` until one
/// says yes. This is the same arrangement: routes are tried in order, and the last source added
/// without a predicate answers whatever nothing else claimed.
///
/// # Why the predicate belongs to the route and not to the source
///
/// A source knows how to read something; it does not know whether it is the one that should.
/// The same `pmtiles` feature's `PmtilesFileSource` serves whichever archives the caller
/// points it at, and a caller that wants two of them — one for a region and one for the world —
/// distinguishes them by URL and not by asking either source to disown the other.
///
/// # Ordering, and the wrapper it has to sit inside
///
/// A route added earlier wins, which matters when one predicate is a prefix of another. And a
/// router goes *inside* the coalescing and caching wrappers rather than outside: dispatch is
/// per-URL, so a router of caches would give each origin its own in-flight table and lose the
/// property §9.3's flatness counters assert.
pub struct Router {
    routes: Vec<(Accepts, Box<dyn FileSource>)>,
    fallback: Option<Box<dyn FileSource>>,
}

impl Router {
    /// A router that claims nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            fallback: None,
        }
    }

    /// Routes every URL the predicate accepts to `source`.
    #[must_use]
    pub fn route(
        mut self,
        accepts: impl Fn(&str) -> bool + Send + Sync + 'static,
        source: impl FileSource + 'static,
    ) -> Self {
        self.routes.push((Box::new(accepts), Box::new(source)));
        self
    }

    /// Sends whatever no route claimed to `source`.
    ///
    /// Replaces any previous fallback, since there can only be one.
    #[must_use]
    pub fn otherwise(mut self, source: impl FileSource + 'static) -> Self {
        self.fallback = Some(Box::new(source));
        self
    }

    /// The source that would answer for `url`.
    fn pick(&self, url: &str) -> Option<&dyn FileSource> {
        self.routes
            .iter()
            .find(|(accepts, _)| accepts(url))
            .map(|(_, source)| source.as_ref())
            .or(self.fallback.as_deref())
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSource for Router {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        match self.pick(url) {
            Some(source) => source.fetch(url),
            // A URL nothing claimed is a configuration fault, and it is reported as one rather
            // than as a 404: "no route" and "the origin does not have it" want different fixes,
            // and a 404 would be quietly absorbed as an edge of coverage.
            None => Err(FetchError::Transport {
                url: url.to_string(),
                message: "no source is configured for this url".to_string(),
            }),
        }
    }

    fn fetch_conditional(&self, url: &str, etag: Option<&str>) -> Result<Response, FetchError> {
        match self.pick(url) {
            Some(source) => source.fetch_conditional(url, etag),
            None => self.fetch(url),
        }
    }
}
