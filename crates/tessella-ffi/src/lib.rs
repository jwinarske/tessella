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
}

extern crate alloc;

use alloc::sync::Arc;
use std::ffi::c_char;
use std::time::Duration;

use tessella_capture_abi::envelope::ViewId;
use tessella_capture_abi::ring::{self, Producer, region_size};
use tessella_orchestrate::cache::TileCache;
use tessella_orchestrate::map::{Map, SpriteAtlas, Tick};
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::source::{Pooled, Readiness, TileSource};
use tessella_storage::http::HttpFileSource;
use tessella_storage::source::Coalescing;
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
    source: Arc<Pooled<HttpFileSource>>,
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

        let files = Arc::new(Coalescing::new(HttpFileSource::new(Duration::from_secs(
            30,
        ))));
        let cache = Arc::new(TileCache::new(64));
        let source = TileSource::new(style_text, files, cache, Pool::shared(), 1);

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
