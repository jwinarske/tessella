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
    /// The slab region had no room for the frame's geometry. Unlike [`Self::RingFull`] this does
    /// not clear by draining: the arena bump allocates, so the space a swept slab left is only
    /// recovered once everything above it has gone. The frame compacts and the next tick retries;
    /// a map that reports this every tick needs a larger `slab_capacity`.
    RegionFull = 7,
    /// Something failed in a way this ABI has no more specific word for. The producer logs it.
    Failed = 6,
    /// A hosted call was made on a map that fetches for itself.
    ///
    /// Distinct from [`Self::Failed`] because it is a fixable mistake with an obvious fix: the
    /// map wanted [`tessella_create_hosted`]. A map created either way is otherwise identical, so
    /// there is nothing else that would tell a caller which one it has.
    NotHosted = 8,
}

extern crate alloc;

use alloc::sync::Arc;
use std::ffi::c_char;

use tessella_capture_abi::ProjectionMode;
use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, Producer, region_size};
use tessella_orchestrate::cache::TileCache;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tessella_orchestrate::deferred::PoolBacked;
use tessella_orchestrate::deferred::{HostTransport, TileTransport};
use tessella_orchestrate::map::{Map, SpriteAtlas, Tick};
use tessella_orchestrate::pool::{Pool, Priority};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tessella_orchestrate::source::Coalesced;
use tessella_orchestrate::source::{Readiness, TileSource};
use tessella_storage::deferred::{DeferredFileSource, Ticket};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tessella_storage::http::HttpFileSource;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tessella_storage::pmtiles::source::{self as pmtiles_source, PmtilesFileSource};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tessella_storage::source::{Coalescing, RangeFetch, Router};
use tessella_style::Style;
use tessella_tile::camera;
use tessella_tile::cover::{self, ViewTransform};

/// The texture the sprite atlas is uploaded as.
///
/// Fixed rather than allocated: there is one sheet per style and the consumer learns of it from
/// the `TextureUpdate` that carries its pixels, so a number chosen here is a number nothing has
/// to agree on.
const SPRITE_TEXTURE: tessella_capture_abi::envelope::TextureId =
    tessella_capture_abi::envelope::TextureId(1);

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
    ///
    /// A pointer and a length rather than a NUL-terminated string, on every target rather than
    /// only the one that needs it. A `CStr` was a convenience for C callers and nothing else: a
    /// browser hands over a byte range in linear memory, which has no terminator to find, and
    /// keeping two signatures honest is harder than keeping one. It also costs a scan the caller
    /// has already done -- a `std::string` or a `Uint8Array` knows its own length.
    pub style_json: *const u8,
    /// Its length in bytes, not counting any terminator the caller happens to have.
    pub style_json_len: usize,
    /// Viewport width in pixels.
    pub width: u32,
    /// Viewport height in pixels.
    pub height: u32,
    /// Ring capacity in bytes. Rounded up to a power of two, which the ring requires.
    pub ring_capacity: usize,
    /// Slab region capacity in bytes, where the frame's geometry is written.
    ///
    /// Zero takes [`DEFAULT_SLAB_CAPACITY`]. The region is where vertex and index bytes live,
    /// and the consumer reads them in place -- so this is the working set of everything on
    /// screen plus what compaction has not yet reclaimed, not a per-frame buffer.
    ///
    /// A frame that does not fit is refused whole and the next one retries after the arena has
    /// compacted, so a region that is too small shows as a map that will not finish drawing
    /// rather than as corruption.
    pub slab_capacity: usize,
}

/// Slab region capacity when [`Config::slab_capacity`] is zero.
///
/// Sixty-four mebibytes. Four street-level views of a dense city settle inside twelve, and the
/// headroom is for the transient during a pan, when the tiles being left have not been swept
/// and the tiles being entered are already allocated.
pub const DEFAULT_SLAB_CAPACITY: usize = 64 << 20;

/// Table slots reserved in the slab region.
///
/// One per live slab, fixed because the table is at the front of the region and a handle indexes
/// it. Sixteen bytes each, so four thousand costs sixty-four kilobytes of the region and is well
/// past what a view holds: a slab is a run of geometry, not a drawable.
pub const SLAB_SLOTS: usize = 4096;

