/* SPDX-License-Identifier: BSD-2-Clause
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
 * is two dozen functions and two structs, and generating it would cost more than it saves. What it
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

#include <stdbool.h>
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
    TESSELLA_NOT_HOSTED = 8,
    /* An annotation document or image could not be read. Distinct from TESSELLA_FAILED for
     * TESSELLA_NOT_HOSTED's reason: it is a fixable mistake in what the caller passed, and the
     * caller is the only one that can fix it. The document is not a GeoJSON feature collection,
     * or the image is not a picture this build decodes. */
    TESSELLA_BAD_ANNOTATIONS = 9,
    /* The style has no GeoJSON source by that name, or the source it names is not GeoJSON.
     * Distinct from TESSELLA_FAILED for TESSELLA_NOT_HOSTED's reason: the caller is the only one
     * that can fix it. */
    TESSELLA_NO_SUCH_SOURCE = 10,
    /* A GeoJSON document could not be read. */
    TESSELLA_BAD_GEOJSON = 11,
    /* The style has not resolved yet, so the map has no sources to name. Not a failure: a map
     * only just created has not read its style. Poll tessella_status and hand the data over once
     * it reports TESSELLA_READY. */
    TESSELLA_NOT_RESOLVED = 12,
    /* An image could not be read, or its pixel ratio was not positive. Distinct from
     * TESSELLA_BAD_ANNOTATIONS, which says the same of an annotation's image: the two calls take
     * different things and a caller fixing one is not looking at the other. */
    TESSELLA_BAD_IMAGE = 13,
    /* A conversion between the screen and the map has no answer for that point.
     *
     * From tessella_screen_to_geo, a pixel whose ray never reaches the surface: above the horizon
     * on a pitched plane, or beside the globe. From tessella_geo_to_screen, a coordinate the
     * camera cannot see: behind it on a plane, or on the globe's far side.
     *
     * Not a failure, and the out parameters are left untouched rather than clamped. There is an
     * answer available in both cases and it is worse than none -- see the two calls. */
    TESSELLA_OFF_THE_MAP = 14,
    /* A cache was asked for on a map that cannot have one.
     *
     * Two ways to get here and the fix differs. The library was built without its `cache` feature,
     * so there is no store to open and the alternative would be fetching everything over a link
     * the caller thought was cached. Or the map is hosted, where the caller does the fetching and
     * therefore owns the caching too, and a cache on this side would see no request to answer. */
    TESSELLA_NO_CACHE = 15,
    /* No region in the store has that identifier -- one the caller deleted, or one from another
     * store. Every call that names a region answers this, so a stale identifier is never mistaken
     * for a region with nothing in it. */
    TESSELLA_NO_SUCH_REGION = 16,
    /* That region is already downloading. A download runs on a thread of its own and reports
     * through tessella_offline_progress, so a second start would be two threads claiming
     * resources for one region. Cancel it or wait for it; a finished one may be started again,
     * which is how a download resumes. */
    TESSELLA_ALREADY_RUNNING = 17
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
    /* Where to keep fetched resources between runs, or null for none.
     *
     * A path to an SQLite file the map opens or creates. With one, every resource the map fetches
     * is stored with its validator and served from there on the next run -- a warm start that
     * reaches first geometry in 0.4 ms against 3.8 ms cold, with no round trips against ten. It is
     * also what makes a downloaded region readable: a region's tiles are rows in this file, and a
     * map pointed at it draws them with no network at all.
     *
     * Null, or a length of zero, means no cache -- which is what a zeroed config asks for, so
     * nothing that predates this field behaves differently.
     *
     * Needs the library's `cache` feature, and TESSELLA_NO_CACHE says so rather than the path
     * being ignored. A map from tessella_create_hosted answers the same: its fetching is the
     * caller's, and so is its caching. */
    const uint8_t* cache_path;
    /* Its length in bytes. */
    size_t cache_path_len;
} tessella_config;

TESSELLA_ASSERT(offsetof(tessella_config, style_json) == 0, "tessella_config.style_json moved");
TESSELLA_ASSERT(offsetof(tessella_config, style_json_len) == sizeof(void*),
                "tessella_config.style_json_len moved");
