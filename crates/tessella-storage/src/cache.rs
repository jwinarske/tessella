//! The on-disk response cache (§12.6).
//!
//! # Schema
//!
//! Transcribed from mbgl's `offline_schema.sql` — `expires`, `modified`, `etag`, `data`,
//! `compressed`, `accessed`, `must_revalidate` — because those columns are not arbitrary. Each
//! answers a question the cache is asked: may this be used, may it be used *without* asking the
//! origin, what do I send to ask, and which entry do I evict.
//!
//! Not transcribed: mbgl's `regions` and `region_resources` tables, which are offline packs
//! rather than an ambient cache, and its separate `tiles` table.
//!
//! # Why one table and not mbgl's two
//!
//! mbgl keys tiles by `(url_template, pixel_ratio, z, x, y)` rather than by URL, and the reason
//! is in its own schema comment: "the URL of the resource without the access token". A style
//! whose tile URL carries a rotating token would otherwise miss the cache on every request,
//! because the key would change with the token.
//!
//! Doing that here needs the fetch interface to carry tile identity, not just a URL — mbgl's
//! `Resource` has a `TileData` beside its URL for exactly this. That is a change to
//! [`crate::source::FileSource`], not to this file, and until it happens a token in the query
//! string defeats the cache. Recorded rather than worked around, because the workaround —
//! stripping parameters that look like tokens — guesses at which ones matter.
//!
//! # Is SQLite the right engine
//!
//! Measured, on twenty 471 KiB tiles from the page cache: a `get` costs 0.59 ms against a
//! plain `fs::read` of the same bytes at 0.26 ms, and against 0.50 ms for SQLite reading the
//! blob column and nothing else. So a lookup is within a fifth of what SQLite can possibly do
//! for this workload, and about twice a raw file read — which for a nine-tile cover is 0.26 ms
//! against 0.11 ms, noise beside the fifteen milliseconds that decode and tessellation take.
//!
//! The first measurement said something more useful: 71% of a lookup was the LRU bookkeeping,
//! not the read. That is fixed below and is worth three times more than any engine change
//! would have been.
//!
//! What would argue for replacing SQLite is not speed. It is that `rusqlite` bundles the last C
//! in the tree (§3.1), which is why this whole module is behind an off-by-default feature and
//! cannot be built for the cross target. A pure-Rust store would remove that. Against it: an
//! `.mbtiles` archive *is* an SQLite database, so having it linked is what would let one be
//! read directly. That is a decision for the §8 dependency table, not for this file.
//!
//! # WAL, and what it is for
//!
//! §12.6 asks for WAL so a reader is not blocked by a writer. That matters here more than in an
//! ordinary application: the readers are decode workers on the critical path of a cold start,
//! and the writer is whichever of them happens to have just fetched something.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, params};

use crate::source::Response;

/// Why a cache operation failed.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The database could not be opened or prepared.
    #[error("opening the cache: {0}")]
    Open(#[source] rusqlite::Error),
    /// A statement failed.
    #[error("cache query: {0}")]
    Query(#[from] rusqlite::Error),
}

/// A cached response and what is known about its freshness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The response as it was received.
    pub response: Response,
    /// When it stops being fresh, resolved against the moment it was stored.
    ///
    /// Resolved here rather than carried relative, because an entry outlives the request that
    /// produced it: a `max-age` of ten seconds means ten seconds from *then*, and an entry that
    /// re-derived it on every read would never expire.
    pub expires: Option<i64>,
    /// When it was last written, in seconds since the Unix epoch.
    pub modified: i64,
    /// When it was last read, in seconds since the Unix epoch.
    pub accessed: i64,
}

impl Entry {
    /// Whether this may be served without asking the origin.
    ///
    /// Two conditions, and they are different. A `must-revalidate` entry may never be served
    /// unasked, however fresh — the origin said so. An entry with no stated expiry is fresh,
    /// because silence is not staleness (mbgl's `isFresh`).
    #[must_use]
    pub const fn is_usable(&self, now: i64) -> bool {
        if self.response.must_revalidate {
            return false;
        }
        match self.expires {
            Some(expires) => expires > now,
            None => true,
        }
    }
}

/// The schema, applied to a fresh database.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS responses (
  url             TEXT NOT NULL PRIMARY KEY,
  status          INTEGER NOT NULL,
  expires         INTEGER,
  modified        INTEGER NOT NULL,
  etag            TEXT,
  data            BLOB,
  must_revalidate INTEGER NOT NULL DEFAULT 0,
  accessed        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS responses_accessed ON responses (accessed);
";

