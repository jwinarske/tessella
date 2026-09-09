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
use web_time::Instant;

use tessella_glyph::manager::FontStack;
use tessella_glyph::pbf::Range;
use tessella_storage::deferred::Ticket;
use tessella_storage::source::{Coalescing, FetchError, FileSource, Response};
use tessella_tile::cover::{TileCoord, ViewTransform};
use tessella_tile::store::TileKey;

use crate::boot::{self, BootError, Sources};
use crate::cache::TileCache;
use crate::deferred::{PoolBacked, TileTransport};
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
    /// The ranges asked for and not yet answered, while a fetch is in flight.
    ///
    /// Glyphs go through the same transport tiles do, so the same thing is true of them: the
    /// request returns without bytes and something has to come back for the answer. That is
    /// [`TileSource::drain`], which is why this is state rather than a local in a pool job.
    fetching: Option<GlyphFetch>,
}

/// One glyph fetch, part-way through.
///
/// The ranges are asked for together and accepted together. Not one at a time as they land,
/// because packing is over a whole stack: an atlas packed from a half-loaded font has shelf slots
/// for the glyphs it had and nowhere to put the rest.
struct GlyphFetch {
    /// What the ranges were asked for, which `packed` needs again at the end.
    wanted: tessella_glyph::fonts::Dependencies,
    /// The glyph URL template, so the `Fonts` can be built when the bytes are in.
    url: String,
    /// Asked for and not answered.
    outstanding: BTreeMap<Ticket, (FontStack, Range)>,
    /// Answered and not yet accepted.
    landed: Vec<(FontStack, Range, Arc<Response>)>,
    /// A range that would not load. The whole fetch is abandoned, as it was when one `?` in
    /// `Fonts::fetch` ended it -- a partial alphabet is the thing this path exists to avoid.
    failed: bool,
}

/// What a source is doing, behind one lock.
struct Inner {
    readiness: Readiness,
    sources: Option<Arc<Sources>>,
    /// Tiles submitted and not yet landed, so a camera that keeps moving over the same gap asks
    /// for it once rather than once per tick.
    inflight: BTreeSet<TileKey>,
    /// The style's manifests, while they are in flight.
    ///
    /// Resolution goes through the same transport tiles and glyphs do, so the same thing is true
    /// of it: the request returns without bytes and [`TileSource::drain`] is what comes back for
    /// the answer.
    resolving: Option<Resolving>,
}

/// A style part-way through resolving.
struct Resolving {
    /// What the document said, and what it needs fetched.
    plan: boot::ResolvePlan,
    /// Asked for and not answered, by the index its answer belongs at.
    outstanding: BTreeMap<Ticket, usize>,
    /// Answers so far, in ask order.
    answers: Vec<Option<boot::Answer>>,
    /// When resolution started, which the timings in `Sources` are measured against.
    started: Instant,
}

/// Clears a tile from the in-flight set on the way out of its build, however that happens.
struct Clearing<D: TileTransport + 'static> {
    source: Arc<TileSource<D>>,
    key: TileKey,
}

impl<D: TileTransport + 'static> Drop for Clearing<D> {
    fn drop(&mut self) {
        self.source.finish(&self.key);
    }
}

/// A tile whose bytes were asked for and have not landed.
///
/// The job outlives the request because the request only produces bytes: what to do with them --
/// which style, which source, which tile, and how urgent -- was decided when the cover was
/// planned, and re-deriving it on arrival would mean planning twice and risking two answers.
struct InFlight {
    job: boot::Job,
    sources: Arc<Sources>,
    priority: Priority,
}

