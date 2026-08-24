//! Running a region download across the process pool (§5.4).
//!
//! # Why this is here and not in `tessella-storage`
//!
//! `tessella-storage` owns what it means to get one resource into a region — the 404 that is an
//! edge rather than a failure, the already-held resource that is claimed rather than refetched,
//! the transaction that keeps a body and its claim together. That decision belongs beside the
//! store, and [`Download::fetch_one`] is where it lives.
//!
//! What it does not own is a worker pool. §7 puts the pool with the orchestrator, and storage
//! sits below orchestrate in the crate graph, so the scheduling half lands here. The split is
//! not an accident of dependency order: one crate decides what a resource costs, the other
//! decides when to pay for it.
//!
//! # Background, always
//!
//! A region download is hours of fetching that nobody is watching, competing with tiles someone
//! is. It is submitted at [`Priority::Background`] and never above — and because a waiting
//! foreground start will not help with work below its own class, a download in flight cannot
//! end up on the critical path of a view that is trying to draw.
//!
//! # Progress is polled, not pushed
//!
//! The serial driver takes a callback per resource. Fanned out, that becomes a callback from
//! many threads, which either needs a lock the jobs then contend on or hands the caller a
//! progress bar that jumps backwards. So this counts into shared atomics and the caller reads
//! them whenever it wants to draw — which is what a progress bar does anyway.

use alloc::collections::BTreeSet;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use tessella_storage::cache::{RegionId, SqliteCache};
use tessella_storage::download::{Download, DownloadError, Got};
use tessella_storage::offline::{Plan, Region};
use tessella_storage::source::FileSource;

use crate::pool::{Pool, Priority};

/// Which of the two passes a scatter is running.
///
/// The scheduling is identical and only the per-resource step differs, so this rides along
/// rather than being two copies of the fan-out that could drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Accept whatever is held.
    Download,
    /// Ask the origin whether what is held is still current.
    Refresh,
}

/// What a download has got to, readable while it runs.
///
/// Every field is an atomic read: a caller drawing a progress bar takes no lock and blocks no
/// worker, which matters when the workers are the thing making progress.
#[derive(Debug, Default)]
pub struct Counters {
    /// Resources dealt with — fetched, claimed or found absent.
    pub completed: AtomicU64,
    /// Of those, fetched from the origin.
    pub fetched: AtomicU64,
    /// Of those, already held and merely claimed.
    pub held: AtomicU64,
    /// Of those, confirmed unchanged by the origin. Only a refresh produces these.
    pub unchanged: AtomicU64,
    /// Of those, absent at the origin.
    pub missing: AtomicU64,
    /// Resources the plan named.
    pub required: AtomicU64,
}

impl Counters {
    /// Completion in `0.0..=1.0`, or `None` before anything is required.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        let required = self.required.load(Ordering::Acquire);
        if required == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some((self.completed.load(Ordering::Acquire) as f64 / required as f64).min(1.0))
    }
}

/// How a download ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Resources fetched from the origin.
    pub fetched: u64,
    /// Resources already held and merely claimed.
    pub held: u64,
    /// Resources the origin confirmed unchanged. Only a refresh produces these.
    pub unchanged: u64,
    /// Claims released because the plan no longer names them. Only a refresh produces these.
    pub released: u64,
    /// Resources the origin did not have.
    pub missing: u64,
    /// Whether it stopped because it was asked to.
    pub cancelled: bool,
}

/// A region download, fanned out across the pool.
#[derive(Debug, Clone)]
pub struct RegionDownload<'a, S> {
    /// Which threads run it. Normally [`Pool::shared`].
    pub pool: &'a Pool,
    /// Where resources are stored and claimed.
    pub cache: Arc<SqliteCache>,
    /// Where they are fetched from.
    pub files: Arc<S>,
    /// Which region claims them.
    pub region: RegionId,
    /// What was asked for.
    pub definition: Arc<Region>,
    /// Set to stop. Polled before each resource.
    pub cancel: Arc<AtomicBool>,
    /// The time to record against what is stored, in seconds since the Unix epoch.
    pub now: i64,
    /// Where progress is reported.
    pub counters: Arc<Counters>,
}

impl<S: FileSource + 'static> RegionDownload<'_, S> {
    /// Fetches, stores and claims everything a plan names.
    ///
    /// Assets first and tiles second, with a barrier between them: a download stopped halfway is
    /// far more useful with a style and no tiles than with tiles and nothing to draw them with,
    /// and fanning both out together would interleave them into no order at all.
    ///
    /// # Errors
    ///
    /// The first [`DownloadError`] any resource produced. Whatever was already stored stays
    /// stored, so running it again resumes rather than restarts.
    pub fn run(&self, plan: &Plan) -> Result<Outcome, DownloadError> {
        self.pass(plan, Pass::Download)
    }

