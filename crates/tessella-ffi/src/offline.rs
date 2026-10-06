// SPDX-License-Identifier: BSD-2-Clause
//! Regions a user asked to have available offline, over the C surface.
//!
//! The Rust half is complete and was unreachable from `tessella.h` (tessella#336): a region is a
//! row in the same SQLite store `tessella_config.cache_path` names, so a map pointed at that path
//! draws a downloaded region's tiles with no network at all. What was missing is the half that
//! creates one, sizes it, fills it and removes it.
//!
//! # A handle of its own, not a map's
//!
//! A region outlives every map that draws it, and a host may manage one with no map alive at all
//! -- a settings screen listing what is downloaded, or a background refresh. So this is a second
//! handle over the same file rather than a call on a map. SQLite is in WAL mode, so a download
//! writing does not block a map reading.
//!
//! # Progress is polled
//!
//! A download is hours of fetching on a thread of its own, and `tessella-orchestrate`'s own
//! `offline::Counters` is already shaped for a caller that reads whenever it draws: atomics, no
//! lock, and no callback from a worker thread. This exposes that, plus what the store durably holds
//! -- which is the half that survives a restart, and the half a progress bar should show for a
//! download nobody has resumed yet.
//!
//! Named rather than linked, because that type is behind a feature of its own: a doc link from this
//! module's own docs -- which compile whether the `cache` feature is on or not -- is a broken link
//! in every build that does not have it. CI's `cargo doc` runs on default features and said so.

// Every import but the status is the store's, and the store is behind the feature. Imported there
// rather than qualified at each use, which is what the rest of this crate does.
#[cfg(feature = "cache")]
use alloc::collections::BTreeMap;
#[cfg(feature = "cache")]
use alloc::string::String;
#[cfg(feature = "cache")]
use alloc::sync::Arc;
#[cfg(feature = "cache")]
use core::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "cache")]
use std::sync::Mutex;

use crate::{Status, guarded};

/// What a region is, as a caller states it.
///
/// Byte ranges rather than C strings, as everything on this surface is, and doubles for every
/// number so there is one convention. `geojson` is optional: with it the area is that shape, and
/// without it the four bounds are a box.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RegionSpec {
    /// The style to make available, as a URL. Fetched by the download as its first resource.
    ///
    /// A URL here where [`crate::Config::style_json`] takes bytes, and the asymmetry is the point:
    /// a map draws a style the host already holds, while a region has to be able to fetch its
    /// style again on a device that has been offline since.
    pub style_url: *const u8,
    /// Its length in bytes.
    pub style_url_len: usize,
    /// The area, as a GeoJSON `Polygon` or `MultiPolygon` geometry, or null for the box below.
    pub geojson: *const u8,
    /// Its length in bytes.
    pub geojson_len: usize,
    /// What a user called it, or null. Stored and handed back by nothing yet; it is what a list
    /// in a settings screen displays.
    pub description: *const u8,
    /// Its length in bytes.
    pub description_len: usize,
    /// Western edge, used when `geojson` is null.
    pub west: f64,
    /// Southern edge.
    pub south: f64,
    /// Eastern edge.
    pub east: f64,
    /// Northern edge.
    pub north: f64,
    /// Lowest zoom to include.
    pub min_zoom: f64,
    /// Highest zoom to include.
    pub max_zoom: f64,
    /// Device pixel ratio, which selects between `@2x` and plain assets.
    pub pixel_ratio: f64,
    /// Whether to download CJK glyph ranges, which are the bulk of a glyph download.
    pub include_ideographs: u8,
}

/// What a region will cost.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Cost {
    /// Tiles, across every source.
    pub tiles: u64,
    /// Every resource, tiles included.
    pub resources: u64,
    /// Whether `resources` is exact or a lower bound.
    ///
    /// A source given by TileJSON URL states its zoom range in a manifest rather than in the
    /// style, so its tiles cannot be counted until that is fetched -- and fetching it before the
    /// question can be put is what makes asking as expensive as agreeing. A `text-font` computed
    /// per feature is the other way of not knowing.
    pub precise: u8,
}

