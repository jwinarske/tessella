//! The C ABI a consumer embeds tessella through.
//!
//! # What this is and is not
//!
//! It is not a second protocol. Everything a consumer draws from arrives on the capture stream,
//! described by the one generated header (`include/tessella_capture_abi.h`); this is only the
//! handful of calls that get a producer running and a frame emitted. Anything that could travel
//! as a record does travel as a record, because a second way to say the same thing is a second
//! thing to keep in agreement.
//!
//! # Why a `staticlib`
//!
//! §3.5: the frontend's only process coupling is the ring, so a consumer links this into its own
//! shared object and the ring is ordinary memory. Promoting that to a separate process is a
//! linker change rather than a redesign — no envelope carries an in-process pointer, and slab
//! handles are offsets — so the same header and the same records serve both.
//!
//! In-process is the case worth optimising for and the case this exists for: the consumer reads
//! geometry out of the producer's own arena, and "zero copy" is not a technique but the absence
//! of a reason to copy.
//!
//! # The rules every entry point here follows
//!
//! - **Borrowed in, owned nowhere.** A `const char*` is copied before the call returns.
//! - **No panics across the boundary.** Rust unwinding into C is undefined; every entry point
//!   returns a status and reports failure rather than unwinding.
//! - **A handle is opaque and non-null.** Zero is the failure value, so a caller that ignores
//!   the status still cannot mistake a failed create for a working map.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)]

/// How a call went.
///
/// A single `Ok` and a reason for everything else. The reasons are stable numbers because a
/// consumer logs them and a log outlives the build that wrote it.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It worked.
    Ok = 0,
    /// A pointer argument was null where the call requires one.
    NullArgument = 1,
    /// A handle did not name a live map.
    NoSuchMap = 2,
    /// A string argument was not UTF-8.
    NotUtf8 = 3,
    /// The style did not parse.
    BadStyle = 4,
    /// The ring could not take the frame. The consumer is behind; drain and retry.
    RingFull = 5,
    /// Something failed in a way this ABI has no more specific word for. The producer logs it.
    Failed = 6,
}

extern crate alloc;

use alloc::sync::Arc;
use std::ffi::{CStr, c_char};
use std::time::Duration;

use tessella_capture_abi::ring::{self, Producer, region_size};
use tessella_orchestrate::SlabArena;
use tessella_orchestrate::boot::{self, ColdStart};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::pool::{Pool, Priority};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
use tessella_style::Style;
use tessella_tile::camera;
use tessella_tile::cover::ViewTransform;

/// A live map, opaque to C.
///
/// The handle *is* the state, boxed. Not an index into a registry: a registry means a lock and a
/// lookup on every call, and there is nothing for either to buy here. A map is driven from one
/// thread — the same contract every consumer of this kind already has, and the same one mbgl's
/// own sink states — so the calls that would contend never race.
///
/// Null is never handed out, so a caller that ignores a status cannot mistake a failed create
/// for a working map.
pub type MapHandle = *mut MapState;

/// Where a consumer reads from.
///
/// Two ranges in *this process's* address space. That is the whole point of the staticlib
/// arrangement (§3.5): the ring and the arena are ordinary memory the consumer can read
/// directly, so geometry reaches the GPU out of the producer's own allocation and nothing is
/// copied to make it reachable. Across a process boundary the same two ranges would be mapped
/// instead, and nothing else about the protocol would change.
///
/// Valid until the map is destroyed. The ring's control block is at its start, as the header
/// describes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Regions {
    /// The ring: control block, then the data region.
    pub ring: *const u8,
    /// Its length in bytes.
    pub ring_len: usize,
    /// The slab region every `tsl_slab_ref` resolves against.
    pub slabs: *const u8,
    /// Its length in bytes.
    pub slabs_len: usize,
}

/// How the map is set up.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// The style document, as JSON. A URL is not accepted here: fetching it is the caller's,
    /// because a caller that already has the bytes should not be made to serve them back.
    pub style_json: *const c_char,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Ring capacity in bytes. Rounded up to a power of two, which the ring requires.
    pub ring_capacity: usize,
}

/// One map's state.
///
/// Boxed and handed to C as its handle. The ring's backing buffer is a `Vec<u64>`, so the
/// `Producer`'s pointer into it survives this struct being moved — the heap allocation does not
/// move when the `Vec` does.
// Held for the tick, which is not written: nothing in `tessella-orchestrate` composes cover ->
// build -> emit into a running frame loop yet, so there is no warm entry point to call. See the
// note on `tessella_regions`.
#[allow(dead_code)]
pub struct MapState {
    style: Style,
    view: ViewTransform,
    /// The ring's backing memory. `u64` so it is eight-aligned, which `ring::init` requires.
    region: Vec<u64>,
    producer: Producer,
    arena: SlabArena,
    files: Arc<Coalescing<HttpFileSource>>,
    cache: Arc<TileCache<boot::BootError>>,
    /// Slabs packed by the last frame, kept alive until the consumer acknowledges them.
    packed: Vec<u8>,
}