/// One map's state.
///
/// Boxed and handed to C as its handle. The ring's backing buffer is a `Vec<u64>`, so the
/// `Producer`'s pointer into it survives this struct being moved -- the heap allocation does not
/// move when the `Vec` does.
///
/// Where a map's bytes come from.
///
/// An enum rather than a `dyn` or a second map type. The two arrangements are genuinely different
/// -- one blocks on a worker, the other writes down what it needs and waits to be told -- and
/// there are exactly two of them, so naming both costs a match and keeps the job path free of a
/// virtual call. The same reasoning DR-24 gives for `Pool`.
pub enum Transport {
    /// Native: a blocking source, waited on where waiting is cheap.
    ///
    /// Absent on wasm32, where `ureq` is `std::net` and there are no sockets. Leaving it in would
    /// be a megabyte of dead weight behind an entry point that could only ever fail.
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    Pooled(PoolBacked<Coalesced<Router>>),
    /// Hosted: the consumer fetches, which is the only thing a browser can do.
    Hosted(HostTransport),
}

impl DeferredFileSource for Transport {
    fn request(&self, url: &str, etag: Option<&str>) -> Ticket {
        match self {
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(transport) => transport.request(url, etag),
            Self::Hosted(transport) => transport.request(url, etag),
        }
    }

    fn poll(&self, ticket: Ticket) -> Option<tessella_storage::source::Fetched> {
        match self {
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(transport) => transport.poll(ticket),
            Self::Hosted(transport) => transport.poll(ticket),
        }
    }

    fn cancel(&self, ticket: Ticket) {
        match self {
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(transport) => transport.cancel(ticket),
            Self::Hosted(transport) => transport.cancel(ticket),
        }
    }

    fn outstanding(&self) -> usize {
        match self {
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(transport) => transport.outstanding(),
            Self::Hosted(transport) => transport.outstanding(),
        }
    }
}

impl TileTransport for Transport {
    fn request_at(&self, priority: Priority, url: &str, etag: Option<&str>) -> Ticket {
        match self {
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(transport) => transport.request_at(priority, url, etag),
            // One queue, so the class has nowhere to go. Not dropped on the floor: the default
            // is what a transport with one queue means by it.
            Self::Hosted(transport) => transport.request_at(priority, url, etag),
        }
    }
}

impl Transport {
    /// The hosted half, for the calls that only make sense on one.
    fn hosted(&self) -> Option<&HostTransport> {
        match self {
            Self::Hosted(transport) => Some(transport),
            #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
            Self::Pooled(_) => None,
        }
    }
}