TESSELLA_ASSERT(offsetof(tessella_config, width) == 2 * sizeof(void*),
                "tessella_config.width moved");
/* The tail, which is the half a mismatched build gets wrong: a caller compiled against an earlier
 * header passes a shorter struct, and the fields past its end are whatever was on the stack.
 *
 * The `+ 8` is `width` and `height`, which are four bytes each whatever a pointer is -- so this
 * arithmetic holds on a 32-bit consumer as well, where they do not share a word with anything. The
 * assertions that predate these were written the same way and for the same reason. Written as
 * `5 * sizeof(void*)` and `7 * sizeof(void*)`, which is what the 64-bit numbers also come to, they
 * fail to compile on an ILP32 target -- checked rather than reasoned, with `-m32`. */
TESSELLA_ASSERT(offsetof(tessella_config, cache_path) == 4 * sizeof(void*) + 8,
                "tessella_config.cache_path moved");
TESSELLA_ASSERT(sizeof(tessella_config) == 6 * sizeof(void*) + 8,
                "tessella_config changed size");

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
 * calls that before the first tick rather than covering a view it will not draw.
 *
 * Maps created with the same style share what they build: a tile one of them has built is not
 * fetched or built again for another, which is what keeps four views of one style from costing
 * four times one. */
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
 * the tick is what notices what has arrived and decides what to want next.
 *
 * Hosted maps created with the same style share what they build: a tile one of them has built is
 * not fetched or built again for another. So a URL is taken to answer the same bytes to every one
 * of them, and a host that answers two of them differently gets whichever answer was built first
 * for both. Maps that fetch for themselves share among themselves and never with these. */
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
 * A ticket that was canceled, already answered, or never issued is ignored and answers
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

/* Gives a running map a new style.
 *
 * A compiled style is immutable and a change is a new revision, so this is a replacement: the
 * document a host switches to -- day for night, a layer toggled, a config value such as the label
 * language, its own layers for a route or a puck -- becomes the next revision and the tiles are
 * rebuilt against it, because a changed filter admits different features.
 *
 * The alternative it replaces is destroying the map and creating another, which loses the camera,
 * the label identities and their fades, the drawable ids the consumer is holding, and every tile. A
 * restyle keeps all of that but the second pair: the camera and the viewport are untouched, the
 * session goes on numbering drawables, and the arena keeps its geometry until the frame that
 * replaces it.
 *
 * What it costs: the buckets, always, since the revision is in every tile key precisely so that a
 * bucket built against one style is not reused against another. The *bytes*, only without a store --
 * a map created with tessella_config.cache_path serves every tile from it and a restyle reaches no
 * origin at all. Without one, a restyle refetches what it rebuilds.
 *
 * Afterwards the map is resolving again, so tessella_status reports its readiness from the start and
 * the first few ticks draw what the previous style left until the new buckets land.
 *
 * A style that does not parse changes nothing at all and answers TESSELLA_BAD_STYLE, so a host can
 * offer a document it is not sure of.
 *
 * Annotations survive, because the map holds them and re-applies them to the new revision. Data
 * pushed with tessella_set_geojson_data does not: the style that named the source is gone, and
 * nothing here kept a copy. Re-apply it once the new style reports ready. */
tessella_result tessella_set_style(tessella_map* map,
                                   const uint8_t* style_json,
                                   size_t style_json_len);

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

/* Which side owns a map's camera (DR-9). */
typedef enum tessella_camera_owner {
    /* The producer's own, moved with tessella_set_camera. The default. */
    TESSELLA_CAMERA_OWNER_PRODUCER = 0,
    /* The consumer's, published with tessella_publish_camera and read back each tick. */
    TESSELLA_CAMERA_OWNER_CONSUMER = 1,
} tessella_camera_owner;

/* Says which side owns this map's camera.
 *
 * Under TESSELLA_CAMERA_OWNER_CONSUMER the map takes its camera from what was last published, at
 * the start of each tick, and tessella_set_camera stops being the thing that moves it -- a
 * consumer that keeps calling both is telling the map two different things and the published one
 * wins.
 *
 * Switching before anything is published leaves the camera where it was: a mode is not a camera,
 * and a map that blanked itself on the switch would flash. */