/// An SQLite-backed response cache, bounded by the bytes it holds.
///
/// # Why bytes and not entries
///
/// A count is meaningless when a TileJSON is two hundred bytes and a dense z14 tile is half a
/// megabyte: a thousand of one is nothing and a thousand of the other is half a gigabyte. What
/// a storage-constrained target has a limit on is bytes, so that is what is bounded. mbgl
/// bounds the same thing, and defaults to the same fifty megabytes.
#[derive(Debug)]
pub struct SqliteCache {
    connection: Mutex<Connection>,
    capacity: u64,
}

impl SqliteCache {
    /// The default bound, which is mbgl's `DEFAULT_MAX_CACHE_SIZE`.
    pub const DEFAULT_CAPACITY: u64 = 50 * 1024 * 1024;

    /// How stale a read timestamp may get before it is worth rewriting, in seconds.
    ///
    /// Eviction asks which entries are cold. A minute answers that as precisely as a second
    /// does, and costs a write per entry per minute instead of a write per read.
    pub const ACCESS_GRANULARITY: i64 = 60;

    /// Opens or creates a cache at `path`, bounded to [`Self::DEFAULT_CAPACITY`].
    ///
    /// # Errors
    ///
    /// [`CacheError::Open`] when the file cannot be opened or the schema cannot be applied.
    pub fn open(path: &Path) -> Result<Self, CacheError> {
        Self::with_capacity(path, Self::DEFAULT_CAPACITY)
    }