/// The tiles come from a [`TileSource`], which is where every blocking thing now lives: §16
/// closed on Fluorite's answer that no blocking call is acceptable on the thread that will make
/// it, so `create` parses the style and nothing else, and the network starts on the first tick.
pub struct MapState {
    map: Map,
    /// The ring's backing memory. `u64` so it is eight-aligned, which `ring::init` requires.
    region: Vec<u64>,
    /// The slab region's backing memory, which the map's arena writes into and C reads through
    /// `tessella_regions`. Allocated once and never resized, so the `Mapping` the arena holds
    /// stays valid for as long as this state does.
    slabs: Vec<u64>,
    producer: Producer,
    /// Where tiles come from. Shared between views by construction, so a tile two maps want is
    /// fetched once and built once.
    source: Arc<TileSource<Transport>>,
    /// What the source had landed when this map last drew.
    ///
    /// A tile arriving on a worker does not move the camera, so the damage gate would call the
    /// frame idle and the tiles would sit in the source, built and never drawn. Comparing this is
    /// what turns a landing back into a frame worth emitting.
    generation: u64,
    /// Whether the sprite sheet has been handed to the map.
    ///
    /// Once, not every tick: `set_sprites` copies the atlas and marks the map dirty, so repeating
    /// it would re-upload a texture that has not changed and defeat the damage gate that makes a
    /// settled map free.
    sprites_set: bool,
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

/// Copies a borrowed byte range, if it is a string.
///
/// [`None`] means the bytes are not UTF-8. That is a *content* fault rather than an argument
/// fault, and the caller is told so: a document that arrived mis-decoded should be reported as a
/// style that did not parse, not built into a map full of replacement characters. Nullness is the
/// caller's to check first, because `(null, 0)` means "not supplied" and `(p, 0)` means "supplied,
/// and empty" -- two different mistakes deserving two different answers.
///
/// # Safety
///
/// `text` must be non-null and valid for reads of `len` bytes.
unsafe fn borrowed(text: *const u8, len: usize) -> Option<String> {
    // SAFETY: the caller guarantees `len` readable bytes at a non-null `text`. A zero length is
    // allowed and yields an empty slice, which `from_raw_parts` requires an aligned non-null
    // pointer for -- hence the caller's null check rather than a shrug here.
    let bytes = unsafe { core::slice::from_raw_parts(text, len) };
    core::str::from_utf8(bytes).ok().map(ToOwned::to_owned)
}

/// Creates a map. Parses the style, and does nothing else.
///
/// No network, no cover, no tiles: §16 closed on Fluorite's answer that no blocking call is
/// acceptable on the thread that will make it -- its bindings are `@Native` on the calling
/// isolate with no hop, so a blocking create freezes the application rather than the map. The
/// first [`tessella_tick`] is what starts the network.
///
/// A style that does not parse still fails here, which is the one failure worth reporting at the
/// call that can act on it. A style that parses but whose *sources* will not resolve cannot fail
/// here, because finding that out is the round trip this call exists not to make -- so it is
/// reported by [`tessella_status`], which is what a consumer reads before it wonders why the map
/// is empty.
///
/// The camera starts where [`tessella_set_camera`] would put it; a caller that wants somewhere
/// else calls that before the first tick rather than covering a view it will not draw.
///
/// # Safety
///
/// `config` and `out` must be valid pointers, and `config.style_json` either null or valid for
/// reads of `config.style_json_len` bytes.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_create(
    config: *const Config,
    latitude: f64,
    longitude: f64,
    zoom: f64,
    out: *mut MapHandle,
) -> Status {
    // SAFETY: the caller's contract, unchanged, and passed straight through.
    unsafe {
        create(config, latitude, longitude, zoom, out, || {
            Transport::Pooled(PoolBacked::new(
                Arc::new(Coalesced(Arc::new(Coalescing::new(origins())))),
                Pool::shared(),
                Priority::Background,
            ))
        })
    }
}

/// Creates a map whose fetching the caller does.
///
/// As [`tessella_create`], but nothing is fetched by the map. It writes down what it needs and
/// the caller brings it back through [`tessella_take_request`] and [`tessella_answer`].
///
/// For a browser, where there is no other option: `std::net` has no sockets there and blocking on
/// the thread that draws is not available either. It is not only for a browser -- a host with its
/// own connection pool, its own cache, or its own idea of when a fetch is allowed uses the same
/// three calls, and a test uses them to drive a map with no network at all.
///
/// The map still needs ticking. A hosted map with nobody calling [`tessella_tick`] asks for
/// nothing, because it is the tick that notices what has arrived and decides what to want next.
///
/// # Safety
///
/// As [`tessella_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_create_hosted(
    config: *const Config,
    latitude: f64,
    longitude: f64,
    zoom: f64,
    out: *mut MapHandle,
) -> Status {
    // SAFETY: the caller's contract, unchanged, and passed straight through.
    unsafe {
        create(config, latitude, longitude, zoom, out, || {
            Transport::Hosted(HostTransport::new())
        })
    }
}

/// Where a native map's bytes come from, by url.
///
/// One HTTP source, shared: the router's fallback fetches whole resources, and the same transport
/// reads byte ranges out of a `pmtiles://` archive. Two would mean two connection pools to the
/// same origin.
///
/// The router goes *inside* the coalescing wrapper rather than outside, which is the arrangement
/// `Router` documents and §9.3's flatness counters depend on: a router of coalescers gives each
/// origin its own in-flight table, and four views over one cover stop costing one fetch.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn origins() -> Router {
    let http = Arc::new(HttpFileSource::new(std::time::Duration::from_secs(30)));
    Router::new()
        // An archive read where it lies. Without the range transport this would refuse every
        // `pmtiles://https://` url naming the constructor it wanted, which is why it is handed
        // the same source the fallback uses.
        .route(
            pmtiles_source::accepts,
            PmtilesFileSource::with_ranges(Arc::clone(&http) as Arc<dyn RangeFetch>),
        )
        .otherwise(http)
}

