/* SPDX-License-Identifier: Apache-2.0
 *
 * The C surface a consumer embeds tessella through.
 *
 * # What this is, and what it is not
 *
 * It is not a second protocol. Everything a consumer draws from arrives on the capture stream,
 * described by `tessella_capture_abi.h`; this is only the handful of calls that get a producer
 * running and a frame emitted. Anything that could travel as a record does travel as a record,
 * because a second way to say the same thing is a second thing to keep in agreement.
 *
 * # How this header is kept honest
 *
 * By hand, and checked rather than trusted. `tessella_capture_abi.h` is generated from mbgl's own
 * declarations (DR-6) because it is a large table nobody could keep in step by reading it; this
 * is six functions and two structs, and generating it would cost more than it saves. What it
 * would cost instead is drift, so `c_surface.rs` compiles a probe against this header, links it
 * to the staticlib and drives a whole map lifecycle through it. A declaration that disagrees with
 * the Rust fails to link or fails to run.
 *
 * The static assertions below cover the other half: a struct whose layout differs is a mismatch
 * the linker cannot see, because the symbol is the same either way.
 *
 * # The rules every entry point follows
 *
 * - Borrowed in, owned nowhere. A `const char*` is copied before the call returns.
 * - No panics cross the boundary. Every entry point returns a status.
 * - A handle is opaque and non-null. Zero is the failure value, so a caller that ignores the
 *   status still cannot mistake a failed create for a working map.
 * - A map is driven from one thread. The calls that would contend never race, which is the same
 *   contract every consumer of this kind already has.
 */

#ifndef TESSELLA_H
#define TESSELLA_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(__cplusplus) && __cplusplus >= 201103L
#define TESSELLA_ASSERT(cond, msg) static_assert(cond, msg)
#elif defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
#define TESSELLA_ASSERT(cond, msg) _Static_assert(cond, msg)
#else
#define TESSELLA_ASSERT(cond, msg)
#endif

/* How a call went.
 *
 * A single OK and a reason for everything else. The reasons are stable numbers because a
 * consumer logs them and a log outlives the build that wrote it. */
typedef enum tessella_result {
    /* It worked. */
    TESSELLA_OK = 0,
    /* A pointer argument was null where the call requires one. */
    TESSELLA_NULL_ARGUMENT = 1,
    /* A handle did not name a live map. */
    TESSELLA_NO_SUCH_MAP = 2,
    /* A string argument was not UTF-8. */
    TESSELLA_NOT_UTF8 = 3,
    /* The style did not parse. */
    TESSELLA_BAD_STYLE = 4,
    /* The ring could not take the frame. The consumer is behind; drain and retry. */
    TESSELLA_RING_FULL = 5,
    /* Something failed in a way this ABI has no more specific word for. The producer logs it. */
    TESSELLA_FAILED = 6,
    /* The slab region could not take the frame's geometry. Unlike TESSELLA_RING_FULL this does
     * not clear by draining: the arena bump allocates, so space a swept slab left is recovered
     * only once everything above it has gone. The frame compacts and the next tick retries; a
     * map reporting this every tick needs a larger slab_capacity. */
    TESSELLA_REGION_FULL = 7,
    /* A hosted call was made on a map that fetches for itself. Distinct from TESSELLA_FAILED
     * because it is a fixable mistake with an obvious fix: the map wanted
     * tessella_create_hosted. A map created either way is otherwise identical, so nothing else
     * would tell a caller which one it has. */
    TESSELLA_NOT_HOSTED = 8
} tessella_result;

/* How far along a map's sources are.
 *
 * A map is progressive: `tessella_create` parses the style and does no network, so the first
 * frames draw the background while the sources resolve and the tiles land. That makes "empty" an
 * ordinary state rather than an error, and this is how a consumer tells the ordinary kind from
 * the kind that will never resolve. */
typedef enum tessella_readiness {
    /* Nothing has been asked for yet. The first tick starts resolution. */
    TESSELLA_IDLE = 0,
    /* The style's sources are resolving. No tile can be asked for until they do, because the
     * manifests carry the templates a tile's URL is built from. */
    TESSELLA_RESOLVING = 1,
    /* Resolved. Tiles are built as they are wanted and land as they finish. */
    TESSELLA_READY = 2,
    /* A source did not resolve. Terminal: nothing retries, because a manifest that will not parse
     * will not parse the second time either. */
    TESSELLA_FAILED_TO_RESOLVE = 3
} tessella_readiness;