    /// Brings the region up to date against its origin, fanned out the same way.
    ///
    /// Unlike [`Self::run`], a held resource is revalidated rather than accepted — see
    /// [`Download::refresh_one`] for why a download alone leaves a region a snapshot of the day
    /// it was taken. A completed refresh also releases claims the plan no longer names.
    ///
    /// # Errors
    ///
    /// As [`Self::run`].
    pub fn refresh(&self, plan: &Plan) -> Result<Outcome, DownloadError> {
        self.pass(plan, Pass::Refresh)
    }

    fn pass(&self, plan: &Plan, pass: Pass) -> Result<Outcome, DownloadError> {
        self.counters
            .required
            .store(plan.len() as u64 + 1, Ordering::Release);

        let failure: Arc<Mutex<Option<DownloadError>>> = Arc::new(Mutex::new(None));

        // The style document, then the assets. One batch, because the style is one resource and
        // a batch of one costs nothing.
        let assets: Vec<&str> = core::iter::once(self.definition.style_url.as_str())
            .chain(plan.assets.iter().map(alloc::string::String::as_str))
            .collect();
        self.scatter(&assets, pass, &failure)?;

        // The barrier. A download cancelled during its assets never starts a tile, which is what
        // makes "assets first" a guarantee rather than a tendency.
        if !self.cancelled() {
            let tiles: Vec<&str> = plan
                .tiles
                .iter()
                .map(alloc::string::String::as_str)
                .collect();
            self.scatter(&tiles, pass, &failure)?;
        }

        let cancelled = self.cancelled();
        // Only a completed refresh prunes. A cancelled one has not visited every URL, so what
        // looks orphaned may simply not have been reached — releasing those would turn an
        // interrupted refresh into a partial delete, which for a region downloaded over hours
        // is the worst thing that could happen to it.
        let released = if pass == Pass::Refresh && !cancelled {
            let keep: BTreeSet<&str> = core::iter::once(self.definition.style_url.as_str())
                .chain(plan.assets.iter().map(alloc::string::String::as_str))
                .chain(plan.tiles.iter().map(alloc::string::String::as_str))
                .collect();
            self.cache.prune_claims(self.region, &keep)? as u64
        } else {
            0
        };

        Ok(Outcome {
            fetched: self.counters.fetched.load(Ordering::Acquire),
            held: self.counters.held.load(Ordering::Acquire),
            unchanged: self.counters.unchanged.load(Ordering::Acquire),
            released,
            missing: self.counters.missing.load(Ordering::Acquire),
            cancelled,
        })
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Runs one group of URLs across the pool and waits for it.
    fn scatter(
        &self,
        urls: &[&str],
        pass: Pass,
        failure: &Arc<Mutex<Option<DownloadError>>>,
    ) -> Result<(), DownloadError> {
        let batch = self.pool.batch(Priority::Background);
        for url in urls {
            let url = url.to_string();
            let cache = Arc::clone(&self.cache);
            let files = Arc::clone(&self.files);
            let cancel = Arc::clone(&self.cancel);
            let counters = Arc::clone(&self.counters);
            let failure = Arc::clone(failure);
            let definition = Arc::clone(&self.definition);
            let region = self.region;
            let now = self.now;

            batch.submit(move || {
                // Both checked per resource rather than per batch: a cancel or a failure part
                // way through a country's worth of tiles should stop the rest, and the jobs are
                // all queued by now so there is nothing left to stop queueing.
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                if failure
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some()
                {
                    return;
                }

                let download = Download {
                    cache: &cache,
                    files: &*files,
                    region,
                    definition: &definition,
                    now,
                };
                let outcome = match pass {
                    Pass::Download => download.fetch_one(&url),
                    Pass::Refresh => download.refresh_one(&url),
                };
                match outcome {
                    Ok(got) => {
                        let counted = match got {
                            Got::Fetched => &counters.fetched,
                            Got::Held => &counters.held,
                            Got::Unchanged => &counters.unchanged,
                            Got::Missing => &counters.missing,
                        };
                        counted.fetch_add(1, Ordering::AcqRel);
                        // Last, so a caller that reads `completed` and then the breakdown never
                        // sees a total that outruns its own parts.
                        counters.completed.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(error) => {
                        let mut held = failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if held.is_none() {
                            *held = Some(error);
                        }
                    }
                }
            });
        }

        // A panicking download job is a bug, and reported as one rather than as a region that
        // quietly stored fewer resources than it claimed.
        if let Err(panicked) = batch.wait() {
            return Err(DownloadError::Panicked {
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
        Ok(())
    }
}