/// The tiles a warm map draws from.
///
/// Process-scoped and shared: one of these serves every view, so a tile wanted by two of them is
/// fetched once, built once, and held once.
pub struct TileSource<D> {
    style_text: String,
    /// Everything this source fetches: manifests, the sprite, glyph ranges and tiles.
    ///
    /// One transport rather than a blocking source beside it. That is the whole of what the wasm
    /// work bought -- while style resolution and glyphs were still blocking, this type had to
    /// carry a `FileSource` as well, and a browser could not supply one.
    deferred: Arc<D>,
    /// Tiles whose bytes have been asked for and have not arrived.
    ///
    /// A separate lock from `inner` because [`TileSource::drain`] walks it on the tick thread
    /// while workers are clearing `inflight`, and the two must not queue behind each other.
    pending: Mutex<BTreeMap<Ticket, InFlight>>,
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
/// Public because it names part of [`TileSource`]'s default transport, which a caller has to be
/// able to spell.
///
/// `Coalescing` answers with an `Arc<Response>`, because a response joined by several waiters is
/// one response shared rather than one each; the trait predates that and wants the value. Glyphs
/// go through it rather than around it because two views wanting the same range is exactly the
/// case coalescing exists for, and a second file source beside it would fetch the range twice and
/// cache it in neither.
pub struct Coalesced<S>(pub Arc<Coalescing<S>>);

impl<S: FileSource> FileSource for Coalesced<S> {
    fn fetch(&self, url: &str) -> Result<Response, FetchError> {
        self.0.fetch(url).map(|response| (*response).clone())
    }
}

/// Where the warm path's byte counter goes.
///
/// A tile built off a `wanted()` list is not part of anyone's cold-start timing, so the count has
/// no reader. One static rather than one per source: nothing reads it, and giving each source its
/// own would only make that more convincing than it deserves to be.
static DISCARDED: AtomicUsize = AtomicUsize::new(0);

/// A source over the process pool and the coalescing store, which is the native arrangement.
pub type Pooled<S> = TileSource<PoolBacked<Coalesced<S>>>;

impl<S: FileSource + 'static> Pooled<S> {
    /// Holds a style and the process-scoped things a build needs. Fetches nothing.
    ///
    /// Nothing happens until the first [`Self::want`], which is what makes creating a map cheap:
    /// §16's `create` parses the style to reject a bad one and then hands back a handle, and the
    /// first tick is what starts the network.
    ///
    /// The transport is the pool-backed one, which is the whole of the native answer: a blocking
    /// source, waited on where waiting is cheap. A caller with a transport of its own --- a
    /// browser, or a test that wants to decide when bytes arrive --- uses
    /// [`Self::with_transport`](TileSource::with_transport) instead.
    pub fn new(
        style_text: String,
        files: Arc<Coalescing<S>>,
        cache: Arc<TileCache<BootError>>,
        pool: &'static Pool,
        style_rev: u64,
    ) -> Arc<Self> {
        // Background rather than foreground by default: `dispatch` names a class per request,
        // and the one it never names is this. A request that reached here without a class would
        // be a tile nobody said was urgent, so it should not outrank one that is.
        let deferred = Arc::new(PoolBacked::new(
            Arc::new(Coalesced(Arc::clone(&files))),
            pool,
            Priority::Background,
        ));
        TileSource::with_transport(style_text, deferred, cache, pool, style_rev)
    }
}

