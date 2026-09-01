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
    TESSELLA_FAILED = 6
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
     * because a caller that already has the bytes should not be made to serve them back. */
    const char* style_json;
    /* Viewport width in pixels. */
    uint32_t width;
    /* Viewport height in pixels. */
    uint32_t height;
    /* Ring capacity in bytes. Rounded up to a power of two, which the ring requires. */
    size_t ring_capacity;
} tessella_config;

TESSELLA_ASSERT(offsetof(tessella_config, style_json) == 0, "tessella_config.style_json moved");
TESSELLA_ASSERT(offsetof(tessella_config, width) == sizeof(void*), "tessella_config.width moved");

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
