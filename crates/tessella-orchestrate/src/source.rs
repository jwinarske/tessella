//! Where a warm map's tiles come from (§5.5, §16).
//!
//! # Why this is not part of a map
//!
//! §5.5 lists the file sources, the tile cache and the worker pool as process-owned, and this is
//! the thing that holds all three together. Two views over one city want one fetch and one build
//! between them, which a per-view source cannot give however carefully it is written — so the
//! source is shared and the views are what is cheap.
//!
//! # Why nothing here blocks
//!
//! §16 settled it: Fluorite's bindings are `@Native` on the calling isolate with no hop, so a
//! blocking call freezes the application rather than the map. Every entry point here returns
//! immediately. [`TileSource::want`] says what is wanted and schedules; [`TileSource::buckets`]
//! answers with what has landed, and a tile that has not arrived is simply absent — which is the
//! answer the frame loop already knows how to draw, by substituting the ancestor it has.
//!
//! That makes "silently blank" the hazard, which this project has caught three times. It is
//! answered by [`TileSource::readiness`] rather than by a stall: a consumer that sees an empty
//! map has something to read that says whether the style failed, and which.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicUsize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::Instant;

use tessella_storage::source::{Coalescing, FileSource};
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::store::TileKey;

use crate::boot::{self, BootError, Sources};
use crate::cache::TileCache;
use crate::map::Tiles;
use crate::pool::{Pool, Priority};
use crate::tile::{LayerBucket, TileId};

/// How far along a source is.
///
/// Reported rather than inferred. A consumer looking at an empty map cannot tell a style that is
/// still resolving from one that failed, and guessing from the absence of tiles gets it wrong in
/// both directions — so this says which, and [`Self::Failed`] carries the reason the producer
/// would otherwise only have logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Nothing has been asked for yet. The first [`TileSource::want`] starts resolution.
    Idle,
    /// The style's sources are resolving. No tile can be planned until they do, because the
    /// manifests carry the templates a tile's URL is built from.
    Resolving,
    /// Resolved. Tiles are built as they are wanted, and land as they finish.
    Ready,
    /// The style did not parse, or a source did not resolve. Terminal: nothing retries, because
    /// a style that will not parse will not parse the second time either.
    Failed(String),
}

/// What has landed, by address.
///
/// A tile is one thing per *source*, so a style overlaying a local extract on a world basemap
/// has both merged at one address — the frame draws a tile's buckets together whichever source
/// produced them.
#[derive(Default)]
struct Landed {
    by_tile: BTreeMap<TileId, Arc<Vec<LayerBucket>>>,
    sourceless: BTreeMap<TileId, Arc<Vec<LayerBucket>>>,
}

/// What a source is doing, behind one lock.
struct Inner {
    readiness: Readiness,
    sources: Option<Arc<Sources>>,
    /// Tiles submitted and not yet landed, so a camera that keeps moving over the same gap asks
    /// for it once rather than once per tick.
    inflight: BTreeSet<TileKey>,
}

/// The tiles a warm map draws from.
///
/// Process-scoped and shared: one of these serves every view, so a tile wanted by two of them is
/// fetched once, built once, and held once.
pub struct TileSource<S> {
    style_text: String,
    files: Arc<Coalescing<S>>,
    cache: Arc<TileCache<BootError>>,
    pool: &'static Pool,
    style_rev: u64,
    inner: Mutex<Inner>,
    /// Separate from [`Self::inner`], and a `RwLock` rather than a `Mutex`, because reading it is
    /// what a frame does: every tick asks for every tile of its cover, and those reads must not
    /// queue behind a worker recording that an unrelated tile has landed.
    landed: RwLock<Landed>,
}

/// Where the warm path's byte counter goes.
///
/// A tile built off a `wanted()` list is not part of anyone's cold-start timing, so the count has
/// no reader. One static rather than one per source: nothing reads it, and giving each source its
/// own would only make that more convincing than it deserves to be.
static DISCARDED: AtomicUsize = AtomicUsize::new(0);