    /// As [`Self::open`], bounded to `capacity` bytes of response bodies.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn with_capacity(path: &Path, capacity: u64) -> Result<Self, CacheError> {
        Self::prepare(Connection::open(path).map_err(CacheError::Open)?, capacity)
    }

    /// A cache that lives only as long as this object.
    ///
    /// # Errors
    ///
    /// [`CacheError::Open`] when the database cannot be created.
    pub fn in_memory() -> Result<Self, CacheError> {
        Self::in_memory_with_capacity(Self::DEFAULT_CAPACITY)
    }

    /// As [`Self::in_memory`], bounded to `capacity` bytes.
    ///
    /// # Errors
    ///
    /// As [`Self::in_memory`].
    pub fn in_memory_with_capacity(capacity: u64) -> Result<Self, CacheError> {
        Self::prepare(
            Connection::open_in_memory().map_err(CacheError::Open)?,
            capacity,
        )
    }

    /// The bound, in bytes.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    fn prepare(connection: Connection, capacity: u64) -> Result<Self, CacheError> {
        // WAL so a decode worker reading is not blocked by whichever worker is writing what it
        // just fetched. `query_row` rather than `execute`: the pragma returns the resulting
        // journal mode, and `execute` refuses a statement that returns rows.
        connection
            .query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
            .map_err(CacheError::Open)?;
        connection.execute_batch(SCHEMA).map_err(CacheError::Open)?;
        Ok(Self {
            connection: Mutex::new(connection),
            capacity,
        })
    }

    /// Looks a URL up, marking it as used.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn get(&self, url: &str, now: i64) -> Result<Option<Entry>, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let entry = connection
            .query_row(
                "SELECT status, expires, modified, etag, data, must_revalidate, accessed
                 FROM responses WHERE url = ?1",
                params![url],
                |row| {
                    Ok(Entry {
                        response: Response {
                            status: row.get::<_, i64>(0)? as u16,
                            etag: row.get(3)?,
                            body: row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
                            must_revalidate: row.get::<_, i64>(5)? != 0,
                            // The stated headers are not kept: what matters after storage is
                            // the resolved expiry beside them.
                            max_age: None,
                            expires_at: None,
                        },
                        expires: row.get(1)?,
                        modified: row.get(2)?,
                        accessed: row.get(6)?,
                    })
                },
            )
            .optional()?;

        // Eviction is least-recently-used, so a read has to record that it happened — and that
        // makes every read a write. Measured, it was *seventy-one percent* of the cost of a
        // lookup: the read itself is 0.52 ms for nine megabytes and the bookkeeping around it
        // is another 1.2 ms. On flash it is write amplification as well as latency.
        //
        // So the timestamp is only rewritten when it would move by more than
        // [`Self::ACCESS_GRANULARITY`]. Eviction wants to know which entries are cold, and
        // minute resolution answers that as well as second resolution does; a tile read twenty
        // times in a frame is written once.
        if let Some(entry) = &entry
            && now.saturating_sub(entry.accessed) > Self::ACCESS_GRANULARITY
        {
            connection.execute(
                "UPDATE responses SET accessed = ?2 WHERE url = ?1",
                params![url, now],
            )?;
        }
        Ok(entry)
    }

    /// Stores a response, replacing any earlier one for the same URL.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn put(&self, url: &str, response: &Response, now: i64) -> Result<(), CacheError> {
        // An entry larger than the whole cache is not stored. Storing it would evict everything
        // else to make room for something that cannot help — and on the next write it would be
        // evicted itself, having thrown away the tiles that were being used.
        if response.body.len() as u64 > self.capacity {
            return Ok(());
        }

        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connection.execute(
            "INSERT INTO responses
                   (url, status, expires, modified, etag, data, must_revalidate, accessed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?4)
                 ON CONFLICT(url) DO UPDATE SET
                   status = ?2, expires = ?3, modified = ?4, etag = ?5,
                   data = ?6, must_revalidate = ?7, accessed = ?4",
            params![
                url,
                i64::from(response.status),
                response.expires(now),
                now,
                response.etag,
                response.body,
                i64::from(response.must_revalidate),
            ],
        )?;

        // Every write, so a caller cannot forget. The common case costs one COUNT-like query
        // and no deletes.
        Self::evict_within(&connection, self.capacity)?;
        Ok(())
    }

    /// Records that the origin confirmed a held copy, without rewriting its body.
    ///
    /// The point of revalidation: a `304` costs a round trip and no bytes, so the body stays
    /// where it is and only the freshness moves. Rewriting the blob would turn the saving back
    /// into a disk write of the whole tile.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn refresh(
        &self,
        url: &str,
        expires: Option<i64>,
        must_revalidate: bool,
        now: i64,
    ) -> Result<(), CacheError> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execute(
                "UPDATE responses
                 SET expires = ?2, must_revalidate = ?3, modified = ?4, accessed = ?4
                 WHERE url = ?1",
                params![url, expires, i64::from(must_revalidate), now],
            )?;
        Ok(())
    }

    /// Removes a URL.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn remove(&self, url: &str) -> Result<(), CacheError> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execute("DELETE FROM responses WHERE url = ?1", params![url])?;
        Ok(())
    }

    /// The bytes of response bodies held.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn size(&self) -> Result<u64, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::size_of(&connection)
    }

    fn size_of(connection: &Connection) -> Result<u64, CacheError> {
        let bytes: i64 = connection.query_row(
            "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM responses",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    /// Drops least-recently-used entries until the cache is inside its bound.
    ///
    /// Returns how many were dropped. Called automatically after every write, so a caller does
    /// not have to remember to — an unbounded cache on a storage-constrained target is a defect
    /// rather than a missing feature, and one that only shows up after a long drive.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when a statement fails.
    pub fn evict(&self) -> Result<usize, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict_within(&connection, self.capacity)
    }

    fn evict_within(connection: &Connection, capacity: u64) -> Result<usize, CacheError> {
        // Keep the newest entries whose running total stays inside the bound, and drop the
        // rest. One statement, and exactly the rows that do not fit.
        //
        // # Why not mbgl's batch
        //
        // mbgl takes the `accessed` of the fiftieth-oldest entry and deletes everything at or
        // before it, looping. With fewer than fifty entries that timestamp is the *newest*
        // one's, so the delete takes the whole cache — including the entry just touched, which
        // is the one thing LRU exists to keep. It rarely bites there because a real cache holds
        // thousands; it bites immediately in a test, which is how it was found. A window
        // function costs the same one statement and cannot over-delete.
        let removed = connection.execute(
            "DELETE FROM responses WHERE url IN (
               SELECT url FROM (
                 SELECT url, SUM(LENGTH(data)) OVER (
                   ORDER BY accessed DESC, url DESC
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                 ) AS running
                 FROM responses
               ) WHERE running > ?1)",
            params![i64::try_from(capacity).unwrap_or(i64::MAX)],
        )?;
        Ok(removed)
    }

    /// How many entries are held.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn len(&self) -> Result<usize, CacheError> {
        let count: i64 = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .query_row("SELECT COUNT(*) FROM responses", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// True when the cache holds nothing.
    ///
    /// # Errors
    ///
    /// As [`Self::len`].
    pub fn is_empty(&self) -> Result<bool, CacheError> {
        Ok(self.len()? == 0)
    }
}

/// Wraps a source so responses are served from, and written to, a cache.
///
/// # Where this sits, and why
///
/// Below coalescing: `Coalescing::new(CachingFileSource::new(http, cache))`. Four views wanting
/// one tile then produce one leader, and that leader consults the cache once. The other order
/// works too and asks the cache four times for an answer it already has.
///
/// # The three outcomes
///
/// A usable entry is served without touching the network. A stale one is *revalidated*: the
/// origin is asked with `If-None-Match`, and a `304` means the held body is still good and cost
/// a round trip rather than a download. Anything else is a fetch, stored on the way back.
///
/// # A stale entry survives a dead origin
///
/// If revalidation fails at the transport, the held copy is served anyway. mbgl's schema says
/// the same thing in its own words — "expired tiles can still be rendered" — and the reason is
/// that a map which goes blank when the link drops is worse than one showing tiles from a
/// minute ago. `must-revalidate` is the exception the origin can ask for, and then the error
/// propagates.
#[derive(Debug)]
pub struct CachingFileSource<S> {
    inner: S,
    cache: SqliteCache,
    clock: fn() -> i64,
    stats: CacheStats,
}

