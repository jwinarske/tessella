//! Cold start: style to first drawable geometry (§12.5).
//!
//! Cold-boot-to-map is an IVI number and R1's remaining exit criterion, so this is written to
//! be *measured* rather than merely to work: [`BootTrace`] records when each stage finished,
//! and the one that matters is [`BootTrace::first_bucket`] — the moment something exists to
//! draw, not the moment everything does.
//!
//! # The shape §12.5 asks for
//!
//! The naive cold start serializes style → manifests → tiles → decode → buckets, and each
//! stage waits for all of the previous one. Two of those waits are avoidable and this removes
//! them:
//!
//! - **Tiles fan out.** The cover's tiles are independent, so they are fetched, decoded and
//!   built concurrently. Measured against a local Protomaps extract that takes a nine-tile
//!   cover from 72 ms to 22 ms, and first geometry from 12.7 ms to 6.7 ms.
//! - **Only the sources a layer draws from are resolved.** A manifest is a round trip on the
//!   critical path, and a style may declare sources nothing reads.
//!
//! # What is not done, and what it would be worth
//!
//! Paint properties are still resolved inside `build_mvt_tile`, so expression compilation runs
//! once per layer *per tile* rather than once per layer — process-scoped work (§5.5) charged
//! per tile. Measured at 23 µs for a four-layer style, so 209 µs over a nine-tile cover
//! against a 22 ms cold start: about one percent, and not the reason to restructure the tile
//! builder's signature today. Recorded rather than fixed, with the number, so the decision can
//! be made on it rather than on the shape of the code.
//!
//! Also absent: the compiled-style cache keyed by style etag that §12.5 wants for warm start,
//! and the sprite and glyph fetches a symbol layer would need. Issuing tile fetches before the
//! manifest arrives is not possible — the manifest carries the templates — so the round trip
//! it costs is irreducible without a cache.
//!
//! # First tile, not first frame
//!
//! This measures to the first *bucket*. What happens between a bucket and a photon is the
//! consumer's, and §11.6's pan-to-photon covers it from the other side. Splitting them is
//! deliberate: a producer that reports a number including the consumer's compositor cannot say
//! whether a regression is its own.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessella_source::GeoJsonFeature;
use tessella_source::mvt;
use tessella_source::tiling::TilingOptions;
use tessella_storage::fetch_zoom;
use tessella_storage::offline::SourceKind;
use tessella_storage::source::{Coalescing, FileSource};
use tessella_storage::tileset::{self, TileSet};
use tessella_style::{LayerKind, RejectedLayer, Source, Style};
use tessella_tile::cover::{self, ViewTransform};

use crate::cache::TileCache;
use crate::pool::{Pool, Priority};
use crate::tile::{LayerBucket, TileId, build_mvt_tile, build_raster_tile, build_tile};

/// The tile zoom a source is covered at.
///
/// mbgl's `coveringZoomLevel`: a vector source floors the view's zoom, and a raster source
/// shifts it by `log2(512 / tileSize)` and *rounds*. The rounding is not a detail — flooring a
/// 256-pixel source would leave it consistently one level too coarse, which is a satellite
/// basemap at half the resolution of the labels drawn over it.
///
/// Clamped to what a cover can address. The zoom arrives from the consumer's camera over the
/// reverse channel (DR-9) and is not a trusted number, and the shift makes it larger.
fn covering_zoom(kind: SourceKind, zoom: f64) -> u8 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        kind.covering_zoom(zoom)
            .clamp(0.0, f64::from(cover::MAX_ZOOM)) as u8
    }
}

/// When each stage of a cold start finished, measured from the moment it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootTrace {
    /// The style document parsed.
    pub style_parsed: Duration,
    /// Every source's manifest fetched and its templates known.
    pub sources_resolved: Duration,
    /// The cover computed.
    pub cover_computed: Duration,
    /// The first tile's bytes arrived.
    pub first_fetch: Duration,
    /// The first tile's buckets were built — the number this exists to report.
    pub first_bucket: Duration,
    /// Every tile of the cover was built.
    pub complete: Duration,
}