/// The body both constructors share, differing only in where the bytes will come from.
///
/// # Safety
///
/// As [`tessella_create`].
unsafe fn create(
    config: *const Config,
    latitude: f64,
    longitude: f64,
    zoom: f64,
    out: *mut MapHandle,
    transport: impl FnOnce() -> Transport,
) -> Status {
    guarded(move || {
        if config.is_null() || out.is_null() {
            return Status::NullArgument;
        }
        let config = unsafe { *config };
        if config.style_json.is_null() {
            return Status::NullArgument;
        }
        // Not UTF-8 is a bad style rather than a bad argument. The pointer was fine; what it
        // pointed at was not a document.
        let Some(style_text) = (unsafe { borrowed(config.style_json, config.style_json_len) })
        else {
            return Status::BadStyle;
        };
        let Ok(style) = Style::parse(&style_text) else {
            return Status::BadStyle;
        };

        // Constrained here too: the first camera is a camera, and one that shows the world's
        // edge is refused at create for the same reason it is refused later.
        let view = camera::settled(&camera::constrained(&ViewTransform {
            longitude,
            latitude,
            zoom,
            width: f64::from(config.width),
            height: f64::from(config.height),
            bearing: 0.0,
            pitch: 0.0,
        }));

        // Eight-aligned by construction, which `ring::init` requires, and sized to a power of
        // two, which the ring's masking arithmetic does.
        let capacity = config.ring_capacity.max(1 << 20).next_power_of_two();
        let mut region = alloc::vec![0u64; region_size(capacity).div_ceil(8)];
        // SAFETY: the buffer is `region_size(capacity)` bytes, eight-aligned because it is a
        // `Vec<u64>`, and lives as long as the state that owns it. The consumer half is dropped:
        // C reads the region directly through `tessella_regions`.
        let (producer, _consumer) =
            unsafe { ring::init(region.as_mut_ptr().cast::<u8>(), capacity) };

        // The slab region. The arena writes the frame's geometry straight into it and the
        // consumer reads it there, so there is no serialise step: an owned arena has to rebuild
        // the whole region every frame, which on a moving map is most of the frame.
        let slab_capacity = if config.slab_capacity == 0 {
            DEFAULT_SLAB_CAPACITY
        } else {
            config.slab_capacity
        };
        let mut slabs = alloc::vec![0u64; slab_capacity.div_ceil(8)];
        // SAFETY: the buffer is `slab_capacity` bytes rounded up to eight, eight-aligned because
        // it is a `Vec<u64>`, and lives as long as the state that owns it -- it is allocated
        // here and never resized, so the heap block does not move when the state does. Nothing
        // else writes it: the arena is the only writer and C only reads.
        let mapping = unsafe {
            tessella_capture_abi::mapping::Mapping::new(
                slabs.as_mut_ptr().cast::<u8>(),
                slabs.len() * core::mem::size_of::<u64>(),
            )
        };
        let arena = tessella_orchestrate::emit::SlabArena::in_region(mapping, SLAB_SLOTS);

        let cache = Arc::new(TileCache::new(64));
        let source =
            TileSource::with_transport(style_text, Arc::new(transport()), cache, Pool::shared(), 1);

        let state = Box::new(MapState {
            map: Map::with_arena(style, view, ViewId(0), arena),
            region,
            slabs,
            producer,
            source,
            generation: 0,
            sprites_set: false,
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
        // Constrained first, the way mbgl's Transform constrains every camera it is given: a
        // zoom that would show the world's edge is not a camera a map accepts. See
        // `camera::constrained`.
        state
            .map
            .look_at(camera::settled(&camera::constrained(&ViewTransform {
                longitude,
                latitude,
                zoom,
                bearing,
                pitch,
                ..*state.map.view()
            })));
        Status::Ok
    })
}

/// Tells a map how much time has passed, so its labels can fade.
///
/// A map that is never told this behaves as a still picture: a fade completes in one step and a
/// label appears or disappears outright. That is what `mbgl-render` does -- `symbolFadeChange`
/// returns one in static map mode -- and it is what every parity capture on both sides has been
/// comparing, so it stays the default.
///
/// It is the wrong behaviour for a map somebody is looking at. A label that stops being placed at
/// one anchor and starts at another along the same road, with nothing fading between the two, is
/// read as the text having *moved*. Call this once a frame with the milliseconds since the last
/// one and the fades run at mbgl's rate.
///
/// Does not emit; the next [`tessella_tick`] does.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_advance(map: MapHandle, elapsed_millis: f64) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };
        state.map.advance(elapsed_millis);
        Status::Ok
    })
}