/// How far a download has got.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    /// Resources dealt with -- fetched, claimed or found absent. Live, so zero before a start.
    pub completed: u64,
    /// Of those, fetched from the origin.
    pub fetched: u64,
    /// Of those, already held and merely claimed.
    pub held: u64,
    /// Of those, confirmed unchanged by the origin. Only a refresh produces these.
    pub unchanged: u64,
    /// Of those, absent at the origin.
    pub missing: u64,
    /// Resources the plan named, or zero before it is known.
    pub required: u64,
    /// Resources the store holds against this region, which survives a restart.
    pub stored_resources: u64,
    /// Bytes those resources occupy.
    pub stored_bytes: u64,
    /// One of [`State`].
    pub state: u32,
}

/// Where a region's download is.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Nothing has been started in this process. The stored counts still say what it holds.
    Idle = 0,
    /// Running now, and the live counters are moving.
    Running = 1,
    /// Finished, every resource dealt with.
    Done = 2,
    /// Stopped because it was asked to. Whatever was stored stays stored, so starting it again
    /// resumes rather than restarts.
    Canceled = 3,
    /// Stopped by a failure. As above: what was stored stays.
    Failed = 4,
}

/// A store's regions, and the downloads filling them.
pub struct OfflineState {
    #[cfg(feature = "cache")]
    inner: Inner,
}

/// The handle C holds.
pub type OfflineHandle = *mut OfflineState;

#[cfg(feature = "cache")]
struct Inner {
    cache: Arc<tessella_storage::cache::SqliteCache>,
    /// Where a download fetches from: the router alone, with no store in front of it.
    ///
    /// A download writes its own rows -- `Download::fetch_one` claims what is held and stores what
    /// it fetches, against the region -- so a caching source here would store every body twice,
    /// once unclaimed.
    files: Arc<crate::Origins>,
    running: Mutex<BTreeMap<i64, Running>>,
}

#[cfg(feature = "cache")]
struct Running {
    cancel: Arc<AtomicBool>,
    counters: Arc<tessella_orchestrate::offline::Counters>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// `None` while it runs, then how it ended.
    finished: Arc<Mutex<Option<Ending>>>,
}

/// How a download ended, as the state a caller is told.
#[cfg(feature = "cache")]
#[derive(Debug, Clone, Copy)]
enum Ending {
    Done,
    Canceled,
    Failed,
}

/// Seconds since the Unix epoch, which is what the store records against what it holds.
#[cfg(feature = "cache")]
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(0))
}

/// Opens or creates the store at `path`, for managing its regions.
///
/// The same file a map takes as `tessella_config.cache_path`, and the reason the two are separate
/// handles is in the module note: a region outlives every map that draws it.
///
/// [`Status::NoCache`] when this build has no store at all, which is the `cache` feature being off.
///
/// # Safety
///
/// `path` must be valid for reads of `path_len` bytes and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_open(
    path: *const u8,
    path_len: usize,
    out: *mut OfflineHandle,
) -> Status {
    guarded(move || {
        if path.is_null() || out.is_null() {
            return Status::NullArgument;
        }
        // SAFETY: the caller's contract.
        let Some(path) = (unsafe { crate::borrowed(path, path_len) }) else {
            return Status::NotUtf8;
        };
        #[cfg(feature = "cache")]
        {
            let Ok(cache) = tessella_storage::cache::SqliteCache::open(std::path::Path::new(&path))
            else {
                return Status::NoCache;
            };
            let Ok(files) = crate::origins(None) else {
                return Status::Failed;
            };
            let state = alloc::boxed::Box::new(OfflineState {
                inner: Inner {
                    cache: Arc::new(cache),
                    files: Arc::new(files),
                    running: Mutex::new(BTreeMap::new()),
                },
            });
            // SAFETY: checked non-null above.
            unsafe { *out = alloc::boxed::Box::into_raw(state) };
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = path;
            Status::NoCache
        }
    })
}

/// Stops every download this handle started and releases it.
///
/// Waits for each to notice, which is at most one resource. A download stopped this way resumes
/// rather than restarts: whatever was stored stays claimed.
///
/// Null is accepted and does nothing, as every destructor on this surface is.
///
/// # Safety
///
/// `offline` must be a handle from [`tessella_offline_open`] that has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_close(offline: OfflineHandle) {
    if offline.is_null() {
        return;
    }
    let _ = guarded(move || {
        // SAFETY: the caller's contract, and the box is not used again.
        let state = unsafe { alloc::boxed::Box::from_raw(offline) };
        #[cfg(feature = "cache")]
        {
            let mut running = state
                .inner
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for entry in running.values() {
                entry.cancel.store(true, Ordering::Release);
            }
            for entry in running.values_mut() {
                if let Some(thread) = entry.thread.take() {
                    let _ = thread.join();
                }
            }
        }
        drop(state);
        Status::Ok
    });
}