/// One tile of one source, built.
#[derive(Debug, Clone)]
pub struct BuiltTile {
    /// Which tile.
    pub tile: TileId,
    /// Which source it was built from.
    pub source: String,
    /// Its buckets, shared with every other view that wanted them.
    pub buckets: alloc::sync::Arc<Vec<LayerBucket>>,
}

/// What a cold start produced.
#[derive(Debug)]
pub struct Boot {
    /// Buckets per `(tile, source)`, in cover order.
    ///
    /// A tile is one thing per *source*: a style overlaying a local extract on a world basemap
    /// has two entries at the same address, and each carries only the layers that draw from its
    /// own source.
    pub tiles: Vec<BuiltTile>,
    /// Layers that draw from no source, per tile of the cover.
    ///
    /// A background fills the viewport rather than reading a tile, so it is per tile but not
    /// per source. Building it inside a source's pass would emit one copy per source of a
    /// thing the oracle emits once.
    pub sourceless: Vec<(TileId, Vec<LayerBucket>)>,
    /// Layers the style asked for that this build cannot compile, and why.
    ///
    /// Reported rather than logged, and not empty on real styles: see
    /// [`Style::reject_uncompilable`]. Every layer left in the style compiled, so a tile that
    /// fails after this point failed on its *data* rather than on the document.
    pub rejected_layers: Vec<RejectedLayer>,
    /// Stage timings.
    pub trace: BootTrace,
    /// Tile bodies handled, in bytes.
    ///
    /// *Handled*, not fetched. A cache hit returns a response indistinguishable from a fetched
    /// one — that is what makes a cache transparent — so this counts bytes that reached the
    /// decoder however they got there. Whether any of them crossed the network is a question
    /// for the file source's own counters, which is where the answer actually is
    /// (`CachingFileSource::stats`).
    pub bytes: usize,
}