/// Changes the viewport a map draws into.
///
/// A window resize is not a new map. Before this the size was settable only at
/// [`tessella_create`], so a consumer whose surface changed had no option but to destroy the map
/// and build another -- every tile refetched, every bucket rebuilt, every glyph re-shaped, for a
/// change that moves no camera. What survives a resize now is everything a resize does not change:
/// the tiles, their buckets, the layouts, and the label identities with the fades keyed on them.
///
/// Does not emit. The next tick sees a changed camera -- the viewport is part of what makes a
/// camera differ -- and rewrites the matrices by the path a pan takes.
///
/// A width or height that is zero or not finite is ignored rather than refused: a surface being
/// torn down reports one, and a map that returned an error there would have the consumer handling
/// a condition that resolves itself on the next resize.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_set_viewport(map: MapHandle, width: u32, height: u32) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };
        state.map.resize(f64::from(width), f64::from(height));
        Status::Ok
    })
}

/// What surface a view's tiles will be drawn on.
///
/// The producer's whole part in the globe. Placement is unaffected — a globe bends Mercator
/// geometry per vertex in the consumer's material, so what travels on the wire is the ordinary
/// flat placement either way — but *selection* is not: a Mercator plane repeats horizontally and
/// a sphere does not, so a globe drawing a flat cover draws the same patch of the world once per
/// world copy.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldCopies {
    /// A plane, which repeats horizontally. The default, and what a Mercator map wants.
    Repeated = 0,
    /// A sphere, which has one of everything.
    ///
    /// At zoom 0 four of five cover tiles are copies and at zoom 1 four of eight — most of the
    /// cover rather than an edge case — and drawing them is z-fighting on the surface plus
    /// subdivision paid twice at the levels where subdivision is dearest.
    One = 1,
}

/// Sets the surface a map's tiles are covered for.
///
/// Does not emit, and needs no invalidation: the cover is recomputed every frame, so the next
/// tick sees a different set of tiles by the same path a pan takes.
///
/// The horizon is deliberately not here. Tiles a sphere has curved out of sight are four to six
/// of the cheapest on the map between zoom 1 and 2.5 and *none* outside that band, which does not
/// pay for a spherical cull on this side — it is one dot product per tile in the consumer, before
/// it subdivides, which removes the draw as well as the tile.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_set_world_copies(map: MapHandle, copies: WorldCopies) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };
        state.map.draw_on(match copies {
            WorldCopies::Repeated => cover::WorldCopies::Repeated,
            WorldCopies::One => cover::WorldCopies::One,
        });
        Status::Ok
    })
}

/// The surface a map projects its tiles through.
///
/// Mirrors `tsl_projection_mode` on the wire. A toggle rather than a mode a map is created in:
/// MapLibre switches projection at runtime and so does this.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projection {
    /// The plane. The camera's `proj_matrix` is the whole projection.
    Mercator = 0,
    /// The sphere. The camera carries a `globe_matrix` and the consumer's vertex stage supplies
    /// the nonlinear step between them.
    Globe = 1,
}

/// Sets the projection a map draws through.
///
/// Does not emit and needs no invalidation: the camera block is rebuilt every frame, so the next
/// one carries the new matrix by the same path a pan takes.
///
/// This does *not* change [`tessella_set_world_copies`]. A globe almost always wants
/// [`WorldCopies::One`] alongside it — every wrap of a tile bends to the same patch, so a globe
/// drawing a repeated cover draws that patch twice and z-fights with itself — but one call
/// silently moving another setting is worse than two calls, and the cover policy is measurable on
/// its own where the projection is not.
///
/// Under [`Projection::Globe`] the consumer's vertex stage owes the bend: `tile-local ->
/// normalized Mercator -> sphere -> clip`, of which the camera carries the last step. A consumer
/// that sets this and draws nothing different has not implemented it, and the producer cannot
/// tell.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_set_projection(map: MapHandle, projection: Projection) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };
        state.map.project_on(match projection {
            Projection::Mercator => ProjectionMode::Mercator,
            Projection::Globe => ProjectionMode::Globe,
        });
        Status::Ok
    })
}