/// The region a spec describes, or the reason it is not one.
#[cfg(feature = "cache")]
unsafe fn region_of(spec: &RegionSpec) -> Result<tessella_storage::offline::Region, Status> {
    use tessella_storage::offline::Area;

    if spec.style_url.is_null() {
        return Err(Status::NullArgument);
    }
    // SAFETY: the caller's contract for the spec's pointers.
    let Some(style_url) = (unsafe { crate::borrowed(spec.style_url, spec.style_url_len) }) else {
        return Err(Status::NotUtf8);
    };

    let area = if spec.geojson.is_null() || spec.geojson_len == 0 {
        Area::Box(tessella_tile::cover::Bounds::new(
            spec.west, spec.south, spec.east, spec.north,
        ))
    } else {
        // SAFETY: as above.
        let Some(text) = (unsafe { crate::borrowed(spec.geojson, spec.geojson_len) }) else {
            return Err(Status::NotUtf8);
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Err(Status::BadGeojson);
        };
        // A shape this cannot cover is a bad document rather than a failure: the caller passed a
        // geometry that is not a polygon, and only the caller can fix that.
        Area::from_geometry(&value).map_err(|_| Status::BadGeojson)?
    };

    Ok(tessella_storage::offline::Region {
        style_url,
        area,
        min_zoom: spec.min_zoom,
        max_zoom: spec.max_zoom,
        #[allow(clippy::cast_possible_truncation)]
        pixel_ratio: spec.pixel_ratio as f32,
        include_ideographs: spec.include_ideographs != 0,
    })
}

/// Records a region, and returns the identifier every other call names it by.
///
/// Creating it claims nothing: the region exists with no resources until a download stores them,
/// which is what makes a download resumable rather than all-or-nothing. It appears in
/// [`tessella_offline_list`] immediately, at nothing percent.
///
/// [`Status::BadGeojson`] for a geometry that is not a `Polygon` or `MultiPolygon`.
///
/// # Safety
///
/// `offline` must be a live handle, `spec` and `out_region` must be readable and writable, and the
/// spec's byte ranges must be valid for their stated lengths.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_define(
    offline: OfflineHandle,
    spec: *const RegionSpec,
    out_region: *mut u64,
) -> Status {
    guarded(move || {
        if spec.is_null() || out_region.is_null() {
            return Status::NullArgument;
        }
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            let Some(state) = (unsafe { offline.as_ref() }) else {
                return Status::NoSuchMap;
            };
            // SAFETY: as above.
            let spec = unsafe { *spec };
            let region = match unsafe { region_of(&spec) } {
                Ok(region) => region,
                Err(status) => return status,
            };
            // SAFETY: as above.
            let description = if spec.description.is_null() {
                None
            } else {
                match unsafe { crate::borrowed(spec.description, spec.description_len) } {
                    Some(text) => Some(text),
                    None => return Status::NotUtf8,
                }
            };
            let Ok(id) = state
                .inner
                .cache
                .create_region(&region, description.as_deref(), now())
            else {
                return Status::Failed;
            };
            #[allow(clippy::cast_sign_loss)]
            // SAFETY: checked non-null above.
            unsafe {
                *out_region = id.get() as u64;
            }
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = offline;
            Status::NoCache
        }
    })
}

