//! Running a region download: enumerate, fetch, pin, report.
//!
//! # The shape of it
//!
//! Resolve each source's manifest, plan the URLs from those, then fetch them one at a time and
//! store each against the region as it arrives. Assets go first — manifests, glyphs, sprites —
//! so that a download stopped halfway still has a style that renders whatever tiles did land,
//! rather than a pile of tiles and nothing to draw them with.
//!
//! # Why it is resumable rather than transactional
//!
//! A country at street zoom is hours of downloading over a connection that will drop. Treating
//! that as one unit of work means an interruption discards everything, and the user starts
//! again. So each resource is its own transaction: the region row exists from the moment it is
//! asked for, every resource is claimed as it arrives, and running the download again skips
//! what is already held. Cancelling is then just stopping — there is nothing to unwind.
//!
//! # What is not an error
//!
//! A source's coverage is not a rectangle. Asking for a tile outside it returns 404, and that
//! is how the edge is found rather than a failure: the tile is counted as done and the download
//! moves on. Only a transport failure is worth reporting, and even then the region keeps what it
//! got so the next attempt is shorter.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use tessella_style::{Source, Style};

use crate::cache::{CacheError, RegionId, SqliteCache};
use crate::offline::{Plan, Region, plan};
use crate::source::{FetchError, FileSource};
use crate::tileset::{self, ResolveError, TileSet};

/// Why a download could not be run.
///
/// Note what is *not* here: a tile that 404s, and a resource that was already held. Neither
/// stops a download, and treating either as a failure would make a region over a coastline
/// impossible to complete.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// A source's manifest could not be resolved, so its tiles cannot be named.
    #[error("resolving source `{id}`: {source}")]
    Source {
        /// Which source.
        id: String,
        /// What went wrong.
        #[source]
        source: ResolveError,
    },
    /// A resource could not be fetched.
    #[error("fetching `{url}`: {source}")]
    Fetch {
        /// What was being fetched.
        url: String,
        /// What went wrong.
        #[source]
        source: FetchError,
    },
    /// A resource came back with a status that is neither the thing nor a definite absence.
    ///
    /// A 500 or a 403 is transient or fixable, and recording it as done would leave a region
    /// permanently short of a resource it never retries.
    #[error("`{url}` returned {status}")]
    Status {
        /// What was asked for.
        url: String,
        /// What came back.
        status: u16,
    },
    /// A resource's work panicked.
    ///
    /// A bug rather than a resource that failed to arrive, and separate from the rest so it
    /// cannot be mistaken for one: a region that quietly stored fewer resources than it claimed
    /// would show up offline as a map with holes and nothing anywhere saying why.
    #[error("{jobs} download job(s) panicked")]
    Panicked {
        /// How many.
        jobs: usize,
    },
    /// The store could not be written.
    #[error(transparent)]
    Cache(#[from] CacheError),
}

/// How far a download has got, as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// Resources stored and claimed for the region.
    pub completed_resources: u64,
    /// Bytes those resources occupy.
    pub completed_bytes: u64,
    /// Resources the plan names.
    pub required_resources: u64,
    /// Whether [`Self::required_resources`] is exact.
    ///
    /// False when a source could not be enumerated or a `text-font` is data-driven. A progress
    /// bar that silently grows is confusing; one that claims a precision it does not have is
    /// worse, so this is surfaced rather than smoothed over.
    pub required_precise: bool,
}

impl Progress {
    /// Completion in `0.0..=1.0`, or `None` before anything is required.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        if self.required_resources == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some((self.completed_resources as f64 / self.required_resources as f64).min(1.0))
    }
}

/// How a download ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// Where it got to.
    pub progress: Progress,
    /// Resources actually fetched this run, as opposed to already held.
    pub fetched: u64,
    /// Resources the origin confirmed unchanged.
    ///
    /// Only a refresh produces these: a round trip each and no bytes, which is the whole reason
    /// refreshing is affordable where re-downloading is not.
    pub unchanged: u64,
    /// Claims released because the plan no longer names them.
    ///
    /// Only a refresh produces these.
    pub released: u64,
    /// Resources the origin said it did not have.
    ///
    /// Expected rather than alarming for tiles at the edge of a source's coverage; worth
    /// looking at if it is most of the region. They are stored as the empty responses they are,
    /// so a resumed download does not ask the sea for tiles a second time.
    pub missing: u64,
    /// Whether it stopped because it was asked to.
    pub cancelled: bool,
}