/* One live map. Opaque: the handle is the state. */
typedef struct tessella_map tessella_map;

/* How a map is set up. */
typedef struct tessella_config {
    /* The style document, as JSON. A URL is not accepted here: fetching it is the caller's,
     * because a caller that already has the bytes should not be made to serve them back.
     *
     * A pointer and a length rather than a NUL-terminated string, on every target rather than
     * only the one that needs it. A C string was a convenience for C callers and nothing else:
     * a browser hands over a byte range in linear memory, which has no terminator to find, and
     * one signature is easier to keep honest than two. Pass `s.data(), s.size()`. */
    const uint8_t* style_json;
    /* Its length in bytes, not counting any terminator the caller happens to have. */
    size_t style_json_len;
    /* Viewport width in pixels. */
    uint32_t width;
    /* Viewport height in pixels. */
    uint32_t height;
    /* Ring capacity in bytes. Rounded up to a power of two, which the ring requires. */
    size_t ring_capacity;
    /* Slab region capacity in bytes, where the frame's geometry is written. Zero takes the
     * default, which is 64 MiB.
     *
     * The consumer reads the geometry in place, so this is the working set of everything on
     * screen plus what compaction has not yet reclaimed -- not a per-frame buffer. A frame that
     * does not fit is refused whole and retried after the arena compacts, so a region that is
     * too small shows as a map that will not finish drawing. */
    size_t slab_capacity;
} tessella_config;

TESSELLA_ASSERT(offsetof(tessella_config, style_json) == 0, "tessella_config.style_json moved");
TESSELLA_ASSERT(offsetof(tessella_config, style_json_len) == sizeof(void*),
                "tessella_config.style_json_len moved");
TESSELLA_ASSERT(offsetof(tessella_config, width) == 2 * sizeof(void*),
                "tessella_config.width moved");

/* Where a consumer reads from.
 *
 * Two ranges in *this process's* address space. That is the point of the staticlib arrangement:
 * the ring and the arena are ordinary memory the consumer reads directly, so geometry reaches the
 * GPU out of the producer's own allocation and nothing is copied to make it reachable. Across a
 * process boundary the same two ranges would be mapped instead, and nothing else about the
 * protocol would change.
 *
 * Valid until the map is destroyed. The ring's control block is at its start.
 *
 * Named `tessella_map_regions` rather than `tessella_regions` because in C a typedef and a
 * function share one namespace, and `tessella_regions` is the call that fills this in. The
 * function keeps the plain name: it is the one of the two a consumer writes. */
typedef struct tessella_map_regions {
    /* The ring: control block, then the data region. */
    const uint8_t* ring;
    /* Its length in bytes. */
    size_t ring_len;
    /* The slab region every `tsl_slab_ref` resolves against. */
    const uint8_t* slabs;
    /* Its length in bytes. */
    size_t slabs_len;
} tessella_map_regions;

TESSELLA_ASSERT(sizeof(tessella_map_regions) == 4 * sizeof(void*),
                "tessella_map_regions is not four words");

/* Creates a map. Parses the style, and does nothing else.
 *
 * No network, no cover, no tiles: a blocking create freezes the calling thread, and for a
 * consumer whose bindings run on a UI thread that freezes the application rather than the map.
 * The first `tessella_tick` is what starts the network.
 *
 * A style that does not parse fails here, which is the one failure worth reporting where it is
 * actionable. A style that parses but whose *sources* will not resolve cannot fail here, because
 * finding that out is the round trip this call exists not to make -- `tessella_status` carries
 * that instead.
 *
 * The camera starts where `tessella_set_camera` would put it; a caller that wants somewhere else
 * calls that before the first tick rather than covering a view it will not draw. */
tessella_result tessella_create(const tessella_config* config,
                                double latitude,
                                double longitude,
                                double zoom,
                                tessella_map** out);