/// Emits a frame, if anything changed.
///
/// Returns [`Status::Ok`] whether or not a frame was emitted: a settled map sending nothing is
/// the ordinary case rather than a condition to report, and a caller polling at display rate
/// would spend more code distinguishing the two than acting on it. What changed is on the ring;
/// what did not is the absence of records.
///
/// # What it costs when nothing happened
///
/// A comparison. The damage gate returns before the cover, the cache, the arena or the ring are
/// touched, which is what makes calling this every vsync the right thing to do.
///
/// # Safety
///
/// `map` must be a handle from [`tessella_create`] that has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_tick(map: MapHandle) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_mut() }) else {
            return Status::NoSuchMap;
        };

        // Anything landed since the last frame makes this one worth drawing.
        // A pool with no workers has nobody to run its jobs but this thread. Twice, around the
        // transport's drain, because the two feed each other: the first call runs the fetches
        // and the style resolution queued last tick, the transport's drain turns whatever landed
        // into builds, and the second runs those builds so this frame draws them rather than the
        // next one. A pool that *has* workers is left alone -- helping would spend the frame
        // budget on somebody else's decode, which is what the workers are for.
        //
        // Unbounded on purpose. This is the deterministic mode, and a trace only reproduces if a
        // tick means "everything that was ready", not "as much as fitted".
        let serial = tessella_orchestrate::pool::Pool::shared();
        if serial.workers() == 0 {
            serial.drain(usize::MAX);
        }

        // Before anything else, and `generation` in particular: a tile whose bytes landed since
        // the last tick becomes a build here, and a drain that ran after the read below would
        // put a frame of lag on every tile the deferred transport delivers.
        state.source.drain();

        if serial.workers() == 0 {
            serial.drain(usize::MAX);
        }

        let generation = state.source.generation();
        if generation != state.generation {
            state.generation = generation;
            state.map.mark_dirty();
        }

        // Glyphs are a hand-off rather than a copy -- `Fonts` is not `Clone` -- so this answers
        // on exactly one tick, the one after the fetch lands.
        if let Some(fonts) = state.source.take_fonts() {
            state.map.set_fonts(fonts);
        }

        // The sheet, once. Behind the `image` feature because a sheet arrives as a PNG: a build
        // without it draws patterns as plain fills and icons not at all, which is the same frame
        // a style with no `sprite` produces.
        #[cfg(feature = "image")]
        if !state.sprites_set
            && let Some(sources) = state.source.sources()
            && let Some((sheet, _)) = sources.sprite_outcome.as_ref()
        {
            let (width, height) = sheet.atlas().size();
            state.map.set_sprites(SpriteAtlas {
                texture: SPRITE_TEXTURE,
                size: [
                    u16::try_from(width).unwrap_or(u16::MAX),
                    u16::try_from(height).unwrap_or(u16::MAX),
                ],
                positions: sheet.positions().clone(),
                pixels: sheet.atlas().pixels().to_vec(),
            });
            state.sprites_set = true;
        }

        let outcome = match state.map.tick(&mut state.producer, &state.source) {
            Ok(Tick::Idle) => Status::Ok,
            // Nothing to pack: the arena wrote into the shared region as it went, and the
            // record naming a slab was published after the bytes were in place. §11.3's window
            // is closed by construction rather than by ordering a copy after the frame.
            Ok(Tick::Emitted(_)) => Status::Ok,
            // The consumer is behind. Nothing was emitted and nothing was retired, so draining
            // and calling again resumes from where this attempt started.
            //
            // Marked dirty again, which is the whole of the retry. Without it the failed attempt
            // has *spent* the damage gate: the frame did not fit, nothing was emitted, and the
            // next tick sees a map with nothing to redraw, so it goes idle for ever holding a
            // frame it never sent. Found on OpenFreeMap's liberty, whose first frame is 1.5
            // million vertices and does not fit a four-megabyte ring -- it reported a ready map,
            // every tile landed, and six records on the wire.
            Err(tessella_orchestrate::frame::FrameError::RegionFull) => {
                // The frame emitted nothing, but it did run the compaction that empties
                // poorly-packed slabs, so the retry has room the attempt did not. Dirty for the
                // same reason a full ring is: without it the spent damage gate leaves the map
                // idle holding a frame it never sent.
                state.map.mark_dirty();
                Status::RegionFull
            }
            Err(_) => {
                state.map.mark_dirty();
                Status::RingFull
            }
        };

        // Says what to fetch *after* drawing, not before, because `tick` is what recomputes the
        // list: asking first would schedule the previous frame's wants and put a frame of lag on
        // every pan. The call schedules and returns -- what has not landed is simply absent from
        // the next frame, which the loop draws by substituting the ancestor it has.
        //
        // Runs even when the ring was full. Nothing was emitted, but what the camera wants did
        // not depend on that, and a consumer that has fallen behind is the last one that should
        // also be made to wait for its tiles.
        let view = *state.map.view();
        state
            .source
            .want(&view, state.map.wanted(), state.map.speculative());
        outcome
    })
}