/// A download in progress: what it is downloading, from where, and into what.
///
/// Grouped rather than passed as arguments because these five never vary across the calls that
/// make up one download — planning and running share every one of them, and threading them
/// separately through both is how a caller ends up planning against one region and running
/// against another.
#[derive(Clone, Copy)]
pub struct Download<'a> {
    /// Where resources are stored and claimed.
    pub cache: &'a SqliteCache,
    /// Where they are fetched from.
    pub files: &'a dyn FileSource,
    /// Which region claims them.
    pub region: RegionId,
    /// What was asked for.
    pub definition: &'a Region,
    /// The time to record against what is stored, in seconds since the Unix epoch.
    pub now: i64,
}

impl Download<'_> {
    /// Resolves every source manifest and works out what has to be fetched.
    ///
    /// Separate from [`Self::run`] so a caller can show the user a size and let them decline:
    /// plan, ask, then run the same plan.
    ///
    /// # Errors
    ///
    /// [`DownloadError::Source`] naming the source that could not be resolved. Fatal, unlike a
    /// missing tile: a source whose zoom range is unknown cannot be enumerated at all, and a
    /// region silently missing one of its layers is worse than one that was never downloaded.
    pub fn plan(&self, style: &Style) -> Result<Plan, DownloadError> {
        let manifests = resolve_manifests(style, self.files)?;
        Ok(plan(style, self.definition, &manifests))
    }

    /// Plans and runs in one go.
    ///
    /// # Errors
    ///
    /// As [`Self::plan`] and [`Self::run`].
    pub fn all(
        &self,
        style: &Style,
        cancel: &AtomicBool,
        observe: &mut dyn FnMut(Progress),
    ) -> Result<Summary, DownloadError> {
        let plan = self.plan(style)?;
        self.run(&plan, cancel, observe)
    }

    /// Fetches, stores and claims everything a plan names.
    ///
    /// `cancel` is polled between resources: a download stopped this way keeps everything it
    /// already stored, and running it again resumes from there.
    ///
    /// `observe` is called after each resource. It is where a caller drives a progress bar, and
    /// it is deliberately synchronous — a download that outran its own reporting would show a
    /// bar that jumps.
    ///
    /// # Errors
    ///
    /// [`DownloadError::Fetch`] on a transport failure, [`DownloadError::Status`] on a status
    /// that is neither the resource nor a definite absence, [`DownloadError::Cache`] when the
    /// store cannot be written. In every case what was already stored stays stored.
    pub fn run(
        &self,
        plan: &Plan,
        cancel: &AtomicBool,
        observe: &mut dyn FnMut(Progress),
    ) -> Result<Summary, DownloadError> {
        let mut summary = Summary {
            progress: Progress {
                // The style document, and then everything it names.
                required_resources: plan.len() as u64 + 1,
                required_precise: plan.complete,
                ..Progress::default()
            },
            fetched: 0,
            unchanged: 0,
            released: 0,
            missing: 0,
            cancelled: false,
        };

        // The style itself, then assets, then tiles. Assets before tiles because a region
        // stopped halfway is far more useful with a style and no tiles than the other way
        // around.
        let urls = std::iter::once(&self.definition.style_url)
            .chain(&plan.assets)
            .chain(&plan.tiles);

        for url in urls {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            match self.fetch_one(url)? {
                Got::Fetched => summary.fetched += 1,
                Got::Missing => summary.missing += 1,
                Got::Held | Got::Unchanged => {}
            }

            let held = self.cache.region_progress(self.region)?;
            summary.progress.completed_resources = held.completed_resources;
            summary.progress.completed_bytes = held.completed_bytes;
            observe(summary.progress);
        }

        Ok(summary)
    }

    /// Brings a region up to date against its origin.
    ///
    /// # Why this is not just running the download again
    ///
    /// A download treats a held resource as done, which is what makes it resumable. Run twice,
    /// it fills gaps and changes nothing else — mbgl's offline download works the same way and
    /// has no refresh at all. So a region is a snapshot of the day it was taken, and stays one:
    /// roads that have been built since are missing, and roads that have been removed are still
    /// there, with nothing to tell the user either way.
    ///
    /// A refresh revalidates instead. Every resource is asked about with its stored etag, so an
    /// unchanged tile costs one round trip and no bytes — a region whose area has not changed
    /// costs its tile count in requests and almost nothing in transfer, which is what makes
    /// this affordable on the connection a downloaded region exists to avoid needing.
    ///
    /// # Claims the plan no longer names
    ///
    /// A style can drop a layer, a source can lower its maximum zoom, a user can redraw an area
    /// smaller. The resources those changes orphan are released here — they would otherwise
    /// stay pinned for the life of the region: outside the ambient bound, never evicted, and
    /// never used either. The bytes go when someone packs.
    ///
    /// # Errors
    ///
    /// As [`Self::run`].
    pub fn refresh(
        &self,
        plan: &Plan,
        cancel: &AtomicBool,
        observe: &mut dyn FnMut(Progress),
    ) -> Result<Summary, DownloadError> {
        let mut summary = Summary {
            progress: Progress {
                required_resources: plan.len() as u64 + 1,
                required_precise: plan.complete,
                ..Progress::default()
            },
            fetched: 0,
            unchanged: 0,
            released: 0,
            missing: 0,
            cancelled: false,
        };

        let urls = core::iter::once(&self.definition.style_url)
            .chain(&plan.assets)
            .chain(&plan.tiles);

        for url in urls {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break;
            }
            match self.refresh_one(url)? {
                Got::Fetched => summary.fetched += 1,
                Got::Unchanged => summary.unchanged += 1,
                Got::Missing => summary.missing += 1,
                Got::Held => {}
            }

            let held = self.cache.region_progress(self.region)?;
            summary.progress.completed_resources = held.completed_resources;
            summary.progress.completed_bytes = held.completed_bytes;
            observe(summary.progress);
        }

        // Only when the pass finished. A cancelled refresh has not visited every URL, so what
        // looks orphaned may simply not have been reached — releasing those would turn an
        // interrupted refresh into a partial delete.
        if !summary.cancelled {
            let keep: BTreeSet<&str> = core::iter::once(self.definition.style_url.as_str())
                .chain(plan.assets.iter().map(String::as_str))
                .chain(plan.tiles.iter().map(String::as_str))
                .collect();
            summary.released = self.cache.prune_claims(self.region, &keep)? as u64;
        }

        Ok(summary)
    }

    /// Gets one resource into the region, however it has to.
    ///
    /// The unit of work. Everything above this is scheduling, which is why it is public: a
    /// caller with a worker pool fans this out itself rather than reimplementing what it does,
    /// and there stays exactly one place that decides what a 404 means.
    ///
    /// Safe to call from several threads against one region. The fetch happens outside the
    /// store's lock, so what serialises is the write and not the network — which is the whole
    /// reason fanning it out is worth doing.
    ///
    /// # Errors
    ///
    /// [`DownloadError::Fetch`] on a transport failure, [`DownloadError::Status`] on a status
    /// that is neither the resource nor a definite absence, [`DownloadError::Cache`] when the
    /// store cannot be written.
    pub fn fetch_one(&self, url: &str) -> Result<Got, DownloadError> {
        // Already held — by the ambient cache from ordinary use, by an earlier run of this
        // download, or by another region that overlaps. Claiming costs nothing, and is why a
        // resumed download is short and a region over familiar ground is cheap.
        if self.cache.get(url, self.now)?.is_some() {
            self.cache.claim(self.region, url)?;
            return Ok(Got::Held);
        }

        let response = self
            .files
            .fetch(url)
            .map_err(|source| DownloadError::Fetch {
                url: url.to_string(),
                source,
            })?;
        let got = match response.status {
            200 => Got::Fetched,
            // The origin has nothing there. For a tile at the edge of a source's coverage that
            // is the ordinary answer, so the empty response is stored as the answer it is: the
            // region reaches a hundred percent, and a resumed download does not ask the sea for
            // tiles a second time.
            404 | 410 => Got::Missing,
            status => {
                return Err(DownloadError::Status {
                    url: url.to_string(),
                    status,
                });
            }
        };
        self.cache
            .put_region_resource(self.region, url, &response, self.now)?;
        Ok(got)
    }

    /// Brings one resource up to date, rather than accepting whatever is held.
    ///
    /// # How this differs from [`Self::fetch_one`]
    ///
    /// A download treats a held resource as done — that is what makes it resumable, and it is
    /// what mbgl does too. A refresh does not: it asks the origin whether the copy is still
    /// current, using the stored etag, so an unchanged tile costs a round trip and no bytes and
    /// a changed one is replaced.
    ///
    /// # What a 404 means here
    ///
    /// That the origin no longer has it. The empty response is stored, exactly as a download
    /// would store it, so the region stops claiming to hold a tile that has ceased to exist.
    /// Leaving the old body would be the more comfortable choice and the wrong one: a user who
    /// refreshed would keep seeing a road that has been removed, with nothing to tell them the
    /// map is out of date.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch_one`].
    pub fn refresh_one(&self, url: &str) -> Result<Got, DownloadError> {
        let Some(held) = self.cache.get(url, self.now)? else {
            // Nothing to revalidate — a resource the plan gained since the region was taken, or
            // one that was evicted from under it.
            return self.fetch_one(url);
        };

        let response = self
            .files
            .fetch_conditional(url, held.response.etag.as_deref())
            .map_err(|source| DownloadError::Fetch {
                url: url.to_string(),
                source,
            })?;

        if response.is_not_modified() {
            // The body stands; only its freshness moves. Rewriting the blob would turn the
            // saving back into a disk write of the whole tile, which is most of what a refresh
            // exists to avoid.
            self.cache.refresh(
                url,
                response.expires(self.now),
                response.must_revalidate,
                self.now,
            )?;
            self.cache.claim(self.region, url)?;
            return Ok(Got::Unchanged);
        }

        let got = match response.status {
            200 => Got::Fetched,
            404 | 410 => Got::Missing,
            status => {
                return Err(DownloadError::Status {
                    url: url.to_string(),
                    status,
                });
            }
        };
        self.cache
            .put_region_resource(self.region, url, &response, self.now)?;
        Ok(got)
    }
}