/// Why a cold start did not complete.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BootError {
    /// The style document did not parse.
    #[error("parsing the style: {0}")]
    Style(String),
    /// A source could not be resolved.
    ///
    /// The field is `name`, not `source`: `thiserror` reads a field called `source` as the
    /// error's cause and would try to treat a `String` as one.
    #[error("source `{name}`: {message}")]
    Source {
        /// Which source.
        name: String,
        /// What went wrong.
        message: String,
    },
    /// A tile's work panicked.
    ///
    /// A bug rather than a tile that failed to load, and separate from the rest so it cannot be
    /// mistaken for one: a start that quietly returned fewer tiles than it covered would show
    /// up as a map with holes and no error anywhere.
    #[error("{jobs} tile job(s) panicked")]
    Panicked {
        /// How many.
        jobs: usize,
    },
    /// A tile could not be fetched.
    #[error("fetching `{url}`: {message}")]
    Fetch {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// A tile's bytes did not decode.
    #[error("decoding `{url}`: {message}")]
    Decode {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// A tile's buckets did not build.
    ///
    /// The cause is a string rather than a `TileError`: it is shared with every caller waiting
    /// on the same tile, so it has to be cloneable, and a `TileError` carries owned strings
    /// that would set the size of every `Result` this module returns.
    #[error("building `{url}`: {message}")]
    Build {
        /// What was asked for.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The caller that owned a tile's build unwound without producing one.
    ///
    /// Retryable: the key is free again, so the next attempt becomes a new leader.
    #[error("the build of `{url}` was abandoned")]
    Abandoned {
        /// What was being built.
        url: String,
    },
    /// The view covers nothing any source provides.
    #[error("no source covers this view")]
    Uncovered,
}

/// How many threads share the tile work.
///
/// # Why this is not `available_parallelism`
///
/// The obvious default is the host's core count, and it is wrong for this target class. An
/// RK3566 has four cores that the deployment wants split, so a cold start that took every one
/// would take them from the things that have to stay responsive. And a number derived from the
/// host makes a measurement on a workstation say nothing about the device, which is the
/// measurement that matters.
///
/// *Which* cores those are is not written down anywhere here — see [`crate::topology`], where
/// the part is asked instead. This paragraph used to say §5.4's "little cores for decode, big
/// cores for the orchestrator", which is an RK3588 and not the board the number was taken on.
/// The count survives that correction because its reason never depended on the split: four
/// workers leave something over on a four-core part however alike those cores are.
///
/// mbgl reaches the same conclusion: its background `ThreadPool` is a fixed three, not a
/// derived count.
///
/// # Why four rather than mbgl's three
///
/// mbgl's pool does decode and layout while its I/O happens elsewhere. A worker here does the
/// fetch too, so a blocked worker is not merely idle — it is holding a slot that has no CPU
/// work to do. One more than the CPU-bound count is the cheapest way to keep the others busy
/// across a round trip. It is a starting point with a reason, not a tuned number; §5.4's pool
/// with priority classes is where tuning belongs, and it does not exist yet.
///
/// # Never more workers than tiles
///
/// [`Self::for_jobs`] clamps to the work available. Nine tiles on a sixteen-core host is nine
/// threads, not sixteen; the rest would start, find the queue empty and exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Workers(usize);

impl Workers {
    /// The default worker count. See the type's note for why it is a constant.
    pub const DEFAULT: usize = 4;

    /// A pool of `count` workers, with a floor of one.
    ///
    /// Zero is treated as one rather than refused: a caller asking for no workers wants the
    /// work done, and a cold start that silently did nothing would be worse than a slow one.
    #[must_use]
    pub const fn new(count: usize) -> Self {
        Self(if count == 0 { 1 } else { count })
    }

    /// A serial start, which is what a trace is compared against.
    #[must_use]
    pub const fn serial() -> Self {
        Self(1)
    }

    /// The number to actually spawn for `jobs` pieces of work.
    #[must_use]
    pub const fn for_jobs(self, jobs: usize) -> usize {
        if jobs < self.0 { jobs } else { self.0 }
    }

    /// The configured count, before clamping.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for Workers {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

/// What resolving one source produced.
enum Resolved {
    /// A tiled source's manifest, and whether its tiles are features or a picture.
    Tiles(TileSet, SourceKind),
    /// A GeoJSON document, already read into features.
    Document(alloc::sync::Arc<Vec<GeoJsonFeature>>),
    /// A GeoJSON document the source asked to be clustered, indexed once for every zoom.
    ///
    /// Built here rather than per tile because that is what it is for: the levels are built
    /// deepest-first from the whole document, and every tile of every zoom is then a range
    /// query. Building one per tile would cluster the world once per tile of the cover.
    Clustered(alloc::sync::Arc<tessella_source::cluster::Clustered>),
}

/// The clustering a GeoJSON source asks for, or `None` if it asks for none.
///
/// `cluster` is the switch; the other two are the knobs, and the spec's defaults are
/// supercluster's own. `clusterMaxZoom` defaults to one below the source's maximum rather than
/// to supercluster's sixteen, which is what the style spec says and what makes the deepest zoom
/// show individual points.
fn clustering_for(
    source: &tessella_style::document::GeojsonSource,
) -> Option<tessella_source::cluster::Options> {
    if source.cluster != Some(true) {
        return None;
    }
    let defaults = tessella_source::cluster::Options::default();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(tessella_source::cluster::Options {
        radius: source.cluster_radius.unwrap_or(defaults.radius),
        max_zoom: source
            .cluster_max_zoom
            .map_or(defaults.max_zoom, |zoom| zoom as u8),
        ..defaults
    })
}

/// One tile's work, resolved before any of it is done.
/// What a tile's work needs, which differs by source kind.
///
/// A vector tile is cut by the server, so its work starts with a request. A GeoJSON tile is cut
/// from a document already in hand — one fetched once during source resolution, or written
/// into the style — so its work is tessellation and nothing else. The distinction is here
/// rather than in the worker because it is a property of the *source*, decided once, not
/// something to re-derive per tile.
enum Work {
    /// Fetch, decode, then build.
    Vector { url: String },
    /// Fetch, decode the picture, then build the quad it goes on.
    Raster { url: String },
    /// Build from the document the source already resolved to.
    Geojson {
        features: alloc::sync::Arc<Vec<GeoJsonFeature>>,
    },
}

struct Job {
    tile: TileId,
    source: String,
    work: Work,
    key: tessella_tile::store::TileKey,
}

impl Job {
    /// What to name in an error. A GeoJSON tile has no URL of its own — the document's was
    /// spent during resolution — so it names the tile instead.
    fn what(&self) -> String {
        match &self.work {
            Work::Vector { url } | Work::Raster { url } => url.clone(),
            Work::Geojson { .. } => alloc::format!("{}/{}", self.source, self.tile),
        }
    }
}

/// What a cold start needs.
///
/// A struct rather than six arguments: `files` and `cache` are process-scoped and the rest are
/// per-start, and a positional list of that shape is one whose call sites stop being readable
/// after the third one.
#[derive(Debug, Clone)]
pub struct ColdStart<'a, S> {
    /// The style document.
    pub style: &'a str,
    /// The camera to cover.
    pub view: &'a ViewTransform,
    /// Where bytes come from. Shared between views, so one tile is fetched once.
    ///
    /// Held by [`Arc`] rather than borrowed because the tile jobs outlive this call's stack
    /// frame as far as the type system can see — they go to a pool that was running before this
    /// start began. §5.5 already lists file sources as process-owned, so this is the ownership
    /// the table describes rather than a concession to the pool.
    pub files: Arc<Coalescing<S>>,
    /// Where built tiles live. Shared between views, so one tile is built once.
    ///
    /// [`Arc`] for the reason [`Self::files`] gives.
    pub cache: Arc<TileCache<BootError>>,
    /// Which threads run the tile work.
    ///
    /// Normally [`Pool::shared`]. A caller wanting the serial baseline a trace is compared
    /// against passes a `Pool::new(Workers::serial())` of its own.
    pub pool: &'a Pool,
    /// Which class the tile work competes in.
    ///
    /// [`Priority::Foreground`] for a view someone is looking at, which is what a cold start
    /// normally is.
    pub priority: Priority,
    /// Which revision of the style this is.
    ///
    /// Part of the cache key: a bucket built against one style is not valid against another,
    /// since a changed filter admits different features and a changed paint property changes
    /// what is data-driven (§5.1). A caller that reuses a revision across an edited style gets
    /// the old buckets.
    pub style_rev: u64,
}

impl<S: FileSource + 'static> ColdStart<'_, S> {
    /// Runs the start and reports how long each stage took.
    ///
    /// # Errors
    ///
    /// [`BootError`] when the style, a source, or any tile of the cover fails.
    pub fn run(&self) -> Result<Boot, BootError> {
        cold_start(self)
    }
}

/// Runs a cold start and reports how long each stage took.
///
/// `workers` threads share the tile work, clamped to the number of tiles. [`Workers::default`]
/// carries the rationale for the count; [`Workers::serial`] is the one-thread baseline a trace
/// is compared against.
///
/// The cache is process-scoped and shared between views. A second view over the same cover
/// finds the buckets already built; a second view starting *at the same time* joins the build
/// rather than repeating it, which is the case a cache alone cannot cover (§9.3).
///
/// # Errors
///
/// [`BootError`] when the style, a source, or any tile of the cover fails.
pub fn cold_start<S: FileSource + 'static>(config: &ColdStart<'_, S>) -> Result<Boot, BootError> {
    let &ColdStart {
        style: style_text,
        view,
        ref files,
        ref cache,
        pool,
        priority,
        style_rev,
    } = config;
    let started = Instant::now();

    let mut style =
        Style::parse(style_text).map_err(|error| BootError::Style(error.to_string()))?;
    // As mbgl's parser does, and before anything reads a layer: a document that names one thing
    // this build does not have still draws every layer that does.
    let rejected_layers = style.reject_uncompilable();
    let style_parsed = started.elapsed();

    // Every source a layer actually draws from. A style may declare sources no layer uses, and
    // fetching their manifests would put a round trip on the critical path for nothing.
    let mut wanted: Vec<&str> = style
        .layers
        .iter()
        .filter(|layer| layer.kind != LayerKind::Background)
        .filter_map(|layer| layer.source.as_deref())
        .collect();
    wanted.sort_unstable();
    wanted.dedup();

    // Resolved together rather than one after another. A source described by a TileJSON URL
    // costs a round trip to find out what it offers, and every one of those sat on the critical
    // path in front of the first tile request — four sources on a 40 ms link was 160 ms before
    // anything was asked for. They do not depend on each other, so §12.5's "issue the moment
    // sources parse" starts here.
    let resolved: Arc<Mutex<Vec<(String, Resolved)>>> = Arc::new(Mutex::new(Vec::new()));
    let failure: Arc<Mutex<Option<BootError>>> = Arc::new(Mutex::new(None));
    let batch = pool.batch(priority);
    for name in wanted {
        let Some(source) = style.source(name).cloned() else {
            continue;
        };
        let name = name.to_string();
        let files = Arc::clone(files);
        let resolved = Arc::clone(&resolved);
        let failure = Arc::clone(&failure);
        batch.submit(move || {
            let outcome = match &source {
                Source::Vector(source) => tileset::resolve(source, files.inner())
                    .map(|set| Resolved::Tiles(set, SourceKind::Vector))
                    .map_err(|error| error.to_string()),
                // The same manifest, the same templates: TileJSON does not distinguish, and a
                // raster source is addressed exactly as a vector one is. What differs is the
                // zoom its tiles are asked for at and what arrives in them.
                Source::Raster(source) => tileset::resolve(source, files.inner())
                    .map(|set| {
                        let kind = SourceKind::Raster {
                            tile_size: set.tile_size,
                        };
                        Resolved::Tiles(set, kind)
                    })
                    .map_err(|error| error.to_string()),
                // One fetch for the whole document, or none at all if it is inline. The tiling
                // is this side's, so there is nothing per-tile to ask for afterwards.
                Source::Geojson(source) => {
                    let clustering = clustering_for(source);
                    tessella_storage::geojson::resolve(source, files.inner())
                        .map_err(|error| error.to_string())
                        .and_then(|document| {
                            tessella_source::geojson::read(&document)
                                .map_err(|error| error.to_string())
                        })
                        .map(|features| match clustering {
                            Some(options) => Resolved::Clustered(alloc::sync::Arc::new(
                                tessella_source::cluster::Clustered::new(features, options),
                            )),
                            None => Resolved::Document(alloc::sync::Arc::new(features)),
                        })
                }
                _ => return,
            };
            match outcome {
                Ok(outcome) => resolved
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((name, outcome)),
                Err(message) => fail(&failure, BootError::Source { name, message }),
            }
        });
    }
    if let Err(panicked) = batch.wait() {
        return Err(BootError::Panicked {
            jobs: panicked.jobs,
        });
    }
    if let Some(error) = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(error);
    }

    let mut sets: Vec<(String, TileSet, SourceKind)> = Vec::new();
    let mut documents: Vec<(String, alloc::sync::Arc<Vec<GeoJsonFeature>>)> = Vec::new();
    let mut clustered: Vec<(String, alloc::sync::Arc<tessella_source::cluster::Clustered>)> =
        Vec::new();
    {
        let mut held = resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Sorted, because the order jobs finished in is whatever the scheduler decided and the
        // cover below is built from these — a trace that reordered its tiles run to run would
        // make two runs incomparable.
        held.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, outcome) in held.drain(..) {
            match outcome {
                Resolved::Tiles(set, kind) => sets.push((name, set, kind)),
                Resolved::Document(features) => documents.push((name, features)),
                Resolved::Clustered(index) => clustered.push((name, index)),
            }
        }
    }

    let sources_resolved = started.elapsed();

    // One cover per *source kind*, not one for the map. mbgl computes `tileCover` per source
    // with that source's own `coveringZoomLevel`, and the levels genuinely differ: a 256-pixel
    // raster source needs one zoom more than a vector one to fill the same screen, so a single
    // cover would fetch imagery at half the resolution of the labels drawn over it.
    let cover = cover::cover(view).map_err(|_| BootError::Uncovered)?;
    let mut raster_covers: alloc::collections::BTreeMap<u8, Vec<cover::TileCoord>> =
        alloc::collections::BTreeMap::new();
    for (_, _, kind) in &sets {
        if let SourceKind::Raster { .. } = kind {
            let z = covering_zoom(*kind, view.zoom);
            if let alloc::collections::btree_map::Entry::Vacant(slot) = raster_covers.entry(z) {
                slot.insert(cover::cover_at(view, z).map_err(|_| BootError::Uncovered)?);
            }
        }
    }

    let mut jobs: Vec<Job> = Vec::new();
    // One job per (tile, source). A tile is not one thing: it is one thing *per source*, the
    // way mbgl has a render tile per source-tile, and a style that overlays a local extract on
    // a world basemap wants both at the same address.
    for (name, set, kind) in &sets {
        let (tiles, work): (&[cover::TileCoord], fn(String) -> Work) = match kind {
            SourceKind::Vector => (&cover, |url| Work::Vector { url }),
            SourceKind::Raster { .. } => (
                raster_covers[&covering_zoom(*kind, view.zoom)].as_slice(),
                |url| Work::Raster { url },
            ),
        };

        for tile in tiles {
            let Some(z) = fetch_zoom(tile.z, set.zooms) else {
                continue;
            };
            let shift = tile.z - z;
            let (x, y) = (tile.x >> shift, tile.y >> shift);
            let Some(url) = set.url_for(z, x, y, 1.0) else {
                continue;
            };
            let id = TileId::overscaled(z, x, y, tile.z);
            jobs.push(Job {
                source: name.clone(),
                key: tessella_tile::store::TileKey::overscaled(
                    name.as_str(),
                    id.z,
                    id.x,
                    id.y,
                    id.overscaled_z,
                    style_rev,
                ),
                tile: id,
                work: work(url),
            });
        }
    }

    // A GeoJSON source has no zoom range to clamp against and nothing to fetch: every tile
    // of the cover is cut from the one document, at the cover's own zoom.
    for (name, features) in &documents {
        for tile in &cover {
            let id = TileId::new(tile.z, tile.x, tile.y);
            jobs.push(Job {
                source: name.clone(),
                key: tessella_tile::store::TileKey::new(name.as_str(), id.z, id.x, id.y, style_rev),
                tile: id,
                work: Work::Geojson {
                    features: alloc::sync::Arc::clone(features),
                },
            });
        }
    }

    // A clustered source is cut from the index rather than from the document, and the features
    // a tile gets are the clusters at *its* zoom — which is the whole of clustering as far as
    // everything downstream is concerned. They are ordinary points with ordinary properties from
    // there on, so a style draws them with the same circle and symbol layers it draws anything
    // with, and `point_count` is a property like any other.
    for (name, index) in &clustered {
        for tile in &cover {
            let id = TileId::new(tile.z, tile.x, tile.y);
            jobs.push(Job {
                source: name.clone(),
                key: tessella_tile::store::TileKey::new(name.as_str(), id.z, id.x, id.y, style_rev),
                tile: id,
                work: Work::Geojson {
                    features: alloc::sync::Arc::new(index.tile_features(id.z, id.x, id.y)),
                },
            });
        }
    }

    let cover_computed = started.elapsed();

    if jobs.is_empty() {
        return Ok(Boot {
            tiles: Vec::new(),
            sourceless: Vec::new(),
            rejected_layers,
            trace: BootTrace {
                style_parsed,
                sources_resolved,
                cover_computed,
                first_fetch: cover_computed,
                first_bucket: cover_computed,
                complete: started.elapsed(),
            },
            bytes: 0,
        });
    }

    // Results are placed by index so the output order is the cover's however the work is
    // scheduled — a trace that reordered its tiles would make two runs incomparable.
    // `Arc`, because the cache owns the buckets and hands out shares of them: a second view
    // over the same cover gets the same allocation rather than a copy of it.
    // Results are placed by index so the output order is the cover's however the work is
    // scheduled — a trace that reordered its tiles would make two runs incomparable.
    // `Arc`, because the cache owns the buckets and hands out shares of them: a second view
    // over the same cover gets the same allocation rather than a copy of it.
    let done: Arc<Mutex<Slots>> = Arc::new(Mutex::new((0..jobs.len()).map(|_| None).collect()));
    let bytes = Arc::new(AtomicUsize::new(0));
    let first_fetch: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let first_bucket: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let failure: Arc<Mutex<Option<BootError>>> = Arc::new(Mutex::new(None));
    let style = Arc::new(style);

    // What the tail needs from each job, kept back before the jobs themselves are moved into
    // their closures. A `Job` owns its URL and its features, so cloning one per submission
    // would copy a whole GeoJSON document per tile.
    let meta: Vec<(TileId, String)> = jobs
        .iter()
        .map(|job| (job.tile, job.source.clone()))
        .collect();

    let batch = pool.batch(priority);
    for (index, job) in jobs.into_iter().enumerate() {
        let style = Arc::clone(&style);
        let files = Arc::clone(files);
        let cache = Arc::clone(cache);
        let done = Arc::clone(&done);
        let bytes = Arc::clone(&bytes);
        let first_fetch = Arc::clone(&first_fetch);
        let first_bucket = Arc::clone(&first_bucket);
        let failure = Arc::clone(&failure);

        batch.submit(move || {
            let record = |slot: &Mutex<Option<Duration>>| {
                let mut held = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if held.is_none() {
                    *held = Some(started.elapsed());
                }
            };

            // Stop doing work once something has failed; the first error is reported and the
            // rest would be noise. The jobs are all queued now rather than taken off a shared
            // index, so this is a check per job rather than a way to stop taking them — the
            // saving is the fetch and the decode, which is where the cost is.
            if failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
            {
                return;
            }

            // The cache is outermost: a tile whose buckets are already built costs
            // no fetch and no decode. Fetching first and consulting the cache after
            // would make a warm view pay the network for bytes it is about to throw
            // away — which is what a second view over the same cover mostly is.
            let built = cache.get_or_build(
                &job.key,
                || match &job.work {
                    Work::Vector { url } => {
                        let response = files.fetch(url).map_err(|error| BootError::Fetch {
                            url: url.clone(),
                            message: error.to_string(),
                        })?;
                        record(&first_fetch);
                        bytes.fetch_add(response.body.len(), Ordering::Relaxed);

                        // An absent tile is ordinary, not a failure: a source's
                        // coverage is not a rectangle and the cover asks for the whole
                        // viewport. It is cached as an empty tile so the next view does
                        // not ask again.
                        if response.is_absent() {
                            return Ok(Vec::new());
                        }

                        let decoded = mvt::Tile::decode(&response.body).map_err(|error| {
                            BootError::Decode {
                                url: url.clone(),
                                message: error.to_string(),
                            }
                        })?;
                        build_mvt_tile(&style, &job.source, job.tile, &decoded).map_err(|error| {
                            BootError::Build {
                                url: url.clone(),
                                message: error.to_string(),
                            }
                        })
                    }
                    Work::Raster { url } => {
                        let response = files.fetch(url).map_err(|error| BootError::Fetch {
                            url: url.clone(),
                            message: error.to_string(),
                        })?;
                        record(&first_fetch);
                        bytes.fetch_add(response.body.len(), Ordering::Relaxed);

                        // As for a vector tile: a source's coverage is not a rectangle, and the
                        // hole an absent imagery tile leaves is a hole rather than a failure.
                        if response.is_absent() {
                            return Ok(Vec::new());
                        }

                        let image =
                            tessella_source::image::decode(&response.body).map_err(|error| {
                                BootError::Decode {
                                    url: url.clone(),
                                    message: error.to_string(),
                                }
                            })?;
                        // The whole tile. A cold start's cover is one zoom level, so no tile in
                        // it is an ancestor of another and every mask is the whole tile — which
                        // is why no capture ever shows one. A view that substitutes a parent
                        // while its children load computes the mask over its own renderable set
                        // and rebuilds the geometry, because the mask belongs to that view's
                        // moment rather than to the tile.
                        build_raster_tile(
                            &style,
                            &job.source,
                            alloc::sync::Arc::new(image),
                            &[tessella_tile::mask::WHOLE_TILE],
                        )
                        .map_err(|error| BootError::Build {
                            url: url.clone(),
                            message: error.to_string(),
                        })
                    }
                    // Nothing to fetch and nothing to decode: the document arrived
                    // during source resolution, and this cuts a tile out of it.
                    Work::Geojson { features } => build_tile(
                        &style,
                        &job.source,
                        job.tile,
                        features,
                        TilingOptions::default(),
                    )
                    .map_err(|error| BootError::Build {
                        url: job.what(),
                        message: error.to_string(),
                    }),
                },
                || BootError::Abandoned { url: job.what() },
            );
            let buckets = match built {
                Ok(built) => built.tile,
                Err(error) => {
                    fail(&failure, error);
                    return;
                }
            };
            record(&first_bucket);
            done.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = Some(buckets);
        });
    }

    // A panicking decode is a bug, not a tile that failed to load, and it must not be reported
    // as a start that quietly built fewer tiles than it covered.
    if let Err(panicked) = batch.wait() {
        return Err(BootError::Panicked {
            jobs: panicked.jobs,
        });
    }

    if let Some(error) = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        return Err(error);
    }

    let complete = started.elapsed();
    let built = core::mem::take(
        &mut *done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let tiles = meta
        .into_iter()
        .zip(built)
        .filter_map(|((tile, source), buckets)| {
            buckets.map(|buckets| BuiltTile {
                tile,
                source,
                buckets,
            })
        })
        .collect();

    // Cheap enough to do after the fan-out: no I/O, and one paint resolve per source-less
    // layer per tile.
    let mut sourceless = Vec::with_capacity(cover.len());
    for tile in &cover {
        let id = TileId::new(tile.z, tile.x, tile.y);
        sourceless.push((
            id,
            crate::tile::build_sourceless(&style, id).map_err(|error| BootError::Build {
                url: alloc::format!("{id}"),
                message: error.to_string(),
            })?,
        ));
    }

    Ok(Boot {
        tiles,
        sourceless,
        rejected_layers,
        trace: BootTrace {
            style_parsed,
            sources_resolved,
            cover_computed,
            first_fetch: (*first_fetch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner))
            .unwrap_or(cover_computed),
            first_bucket: (*first_bucket
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner))
            .unwrap_or(cover_computed),
            complete,
        },
        bytes: bytes.load(Ordering::Relaxed),
    })
}

/// Built buckets by cover index, filled in as the jobs land.
///
/// The inner `Arc` is the cache's: it owns the buckets and hands out shares of them, so a second
/// view over the same cover gets the same allocation rather than a copy.
type Slots = Vec<Option<Arc<Vec<LayerBucket>>>>;

/// Records the first failure, leaving any later one alone.
fn fail(slot: &Mutex<Option<BootError>>, error: BootError) {
    let mut held = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.is_none() {
        *held = Some(error);
    }
}

impl Boot {
    /// Total vertices across every layer of every tile.
    #[must_use]
    pub fn vertices(&self) -> usize {
        self.tiles
            .iter()
            .flat_map(|built| built.buckets.iter())
            .map(|bucket| {
                bucket.content.as_fill().map_or(0, |b| b.vertices.len())
                    + bucket.content.as_line().map_or(0, |b| b.vertices.len())
                    + bucket.content.as_circle().map_or(0, |b| b.vertices.len())
            })
            .sum()
    }
}
