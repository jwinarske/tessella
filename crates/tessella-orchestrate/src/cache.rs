//! The process-scoped bucket cache (§5.1, §5.5, §9.3).
//!
//! Coalescing gets a tile's *bytes* fetched once however many views want them. This does the
//! same for what happens next: decoding them and building their buckets. §5.5 lists both as
//! process-scoped, and §9.3's counters assert neither scales with view count — so a build that
//! ran per view would be a bug whether or not the fetch was shared.
//!
//! # Two mechanisms, because they answer different questions
//!
//! A *store* answers "has this been built": the second view to want a tile finds it. A
//! *shared-work table* answers "is this being built right now": four views starting on the same
//! camera all miss the store, because none of them has finished yet. Neither alone is enough,
//! and the window the second closes is exactly the startup burst §12.5 is about.
//!
//! # Decoding is not shared separately
//!
//! It does not need to be. A decode only happens on the way to a build, so a build that runs
//! once decodes once. Sharing them separately would matter if two *styles* wanted the same
//! tile — the decode is style-independent and the buckets are not — and that is worth doing
//! when a second style exists to want it, not before.
//!
//! # Failures are not cached
//!
//! A tile that failed to build is not stored, so the next view retries rather than inheriting a
//! poisoned entry. It *is* shared for the duration of the attempt, because four views failing
//! in parallel on the same malformed tile should cost one attempt, not four.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use tessella_storage::shared::Shared;
use tessella_tile::store::{Lookup, TileKey, TileStore};

use crate::tile::LayerBucket;

/// A tile's built buckets, as the cache holds them.
pub type Cached = Arc<Vec<LayerBucket>>;

/// What a lookup did, and what it produced.
#[derive(Debug, Clone)]
pub struct Built {
    /// The buckets.
    pub tile: Cached,
    /// Whether this call did the work.
    pub lookup: Lookup,
}

/// A process-scoped cache of built tiles, safe to share across workers and views.
///
/// Generic over the failure type rather than flattening it to a string. A caller distinguishes
/// a dead origin from a malformed tile — one is worth retrying and the other never will be —
/// and a cache that returned "something went wrong" for both would take that distinction away
/// from the only code able to act on it. `E` must be `Clone` because one failure is handed to
/// every caller waiting on the same tile.
#[derive(Debug)]
pub struct TileCache<E = String> {
    store: Mutex<TileStore<Vec<LayerBucket>>>,
    building: Shared<TileKey, Result<Cached, E>>,
    hits: AtomicU64,
}

impl<E: Clone> TileCache<E> {
    /// A cache holding at most `capacity` unretained tiles.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            store: Mutex::new(TileStore::new(capacity)),
            building: Shared::new(),
            hits: AtomicU64::new(0),
        }
    }

    /// The buckets, if this tile is already built.
    ///
    /// For a caller that decides whether to *fetch*. [`Self::get_or_build`] consults the store
    /// before it runs its closure, which is what keeps a warm view off the network -- but a
    /// deferred caller has to make that decision a whole round trip earlier, before it asks any
    /// origin for bytes it may not need. Same lookup, same hit counter, asked at the point where
    /// the answer can still save the fetch.
    ///
    /// Counts as a hit for exactly that reason: it is a caller answered from the store, and §9.3's
    /// accounting only adds up if every such caller is counted once. The caller that peeks does
    /// not go on to call [`Self::get_or_build`] for the same tile.
    #[must_use]
    pub fn peek(&self, key: &TileKey) -> Option<Cached> {
        let tile = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)?;
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(tile)
    }

    /// Builds a tile, or returns the one already built, or joins the build already running.
    ///
    /// # Errors
    ///
    /// Whatever `work` reported, or [`Abandoned`](tessella_storage::shared::Abandoned) rendered
    /// through `abandoned` when the caller that owned this key unwound without producing a
    /// value. The key is free again in that case, so it is a retryable failure.
    pub fn get_or_build(
        &self,
        key: &TileKey,
        work: impl FnOnce() -> Result<Vec<LayerBucket>, E>,
        abandoned: impl FnOnce() -> E,
    ) -> Result<Built, E> {
        if let Some(tile) = self.peek(key) {
            return Ok(Built {
                tile,
                lookup: Lookup::Hit,
            });
        }

        // The store is *not* held across the build. Holding it would serialize every build in
        // the process behind the slowest one, which is the mistake the shared-work table below
        // exists to avoid making.
        let outcome = self.building.compute(key.clone(), || {
            let built = work()?;
            let mut store = self.store.lock().unwrap_or_else(PoisonError::into_inner);
            let (tile, _) = store.get_or_build(key, || built);
            Ok(tile)
        });

        match outcome {
            Ok(Ok(tile)) => Ok(Built {
                tile,
                lookup: Lookup::Miss,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(abandoned()),
        }
    }

    /// Marks a tile as held by one more view.
    pub fn retain(&self, key: &TileKey) {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(key);
    }

    /// Releases one view's hold.
    pub fn release(&self, key: &TileKey) {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .release(key);
    }

    /// Tiles actually built. The number §9.3 asserts flat in view count.
    #[must_use]
    pub fn builds(&self) -> u64 {
        self.building.stats().computed()
    }

    /// Calls that joined a build already running, rather than starting their own.
    #[must_use]
    pub fn joins(&self) -> u64 {
        self.building.stats().waited()
    }

    /// Calls that found the tile already built.
    ///
    /// The third outcome, and the one that is easy to forget: a caller either builds, joins a
    /// build, or hits. Accounting that omits hits does not add up to the number of callers, and
    /// the omission only shows under load — when some callers arrive late enough to hit rather
    /// than join, which is exactly when a flatness assertion is being exercised hardest.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Every call, however it was answered.
    #[must_use]
    pub fn lookups(&self) -> u64 {
        self.builds() + self.joins() + self.hits()
    }

    /// Tiles currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// True when the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