/// How far along the map's sources are, and why if they failed.
///
/// How much work is still in flight.
///
/// Tiles asked for and not yet answered, plus a glyph fetch that has not finished. Zero means
/// nothing further will arrive without another tick -- not that the map is complete, since a tile
/// that failed is finished and still a hole. [`tessella_status`] answers that half.
///
/// The question a caller waiting for a settled frame is asking, and one that could not be asked
/// before: the render probe waited for records to stop arriving instead, which a source blocked on
/// a fetch satisfies just as well as a source that has finished. The same scene measured 0, 272
/// and 9,520 differing pixels against the oracle across runs that all believed they had settled.
/// A consumer driving a progress indicator reads the same number.
///
/// # Safety
///
/// `map` must be live and `out_pending` a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_pending(map: MapHandle, out_pending: *mut u64) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        if out_pending.is_null() {
            return Status::NullArgument;
        }
        unsafe {
            *out_pending = state.source.outstanding() as u64;
        }
        Status::Ok
    })
}

/// The call §16 traded for a non-blocking [`tessella_create`]. A consumer holding a handle and
/// looking at an empty map cannot tell a style still resolving from one whose sources will never
/// answer, and inferring it from the absence of tiles gets it wrong in both directions -- so it
/// is reported. "Silently blank" is the hazard this project has caught three times; this is what
/// answers it.
///
/// `reason` may be null, and is written only when the readiness is [`Readiness::Failed`]. It is
/// always NUL-terminated when written, and truncated to fit rather than refused.
///
/// # Safety
///
/// `map` must be live, `out_readiness` a valid pointer, and `reason` either null or writable for
/// `reason_cap` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_status(
    map: MapHandle,
    out_readiness: *mut i32,
    reason: *mut c_char,
    reason_cap: usize,
) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        if out_readiness.is_null() {
            return Status::NullArgument;
        }
        let readiness = state.source.readiness();
        let (failed, first) = state.source.failures();
        unsafe {
            *out_readiness = match readiness {
                Readiness::Idle => 0,
                Readiness::Resolving => 1,
                Readiness::Ready => 2,
                Readiness::Failed(_) => 3,
            };
        }

        // A ready map with failing tiles is the state nothing else explains: resolved, drawing,
        // and empty. It is not a failed source -- a tile that will not load is a hole the
        // ancestor fills -- so the readiness stays `Ready` and the reason says what happened.
        let described = match &readiness {
            Readiness::Failed(message) => Some(message.clone()),
            _ if failed > 0 => first
                .map(|reason| alloc::format!("{failed} tile(s) failed to build; first: {reason}")),
            // Ready, nothing failing, and still nothing drawn. The remaining explanation is that
            // the layers which would have drawn were refused before a tile was ever asked for.
            _ => {
                let (rejected, why) = state.source.rejected();
                why.filter(|_| rejected > 0).map(|reason| {
                    alloc::format!("{rejected} layer(s) rejected as uncompilable; first: {reason}")
                })
            }
        };
        if let (Some(message), false) = (described, reason.is_null())
            && reason_cap > 0
        {
            // One byte held back for the terminator, and the copy stops at a character boundary
            // so a truncated reason is still a string rather than half a codepoint.
            let room = reason_cap - 1;
            let mut end = message.len().min(room);
            while end > 0 && !message.is_char_boundary(end) {
                end -= 1;
            }
            let bytes = &message.as_bytes()[..end];
            // SAFETY: `reason` is writable for `reason_cap` bytes, and `end < reason_cap`.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), reason, end);
                *reason.add(end) = 0;
            }
        }
        Status::Ok
    })
}