tessella_result tessella_set_camera_owner(tessella_map* map, tessella_camera_owner owner);

/* Publishes the camera of a map whose owner is the consumer (DR-9).
 *
 * Two cameras go in together because they answer different questions. view_projection -- sixteen
 * doubles, column-major -- says where things land on screen, and is the consumer's own, so a
 * scene camera that is not a map camera is expressible rather than approximated. The scalars say
 * which data at what scale: the fetch and paint zooms, pixels-per-meter, the zoom history.
 * Neither derives the other. They are stored in one seqlock generation, so the producer reads
 * both halves of one frame or retries.
 *
 * The viewport is not an argument: the map already knows what it draws into, set with
 * tessella_set_viewport, and taking it from there is what stops the published camera and the
 * cover disagreeing about the size of the screen.
 *
 * Publishing is not the same as being in the mode. A map still owned by the producer ignores what
 * is published here, which is what lets a consumer publish before it switches. */
tessella_result tessella_publish_camera(tessella_map* map,
                                        const double* view_projection,
                                        double longitude,
                                        double latitude,
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
 * It is the wrong behavior for a map somebody is looking at. A label that stops being placed at one
 * anchor and starts at another along the same road, with nothing fading between the two, is read as
 * the text having moved. Call this once a frame with the milliseconds since the last one and the
 * fades run at mbgl's rate of 300 ms.
 *
 * Does not emit; the next tessella_tick does. */
tessella_result tessella_advance(tessella_map* map, double elapsed_millis);

/* Which coordinate a screen pixel is over.
 *
 * The pixel is in viewport coordinates, x from the left and y *down from the top*, which is where
 * a touch or a pointer arrives in. The answer is against the map's current camera, viewport and
 * projection: a globe is met as a sphere and a plane as a plane, so a host that switches
 * projection does not switch arithmetic.
 *
 * This is what a gesture is built from. Zooming about the point under two fingers is the
 * coordinate under them held fixed while the zoom changes, and panning by pixels is the difference
 * between two of these. Neither is expressible from the camera alone, because the relation between
 * a pixel and the ground is not uniform under pitch: a pixel near the top of a pitched screen
 * covers far more ground than one at the bottom, and no single scale describes both.
 *
 * TESSELLA_OFF_THE_MAP, with the out parameters untouched, when the pixel is over nothing -- a
 * pitched camera's upper screen is sky and a globe does not fill its viewport. A condition of the
 * pixel rather than an error, and the answer for a pixel that is not a number as well.
 * TESSELLA_FAILED when the viewport has no area.
 *
 * Answers against the map's own camera, which under TESSELLA_CAMERA_OWNER_CONSUMER is the camera
 * last published -- its scalars, not its matrix. A consumer's view_projection is in the consumer's
 * own world space, whose origin never travels, so it cannot be inverted here; a consumer with a
 * scene camera that is not a map camera is the side holding both the matrix and the origin, and is
 * the side that can answer. */
tessella_result tessella_screen_to_geo(const tessella_map* map,
                                       double x,
                                       double y,
                                       double* out_latitude,
                                       double* out_longitude);

/* Where a coordinate lands on the screen.
 *
 * The inverse of tessella_screen_to_geo, in the same viewport coordinates with y down from the
 * top. What a host places its own overlays with: a marker, a route's end, a label drawn outside
 * the map, or the arithmetic that asks where a set of coordinates would land before deciding they
 * fit on the screen.
 *
 * TESSELLA_OFF_THE_MAP for a coordinate the camera cannot see, out parameters untouched. Two cases
 * reach it: on a plane a coordinate *behind* a pitched camera, which the projection divides by a
 * negative w and so reflects through the center of the screen; on a globe the half of the world
 * the planet is in front of. Neither has a pixel, and both have one that looks usable. A
 * coordinate that is not a number is answered the same way.
 *
 * TESSELLA_FAILED when the viewport has no area. */
tessella_result tessella_geo_to_screen(const tessella_map* map,
                                       double latitude,
                                       double longitude,
                                       double* out_x,
                                       double* out_y);

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

/* The surface a map projects its tiles through.
 *
 * Mirrors tsl_projection_mode on the capture stream. A toggle rather than a mode a map is created
 * in: MapLibre switches projection at runtime and so does this. */
typedef enum tessella_projection {
    /* The plane. The camera's proj_matrix is the whole projection. */
    TESSELLA_PROJECTION_MERCATOR = 0,
    /* The sphere. The camera carries a globe_matrix and the consumer's vertex stage supplies the
     * nonlinear step between them. */
    TESSELLA_PROJECTION_GLOBE = 1
} tessella_projection;

/* Sets the projection a map draws through.
 *
 * Does not emit and needs no invalidation: the camera block is rebuilt every frame, so the next
 * one carries the new matrix by the same path a pan takes.
 *
 * This does not change tessella_set_world_copies. A globe almost always wants
 * TESSELLA_WORLD_COPIES_ONE alongside it -- every wrap of a tile bends to the same patch, so a
 * globe drawing a repeated cover draws that patch twice and z-fights with itself -- but one call
 * silently moving another setting is worse than two calls, and the cover policy is measurable on
 * its own where the projection is not.
 *
 * Under TESSELLA_PROJECTION_GLOBE the consumer's vertex stage owes the bend: tile-local ->
 * normalized Mercator -> sphere -> clip, of which tsl_camera_update.globe_matrix is the last step.
 * A consumer that sets this and draws nothing different has not implemented it, and the producer
 * cannot tell. */
tessella_result tessella_set_projection(tessella_map* map, tessella_projection projection);

/* Replaces a map's annotations from a GeoJSON feature collection.
 *
 * Annotations are not a style layer: there is no "type": "annotation" and no stylesheet can
 * produce one. They are added here, and the source and layers they draw through are synthesized
 * into the style the map renders.
 *
 * The geometry type picks the annotation class, the way mbgl's own three classes split: a point
 * is a symbol, a line is a line annotation, a polygon is a fill. A feature's "icon" names the
 * image a symbol draws; "opacity", "width", "color" and "outlineColor" become the matching paint
 * property, and a feature silent about one gets the annotation class's own default.
 *
 * Replaces rather than adds -- the document is the whole set. Images are kept, because a symbol
 * names one by id and the ids outlive any one document.
 *
 * Must be called before the first tessella_tick. The layers are synthesized into the style during
 * source resolution, which the first tick starts and which happens once, so a set arriving after
 * it is in no style and draws nothing.
 *
 * TESSELLA_BAD_ANNOTATIONS if the document is not a feature collection this reads. */
tessella_result tessella_set_annotations(tessella_map* map, const uint8_t* geojson,
                                         size_t geojson_len);

/* Replaces a GeoJSON source's data.
 *
 * The style's own "data" is what the map draws until this is called, and this document
 * afterwards. The source's *options* stay the style's -- clustering, its radius and its maximum
 * zoom -- because they describe the source rather than the data.
 *
 * Every tile of that source is built again for the next frame, and no tile of any other source
 * is, so replacing one layer's points does not rebuild the basemap under them. What is already
 * drawn stays until the new tiles land, which is what keeps an animation from blinking.
 *
 * Unlike tessella_set_annotations this may be called whenever the style has resolved, which is
 * what makes it useful: it is how a point moves along a route and how live data arrives. Before
 * then there is no source list to name and the call reports TESSELLA_NOT_RESOLVED.
 *
 * The document is read, and a clustered source's index rebuilt, on the calling thread, because
 * both are functions of the data and a tile cut from a half-built index would be wrong rather
 * than late. The cost follows the document's size, so a caller replacing a large document every
 * frame pays for it every frame.
 *
 * TESSELLA_NO_SUCH_SOURCE if the style has no GeoJSON source by that name, TESSELLA_BAD_GEOJSON
 * if the document is not GeoJSON this reads. */
tessella_result tessella_set_geojson_data(tessella_map* map, const uint8_t* source,
                                          size_t source_len, const uint8_t* geojson,
                                          size_t geojson_len);

/* Adds an image the style's "icon-image" and "*-pattern" can name.
 *
 * GL JS's `map.addImage(id, image)`. The image joins the style's own sheet: it is packed into the
 * same atlas, under a name any layer can ask for, and a style with no "sprite" at all can still
 * have images this way.
 *
 * `image` is an encoded picture -- PNG, JPEG, or WebP where that decoder is built in -- rather
 * than raw pixels, because every caller with an icon has a file and none has a premultiplied RGBA
 * buffer. `sdf` says the picture is a signed distance field, which is what lets "icon-color"
 * recolor it.
 *
 * Distinct from tessella_add_annotation_image, which adds an image an *annotation* names.
 * Annotations are not style layers and their images are their own; this one is the style's.
 *
 * May be called at any time. An icon is laid out against the sheet per frame rather than built
 * into a tile, so an image that arrives late costs a relayout of the symbols that wanted it and
 * no tile is rebuilt. Replacing a name repacks the atlas.
 *
 * TESSELLA_NOT_RESOLVED before the style's own sheet has arrived -- there is nothing to add to
 * yet, and tessella_status says when a map is ready. TESSELLA_BAD_IMAGE if the picture does not
 * decode or the pixel ratio is not positive. */
tessella_result tessella_add_image(tessella_map* map, const uint8_t* id, size_t id_len,
                                   const uint8_t* image, size_t image_len, double pixel_ratio,
                                   bool sdf);

/* Adds an image a symbol annotation's "icon" can name.
 *
 * `image` is an encoded picture -- PNG, JPEG, or WebP where that decoder is built in -- rather
 * than raw pixels, because every caller with an icon has a file and none of them has a
 * premultiplied RGBA buffer.
 *
 * `id` is the caller's own. "default_marker" is the id an annotation with no icon asks for, and a
 * caller that supplies none draws nothing for those, which is mbgl's behavior too.
 *
 * Must be called before the first tessella_tick, for the reason tessella_set_annotations gives.
 *
 * TESSELLA_BAD_ANNOTATIONS if the image does not decode or the pixel ratio is not positive. */
tessella_result tessella_add_annotation_image(tessella_map* map, const uint8_t* id, size_t id_len,
                                              const uint8_t* image, size_t image_len,
                                              double pixel_ratio, bool sdf);

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

/* ---------------------------------------------------------------------------------------------
 * Offline regions
 *
 * An area a user asked to have available offline: a style, a shape, a zoom range. The resources
 * are rows in the same store `tessella_config.cache_path` names, so a map pointed at that path
 * draws a downloaded region with no network at all -- which is the point of the whole thing, and
 * the reason these calls and that field are one feature rather than two.
 *
 * A handle of its own rather than a call on a map. A region outlives every map that draws it, and
 * a host manages one with no map alive: a settings screen listing what is downloaded, or a refresh
 * while the app is in the background. SQLite is in WAL mode, so a download writing does not block
 * a map reading.
 *
 * Every call here answers TESSELLA_NO_CACHE when the library was built without its `cache`
 * feature, rather than failing to link -- the symbols exist either way, so one build of a consumer
 * runs against both. On wasm32 they are absent rather than refusing: there is no filesystem to
 * open a store in, as there are no sockets for tessella_create to fetch over.
 * ------------------------------------------------------------------------------------------- */

/* A store's regions, and the downloads filling them. Opaque and non-null. */
typedef struct tessella_offline tessella_offline;

/* Where a region's download has got to. */
typedef enum tessella_offline_state {
    /* Nothing started in this process. The stored counts still say what it holds. */
    TESSELLA_OFFLINE_IDLE = 0,
    /* Running now, and the live counters are moving. */
    TESSELLA_OFFLINE_RUNNING = 1,
    /* Finished, every resource dealt with. */
    TESSELLA_OFFLINE_DONE = 2,
    /* Stopped because it was asked to. What was stored stays, so starting it again resumes. */
    TESSELLA_OFFLINE_CANCELED = 3,
    /* Stopped by a failure. As above: what was stored stays. */
    TESSELLA_OFFLINE_FAILED = 4
} tessella_offline_state;

/* What a region is, as a caller states it.
 *
 * Byte ranges rather than C strings, as everything here is, and doubles for every number so there
 * is one convention. */
typedef struct tessella_region_spec {
    /* The style to make available, as a URL -- not bytes, which is the asymmetry with
     * tessella_config.style_json and is deliberate: a map draws a style the host already holds,
     * while a region has to fetch its style again on a device that has been offline since. */
    const uint8_t* style_url;
    size_t style_url_len;
    /* The area, as a GeoJSON Polygon or MultiPolygon *geometry*, or null to use the box below. */
    const uint8_t* geojson;
    size_t geojson_len;
    /* What a user called it, or null. */
    const uint8_t* description;
    size_t description_len;
    /* The box, used when `geojson` is null. */
    double west;
    double south;
    double east;
    double north;
    /* The zooms to include. */
    double min_zoom;
    double max_zoom;
    /* Device pixel ratio, which selects between @2x and plain assets. */
    double pixel_ratio;
    /* Whether to download CJK glyph ranges, which are the bulk of a glyph download and are
     * usually rendered locally -- which is why mbgl makes this a choice rather than always
     * fetching them. */
    uint8_t include_ideographs;
} tessella_region_spec;

TESSELLA_ASSERT(sizeof(tessella_region_spec) == 14 * sizeof(void*),
                "tessella_region_spec changed size");
TESSELLA_ASSERT(offsetof(tessella_region_spec, west) == 6 * sizeof(void*),
                "tessella_region_spec.west moved");
TESSELLA_ASSERT(offsetof(tessella_region_spec, include_ideographs) == 13 * sizeof(void*),
                "tessella_region_spec.include_ideographs moved");

/* What a region will cost. */
typedef struct tessella_offline_cost {
    /* Tiles, across every source. */
    uint64_t tiles;
    /* Every resource, tiles included. */
    uint64_t resources;
    /* Whether `resources` is exact or a lower bound. A source given by TileJSON URL states its
     * zoom range in a manifest rather than in the style, so its tiles cannot be counted until
     * that is fetched -- and fetching it before the question can be put is what makes asking as
     * expensive as agreeing. A text-font computed per feature is the other way of not knowing. */
    uint8_t precise;
} tessella_offline_cost;

TESSELLA_ASSERT(sizeof(tessella_offline_cost) == 24, "tessella_offline_cost changed size");

/* How far a download has got, live and stored. */
typedef struct tessella_offline_counters {
    /* Resources dealt with -- fetched, claimed or found absent. Live, so zero before a start. */
    uint64_t completed;
    /* Of those, fetched from the origin. */
    uint64_t fetched;
    /* Of those, already held and merely claimed. */
    uint64_t held;
    /* Of those, confirmed unchanged by the origin. Only a refresh produces these. */
    uint64_t unchanged;
    /* Of those, absent at the origin. */
    uint64_t missing;
    /* Resources the plan named, or zero before it is known. */
    uint64_t required;
    /* Resources the store holds against this region, which survives a restart. */
    uint64_t stored_resources;
    /* Bytes those resources occupy. */
    uint64_t stored_bytes;
    /* One of tessella_offline_state. */
    uint32_t state;
} tessella_offline_counters;

TESSELLA_ASSERT(sizeof(tessella_offline_counters) == 72,
                "tessella_offline_counters changed size");
TESSELLA_ASSERT(offsetof(tessella_offline_counters, state) == 64,
                "tessella_offline_counters.state moved");

/* Opens or creates the store at `path`, for managing its regions.
 *
 * The same file a map takes as tessella_config.cache_path. */
tessella_result tessella_offline_open(const uint8_t* path,
                                      size_t path_len,
                                      tessella_offline** out);

/* Stops every download this handle started and releases it.
 *
 * Waits for each to notice, which is at most one resource. A download stopped this way resumes
 * rather than restarts: whatever was stored stays claimed. Null is accepted and does nothing. */
void tessella_offline_close(tessella_offline* offline);

/* Records a region, and returns the identifier every other call names it by.
 *
 * Creating it claims nothing: the region exists with no resources until a download stores them,
 * which is what makes a download resumable rather than all-or-nothing. It appears in
 * tessella_offline_list immediately, at nothing percent.
 *
 * TESSELLA_BAD_GEOJSON for a geometry that is not a Polygon or MultiPolygon. */
tessella_result tessella_offline_define(tessella_offline* offline,
                                        const tessella_region_spec* spec,
                                        uint64_t* out_region);

/* What a region would cost, from the style the caller is displaying.
 *
 * The style's bytes rather than its URL, because this answers without the network. Takes a spec
 * rather than an identifier, so a host can size a box a user is still dragging. */
tessella_result tessella_offline_estimate(tessella_offline* offline,
                                          const tessella_region_spec* spec,
                                          const uint8_t* style_json,
                                          size_t style_json_len,
                                          tessella_offline_cost* out);

/* Fetches, stores and claims everything the region names, on a thread of its own.
 *
 * Returns as soon as the thread is running; the work is hours of fetching over a connection that
 * will drop, and tessella_offline_progress is how it is watched. It runs at the pool's background
 * class and never above, so a download in flight cannot end up on the critical path of a view
 * that is trying to draw.
 *
 * Assets first and tiles second, with a barrier between them: a download stopped halfway is far
 * more useful with a style and no tiles than with tiles and nothing to draw them with.
 *
 * Resumable rather than transactional. Whatever was stored stays stored and claimed, so calling
 * this again after a cancel or a failure continues rather than starting over.
 *
 * TESSELLA_ALREADY_RUNNING when one is in flight for that region, TESSELLA_NO_SUCH_REGION for an
 * identifier the store does not have, TESSELLA_BAD_STYLE for bytes that will not parse. */
tessella_result tessella_offline_download(tessella_offline* offline,
                                          uint64_t region,
                                          const uint8_t* style_json,
                                          size_t style_json_len);

/* Brings a region up to date against its origin, the same way.
 *
 * Unlike a download, a held resource is revalidated rather than accepted: a download alone leaves
 * a region a snapshot of the day it was taken, which is correct -- the user paid for that snapshot
 * and it is served however old it gets -- and this is how they ask for a newer one. A completed
 * refresh also releases claims the plan no longer names. */
tessella_result tessella_offline_refresh(tessella_offline* offline,
                                         uint64_t region,
                                         const uint8_t* style_json,
                                         size_t style_json_len);

/* How far a region's download has got.
 *
 * Both halves, because they answer different questions: the live counters are this process's
 * download and are zero before one starts, and the stored counts are what the file holds and
 * survive a restart -- which is what a progress bar should show for a region nobody has resumed.
 *
 * A download that ended is reported until the same region is started again, so a caller that polls
 * after the last resource still learns how it finished. */
tessella_result tessella_offline_progress(tessella_offline* offline,
                                          uint64_t region,
                                          tessella_offline_counters* out);

/* Asks a running download to stop, and returns without waiting.
 *
 * Polled before each resource, so it stops within one. TESSELLA_OK for a region with nothing
 * running, which is what cancelling twice means. */
tessella_result tessella_offline_cancel(tessella_offline* offline, uint64_t region);

/* Every region in the store, oldest first, as identifiers.
 *
 * Writes at most `cap` of them and reports how many there are, so a caller whose buffer is too
 * small learns the count and asks again rather than being truncated silently. `out` may be null
 * with a `cap` of zero, which is how the count alone is asked for. */
tessella_result tessella_offline_list(tessella_offline* offline,
                                      uint64_t* out,
                                      size_t cap,
                                      size_t* out_count);

/* Removes a region and releases its claims.
 *
 * The resources are not deleted: one an overlapping region or ordinary use also wants stays, and
 * what is left unclaimed re-enters the ambient budget and is evicted when the store needs the
 * room. That is the whole of what a claim is -- a count on the row, not a copy of the body.
 *
 * A running download is stopped and waited for first, since it would otherwise go on claiming
 * resources for a region that is gone. */
tessella_result tessella_offline_delete(tessella_offline* offline, uint64_t region);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TESSELLA_H */
