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
//! immediately. [`TileSource::want`] says what is wanted and schedules; `TileSource::buckets`
//! answers with what has landed, and a tile that has not arrived is simply absent — which is the
//! answer the frame loop already knows how to draw, by substituting the ancestor it has.
//!
//! That makes "silently blank" the hazard, which this project has caught three times. It is
//! answered by [`TileSource::readiness`] rather than by a stall: a consumer that sees an empty
//! map has something to read that says whether the style failed, and which.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError, RwLock};
use std::time::Instant;

use tessella_storage::source::{Coalescing, FetchError, FileSource};
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
    /// Cover coordinate to the data tile serving it, for the coordinates where they differ.
    ///
    /// Above a source's maxzoom a cover asks for a zoom the source does not have, and a coarser
    /// tile stands in: the cover wants z15 and the data is `overscaled(14, x>>1, y>>1, 15)`. The
    /// frame loop looks a tile up by the coordinate it covered, so without this the lookup misses
    /// and the map goes blank one zoom past whatever the source offers.
    ///
    /// A second index rather than a change of key, because the key is shared across sources -- a
    /// raster source covers at its own zoom, and re-keying merged buckets that belong apart.
    alias: BTreeMap<TileId, TileId>,
    sourceless: BTreeMap<TileId, Arc<Vec<LayerBucket>>>,
}

impl Landed {
    /// The tile serving `tile` and its buckets, whether it was built for that coordinate or is a
    /// coarser tile standing in above the source's maxzoom.
    ///
    /// The identity comes back with the buckets because the caller needs both and they are not
    /// the same thing: the geometry is in the *data* tile's local frame, so that is what places
    /// it, while the coordinate asked for is what wanted it drawn.
    fn lookup(&self, tile: TileId) -> Option<(TileId, Arc<Vec<LayerBucket>>)> {
        if let Some(buckets) = self.by_tile.get(&tile) {
            return Some((tile, Arc::clone(buckets)));
        }
        // Nothing at that coordinate, so ask what is standing in for it.
        self.alias
            .get(&tile)
            .and_then(|data| self.by_tile.get(data).map(|held| (*data, Arc::clone(held))))
    }
}

/// The glyphs a style's labels need, once something has asked for them.
///
/// One-shot, which is what the boot path did and for the same reason: which glyphs a style needs
/// is a property of the *data* rather than of the style -- `text-field` evaluated against each
/// tile's own features -- so nothing can be asked for until tiles have landed. A tile arriving
/// later may name a codepoint no tile so far has used (panning into Athens from Rome), and until
/// that is handled a label outside this set draws nothing rather than drawing wrongly:
/// `Content::is_encodable` withholds a symbol bucket whose glyphs have not arrived, so it stays
/// fresh for the frame that can draw it.
#[derive(Default)]
struct Glyphs {
    /// What has been asked for, so ten ticks over the same labels schedule one fetch.
    ///
    /// The set rather than a flag. A flag said "a fetch has happened" and nothing more, and
    /// because tiles land over several ticks the labels that existed at that moment were not the
    /// labels the map ends up with: everything that arrived afterwards wanted glyphs that were
    /// never asked for, and drew without them, permanently. It also made the frame a function of
    /// arrival timing -- the render probe's icon scene drew between 145 and 298 glyph quads over
    /// twelve identical runs.
    asked: tessella_glyph::fonts::Dependencies,
    /// The landed generation the dependency walk last ran at.
    ///
    /// The walk is over every bucket of every landed tile, calling `dependencies()` on each --
    /// which clones a font stack per layer and iterates every feature's text. It answered the
    /// same thing on every tick of a settled map, and it ran before the subset test that decided
    /// it had been unnecessary. Nothing it reads changes without the generation changing, so
    /// this is what makes it run when a tile lands rather than when a frame is drawn: a fifth of
    /// the producer's time on a moving quad.
    surveyed: Option<u64>,
    /// Whether that fetch is still running.
    ///
    /// Distinct from `scheduled` and from `ready`, and it has to be: `ready` is a hand-off that
    /// [`TileSource::take_fonts`] empties, so "scheduled and nothing ready" is true both while the
    /// fetch is in flight and forever after the map has taken its fonts. Cleared whether the
    /// fetch succeeded or not, because a failure is also finished.
    running: bool,
    /// Waiting to be taken by a tick. `Fonts` is not `Clone` and [`crate::map::Map`] owns the one
    /// it draws from, so this is a hand-off rather than a copy.
    ready: Option<tessella_glyph::fonts::Fonts>,
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
    glyphs: Mutex<Glyphs>,
    /// Tiles whose build failed, and the first reason one did.
    ///
    /// A tile that fails is not a failed *source* -- a 503 on one tile of a cover is a hole, and
    /// the map draws the ancestor in its place -- so it must not turn `readiness` to `Failed`.
    /// But dropping it silently leaves the one state nothing can explain: resolved, ready, and
    /// blank. Counted and named here so that state has an answer, for the same reason
    /// [`Readiness::Failed`] carries its reason rather than only its fact.
    failures: Mutex<(u64, Option<String>)>,
    /// Bumped whenever something lands.
    ///
    /// A map draws when its damage gate says something changed, and a tile arriving on a worker
    /// is a change nothing else would tell it about: the camera has not moved, so the gate would
    /// return idle for ever and the tiles would sit here, built and undrawn. One atomic read per
    /// tick is what turns "a tile landed" back into "the frame is worth drawing".
    generation: AtomicU64,
}