/// Takes the next thing a hosted map wants fetched.
///
/// Answers ticket `0` when there is nothing to fetch, which is not an error: it is what a settled
/// map says, and it is the condition a caller loops until. Zero is never a real ticket, so there
/// is no other value to confuse it with.
///
/// The URL is a byte range in the map's own memory -- the same arrangement [`tessella_regions`]
/// uses for the ring, and for the same reason: the alternative is an allocator export and a copy
/// on each side of it. It stays valid until the ticket is answered, failed, or the map is
/// destroyed. A caller that holds it past any of those holds a dangling pointer.
///
/// [`Status::NotHosted`] for a map created by [`tessella_create`], which fetches for itself.
///
/// # Safety
///
/// `map` must be live, and the three out pointers valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_take_request(
    map: MapHandle,
    out_ticket: *mut u64,
    out_url: *mut *const u8,
    out_url_len: *mut usize,
) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        if out_ticket.is_null() || out_url.is_null() || out_url_len.is_null() {
            return Status::NullArgument;
        }
        let Some(hosted) = state.source.transport().hosted() else {
            return Status::NotHosted;
        };

        // Written before anything else, so a caller that ignores the status still reads "nothing
        // to fetch" rather than whatever was in its variables.
        unsafe {
            *out_ticket = 0;
            *out_url = core::ptr::null();
            *out_url_len = 0;
        }

        // A queued ticket whose URL has gone is one that was answered or cancelled between being
        // queued and being asked for. Skipped rather than reported: nobody wants it fetched.
        while let Some(ticket) = hosted.next_request() {
            if let Some((url, len)) = hosted.url_of(ticket) {
                unsafe {
                    *out_ticket = ticket.into_raw();
                    *out_url = url;
                    *out_url_len = len;
                }
                break;
            }
        }
        Status::Ok
    })
}

/// Answers a request with what the caller fetched.
///
/// `status` is the origin's. A `404` is an answer rather than a failure -- an absent tile is an
/// edge of a source's coverage, which the map draws around, and reporting it as a broken fetch
/// would make a hole look like a fault. [`tessella_fail_request`] is for a fetch that did not
/// happen at all.
///
/// A ticket that was cancelled, already answered, or never issued is ignored and answers
/// [`Status::Ok`]: a caller that has lost track of its own bookkeeping has wasted a fetch, which
/// is not something the map can fix by refusing.
///
/// # Safety
///
/// `map` must be live, and `body` either null or valid for reads of `body_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_answer(
    map: MapHandle,
    ticket: u64,
    status: u16,
    body: *const u8,
    body_len: usize,
) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        let Some(hosted) = state.source.transport().hosted() else {
            return Status::NotHosted;
        };
        // An empty body is legitimate -- a tile with no features is a valid, empty tile -- so a
        // null pointer with a zero length is a real answer rather than a missing argument.
        let bytes = if body.is_null() {
            if body_len != 0 {
                return Status::NullArgument;
            }
            Vec::new()
        } else {
            // SAFETY: the caller guarantees `body_len` readable bytes at `body`. Copied rather
            // than borrowed: the map holds it past this call and the caller's buffer is the
            // caller's.
            unsafe { core::slice::from_raw_parts(body, body_len) }.to_vec()
        };
        hosted.answer(Ticket::from_raw(ticket), status, bytes);
        Status::Ok
    })
}

/// Answers a request the caller could not fetch at all.
///
/// For a connection that never opened, not for an origin that said no -- that is
/// [`tessella_answer`] with the status it said it with. The map treats it as it treats any
/// transport failure: the tile is a hole, counted and named by [`tessella_status`], and the next
/// tick may ask again.
///
/// # Safety
///
/// `map` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessella_fail_request(map: MapHandle, ticket: u64) -> Status {
    guarded(move || {
        let Some(state) = (unsafe { map.as_ref() }) else {
            return Status::NoSuchMap;
        };
        let Some(hosted) = state.source.transport().hosted() else {
            return Status::NotHosted;
        };
        let ticket = Ticket::from_raw(ticket);
        // Named with the URL the map asked for, where it is still known, so the reason a consumer
        // reads back says which fetch it was about.
        let url = hosted.url_of(ticket).map_or(String::new(), |(ptr, len)| {
            // SAFETY: the transport owns these bytes and they are a `String`'s, so they are UTF-8
            // and readable for `len` while this call holds no other reference into the table.
            unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len)) }
                .to_string()
        });
        hosted.fail(ticket, &url, "the host could not fetch it");
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
                slabs: state.slabs.as_ptr().cast::<u8>(),
                slabs_len: state.slabs.len() * core::mem::size_of::<u64>(),
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
