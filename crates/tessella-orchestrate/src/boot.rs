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
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tessella_source::mvt;
use tessella_storage::fetch_zoom;
use tessella_storage::source::{Coalescing, FileSource};
use tessella_storage::tileset::{self, TileSet};
use tessella_style::{LayerKind, Source, Style};
use tessella_tile::cover::{self, ViewTransform};

use crate::tile::{LayerBucket, TileError, TileId, build_mvt_tile};

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

/// What a cold start produced.
#[derive(Debug)]
pub struct Boot {
    /// Buckets per tile of the cover, in cover order.
    pub tiles: Vec<(TileId, Vec<LayerBucket>)>,
    /// Stage timings.
    pub trace: BootTrace,
    /// Tile bodies fetched, in bytes.
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
    /// Boxed: [`TileError`] carries two owned strings and would otherwise set the size of every
    /// `Result` this module returns, on the success path as much as the failure one.
    #[error("building `{url}`")]
    Build {
        /// What was asked for.
        url: String,
        /// What went wrong.
        #[source]
        source: alloc::boxed::Box<TileError>,
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
/// RK3566 has four cores that the deployment wants split — §5.4 puts decode workers on the
/// little ones and reserves the big ones for the orchestrator and the renderer — so a cold
/// start that took every core would take them from the things that have to stay responsive.
/// And a number derived from the host makes a measurement on a workstation say nothing about
/// the device, which is the measurement that matters.
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

/// One tile's work, resolved before any of it is done.
struct Job {
    tile: TileId,
    url: String,
}

/// Runs a cold start and reports how long each stage took.
///
/// `workers` threads share the tile work, clamped to the number of tiles. [`Workers::default`]
/// carries the rationale for the count; [`Workers::serial`] is the one-thread baseline a trace
/// is compared against.
///
/// # Errors
///
/// [`BootError`] when the style, a source, or any tile of the cover fails.
pub fn cold_start<S: FileSource>(
    style_text: &str,
    view: &ViewTransform,
    files: &Coalescing<S>,
    workers: Workers,
) -> Result<Boot, BootError> {
    let started = Instant::now();

    let style = Style::parse(style_text).map_err(|error| BootError::Style(error.to_string()))?;
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

    let mut sets: Vec<(String, TileSet)> = Vec::new();
    for name in wanted {
        let Some(Source::Vector(source)) = style.source(name) else {
            continue;
        };
        let set = tileset::resolve(source, files.inner()).map_err(|error| BootError::Source {
            name: name.to_string(),
            message: error.to_string(),
        })?;
        sets.push((name.to_string(), set));
    }
    let sources_resolved = started.elapsed();

    let cover = cover::cover(view).map_err(|_| BootError::Uncovered)?;
    let mut jobs: Vec<Job> = Vec::new();
    for tile in &cover {
        // One source for now: the cold-start shape is the same for several, and the tile loop
        // below is what changes rather than this.
        let Some((_, set)) = sets.first() else {
            break;
        };
        let Some(z) = fetch_zoom(tile.z, set.zooms) else {
            continue;
        };
        let shift = tile.z - z;
        let (x, y) = (tile.x >> shift, tile.y >> shift);
        let Some(url) = set.url_for(z, x, y, 1.0) else {
            continue;
        };
        jobs.push(Job {
            tile: TileId::overscaled(z, x, y, tile.z),
            url,
        });
    }
    let cover_computed = started.elapsed();

    if jobs.is_empty() {
        return Ok(Boot {
            tiles: Vec::new(),
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
    let done: Mutex<Vec<Option<Vec<LayerBucket>>>> =
        Mutex::new((0..jobs.len()).map(|_| None).collect());
    let next = AtomicUsize::new(0);
    let bytes = AtomicUsize::new(0);
    let first_fetch: Mutex<Option<Duration>> = Mutex::new(None);
    let first_bucket: Mutex<Option<Duration>> = Mutex::new(None);
    let failure: Mutex<Option<BootError>> = Mutex::new(None);

    let record = |slot: &Mutex<Option<Duration>>| {
        let mut held = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.is_none() {
            *held = Some(started.elapsed());
        }
    };

    std::thread::scope(|scope| {
        for _ in 0..workers.for_jobs(jobs.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(index) else {
                        return;
                    };
                    // Stop taking work once something has failed; the first error is reported
                    // and the rest would be noise.
                    if failure
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_some()
                    {
                        return;
                    }

                    let response = match files.fetch(&job.url) {
                        Ok(response) => response,
                        Err(error) => {
                            fail(
                                &failure,
                                BootError::Fetch {
                                    url: job.url.clone(),
                                    message: error.to_string(),
                                },
                            );
                            return;
                        }
                    };
                    record(&first_fetch);
                    bytes.fetch_add(response.body.len(), Ordering::Relaxed);

                    // An absent tile is ordinary, not a failure: a source's coverage is not a
                    // rectangle, and the cover asks for the whole viewport.
                    if response.is_absent() {
                        done.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)[index] =
                            Some(Vec::new());
                        continue;
                    }

                    let decoded = match mvt::Tile::decode(&response.body) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            fail(
                                &failure,
                                BootError::Decode {
                                    url: job.url.clone(),
                                    message: error.to_string(),
                                },
                            );
                            return;
                        }
                    };
                    let buckets = match build_mvt_tile(&style, job.tile, &decoded) {
                        Ok(buckets) => buckets,
                        Err(source) => {
                            fail(
                                &failure,
                                BootError::Build {
                                    url: job.url.clone(),
                                    source: alloc::boxed::Box::new(source),
                                },
                            );
                            return;
                        }
                    };
                    record(&first_bucket);
                    done.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = Some(buckets);
                }
            });
        }
    });

    if let Some(error) = failure
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(error);
    }

    let complete = started.elapsed();
    let built = done
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tiles = jobs
        .into_iter()
        .zip(built)
        .filter_map(|(job, buckets)| buckets.map(|buckets| (job.tile, buckets)))
        .collect();

    Ok(Boot {
        tiles,
        trace: BootTrace {
            style_parsed,
            sources_resolved,
            cover_computed,
            first_fetch: first_fetch
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or(cover_computed),
            first_bucket: first_bucket
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or(cover_computed),
            complete,
        },
        bytes: bytes.load(Ordering::Relaxed),
    })
}

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
            .flat_map(|(_, buckets)| buckets)
            .map(|bucket| {
                bucket.content.as_fill().map_or(0, |b| b.vertices.len())
                    + bucket.content.as_line().map_or(0, |b| b.vertices.len())
                    + bucket.content.as_circle().map_or(0, |b| b.vertices.len())
            })
            .sum()
    }
}