/// A [`FileSource`] over the coalescing store.
///
/// `Coalescing` answers with an `Arc<Response>`, because a response joined by several waiters is
/// one response shared rather than one each; the trait predates that and wants the value. Glyphs
/// go through it rather than around it because two views wanting the same range is exactly the
/// case coalescing exists for, and a second file source beside it would fetch the range twice and
/// cache it in neither.
struct Coalesced<'a, S>(&'a Arc<Coalescing<S>>);

impl<S: FileSource> FileSource for Coalesced<'_, S> {
    fn fetch(&self, url: &str) -> Result<tessella_storage::source::Response, FetchError> {
        self.0.fetch(url).map(|response| (*response).clone())
    }
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
            glyphs: Mutex::new(Glyphs::default()),
            generation: AtomicU64::new(0),
            failures: Mutex::new((0, None)),
        })
    }

    /// Layers the style asked for that this build cannot draw, and why the first one was refused.
    ///
    /// Not a failure: a document naming one thing this build does not have still draws every
    /// layer that does, which is what `reject_uncompilable` is for and what mbgl's parser does
    /// too. It becomes the explanation only in the case nothing else covers -- a style that
    /// resolved, is ready, has no failing tiles, and draws nothing, because every layer that
    /// would have drawn was refused before a tile was ever asked for.
    ///
    /// Empty until the style resolves, since that is when the layers are compiled.
    pub fn rejected(&self) -> (u64, Option<String>) {
        let held = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        held.sources.as_ref().map_or((0, None), |sources| {
            (
                sources.rejected_layers.len() as u64,
                sources
                    .rejected_layers
                    .first()
                    .map(|layer| alloc::format!("`{}`: {}", layer.id, layer.reason)),
            )
        })
    }

    /// How many tile builds failed, and why the first one did.
    ///
    /// The answer to a map that is ready and empty. Zero failures with no geometry means the
    /// cover asked for nothing; failures means it asked and did not get it.
    pub fn failures(&self) -> (u64, Option<String>) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How much has landed, as a number that changes when it does.
    ///
    /// Not a count of anything -- only the change matters. A caller compares it with what it saw
    /// last tick and redraws when the two differ.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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
    pub fn want(
        self: &Arc<Self>,
        view: &ViewTransform,
        coords: &[TileCoord],
        speculative: &[TileCoord],
    ) {
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
                self.dispatch(&sources, view, coords, speculative);
                self.want_glyphs(&sources);
            }
        }
    }

    /// How much work is still in flight, which is what "not finished yet" means.
    ///
    /// Tiles submitted and not yet landed, plus the one glyph fetch if it has been scheduled and
    /// has not produced its fonts. Those are the two things that arrive after a tick rather than
    /// during it; the sprite sheet is not among them, because it rides along with resolution.
    ///
    /// Zero does not promise the map is complete -- a tile that failed is finished and still a
    /// hole, which is why [`Self::failures`] is counted separately. It promises only that nothing
    /// further is coming without another tick, and that is the question a caller waiting for a
    /// settled frame is actually asking.
    ///
    /// Written for the render probe, which could not tell "done" from "blocked" and so measured
    /// frames that were still filling in: the same scene gave 0, 272 and 9,520 differing pixels
    /// across runs that all believed they had settled. A consumer wanting a progress indicator
    /// reads the same number.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        let tiles = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .inflight
            .len();
        let glyphs = usize::from(
            self.glyphs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running,
        );
        tiles + glyphs
    }

    /// What the style resolved to, once it has.
    ///
    /// The sprite sheet rides along here rather than through an accessor of its own: it is part of
    /// what resolution produced, and a consumer that wants it wants the style beside it anyway.
    pub fn sources(&self) -> Option<Arc<Sources>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sources
            .clone()
    }

    /// Takes the glyphs, if a fetch has finished and nothing has taken them yet.
    ///
    /// A hand-off rather than a copy, because `Fonts` is not `Clone` and the map owns the one it
    /// draws from. Answers `None` on every tick but the one after the fetch lands.
    pub fn take_fonts(&self) -> Option<tessella_glyph::fonts::Fonts> {
        self.glyphs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ready
            .take()
    }

    /// Schedules the one glyph fetch, if the tiles that have landed need one.
    ///
    /// Called from [`Self::want`] rather than from a tile's completion because it needs tiles to
    /// have landed *and* a caller to be asking: a source nobody is drawing from should not fetch
    /// fonts for labels nobody will see.
    fn want_glyphs(self: &Arc<Self>, sources: &Arc<Sources>) {
        let Some(url) = sources.style.glyphs.clone() else {
            return;
        };
        let generation = self.generation.load(Ordering::Acquire);
        // Whether anything has been asked for yet, which is what decides whether the drain gate
        // below applies.
        let first;
        {
            let held = self.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
            if held.running || held.surveyed == Some(generation) {
                return;
            }
            first = held.asked.is_empty();
        }

        // Not until the tiles have stopped arriving.
        //
        // This fetch happens once, and it asks for the codepoints the tiles that have landed *so
        // far* need. Firing it on the first tile meant asking for that tile's alphabet and no
        // other: on a cold start the first tile to land is the prefetched ancestor, which the
        // frame does not even draw, and measured on liberty at z16 it wanted 90 codepoints where
        // the two tiles the frame is made of wanted 451 and 476. Every letter outside that ninety
        // was then missing from every label, which is a map captioned in alphabet soup -- and
        // which of the ninety you got depended on which tile won the race, so no two runs agreed.
        //
        // Waiting for the queue to drain is what makes one fetch enough. It is the weakest
        // condition that works: not "the cover is complete", which a single failing tile would
        // block forever, but "nothing further is coming", which a failure satisfies as surely as
        // a success.
        //
        // The *first* fetch only. A map that keeps moving never has an empty queue -- a zoom
        // sweep has tiles in flight from the moment it starts until the moment it stops -- so
        // gating every fetch on the drain meant the alphabet was decided once, at the start, and
        // every script that arrived afterwards drew with whatever was in the atlas. Which is the
        // same alphabet soup this gate was written to prevent, from the other end: too early on
        // a cold start, never again on a moving one.
        //
        // After the first, the subset test below is the gate. It asks only for what is missing,
        // so a fetch mid-flight costs a request for the codepoints a new script brought and
        // nothing for the ones already held.
        if first {
            let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if !inner.inflight.is_empty() {
                return;
            }
        }

        let mut wanted: tessella_glyph::fonts::Dependencies = BTreeMap::new();
        {
            let held = self.landed.read().unwrap_or_else(PoisonError::into_inner);
            for buckets in held.by_tile.values() {
                for bucket in buckets.iter() {
                    if let crate::tile::Content::Symbol(layout) = &bucket.content {
                        for (stack, codepoints) in layout.dependencies() {
                            wanted.entry(stack).or_default().extend(codepoints);
                        }
                    }
                }
            }
        }
        {
            let mut held = self.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
            // Re-checked under the lock: two views ticking together both got past the first look.
            if held.running {
                return;
            }
            // Recorded before the tests below, not after: the walk answered for this generation
            // whatever it found, so a repeat of it would find the same. Recorded under the lock
            // that guards `asked`, so the pair cannot disagree.
            held.surveyed = Some(generation);
            if wanted.is_empty() {
                return;
            }
            // Nothing new, so nothing to do. `wanted` is what the landed tiles need in total, not
            // what is missing, so this is a subset test rather than a difference.
            if wanted.iter().all(|(stack, codepoints)| {
                held.asked
                    .get(stack)
                    .is_some_and(|had| codepoints.is_subset(had))
            }) {
                return;
            }
            for (stack, codepoints) in &wanted {
                held.asked
                    .entry(stack.clone())
                    .or_default()
                    .extend(codepoints.iter().copied());
            }
            // Everything asked for so far, not just what this round added. A fetch builds a
            // *new* `Fonts` with a new atlas and hands it to the map whole, replacing the one
            // before it -- so a fetch of the difference produces a map that can set the newest
            // script and has forgotten the rest. That is the alphabet soup this whole path
            // exists to avoid, arriving by the other door: not "asked too early" but "asked for
            // too little, and threw away the answer to the last question".
            //
            // The ranges already held cost a request each and no bytes worth mentioning; the
            // file source caches them and the loop below skips a range it holds.
            wanted = held.asked.clone();
            held.running = true;
        }

        let this = Arc::clone(self);
        self.pool.submit(Priority::Foreground, move || {
            let mut fonts = tessella_glyph::fonts::Fonts::new(url);
            // A glyph range that will not load costs the labels that need it and not the map, so
            // a failure here leaves `ready` empty rather than failing the source.
            let fetched = fonts.fetch(&wanted, &Coalesced(&this.files)).is_ok();
            {
                let mut held = this.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
                if fetched {
                    held.ready = Some(fonts);
                }
                held.running = false;
            }
            if fetched {
                this.generation.fetch_add(1, Ordering::AcqRel);
            }
        });
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
        speculative: &[TileCoord],
    ) {
        let Ok(jobs) = boot::plan(
            &sources.sets,
            &sources.documents,
            &sources.clustered,
            view,
            coords,
            self.style_rev,
        ) else {
            // Planning is arithmetic and fails only when a cover cannot be computed -- which a
            // raster source can do on its own, at its own zoom, without anything being wrong with
            // the vector one beside it. Dropping it silently loses *every* tile of the frame for
            // one source's arithmetic, and leaves a map that is ready, has no failing tiles and
            // draws nothing. Counted with the others so it has to explain itself.
            let mut held = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            held.0 += 1;
            if held.1.is_none() {
                held.1 = Some(String::from("planning the cover failed"));
            }
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
                        self.generation.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
        }

        // Chosen under the locks, submitted outside them. Holding `inner` across the loop meant
        // holding it across `pool.submit`, while every worker finishing a build needs that same
        // lock to clear its tile from `inflight` -- so a frame that dispatched thirty-one jobs
        // spent its time contending with the workers it had just started, and the map that was
        // meant to draw them never redrew. The two locks are taken once each, briefly, and
        // nothing is submitted while either is held.
        // Recorded before the filter, because an alias is arithmetic rather than a result: it is
        // known the moment the job is planned, and it holds for every cover the data tile serves.
        // Recording it where the tile lands instead loses all but the first -- one z14 tile stands
        // in for sixteen z16 coordinates, the dedup below keeps one job for it, and the other
        // fifteen coordinates never got an entry. That drew one tile of the cover and left the
        // rest of the frame black.
        if jobs.iter().any(|job| job.cover != job.tile) {
            let mut held = self.landed.write().unwrap_or_else(PoisonError::into_inner);
            for job in &jobs {
                if job.cover != job.tile {
                    held.alias.insert(job.cover, job.tile);
                }
            }
        }

        let ready: Vec<boot::Job> = {
            let landed = self.landed.read().unwrap_or_else(PoisonError::into_inner);
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            jobs.into_iter()
                .filter(|job| {
                    !landed.by_tile.contains_key(&job.tile)
                        && inner.inflight.insert(job.key.clone())
                })
                .collect()
        };

        // What the frame is drawing outranks what it might draw. A cover coordinate is urgent
        // even when a speculative copy of the same tile is also wanted -- two world copies share
        // a `TileId` and only one of them may be on screen -- so this is built from the tiles
        // that are *not* speculation rather than from the ones that are.
        let urgent: BTreeSet<TileId> = coords
            .iter()
            .filter(|tile| !speculative.contains(tile))
            .map(|tile| TileId::new(tile.z, tile.x, tile.y))
            .collect();

        for job in ready {
            // `Prefetch` is documented as correct to starve, which is exactly the rank a level
            // the camera is heading for should have: without it a fast zoom fills the pool with
            // the levels it has already left, and the one it arrives at waits behind them.
            let priority = if urgent.contains(&job.cover) {
                Priority::Foreground
            } else {
                Priority::Prefetch
            };
            let this = Arc::clone(self);
            let sources = Arc::clone(sources);
            self.pool.submit(priority, move || {
                let probe = boot::BuildProbe {
                    bytes: &DISCARDED,
                    fetched: &|| {},
                };
                let outcome =
                    boot::build_job(&job, &sources.style, &this.files, &this.cache, &probe);
                if let Err(ref error) = outcome {
                    let mut held = this.failures.lock().unwrap_or_else(PoisonError::into_inner);
                    held.0 += 1;
                    if held.1.is_none() {
                        held.1 = Some(alloc::format!("{error}"));
                    }
                }
                if let Ok(buckets) = outcome {
                    let mut held = this.landed.write().unwrap_or_else(PoisonError::into_inner);
                    // Keyed by the data tile, which is the thing that was built. What the cover
                    // asked for reaches it through `alias`, so one tile serving many coordinates
                    // is stored and decoded once.
                    held.by_tile
                        .entry(job.tile)
                        .and_modify(|existing| {
                            let mut merged = existing.as_ref().clone();
                            merged.extend(buckets.iter().cloned());
                            *existing = Arc::new(merged);
                        })
                        .or_insert(buckets);
                    this.generation.fetch_add(1, Ordering::AcqRel);
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
            .lookup(tile)
            .map(|(_, buckets)| buckets)
    }

    /// Above a source's maxzoom the serving tile is not the one asked for, and the difference is
    /// not cosmetic: the buckets hold tile-local coordinates over the *data* tile's ground, so
    /// placing them by the cover's coordinate draws a z14 tile's contents into a z16 tile's box
    /// -- a sixteenth of the size, which puts sixteen times too much world on screen.
    fn serving(&self, cover: TileId) -> Option<(TileId, Arc<Vec<LayerBucket>>)> {
        self.landed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .lookup(cover)
    }

    fn sourceless(&self, tile: TileId) -> Option<Arc<Vec<LayerBucket>>> {
        self.landed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .sourceless
            .get(&tile)
            .map(Arc::clone)
    }

    /// The covering zoom of every raster source, which is the one the planner fetched them at.
    ///
    /// `plan` keeps a raster source's own cover for exactly this reason and then the frame walked
    /// only the view's, so the tiles landed under coordinates nothing asked about.
    fn extra_zooms(&self, view: &ViewTransform) -> Vec<u8> {
        let Some(sources) = self.sources() else {
            return Vec::new();
        };
        let mut zooms: Vec<u8> = sources
            .sets
            .iter()
            .filter(|(_, _, kind)| {
                matches!(kind, tessella_storage::offline::SourceKind::Raster { .. })
            })
            .map(|(_, _, kind)| boot::covering_zoom(*kind, view.zoom))
            .collect();
        zooms.sort_unstable();
        zooms.dedup();
        zooms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Above a source's maxzoom the frame loop asks for a coordinate the source cannot serve, and
    /// a coarser tile stands in. Without the alias every such lookup misses and the map goes blank
    /// one zoom past whatever the source offers -- which is what it did.
    #[test]
    fn a_cover_past_the_maxzoom_finds_the_tile_standing_in_for_it() {
        let data = TileId::overscaled(14, 8802, 5373, 16);
        let mut landed = Landed::default();
        landed.by_tile.insert(data, Arc::new(Vec::new()));

        // The sixteen z16 coordinates this one z14 tile serves all resolve to it, not just the
        // first: the dedup keeps one job for the tile, so recording aliases as tiles land left
        // fifteen of every sixteen coordinates empty and drew a mostly black frame.
        for x in 0..4 {
            for y in 0..4 {
                let cover = TileId::new(16, 8802 * 4 + x, 5373 * 4 + y);
                assert!(landed.lookup(cover).is_none(), "no alias yet");
                landed.alias.insert(cover, data);
                assert_eq!(
                    landed.lookup(cover).map(|(id, _)| id),
                    Some(data),
                    "{cover:?} should be served by {data:?}"
                );
            }
        }
    }

    /// Within a source's range the two coordinates are the same tile, and the alias must not be
    /// consulted at all -- a direct hit is the common case and stays one lookup.
    #[test]
    fn a_cover_the_source_serves_is_found_directly() {
        let tile = TileId::new(14, 8802, 5373);
        let mut landed = Landed::default();
        landed.by_tile.insert(tile, Arc::new(Vec::new()));
        assert_eq!(landed.lookup(tile).map(|(id, _)| id), Some(tile));
        assert!(landed.alias.is_empty());
    }
}