/// How one resource came to be in the region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Got {
    /// Fetched from the origin.
    Fetched,
    /// Already in the store, and claimed.
    Held,
    /// Held, and the origin confirmed it is still current.
    ///
    /// Only a refresh produces this. It cost a round trip and no bytes, which is the entire
    /// point of refreshing rather than re-downloading: a region whose tiles have not changed
    /// costs its tile count in requests and nothing in transfer.
    Unchanged,
    /// The origin has nothing there, and the absence was recorded.
    Missing,
}

/// Resolves every tiled source's manifest, so the plan can name its tiles.
///
/// # Errors
///
/// [`DownloadError::Source`] naming the source that could not be resolved. Fatal, unlike a
/// missing tile: a source whose zoom range is unknown cannot be enumerated at all, and
/// downloading a region silently missing one of its layers is worse than not downloading it.
pub fn resolve_manifests(
    style: &Style,
    files: &dyn FileSource,
) -> Result<BTreeMap<String, TileSet>, DownloadError> {
    let mut manifests = BTreeMap::new();
    for (id, source) in &style.sources {
        let tiles = match source {
            Source::Vector(tiles) | Source::Raster(tiles) | Source::RasterDem(tiles) => tiles,
            Source::Geojson(_) | Source::Other(_) => continue,
        };
        let resolved = tileset::resolve(tiles, files).map_err(|source| DownloadError::Source {
            id: id.clone(),
            source,
        })?;
        manifests.insert(id.clone(), resolved);
    }
    Ok(manifests)
}