/// How each request was answered.
///
/// Without these a cache is invisible from above: a hit and a fetch return the same response,
/// so a caller counting bytes cannot say whether any of them crossed the network. That is the
/// number a warm start is judged on, and §9.3 counts the same kind of thing for the same
/// reason.
#[derive(Debug, Default)]
pub struct CacheStats {
    hits: AtomicU64,
    revalidated: AtomicU64,
    fetched: AtomicU64,
    stale: AtomicU64,
}

impl CacheStats {
    /// Served from the cache without asking the origin.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Asked the origin, which confirmed the held copy with a `304`.
    ///
    /// A round trip but no body, which is the point of an etag.
    #[must_use]
    pub fn revalidated(&self) -> u64 {
        self.revalidated.load(Ordering::Relaxed)
    }

    /// A body came from the origin.
    #[must_use]
    pub fn fetched(&self) -> u64 {
        self.fetched.load(Ordering::Relaxed)
    }

    /// The origin could not be reached and a stale copy was served instead.
    #[must_use]
    pub fn stale(&self) -> u64 {
        self.stale.load(Ordering::Relaxed)
    }

    /// Requests that reached the origin at all, whether or not a body came back.
    #[must_use]
    pub fn round_trips(&self) -> u64 {
        self.revalidated() + self.fetched()
    }
}

impl<S: crate::source::FileSource> CachingFileSource<S> {
    /// Wraps a source with a cache.
    pub fn new(inner: S, cache: SqliteCache) -> Self {
        Self {
            inner,
            cache,
            clock: unix_now,
            stats: CacheStats::default(),
        }
    }

    /// As [`Self::new`], with a clock a test can control.
    ///
    /// Freshness is the whole of what this decides, so a test that cannot move time can only
    /// check the paths that do not depend on it — which is the interesting half missing.
    pub fn with_clock(inner: S, cache: SqliteCache, clock: fn() -> i64) -> Self {
        Self {
            inner,
            cache,
            clock,
            stats: CacheStats::default(),
        }
    }

    /// The cache.
    pub fn cache(&self) -> &SqliteCache {
        &self.cache
    }

    /// How requests were answered.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// The wrapped source.
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64)
}

impl<S: crate::source::FileSource> crate::source::FileSource for CachingFileSource<S> {
    fn fetch(&self, url: &str) -> Result<Response, crate::source::FetchError> {
        let now = (self.clock)();

        // A cache that cannot be read is a cache miss, not a failure: the network still works,
        // and refusing to draw a map because a local database is unhappy is the wrong trade.
        let held = self.cache.get(url, now).ok().flatten();

        if let Some(entry) = &held
            && entry.is_usable(now)
        {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(entry.response.clone());
        }

        let etag = held
            .as_ref()
            .and_then(|entry| entry.response.etag.as_deref());
        let fetched = self.inner.fetch_conditional(url, etag);

        match fetched {
            Ok(response) if response.is_not_modified() => {
                // The held copy stands. Only its freshness moves, so the body is not rewritten.
                self.stats.revalidated.fetch_add(1, Ordering::Relaxed);
                let entry = held.expect("a 304 is only possible when something was held");
                let _ =
                    self.cache
                        .refresh(url, response.expires(now), response.must_revalidate, now);
                Ok(entry.response)
            }
            Ok(response) => {
                // Only success is worth keeping. Caching a 404 would make a source's coverage
                // permanent, and caching a 500 would make an outage so.
                self.stats.fetched.fetch_add(1, Ordering::Relaxed);
                if response.is_ok() {
                    let _ = self.cache.put(url, &response, now);
                }
                Ok(response)
            }
            Err(error) => match held {
                // Stale is better than blank, unless the origin asked otherwise.
                Some(entry) if !entry.response.must_revalidate => {
                    self.stats.stale.fetch_add(1, Ordering::Relaxed);
                    Ok(entry.response)
                }
                _ => Err(error),
            },
        }
    }

    fn fetch_conditional(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> Result<Response, crate::source::FetchError> {
        // A caller doing its own revalidation is asking to bypass this one; passing its etag
        // through and skipping the cache is the only reading that does not answer a conditional
        // request with an unconditional cached body.
        self.inner.fetch_conditional(url, etag)
    }
}