impl<S: FileSource + 'static> TileSource<S> {
    /// Holds a style and the process-scoped things a build needs. Fetches nothing.
    ///
    /// Nothing happens until the first [`Self::want`], which is what makes creating a map cheap:
    /// §16's `create` parses the style to reject a bad one and then hands back a handle, and the
    /// first tick is what starts the network.
    pub fn new(
        style_text: String,
        files: Arc<Coalescing<S>>,
        cache: Arc<TileCache<BootError>>,
        pool: &'static Pool,
        style_rev: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            style_text,
            files,
            cache,
            pool,
            style_rev,
            inner: Mutex::new(Inner {
                readiness: Readiness::Idle,
                sources: None,
                inflight: BTreeSet::new(),
            }),
            landed: RwLock::new(Landed::default()),
        })
    }

    /// How far along it is.
    pub fn readiness(&self) -> Readiness {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .readiness
            .clone()
    }

    /// Says which tiles are wanted, and schedules whatever is missing.
    ///
    /// Returns immediately, always. On the first call it starts source resolution; on every call
    /// after that resolution is done, it plans `coords` against what resolved and submits the
    /// jobs for tiles that have neither landed nor been asked for already.
    ///
    /// Cheap to call every tick, which is how the frame loop calls it: planning is arithmetic,
    /// and the work it would duplicate is exactly what `inflight` holds back.
    pub fn want(self: &Arc<Self>, view: &ViewTransform, coords: &[TileCoord]) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match inner.readiness {
            Readiness::Idle => {
                inner.readiness = Readiness::Resolving;
                drop(inner);
                let this = Arc::clone(self);
                self.pool
                    .submit(Priority::Foreground, move || this.resolve());
            }
            // Resolution is in flight, or it failed and will not be retried. Either way there is
            // nothing to plan against: a tile's URL comes from a manifest that has not arrived.
            Readiness::Resolving | Readiness::Failed(_) => {}
            Readiness::Ready => {
                let Some(sources) = inner.sources.clone() else {
                    return;
                };
                drop(inner);
                self.dispatch(&sources, view, coords);
            }
        }
    }

    /// Resolves the style's sources, on a worker.
    ///
    /// Runs the same [`boot::resolve_sources`] a cold start runs, and it is safe to call from
    /// inside a pool job: the batch it waits on runs work itself rather than blocking, which is
    /// what keeps a full pool from deadlocking on a job that waits for its own batch.
    fn resolve(self: Arc<Self>) {
        let outcome = boot::resolve_sources(
            &self.style_text,
            &self.files,
            self.pool,
            Priority::Foreground,
            Instant::now(),
        );
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match outcome {
            Ok(sources) => {
                inner.sources = Some(Arc::new(sources));
                inner.readiness = Readiness::Ready;
            }
            Err(error) => inner.readiness = Readiness::Failed(error.to_string()),
        }
    }

    /// Plans `coords` and submits what is missing.
    fn dispatch(
        self: &Arc<Self>,
        sources: &Arc<Sources>,
        view: &ViewTransform,
        coords: &[TileCoord],
    ) {
        let Ok(jobs) = boot::plan(
            &sources.sets,
            &sources.documents,
            &sources.clustered,
            view,
            coords,
            self.style_rev,
        ) else {
            return;
        };

        // The layers that draw from no source. Cheap enough — no fetch, no decode — to do on the
        // calling thread rather than to schedule, and a background must be there for the *first*
        // frame or the map is blank behind tiles that have not arrived.
        {
            let held = self.landed.read().unwrap_or_else(PoisonError::into_inner);
            let missing: Vec<TileId> = coords
                .iter()
                .map(|coord| TileId::new(coord.z, coord.x, coord.y))
                .filter(|id| !held.sourceless.contains_key(id))
                .collect();
            drop(held);
            if !missing.is_empty() {
                let mut held = self.landed.write().unwrap_or_else(PoisonError::into_inner);
                for id in missing {
                    if let Ok(buckets) = crate::tile::build_sourceless(&sources.style, id) {
                        held.sourceless.insert(id, Arc::new(buckets));
                    }
                }
            }
        }

        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        for job in jobs {
            {
                let held = self.landed.read().unwrap_or_else(PoisonError::into_inner);
                if held.by_tile.contains_key(&job.tile) {
                    continue;
                }
            }
            if !inner.inflight.insert(job.key.clone()) {
                continue;
            }
            let this = Arc::clone(self);
            let sources = Arc::clone(sources);
            self.pool.submit(Priority::Foreground, move || {
                let probe = boot::BuildProbe {
                    bytes: &DISCARDED,
                    fetched: &|| {},
                };
                let outcome =
                    boot::build_job(&job, &sources.style, &this.files, &this.cache, &probe);
                if let Ok(buckets) = outcome {
                    let mut held = this.landed.write().unwrap_or_else(PoisonError::into_inner);
                    held.by_tile
                        .entry(job.tile)
                        .and_modify(|existing| {
                            let mut merged = existing.as_ref().clone();
                            merged.extend(buckets.iter().cloned());
                            *existing = Arc::new(merged);
                        })
                        .or_insert(buckets);
                }
                // Removed whether it built or failed. A tile that failed is one the next tick may
                // legitimately ask for again -- a transient 503 is not a permanent absence, and
                // an absent tile is cached as empty by the layer below rather than retried here.
                this.inner
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .inflight
                    .remove(&job.key);
            });
        }
    }
}

impl<S: FileSource + 'static> Tiles for Arc<TileSource<S>> {
    fn buckets(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        self.landed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .by_tile
            .get(&tile)
            .map(Arc::clone)
    }

    fn sourceless(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        self.landed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .sourceless
            .get(&tile)
            .map(Arc::clone)
    }
}