/* Creates a map whose fetching the caller does.
 *
 * As tessella_create, but nothing is fetched by the map. It writes down what it needs and the
 * caller brings it back through tessella_take_request and tessella_answer.
 *
 * For a browser, where there is no other option: no sockets, and no blocking on the thread that
 * draws. Not only for a browser -- a host with its own connection pool, its own cache, or its own
 * idea of when a fetch is allowed uses the same three calls, and a test uses them to drive a map
 * with no network at all.
 *
 * The map still needs ticking. A hosted map with nobody calling tessella_tick asks for nothing:
 * the tick is what notices what has arrived and decides what to want next. */
tessella_result tessella_create_hosted(const tessella_config* config,
                                       double latitude,
                                       double longitude,
                                       double zoom,
                                       tessella_map** out);

/* Takes the next thing a hosted map wants fetched.
 *
 * Answers ticket 0 when there is nothing to fetch, which is not an error: it is what a settled
 * map says, and it is the condition a caller loops until. Zero is never a real ticket.
 *
 * The URL is a byte range in the map's own memory -- the same arrangement tessella_regions uses
 * for the ring, and for the same reason: the alternative is an allocator export and a copy on
 * each side of it. It stays valid until the ticket is answered, failed, or the map is destroyed.
 * A caller that holds it past any of those holds a dangling pointer.
 *
 * TESSELLA_NOT_HOSTED for a map created by tessella_create, which fetches for itself. */
tessella_result tessella_take_request(tessella_map* map,
                                      uint64_t* out_ticket,
                                      const uint8_t** out_url,
                                      size_t* out_url_len);

/* Answers a request with what the caller fetched.
 *
 * `status` is the origin's. A 404 is an answer rather than a failure -- an absent tile is an edge
 * of a source's coverage, which the map draws around, and reporting it as a broken fetch would
 * make a hole look like a fault. tessella_fail_request is for a fetch that did not happen at all.
 *
 * A ticket that was cancelled, already answered, or never issued is ignored and answers
 * TESSELLA_OK: a caller that has lost track of its own bookkeeping has wasted a fetch, which is
 * not something the map can fix by refusing. An empty body is legitimate -- a tile with no
 * features is a valid, empty tile -- so a null pointer with a zero length is a real answer. */
tessella_result tessella_answer(tessella_map* map,
                                uint64_t ticket,
                                uint16_t status,
                                const uint8_t* body,
                                size_t body_len);

/* Answers a request the caller could not fetch at all.
 *
 * For a connection that never opened, not for an origin that said no -- that is tessella_answer
 * with the status it said it with. The map treats it as any transport failure: the tile is a
 * hole, counted and named by tessella_status, and the next tick may ask again. */
tessella_result tessella_fail_request(tessella_map* map, uint64_t ticket);

/* Moves the camera.
 *
 * Does not draw. A camera that has not moved emits nothing on the next tick, which is what keeps
 * traffic proportional to change -- so this is cheap to call every frame and the caller need not
 * track whether anything moved. */
tessella_result tessella_set_camera(tessella_map* map,
                                    double latitude,
                                    double longitude,
                                    double zoom,
                                    double bearing,
                                    double pitch);

/* Tells a map how much time has passed, so its labels can fade.
 *
 * A map that is never told this behaves as a still picture: a fade completes in one step and a
 * label appears or disappears outright. That is what mbgl-render does -- symbolFadeChange returns
 * one in static map mode -- and it is what every parity capture on both sides compares, so it stays
 * the default.
 *
 * It is the wrong behaviour for a map somebody is looking at. A label that stops being placed at one
 * anchor and starts at another along the same road, with nothing fading between the two, is read as
 * the text having moved. Call this once a frame with the milliseconds since the last one and the
 * fades run at mbgl's rate of 300 ms.
 *
 * Does not emit; the next tessella_tick does. */
tessella_result tessella_advance(tessella_map* map, double elapsed_millis);

/* Changes the viewport a map draws into.
 *
 * A window resize is not a new map. Before this the size was settable only at tessella_create, so a
 * consumer whose surface changed had no option but to destroy the map and build another -- every
 * tile refetched, every bucket rebuilt, every glyph re-shaped, for a change that moves no camera.
 * What survives a resize now is everything a resize does not change: the tiles, their buckets, the
 * layouts, and the label identities with the fades keyed on them.
 *
 * Does not emit. The next tick sees a changed camera -- the viewport is part of what makes a camera
 * differ -- and rewrites the matrices by the path a pan takes.
 *
 * A width or height of zero is ignored rather than refused: a surface being torn down reports one,
 * and an error there would have the consumer handling a condition that resolves itself. */