/// What a region would cost, from the style the caller is displaying.
///
/// The style's bytes rather than its URL, because this answers without the network: a round trip
/// before the question can be put is what makes asking as expensive as agreeing. `precise` says
/// whether anything was unknowable -- see [`Cost::precise`].
///
/// Takes a spec rather than an identifier, so a host can size a box a user is still dragging.
///
/// # Safety
///
/// As [`tessella_offline_define`], plus `style_json` valid for `style_json_len` bytes and `out`
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_estimate(
    offline: OfflineHandle,
    spec: *const RegionSpec,
    style_json: *const u8,
    style_json_len: usize,
    out: *mut Cost,
) -> Status {
    guarded(move || {
        if spec.is_null() || style_json.is_null() || out.is_null() {
            return Status::NullArgument;
        }
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            if unsafe { offline.as_ref() }.is_none() {
                return Status::NoSuchMap;
            }
            // SAFETY: as above.
            let spec = unsafe { *spec };
            let region = match unsafe { region_of(&spec) } {
                Ok(region) => region,
                Err(status) => return status,
            };
            // SAFETY: as above.
            let Some(text) = (unsafe { crate::borrowed(style_json, style_json_len) }) else {
                return Status::BadStyle;
            };
            let Ok(style) = tessella_style::Style::parse(&text) else {
                return Status::BadStyle;
            };
            let (sources, assets, complete) =
                tessella_storage::offline::contributions(&style, &BTreeMap::new());
            let estimate = tessella_storage::offline::estimate(&region, &sources, assets);
            // SAFETY: checked non-null above.
            unsafe {
                *out = Cost {
                    tiles: estimate.tiles,
                    resources: estimate.resources,
                    precise: u8::from(estimate.precise && complete),
                };
            }
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, style_json, style_json_len);
            Status::NoCache
        }
    })
}

/// The stored region with that identifier, and its own `RegionId`.
#[cfg(feature = "cache")]
fn stored(
    cache: &tessella_storage::cache::SqliteCache,
    region: u64,
) -> Option<tessella_storage::cache::StoredRegion> {
    #[allow(clippy::cast_possible_wrap)]
    let wanted = region as i64;
    cache
        .regions()
        .ok()?
        .into_iter()
        .find(|stored| stored.id.get() == wanted)
}

/// Starts a download or a refresh on a thread of its own.
#[cfg(feature = "cache")]
fn start(
    offline: OfflineHandle,
    region: u64,
    style_json: *const u8,
    style_json_len: usize,
    refresh: bool,
) -> Status {
    if style_json.is_null() {
        return Status::NullArgument;
    }
    // SAFETY: the caller's contract.
    let Some(state) = (unsafe { offline.as_ref() }) else {
        return Status::NoSuchMap;
    };
    // SAFETY: as above.
    let Some(text) = (unsafe { crate::borrowed(style_json, style_json_len) }) else {
        return Status::BadStyle;
    };
    // Parsed here rather than on the thread: a style that will not parse is an answer the caller
    // can have now, where a thread could only report it as a failed download.
    let Ok(style) = tessella_style::Style::parse(&text) else {
        return Status::BadStyle;
    };
    let Some(stored) = stored(&state.inner.cache, region) else {
        return Status::NoSuchRegion;
    };

    let mut running = state
        .inner
        .running
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A finished entry is kept so its outcome stays readable, and replaced when the same region
    // is started again -- which is how a canceled download resumes.
    if let Some(entry) = running.get(&region.try_into().unwrap_or(i64::MAX))
        && entry
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    {
        return Status::AlreadyRunning;
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(tessella_orchestrate::offline::Counters::default());
    let finished: Arc<Mutex<Option<Ending>>> = Arc::new(Mutex::new(None));

    let cache = Arc::clone(&state.inner.cache);
    let files = Arc::clone(&state.inner.files);
    let id = stored.id;
    let definition = stored.region;
    let thread = {
        let cancel = Arc::clone(&cancel);
        let counters = Arc::clone(&counters);
        let finished = Arc::clone(&finished);
        std::thread::Builder::new()
            .name(String::from("tessella-region"))
            .spawn(move || {
                let at = now();
                // The plan, which turns the style into the URLs this will fetch. It reaches the
                // network for any source whose zooms are in a manifest rather than in the style,
                // which is why it is here and not in the call that returned already.
                let planned = tessella_storage::download::Download {
                    cache: &cache,
                    files: &*files,
                    region: id,
                    definition: &definition,
                    now: at,
                }
                .plan(&style);
                let Ok(plan) = planned else {
                    *finished
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ending::Failed);
                    return;
                };
                let download = tessella_orchestrate::offline::RegionDownload {
                    pool: tessella_orchestrate::pool::Pool::shared(),
                    cache: Arc::clone(&cache),
                    files: Arc::clone(&files),
                    region: id,
                    definition: Arc::new(definition),
                    cancel,
                    now: at,
                    counters,
                };
                let outcome = if refresh {
                    download.refresh(&plan)
                } else {
                    download.run(&plan)
                };
                let ending = match outcome {
                    Ok(outcome) if outcome.canceled => Ending::Canceled,
                    Ok(_) => Ending::Done,
                    Err(_) => Ending::Failed,
                };
                *finished
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ending);
            })
    };
    let Ok(thread) = thread else {
        return Status::Failed;
    };

    running.insert(
        id.get(),
        Running {
            cancel,
            counters,
            thread: Some(thread),
            finished,
        },
    );
    Status::Ok
}