impl<D: TileTransport + 'static> TileSource<D> {
    /// As [`new`](TileSource::new), with the tile transport supplied.
    ///
    /// The seam DR-23 exists for. Tiles reach this source through whatever answers
    /// [`TileTransport`], so a browser's `fetch` and a pool-backed blocking source are the same
    /// shape to everything above -- and a test can be a third, deciding exactly when a tile's
    /// bytes land without a network to arrange it.
    ///
    /// Everything the source fetches goes through it: manifests, the sprite, glyph ranges and
    /// tiles. There is no blocking source beside it any more, which is what makes a browser's
    /// `fetch` sufficient rather than merely present.
    pub fn with_transport(
        style_text: String,
        deferred: Arc<D>,
        cache: Arc<TileCache<BootError>>,
        pool: &'static Pool,
        style_rev: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            style_text,
            deferred,
            pending: Mutex::new(BTreeMap::new()),
            cache,
            pool,
            style_rev,
            inner: Mutex::new(Inner {
                readiness: Readiness::Idle,
                sources: None,
                inflight: BTreeSet::new(),
                resolving: None,
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
    /// Style resolution while it is in flight, tiles submitted and not yet landed, and the one
    /// glyph fetch if it has been scheduled and has not produced its fonts. Those are the things
    /// that arrive after a tick rather than during it; the sprite sheet is not among them,
    /// because it rides along with resolution and is therefore already counted by it.
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
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        // Resolution counts. A tile's URL comes from a manifest, so while this is `Resolving`
        // nothing has been dispatched and `inflight` is empty -- and the number a caller reads
        // to mean "nothing further is coming" was zero for the whole of the round trip that
        // fetches the style's sources. A settle loop stops there, on a frame with no tiles in
        // it, and whatever lands afterwards arrives in some later frame or not at all.
        //
        // `Idle` is zero on purpose: nothing has been asked for yet. `Failed` is zero for the
        // reason a failed tile is -- finished, and still a hole, which is `failures`' half.
        let resolving = usize::from(matches!(inner.readiness, Readiness::Resolving));
        let tiles = inner.inflight.len();
        drop(inner);
        let glyphs = usize::from(
            self.glyphs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running,
        );
        resolving + tiles + glyphs
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

        // What to ask for is computed against an empty store, which is what `wanted` above already
        // accounts for: a fetch builds a *new* `Fonts` and hands it over whole, so every range the
        // dependencies need is asked for, not just the ones the last fetch did not have.
        let asks = tessella_glyph::fonts::Fonts::new(url.clone()).wanted(&wanted);
        if asks.is_empty() {
            // Nothing to ask the origin for -- every codepoint is above the BMP and rasterised
            // locally. There is still an atlas to build, so this goes straight to the accepting
            // half rather than clearing `running` and forgetting about it.
            self.finish_glyphs(GlyphFetch {
                wanted,
                url,
                outstanding: BTreeMap::new(),
                landed: Vec::new(),
                failed: false,
            });
            return;
        }

        // Issued outside the `glyphs` lock, for the reason `dispatch` issues outside `pending`:
        // `request_at` takes the ticket table, and `drain` takes this lock before it polls.
        let mut outstanding = BTreeMap::new();
        for (stack, range, ask) in asks {
            // Foreground, because a label with no glyphs is a hole in a frame that is otherwise
            // finished -- the same rank the fetch had when it was one blocking job.
            let ticket = self.deferred.request_at(Priority::Foreground, &ask, None);
            outstanding.insert(ticket, (stack, range));
        }
        self.glyphs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fetching = Some(GlyphFetch {
            wanted,
            url,
            outstanding,
            landed: Vec::new(),
            failed: false,
        });
    }

    /// Collects whatever the transport has answered for the glyph ranges in flight.
    ///
    /// Called from [`Self::drain`], beside the tiles, because it is the same question asked of the
    /// same transport.
    fn drain_glyphs(self: &Arc<Self>) {
        let outstanding: Vec<Ticket> = {
            let held = self.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
            match &held.fetching {
                Some(fetch) => fetch.outstanding.keys().copied().collect(),
                None => return,
            }
        };

        let mut answers = Vec::new();
        for ticket in outstanding {
            if let Some(outcome) = self.deferred.poll(ticket) {
                answers.push((ticket, outcome));
            }
        }
        if answers.is_empty() {
            return;
        }

        let ready = {
            let mut held = self.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(fetch) = held.fetching.as_mut() else {
                return;
            };
            for (ticket, outcome) in answers {
                let Some((stack, range)) = fetch.outstanding.remove(&ticket) else {
                    continue;
                };
                match outcome {
                    Ok(response) => fetch.landed.push((stack, range, response)),
                    // A glyph range that will not load costs the labels that need it and not the
                    // map, so this abandons the fetch rather than failing the source.
                    Err(_) => fetch.failed = true,
                }
            }
            if fetch.outstanding.is_empty() {
                held.fetching.take()
            } else {
                None
            }
        };

        if let Some(fetch) = ready {
            self.finish_glyphs(fetch);
        }
    }

    /// Builds the atlas from ranges that have all arrived.
    ///
    /// On a worker rather than on the draining thread: parsing a range and packing an atlas is
    /// real work, and it was on a worker before the fetch was split out of it. A pool with no
    /// workers runs it when it is drained, which is the same thread either way and says so.
    fn finish_glyphs(self: &Arc<Self>, fetch: GlyphFetch) {
        let this = Arc::clone(self);
        self.pool.submit(Priority::Foreground, move || {
            let mut fonts = tessella_glyph::fonts::Fonts::new(fetch.url);
            let mut ok = !fetch.failed;
            for (stack, range, response) in &fetch.landed {
                if fonts.accept(stack, *range, response).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                fonts.packed(&fetch.wanted);
            }
            {
                let mut held = this.glyphs.lock().unwrap_or_else(PoisonError::into_inner);
                if ok {
                    held.ready = Some(fonts);
                }
                held.running = false;
            }
            if ok {
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
        let started = Instant::now();
        let plan = match boot::plan_resolution(&self.style_text, started) {
            Ok(plan) => plan,
            // A style that will not parse, or a source that names nowhere to fetch from. Neither
            // needs a request to find out, which is the point of planning first.
            Err(error) => {
                self.inner
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .readiness = Readiness::Failed(error.to_string());
                return;
            }
        };

        // Issued before the state is stored, and outside `inner`, for the reason `dispatch`
        // issues outside `pending`: `request_at` takes the ticket table, and `drain` takes these
        // locks before it polls.
        let mut outstanding = BTreeMap::new();
        for (index, ask) in plan.asks.iter().enumerate() {
            // Foreground, because nothing else can start until these answer: a tile's URL comes
            // from a manifest, so every request the map will ever make is behind these.
            let ticket = self
                .deferred
                .request_at(Priority::Foreground, ask.url(), None);
            outstanding.insert(ticket, index);
        }

        let answers = alloc::vec![None; plan.asks.len()];
        let waiting = outstanding.is_empty();
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolving = Some(Resolving {
            plan,
            outstanding,
            answers,
            started,
        });
        // A style whose sources are all inline asks for nothing at all, so nothing will ever
        // arrive to drive the assembly. It happens on the fixtures this suite is built from and
        // on any style with `tiles` listed inline, which is most of them.
        if waiting {
            self.finish_resolving();
        }
    }

    /// Collects whatever the transport has answered for the manifests in flight.
    fn drain_resolution(self: &Arc<Self>) {
        let outstanding: Vec<Ticket> = {
            let held = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            match &held.resolving {
                Some(resolving) => resolving.outstanding.keys().copied().collect(),
                None => return,
            }
        };

        let mut answers = Vec::new();
        for ticket in outstanding {
            if let Some(outcome) = self.deferred.poll(ticket) {
                answers.push((ticket, outcome));
            }
        }
        if answers.is_empty() {
            return;
        }

        let done = {
            let mut held = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(resolving) = held.resolving.as_mut() else {
                return;
            };
            for (ticket, outcome) in answers {
                let Some(index) = resolving.outstanding.remove(&ticket) else {
                    continue;
                };
                resolving.answers[index] = Some(match outcome {
                    Ok(response) => Ok((*response).clone()),
                    Err(error) => Err(error.to_string()),
                });
            }
            resolving.outstanding.is_empty()
        };

        if done {
            self.finish_resolving();
        }
    }

    /// Builds the resolved sources from answers that have all arrived.
    ///
    /// On a worker rather than on the draining thread: assembling means parsing every manifest and
    /// cutting up a sprite sheet, which is real work and was on a worker before the fetches were
    /// split out of it.
    fn finish_resolving(self: &Arc<Self>) {
        let Some(resolving) = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolving
            .take()
        else {
            return;
        };
        let this = Arc::clone(self);
        self.pool.submit(Priority::Foreground, move || {
            let Resolving {
                plan,
                answers,
                started,
                ..
            } = resolving;
            let outcome = boot::assemble(plan, &answers, started);
            let mut inner = this.inner.lock().unwrap_or_else(PoisonError::into_inner);
            match outcome {
                Ok(sources) => {
                    inner.sources = Some(Arc::new(sources));
                    inner.readiness = Readiness::Ready;
                }
                Err(error) => inner.readiness = Readiness::Failed(error.to_string()),
            }
            drop(inner);
            // A resolved style is a change nothing else would report: no tile has landed and the
            // camera has not moved, so a damage gate reading only those would stay idle holding a
            // map that is finally able to draw.
            this.generation.fetch_add(1, Ordering::AcqRel);
        });
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

            // A tile another view already built costs no fetch at all. The build path checks the
            // cache too -- it has to, since a tile can land between here and there -- but only
            // this check happens *before* the network, and the round trip it saves is the whole
            // reason a second view over the same cover is cheap.
            if let Some(buckets) = self.cache.peek(&job.key) {
                self.land(&job, buckets);
                self.finish(&job.key);
                continue;
            }

            match boot::fetch_url(&job) {
                // Bytes first. `request` returns while the fetch is still running, so a tick
                // that wants thirty tiles issues thirty requests and gets on with drawing the
                // ones it has.
                Some(url) => {
                    let ticket = self.deferred.request_at(priority, url, None);
                    // `pending` is taken *after* `request_at` has returned, never across it.
                    // `request_at` takes the ticket table's lock and `drain` takes `pending`
                    // before the ticket table's -- so nesting them the other way here would be
                    // the two halves of a deadlock.
                    self.pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(
                            ticket,
                            InFlight {
                                job,
                                sources: Arc::clone(sources),
                                priority,
                            },
                        );
                }
                // Nothing to fetch: the document arrived during source resolution. Straight to
                // the pool, which is where it went before the split as well.
                None => self.build(
                    InFlight {
                        job,
                        sources: Arc::clone(sources),
                        priority,
                    },
                    None,
                ),
            }
        }
    }

    /// Runs one tile's build on a worker, from bytes that have already arrived.
    fn build(self: &Arc<Self>, work: InFlight, fetched: Option<Arc<Response>>) {
        let this = Arc::clone(self);
        self.pool.submit(work.priority, move || {
            // Held for the whole job, so the tile leaves the in-flight set whether the build
            // returned, failed, or unwound. Clearing it on the last line instead is the version
            // that was here before, and it leaks the key on a panic: the pool counts the panic
            // and carries on, the tile stays "in flight" for ever, and every later tick filters
            // it out of the cover as already asked for. One malformed tile, and that coordinate
            // is blank until the process restarts.
            let _clearing = Clearing {
                source: Arc::clone(&this),
                key: work.job.key.clone(),
            };
            let probe = boot::BuildProbe {
                bytes: &DISCARDED,
                fetched: &|| {},
            };
            let outcome = boot::build_fetched(
                &work.job,
                &work.sources.style,
                &this.cache,
                fetched.as_deref(),
                &probe,
            );
            match outcome {
                Ok(buckets) => this.land(&work.job, buckets),
                Err(ref error) => this.fail(&alloc::format!("{error}")),
            }
        });
    }

    /// Files a tile's buckets under the tile that was built, and says the frame is worth redrawing.
    fn land(&self, job: &boot::Job, buckets: Arc<Vec<LayerBucket>>) {
        let mut held = self.landed.write().unwrap_or_else(PoisonError::into_inner);
        // Keyed by the data tile, which is the thing that was built. What the cover asked for
        // reaches it through `alias`, so one tile serving many coordinates is stored and decoded
        // once.
        held.by_tile
            .entry(job.tile)
            .and_modify(|existing| {
                let mut merged = existing.as_ref().clone();
                merged.extend(buckets.iter().cloned());
                *existing = Arc::new(merged);
            })
            .or_insert(buckets);
        drop(held);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Counts a tile that did not build, and keeps the first reason one did not.
    fn fail(&self, reason: &str) {
        let mut held = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        held.0 += 1;
        if held.1.is_none() {
            held.1 = Some(String::from(reason));
        }
    }

    /// Clears a tile from the in-flight set.
    ///
    /// Cleared whether it built or failed. A tile that failed is one the next tick may
    /// legitimately ask for again -- a transient 503 is not a permanent absence, and an absent
    /// tile is cached as empty by the layer below rather than retried here.
    fn finish(&self, key: &TileKey) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .inflight
            .remove(key);
    }

    /// Lands whatever the transport has finished, and starts building it.
    ///
    /// The deferred half of the tile path, and the reason `request` is allowed to return without
    /// an answer: something has to come back for the answer, and this is it. Called at the top of
    /// a tick, before anything reads `generation` -- a tile landed here is a redraw, and draining
    /// after that read defers it by a whole frame.
    ///
    /// Cheap when nothing has arrived: one lock, a walk of the tickets outstanding, and no work.
    pub fn drain(self: &Arc<Self>) {
        self.drain_resolution();
        self.drain_glyphs();

        // Snapshotted rather than walked under the lock, because polling takes the ticket
        // table's lock and holding `pending` across that is the nesting `dispatch` is careful
        // not to make from the other side.
        let outstanding: Vec<Ticket> = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();

        let mut landed = Vec::new();
        for ticket in outstanding {
            // A poll consumes its result, so two ticks racing here cannot both take one tile:
            // the loser sees `None` and leaves the entry to the winner.
            if let Some(outcome) = self.deferred.poll(ticket) {
                landed.push((ticket, outcome));
            }
        }
        if landed.is_empty() {
            return;
        }

        let mut ready = Vec::with_capacity(landed.len());
        {
            let mut held = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            for (ticket, outcome) in landed {
                if let Some(work) = held.remove(&ticket) {
                    ready.push((work, outcome));
                }
            }
        }

        for (work, outcome) in ready {
            match outcome {
                // The `Arc` is carried into the job rather than unwrapped: the body is already
                // shared between whatever views joined this fetch, and copying it per tile would
                // undo the sharing coalescing exists to provide.
                Ok(response) => self.build(work, Some(response)),
                // The fetch is what failed, so there is nothing to build and nothing to retry
                // here. Counted, named, and cleared from the in-flight set so the next tick may
                // ask again.
                Err(error) => {
                    self.fail(&alloc::format!(
                        "{}",
                        BootError::Fetch {
                            url: boot::fetch_url(&work.job).unwrap_or_default().to_string(),
                            message: error.to_string(),
                        }
                    ));
                    self.finish(&work.job.key);
                }
            }
        }
    }
}

impl<D: TileTransport + 'static> Tiles for Arc<TileSource<D>> {
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