tessella_result tessella_set_viewport(tessella_map* map, uint32_t width, uint32_t height);

/* What surface a view's tiles will be covered for.
 *
 * The producer's whole part in the globe. Placement is unaffected -- a globe bends Mercator
 * geometry per vertex in the consumer's material, so what travels on the wire is the ordinary flat
 * placement either way -- but selection is not: a Mercator plane repeats horizontally and a sphere
 * does not, so a globe drawing a flat cover draws the same patch of the world once per copy. */
typedef enum tessella_world_copies {
    /* A plane, which repeats horizontally. The default, and what a Mercator map wants. */
    TESSELLA_WORLD_COPIES_REPEATED = 0,
    /* A sphere, which has one of everything.
     *
     * At zoom 0 four of five cover tiles are copies and at zoom 1 four of eight -- most of the
     * cover rather than an edge case -- and drawing them is z-fighting on the surface plus
     * subdivision paid twice at the levels where subdivision is dearest. */
    TESSELLA_WORLD_COPIES_ONE = 1
} tessella_world_copies;

/* Sets the surface a map's tiles are covered for.
 *
 * Does not emit, and needs no invalidation: the cover is recomputed every frame, so the next tick
 * sees a different set of tiles by the same path a pan takes.
 *
 * The horizon is deliberately not here. Tiles a sphere has curved out of sight are four to six of
 * the cheapest on the map between zoom 1 and 2.5 and none outside that band, which does not pay
 * for a spherical cull on this side -- it is one dot product per tile in the consumer, before it
 * subdivides, which removes the draw as well as the tile. */
tessella_result tessella_set_world_copies(tessella_map* map, tessella_world_copies copies);

/* Emits a frame, if anything changed, and asks for what the next one needs.
 *
 * Returns TESSELLA_OK whether or not a frame was emitted: a settled map sending nothing is the
 * ordinary case rather than a condition to report, and a caller polling at display rate would
 * spend more code distinguishing the two than acting on it. What changed is on the ring; what did
 * not is the absence of records.
 *
 * Cheap when nothing happened -- a comparison, before the cover, the cache, the arena or the ring
 * are touched -- which is what makes calling this every vsync the right thing to do. */
tessella_result tessella_tick(tessella_map* map);

/* How far along the map's sources are, and why if they failed.
 *
 * A consumer holding a handle and looking at an empty map cannot tell a style still resolving
 * from one whose sources will never answer, and inferring it from the absence of tiles is wrong
 * in both directions. This is what a consumer reads before it wonders why the map is empty.
 *
 * `reason` may be null, and is written only when the readiness is TESSELLA_FAILED_TO_RESOLVE. It
 * is always NUL-terminated when written, and truncated to fit rather than refused. */
/* How much work is still in flight.
 *
 * Tiles asked for and not yet answered, plus a glyph fetch that has not finished. Zero means
 * nothing further will arrive without another tick -- not that the map is complete, since a tile
 * that failed is finished and still a hole; `tessella_status` answers that half.
 *
 * This is the question a caller waiting for a settled frame is asking. Waiting for records to
 * stop arriving instead is satisfied by a source *blocked* on a fetch just as well as by one that
 * has finished, which is how the render probe came to measure frames that were still filling in.
 * A consumer driving a progress indicator reads the same number. */
tessella_result tessella_pending(tessella_map* map, uint64_t* out_pending);

tessella_result tessella_status(tessella_map* map,
                               int32_t* out_readiness,
                               char* reason,
                               size_t reason_cap);

/* The two ranges a consumer reads from.
 *
 * `slabs` is empty until a frame has been emitted. */
tessella_result tessella_regions(tessella_map* map, tessella_map_regions* out);

/* Destroys a map and everything it owns.
 *
 * The regions it handed out are invalid the moment this returns, so a consumer with buffers still
 * in flight must have acknowledged them first. */
void tessella_destroy(tessella_map* map);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TESSELLA_H */