/// Fetches, stores and claims everything the region names, on a thread of its own.
///
/// Returns as soon as the thread is running; the work is hours of fetching on a connection that
/// will drop, and [`tessella_offline_progress`] is how it is watched. Submitted at the pool's
/// background class and never above, so a download in flight cannot end up on the critical path
/// of a view that is trying to draw.
///
/// Assets first and tiles second, with a barrier between them: a download stopped halfway is far
/// more useful with a style and no tiles than with tiles and nothing to draw them with.
///
/// Resumable rather than transactional. Whatever was stored stays stored and claimed, so calling
/// this again after a cancel or a failure continues rather than starting over.
///
/// [`Status::AlreadyRunning`] when one is in flight for that region, [`Status::NoSuchRegion`] for
/// an identifier the store does not have, and [`Status::BadStyle`] for bytes that will not parse.
///
/// # Safety
///
/// `offline` must be a live handle and `style_json` valid for `style_json_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_download(
    offline: OfflineHandle,
    region: u64,
    style_json: *const u8,
    style_json_len: usize,
) -> Status {
    guarded(move || {
        #[cfg(feature = "cache")]
        {
            start(offline, region, style_json, style_json_len, false)
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, region, style_json, style_json_len);
            Status::NoCache
        }
    })
}

/// Brings a region up to date against its origin, the same way.
///
/// Unlike [`tessella_offline_download`], a held resource is revalidated rather than accepted: a
/// download alone leaves a region a snapshot of the day it was taken, which is correct -- the user
/// paid for that snapshot and it is served however old it gets -- and this is how they ask for a
/// newer one. A completed refresh also releases claims the plan no longer names.
///
/// # Safety
///
/// As [`tessella_offline_download`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_refresh(
    offline: OfflineHandle,
    region: u64,
    style_json: *const u8,
    style_json_len: usize,
) -> Status {
    guarded(move || {
        #[cfg(feature = "cache")]
        {
            start(offline, region, style_json, style_json_len, true)
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, region, style_json, style_json_len);
            Status::NoCache
        }
    })
}

/// How far a region's download has got, live and stored.
///
/// Both halves, because they answer different questions. The live counters are this process's
/// download and are zero before one starts; the stored counts are what the file holds and survive
/// a restart, which is what a progress bar should show for a region nobody has resumed yet.
///
/// `state` is [`State`]. A download that ended is reported until the same region is started again,
/// so a caller that polls after the last resource still learns how it finished.
///
/// # Safety
///
/// `offline` must be a live handle and `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_progress(
    offline: OfflineHandle,
    region: u64,
    out: *mut Counters,
) -> Status {
    guarded(move || {
        if out.is_null() {
            return Status::NullArgument;
        }
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            let Some(state) = (unsafe { offline.as_ref() }) else {
                return Status::NoSuchMap;
            };
            let Some(stored) = stored(&state.inner.cache, region) else {
                return Status::NoSuchRegion;
            };
            let Ok(progress) = state.inner.cache.region_progress(stored.id) else {
                return Status::Failed;
            };

            let mut counters = Counters {
                stored_resources: progress.completed_resources,
                stored_bytes: progress.completed_bytes,
                state: State::Idle as u32,
                ..Counters::default()
            };
            let running = state
                .inner
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = running.get(&stored.id.get()) {
                let live = &entry.counters;
                counters.completed = live.completed.load(Ordering::Acquire);
                counters.fetched = live.fetched.load(Ordering::Acquire);
                counters.held = live.held.load(Ordering::Acquire);
                counters.unchanged = live.unchanged.load(Ordering::Acquire);
                counters.missing = live.missing.load(Ordering::Acquire);
                counters.required = live.required.load(Ordering::Acquire);
                let ending = *entry
                    .finished
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                counters.state = match ending {
                    // Nothing recorded and the thread is gone: it ended without saying how,
                    // which is a panic on the way to recording it. Reported as a failure rather
                    // than as still running -- a progress bar that never moves again is the one
                    // outcome a caller cannot tell from a slow link.
                    None if entry
                        .thread
                        .as_ref()
                        .is_some_and(std::thread::JoinHandle::is_finished) =>
                    {
                        State::Failed as u32
                    }
                    None => State::Running as u32,
                    Some(Ending::Done) => State::Done as u32,
                    Some(Ending::Canceled) => State::Canceled as u32,
                    Some(Ending::Failed) => State::Failed as u32,
                };
            }
            // SAFETY: checked non-null above.
            unsafe { *out = counters };
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, region);
            Status::NoCache
        }
    })
}