/// Runs `body`, turning a panic into a status.
///
/// Unwinding into C is undefined, so nothing may escape. A panic here is a producer bug and the
/// consumer's only useful response is to report it, which is what the status is for.
/// `AssertUnwindSafe` because the compiler's question — could a caller observe a broken
/// invariant after a panic? — is answered by the boundary rather than by the types. A panic ends
/// the call and the caller gets a status; the only state that could be half-written is the map's,
/// and a caller that meets `Failed` has no operation to resume.
fn guarded<F: FnOnce() -> Status>(body: F) -> Status {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(Status::Failed)
}

/// Copies a borrowed C string.
///
/// # Safety
///
/// `text` must be null or a valid NUL-terminated string.
unsafe fn borrowed(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(text) }
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

/// Creates a map and boots it: parses the style, resolves its sources, covers the camera and
/// builds the first tiles.
///
/// The camera starts where [`set_camera`] would put it; a caller that wants somewhere else calls
/// that before the first tick rather than paying for a cover it will not draw.
///
/// # Safety
///
/// `config` and `out` must be valid pointers, and `config.style_json` a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_create(
    config: *const Config,
    latitude: f64,
    longitude: f64,
    zoom: f64,
    out: *mut MapHandle,
) -> Status {
    guarded(move || {
        if config.is_null() || out.is_null() {
            return Status::NullArgument;
        }
        let config = unsafe { *config };
        let Some(style_text) = (unsafe { borrowed(config.style_json) }) else {
            return Status::NullArgument;
        };
        let Ok(style) = Style::parse(&style_text) else {
            return Status::BadStyle;
        };

        let view = camera::settled(&ViewTransform {
            longitude,
            latitude,
            zoom,
            width: f64::from(config.width),
            height: f64::from(config.height),
            bearing: 0.0,
            pitch: 0.0,
        });

        // Eight-aligned by construction, which `ring::init` requires, and sized to a power of
        // two, which the ring's masking arithmetic does.
        let capacity = config.ring_capacity.max(1 << 20).next_power_of_two();
        let mut region = alloc::vec![0u64; region_size(capacity).div_ceil(8)];
        // SAFETY: the buffer is `region_size(capacity)` bytes, eight-aligned because it is a
        // `Vec<u64>`, and lives as long as the state that owns it. The consumer half is dropped:
        // C reads the region directly through `tessella_regions`.
        let (producer, _consumer) =
            unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), capacity) };

        let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
            30,
        ))));
        let cache = Arc::new(TileCache::new(64));
        let booted = boot::cold_start(&ColdStart {
            style: &style_text,
            view: &view,
            files: Arc::clone(&files),
            cache: Arc::clone(&cache),
            pool: Pool::shared(),
            priority: Priority::Foreground,
            style_rev: 1,
        });
        if booted.is_err() {
            return Status::Failed;
        }

        let state = Box::new(MapState {
            style,
            view,
            region,
            producer,
            arena: SlabArena::new(),
            files,
            cache,
            packed: Vec::new(),
        });
        unsafe { *out = Box::into_raw(state) };
        Status::Ok
    })
}

/// Moves the camera.
///
/// Does not draw. A camera that has not moved emits nothing on the next tick, which is what
/// keeps traffic proportional to change — so this is cheap to call every frame and the caller
/// need not track whether anything moved.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_set_camera(
    map: MapHandle,
    latitude: f64,
    longitude: f64,
    zoom: f64,
    bearing: f64,
    pitch: f64,
) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };
        state.view = camera::settled(&ViewTransform {
            longitude,
            latitude,
            zoom,
            bearing,
            pitch,
            ..state.view
        });
        Status::Ok
    })
}

/// The two ranges a consumer reads from.
///
/// `slabs` is empty until a frame has been emitted, and no frame is emitted yet: `frame::emit`
/// exists and is called from thirteen test files, and from nothing in `src/`. There is no warm
/// per-frame entry point in the orchestrator — every test composes cover, build and emit by hand
/// — and writing that composition *here* would put orchestration in the FFI layer, which is the
/// wrong home for it. It belongs beside the cover, the cache, the pool and the registry.
///
/// # Safety
///
/// `map` must be live and `out` a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_regions(map: MapHandle, out: *mut Regions) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        if out.is_null() {
            return Status::NullArgument;
        }
        unsafe {
            *out = Regions {
                ring: state.region.as_ptr().cast::<u8>(),
                ring_len: state.region.len() * core::mem::size_of::<u64>(),
                slabs: state.packed.as_ptr(),
                slabs_len: state.packed.len(),
            };
        }
        Status::Ok
    })
}

/// Destroys a map and everything it owns.
///
/// The regions it handed out are invalid the moment this returns, so a consumer with buffers
/// still in flight must have acknowledged them first.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`], destroyed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_destroy(map: MapHandle) {
    if map.is_null() {
        return;
    }
    // A drop that panics would unwind into C. There is nothing useful to report from here, so it
    // is caught and swallowed rather than allowed out.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        drop(unsafe { Box::from_raw(map) });
    }));
}
