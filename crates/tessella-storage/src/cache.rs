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

use tessella_tile::cover::Bounds;

use crate::offline::{Area, Region};
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
    /// Whether a region claims this.
    ///
    /// Which changes what it means for the entry to be stale — see [`Self::is_usable`].
    pub pinned: bool,
}

impl Entry {
    /// Whether this may be served without asking the origin.
    ///
    /// Two conditions, and they are different. A `must-revalidate` entry may never be served
    /// unasked, however fresh — the origin said so. An entry with no stated expiry is fresh,
    /// because silence is not staleness (mbgl's `isFresh`).
    ///
    /// # Why a claimed resource ignores both
    ///
    /// Freshness answers "may I use this instead of asking?", and for an ambient copy the
    /// answer belongs to the origin. For a resource a region claims it does not: the user
    /// selected an area and paid to have it, and a download is a snapshot of the moment they
    /// took it.
    ///
    /// Deferring to the headers here would undo the download twice over. With no network the
    /// map goes blank the first time a `max-age` runs out, which is precisely the situation the
    /// region exists for. With a network it is worse in a quieter way: every tile of the region
    /// costs a revalidation round trip, so a download taken specifically to avoid a metered or
    /// slow connection puts the user straight back on it. mbgl draws the same line, serving
    /// region resources without regard to expiry until the region is explicitly refreshed.
    #[must_use]
    pub const fn is_usable(&self, now: i64) -> bool {
        if self.pinned {
            return true;
        }
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
  accessed        INTEGER NOT NULL,
  pinned          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS responses_accessed ON responses (accessed);
CREATE INDEX IF NOT EXISTS responses_evictable ON responses (pinned, accessed);

CREATE TABLE IF NOT EXISTS regions (
  id                 INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  style_url          TEXT NOT NULL,
  west               REAL NOT NULL,
  south              REAL NOT NULL,
  east               REAL NOT NULL,
  north              REAL NOT NULL,
  min_zoom           REAL NOT NULL,
  max_zoom           REAL NOT NULL,
  pixel_ratio        REAL NOT NULL,
  include_ideographs INTEGER NOT NULL,
  -- MultiPolygon coordinates for a shape, null for a plain box. The bounding-box columns are
  -- filled either way: a list of regions draws them on a map without parsing anything, and a
  -- shape that is only a box would otherwise be two copies of the same four numbers.
  geometry           TEXT,
  description        TEXT,
  created            INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS region_resources (
  region_id INTEGER NOT NULL REFERENCES regions (id) ON DELETE CASCADE,
  url       TEXT NOT NULL,
  PRIMARY KEY (region_id, url)
);
CREATE INDEX IF NOT EXISTS region_resources_url ON region_resources (url);
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
        // Off by default in SQLite, and the region-to-resource mapping relies on it: deleting a
        // region has to drop its claims, or the resources stay pinned forever and the ambient
        // budget quietly shrinks by whatever the user once downloaded and later removed.
        connection
            .execute_batch("PRAGMA foreign_keys=ON")
            .map_err(CacheError::Open)?;
        connection.execute_batch(SCHEMA).map_err(CacheError::Open)?;
        Self::migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            capacity,
        })
    }

    /// The bytes the database file occupies, free space included.
    ///
    /// Distinct from [`Self::size`], which counts response bodies. The difference is what
    /// deleting things has freed inside the file but not returned to the filesystem — on a
    /// storage-constrained target that difference is the whole question.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn file_size(&self) -> Result<u64, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::file_size_of(&connection)
    }

    fn file_size_of(connection: &Connection) -> Result<u64, CacheError> {
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(u64::try_from(pages.saturating_mul(page_size)).unwrap_or(0))
    }

    /// Pages inside the file that hold nothing and have not been returned to the filesystem.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn free_bytes(&self) -> Result<u64, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let free: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(u64::try_from(free.saturating_mul(page_size)).unwrap_or(0))
    }

    /// Returns freed space to the filesystem, and reports how much.
    ///
    /// # Why this is needed at all
    ///
    /// SQLite never shrinks a file on its own. Deleting a region marks its pages free *inside*
    /// the file and the file stays the size it was, so on a device with no room left a user who
    /// deleted a download to make space would find they had not made any.
    ///
    /// # Why `VACUUM` and not incremental auto-vacuum
    ///
    /// mbgl sets `auto_vacuum=INCREMENTAL` and reclaims a bit at a time, falling back to a full
    /// rebuild once. Measured on this workload, `VACUUM` reaches the same end state around two
    /// hundred and fifty times faster: emptying a 94 MB cache and reclaiming it took 169-201 us
    /// against 41-52 ms, over alternating rounds, both finishing at three or four pages.
    ///
    /// The reason is the shape of the two. Incremental vacuum moves free pages one at a time,
    /// so it costs what was *freed*. `VACUUM` rewrites the live rows, so it costs what
    /// *survives* — and after a large delete almost nothing does. That is exactly the right way
    /// round for "the user just deleted a lot and wants the space back".
    ///
    /// It is not a write-path argument. Maintaining the pointer-map pages that incremental
    /// auto-vacuum needs turned out to cost nothing measurable per write (235/203/262 us
    /// against 241/198/266 us across alternating rounds). An earlier reading of this said
    /// sixty-one per cent; that was two benchmarks running in parallel and polluting each
    /// other, not a real effect.
    ///
    /// # Why it is not automatic
    ///
    /// That cost is proportional to what stays, not to what went — about 1.4 us per KiB
    /// surviving, so 69 ms with 47 MB still live and 256 ms with 189 MB. A user deleting one
    /// small region from a two-gigabyte cache would pay a rewrite of every region they kept. So deletion frees rows and this returns the space, and the two are separate on
    /// purpose. `VACUUM` also needs room for a second copy of the live data while it runs.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when a statement fails.
    pub fn pack(&self) -> Result<u64, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = Self::file_size_of(&connection)?;
        connection.execute_batch("VACUUM")?;
        Ok(before.saturating_sub(Self::file_size_of(&connection)?))
    }

    /// Brings a database written by an earlier version up to the current shape.
    ///
    /// `CREATE TABLE IF NOT EXISTS` adds tables but never columns, so a cache file written
    /// before regions existed has a `responses` table without `pinned` and every statement that
    /// names it fails. Rather than discard the user's cache — which on a metered connection is
    /// real money — the column is added in place, defaulting to unclaimed, which is exactly what
    /// every row in such a file is.
    fn migrate(connection: &Connection) -> Result<(), CacheError> {
        let has_pinned = connection
            .prepare("SELECT 1 FROM pragma_table_info('responses') WHERE name = 'pinned'")
            .map_err(CacheError::Open)?
            .exists([])
            .map_err(CacheError::Open)?;
        if !has_pinned {
            connection
                .execute_batch("ALTER TABLE responses ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")
                .map_err(CacheError::Open)?;
        }
        // Regions written before shapes existed are boxes, which is what a null geometry means.
        let has_geometry = connection
            .prepare("SELECT 1 FROM pragma_table_info('regions') WHERE name = 'geometry'")
            .map_err(CacheError::Open)?
            .exists([])
            .map_err(CacheError::Open)?;
        if !has_geometry {
            connection
                .execute_batch("ALTER TABLE regions ADD COLUMN geometry TEXT")
                .map_err(CacheError::Open)?;
        }
        Ok(())
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
                "SELECT status, expires, modified, etag, data, must_revalidate, accessed, pinned
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
                        pinned: row.get::<_, i64>(7)? > 0,
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
            // A row that arrives where a claim is already waiting is born pinned. Claims can
            // outrun bodies — a region may claim a URL the cache does not hold yet, and
            // `remove` drops a body while leaving the claims that named it — so the count is
            // recomputed on insert rather than assumed zero. Without that, an ambient write
            // could resurrect a claimed resource as evictable and the next fill would take it
            // out from under the region that owns it. On conflict the existing count stands.
            "INSERT INTO responses
                   (url, status, expires, modified, etag, data, must_revalidate, accessed, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?4,
                         (SELECT COUNT(*) FROM region_resources WHERE url = ?1))
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
        //
        // # Why pinned rows are excluded rather than ranked last
        //
        // A resource a region claims is not a cache entry that happens to be popular; it is the
        // thing the user asked to have offline. Ranking it last would still evict it once the
        // region outgrew the bound, which is exactly the case downloading a region is for. So
        // it is outside the query, and outside the budget: a two-gigabyte download of Berlin
        // does not cost the ambient cache its fifty megabytes, and driving for a week does not
        // cost Berlin.
        //
        // # Why a count on the row rather than a join
        //
        // The obvious spelling is `url NOT IN (SELECT url FROM region_resources)`. Measured, it
        // costs 238 us per ambient write with nothing pinned, 2.9 ms with ten thousand claims
        // and 33 ms with a hundred thousand — linear in the size of the download, and paid on
        // every tile the map writes forever after. A user who downloads a country would find
        // that every subsequent write costs more than a frame. `pinned` is a count maintained
        // where claims are made, so eviction reads a column on the row it already has and the
        // `responses_evictable` index lets it walk only the rows it may take.
        let removed = connection.execute(
            "DELETE FROM responses WHERE url IN (
               SELECT url FROM (
                 SELECT url, SUM(LENGTH(data)) OVER (
                   ORDER BY accessed DESC, url DESC
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                 ) AS running
                 FROM responses
                 WHERE pinned = 0
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

    /// The bytes held for regions, which are outside the ambient bound.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn region_size(&self) -> Result<u64, CacheError> {
        let bytes: i64 = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM responses WHERE pinned > 0",
                [],
                |row| row.get(0),
            )?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    /// Records a region a user asked to have offline, and returns its identifier.
    ///
    /// Creating it claims nothing: the region exists with no resources until a download stores
    /// them against it. That ordering is deliberate — a region that appears in the list the
    /// moment it is asked for, at zero percent, is what makes a download resumable and
    /// cancellable rather than all-or-nothing.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn create_region(
        &self,
        region: &Region,
        description: Option<&str>,
        now: i64,
    ) -> Result<RegionId, CacheError> {
        let bounds = region.area.bounds();
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connection.execute(
            "INSERT INTO regions
               (style_url, west, south, east, north, min_zoom, max_zoom,
                pixel_ratio, include_ideographs, geometry, description, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                region.style_url,
                bounds.west,
                bounds.south,
                bounds.east,
                bounds.north,
                region.min_zoom,
                region.max_zoom,
                f64::from(region.pixel_ratio),
                i64::from(region.include_ideographs),
                region.area.geometry().map(|value| value.to_string()),
                description,
                now,
            ],
        )?;
        Ok(RegionId(connection.last_insert_rowid()))
    }

    /// Every region, oldest first.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn regions(&self) -> Result<Vec<StoredRegion>, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut statement = connection.prepare(
            "SELECT id, style_url, west, south, east, north, min_zoom, max_zoom,
                    pixel_ratio, include_ideographs, geometry, description, created
             FROM regions ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            // A stored shape that no longer parses is an error rather than a silent fall back
            // to its bounding box: downgrading a city outline to the sea around it, without
            // saying so, is a download the user never agreed to.
            let unreadable = |error: Box<dyn std::error::Error + Send + Sync>| {
                rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, error)
            };
            let area = match row.get::<_, Option<String>>(10)? {
                Some(geometry) => {
                    let value: serde_json::Value =
                        serde_json::from_str(&geometry).map_err(|e| unreadable(Box::new(e)))?;
                    Area::from_geometry(&value).map_err(|e| unreadable(Box::new(e)))?
                }
                None => Area::Box(Bounds::new(
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                )),
            };
            Ok(StoredRegion {
                id: RegionId(row.get(0)?),
                region: Region {
                    style_url: row.get(1)?,
                    area,
                    min_zoom: row.get(6)?,
                    max_zoom: row.get(7)?,
                    #[allow(clippy::cast_possible_truncation)]
                    pixel_ratio: row.get::<_, f64>(8)? as f32,
                    include_ideographs: row.get::<_, i64>(9)? != 0,
                },
                description: row.get(11)?,
                created: row.get(12)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CacheError::from)
    }

    /// Forgets a region, releasing its claim on every resource it held.
    ///
    /// The bytes are not deleted here. What the region held becomes ordinary cache, subject to
    /// the ambient bound like anything else — so a user who removes a downloaded city and
    /// immediately looks at it still sees it, and the space comes back as it is needed rather
    /// than in one stall.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn delete_region(&self, id: RegionId) -> Result<(), CacheError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transaction = connection.transaction()?;
        // Before the cascade removes the mappings, so there is still something to count from.
        // One linear pass over this region's claims, paid once when a user removes a download
        // rather than on every write afterwards.
        transaction.execute(
            "UPDATE responses SET pinned = pinned - 1
             WHERE url IN (SELECT url FROM region_resources WHERE region_id = ?1)",
            params![id.0],
        )?;
        transaction.execute("DELETE FROM regions WHERE id = ?1", params![id.0])?;
        transaction.commit()?;
        // The rows are unpinned now and may well put the cache over its bound.
        Self::evict_within(&connection, self.capacity)?;
        // The pages those deletions freed stay inside the file until someone calls
        // [`Self::pack`], and that is deliberate — its cost is proportional to what *survives*,
        // so packing here would make deleting one small region from a large cache rewrite every
        // region the user kept.
        Ok(())
    }

    /// Stores a resource and claims it for a region.
    ///
    /// Unlike [`Self::put`] this does not evict and is not bounded by [`Self::capacity`]: the
    /// user asked for these bytes by name, and a download that silently dropped its own tail to
    /// stay under an ambient cache limit would report success and produce a map with holes.
    ///
    /// A resource already held ambiently is claimed rather than refetched — which is why a
    /// region covering somewhere the user has been downloads less than one covering somewhere
    /// they have not.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when a statement fails.
    pub fn put_region_resource(
        &self,
        id: RegionId,
        url: &str,
        response: &Response,
        now: i64,
    ) -> Result<(), CacheError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One transaction: a body stored without its claim is an unpinned tile that eviction
        // may take before the download finishes, and a claim stored without its body is a
        // region that reports complete and renders nothing.
        let transaction = connection.transaction()?;
        transaction.execute(
            // As [`Self::put`]: the count comes from the claims, not from zero.
            "INSERT INTO responses
               (url, status, expires, modified, etag, data, must_revalidate, accessed, pinned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?4,
                     (SELECT COUNT(*) FROM region_resources WHERE url = ?1))
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
        Self::claim_within(&transaction, id, url)?;
        transaction.commit()?;
        Ok(())
    }

    /// Claims a resource the cache already holds for a region.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails, including when no such region exists.
    pub fn claim(&self, id: RegionId, url: &str) -> Result<(), CacheError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transaction = connection.transaction()?;
        Self::claim_within(&transaction, id, url)?;
        transaction.commit()?;
        Ok(())
    }

    /// Records one region's claim and keeps `responses.pinned` in step with it.
    ///
    /// The two have to move together or the count drifts, and a drifted count either evicts
    /// something a region holds or holds space nothing claims — so this is only ever called
    /// inside a transaction.
    fn claim_within(
        transaction: &rusqlite::Transaction<'_>,
        id: RegionId,
        url: &str,
    ) -> Result<(), CacheError> {
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO region_resources (region_id, url) VALUES (?1, ?2)",
            params![id.0, url],
        )?;
        // Only a claim that was actually new increments. Claiming twice from the same region is
        // ordinary — a download that resumes re-walks tiles it already has — and must not leave
        // the resource pinned twice over.
        if inserted > 0 {
            transaction.execute(
                "UPDATE responses SET pinned = pinned + 1 WHERE url = ?1",
                params![url],
            )?;
        }
        Ok(())
    }

    /// How far a region's download has got.
    ///
    /// # Errors
    ///
    /// [`CacheError::Query`] when the statement fails.
    pub fn region_progress(&self, id: RegionId) -> Result<RegionProgress, CacheError> {
        let connection = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A left join, not an inner one: a claim whose body has been lost should count as a
        // resource still owed rather than vanish from the total and let the region read
        // complete.
        let (resources, bytes) = connection.query_row(
            "SELECT COUNT(responses.url), COALESCE(SUM(LENGTH(responses.data)), 0)
             FROM region_resources
             LEFT JOIN responses ON responses.url = region_resources.url
             WHERE region_resources.region_id = ?1",
            params![id.0],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        Ok(RegionProgress {
            completed_resources: u64::try_from(resources).unwrap_or(0),
            completed_bytes: u64::try_from(bytes).unwrap_or(0),
        })
    }
}

/// A region's identifier, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(i64);

impl RegionId {
    /// The underlying row identifier, for callers that have to name a region across a boundary.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// A region as recorded, with what the store knows about it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRegion {
    /// Its identifier.
    pub id: RegionId,
    /// What was asked for.
    pub region: Region,
    /// Whatever the caller labelled it, typically a name a user typed.
    pub description: Option<String>,
    /// When it was created, in seconds since the Unix epoch.
    pub created: i64,
}

/// What a region has downloaded so far.
///
/// The other half of a progress bar is [`crate::offline::Estimate`], which says what it is
/// working towards — and which is a lower bound until every source manifest has arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionProgress {
    /// Resources stored and claimed.
    pub completed_resources: u64,
    /// Bytes those resources occupy.
    pub completed_bytes: u64,
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