/// Asks a running download to stop, and returns without waiting.
///
/// Polled before each resource, so it stops within one. Whatever was stored stays stored and
/// claimed: a canceled download is a resumable one, which is the whole reason a region exists in
/// the list at nothing percent rather than appearing when it completes.
///
/// [`Status::Ok`] for a region with nothing running, which is what a caller cancelling twice or
/// cancelling a finished download means.
///
/// # Safety
///
/// `offline` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_cancel(offline: OfflineHandle, region: u64) -> Status {
    guarded(move || {
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            let Some(state) = (unsafe { offline.as_ref() }) else {
                return Status::NoSuchMap;
            };
            let Some(stored) = stored(&state.inner.cache, region) else {
                return Status::NoSuchRegion;
            };
            let running = state
                .inner
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = running.get(&stored.id.get()) {
                entry.cancel.store(true, Ordering::Release);
            }
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, region);
            Status::NoCache
        }
    })
}

/// Every region in the store, oldest first, as identifiers.
///
/// Writes at most `cap` of them and reports how many there are, so a caller with a buffer that is
/// too small learns the count and asks again rather than being truncated silently. `out` may be
/// null with a `cap` of zero, which is how the count alone is asked for.
///
/// # Safety
///
/// `offline` must be a live handle, `out` valid for `cap` identifiers, and `out_count` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_list(
    offline: OfflineHandle,
    out: *mut u64,
    cap: usize,
    out_count: *mut usize,
) -> Status {
    guarded(move || {
        if out_count.is_null() || (out.is_null() && cap > 0) {
            return Status::NullArgument;
        }
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            let Some(state) = (unsafe { offline.as_ref() }) else {
                return Status::NoSuchMap;
            };
            let Ok(regions) = state.inner.cache.regions() else {
                return Status::Failed;
            };
            // SAFETY: checked above.
            unsafe { *out_count = regions.len() };
            #[allow(clippy::cast_sign_loss)]
            for (at, stored) in regions.iter().take(cap).enumerate() {
                // SAFETY: `at` is below `cap` and `out` is valid for that many.
                unsafe { *out.add(at) = stored.id.get() as u64 };
            }
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, out, cap);
            Status::NoCache
        }
    })
}

/// Removes a region and releases its claims.
///
/// The resources themselves are not deleted: one an overlapping region or ordinary use also wants
/// stays, and what is left unclaimed re-enters the ambient budget and is evicted when the store
/// needs the room. That is the whole of what a claim is -- a count on the row rather than a copy of
/// the body.
///
/// A running download is stopped first, since it would otherwise go on claiming resources for a
/// region that is gone.
///
/// # Safety
///
/// `offline` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_offline_delete(offline: OfflineHandle, region: u64) -> Status {
    guarded(move || {
        #[cfg(feature = "cache")]
        {
            // SAFETY: the caller's contract.
            let Some(state) = (unsafe { offline.as_mut() }) else {
                return Status::NoSuchMap;
            };
            let Some(stored) = stored(&state.inner.cache, region) else {
                return Status::NoSuchRegion;
            };
            // Stopped and waited for, not merely asked: a worker still claiming rows against a
            // deleted region would fail its foreign key, and the delete is the caller's answer.
            let mut running = state
                .inner
                .running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = running.get_mut(&stored.id.get()) {
                entry.cancel.store(true, Ordering::Release);
                if let Some(thread) = entry.thread.take() {
                    let _ = thread.join();
                }
            }
            running.remove(&stored.id.get());
            drop(running);

            if state.inner.cache.delete_region(stored.id).is_err() {
                return Status::Failed;
            }
            Status::Ok
        }
        #[cfg(not(feature = "cache"))]
        {
            let _ = (offline, region);
            Status::NoCache
        }
    })
}
