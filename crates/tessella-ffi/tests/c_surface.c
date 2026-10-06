/* SPDX-License-Identifier: BSD-2-Clause
 *
 * Drives a whole map lifecycle through `tessella.h` and nothing else.
 *
 * This is the thing that keeps the hand-written header honest. It sees only the declarations, so
 * a signature that disagrees with the Rust fails to compile or fails to link, and a struct whose
 * layout disagrees fails here rather than in a consumer six months from now. It is C rather than
 * C++ deliberately: the header claims to be a C surface, and a C++ compiler would accept things C
 * does not.
 *
 * Prints `name value` lines for the Rust side to read.
 */

/* nanosleep, which strict -std=c11 does not declare. Asked for explicitly rather than by
 * relaxing the standard to gnu11: compiling this against strict ISO C is part of what the header
 * is being checked for. */
#define _POSIX_C_SOURCE 199309L

#include <tessella.h>

#include <stdio.h>
#include <time.h>
#include <string.h>

/* A document that does not parse, for the calls that have to refuse one. */
static const char* const BAD = "{ this is not a style";

static const char* const STYLE =
    "{\"version\": 8, \"sources\": {}, \"layers\": ["
    "{\"id\": \"bg\", \"type\": \"background\","
    " \"paint\": {\"background-color\": \"#101418\"}}]}";

int main(int argc, char** argv) {
    /* Where a cached map may keep its store, handed over by the Rust side so the probe writes
     * nothing of its own choosing. Absent means that part is skipped. */
    const char* cache_path = argc > 1 ? argv[1] : NULL;

    tessella_config config = {0};
    /* A byte range, so a C caller casts rather than relying on a terminator the ABI no longer
     * looks for. `strlen` here because the literal is one; a caller with a `std::string` or a
     * buffer off the network already knows the length. */
    config.style_json = (const uint8_t*)STYLE;
    config.style_json_len = strlen(STYLE);
    config.width = 1024;
    config.height = 768;
    config.ring_capacity = 1u << 22;
    config.slab_capacity = 0; /* the default */

    tessella_map* map = NULL;
    printf("create %d\n", (int)tessella_create(&config, 51.505, -0.11, 13.0, &map));
    printf("handle_non_null %d\n", map != NULL ? 1 : 0);
    if (map == NULL) {
        return 1;
    }

    /* A style that will not parse must fail at create, and must not hand back a handle. */
    tessella_config bad = config;
    bad.style_json = (const uint8_t*)BAD;
    bad.style_json_len = strlen(BAD);
    tessella_map* rejected = NULL;
    printf("bad_style %d\n", (int)tessella_create(&bad, 0.0, 0.0, 0.0, &rejected));
    printf("bad_style_handle_null %d\n", rejected == NULL ? 1 : 0);

    /* Null arguments are answered rather than dereferenced. */
    printf("null_config %d\n", (int)tessella_create(NULL, 0.0, 0.0, 0.0, &map));
    printf("null_out %d\n", (int)tessella_create(&config, 0.0, 0.0, 0.0, NULL));
    printf("null_map_tick %d\n", (int)tessella_tick(NULL));

    /* The store. A cached map opens it at create -- the fetching starts on the first tick, so this
     * says the file was opened and nothing was asked of the network. */
    if (cache_path != NULL) {
        tessella_config cached = config;
        cached.cache_path = (const uint8_t*)cache_path;
        cached.cache_path_len = strlen(cache_path);

        tessella_map* with_store = NULL;
        printf("cache_native %d\n",
               (int)tessella_create(&cached, 51.505, -0.11, 13.0, &with_store));
        printf("cache_native_handle %d\n", with_store != NULL ? 1 : 0);
        tessella_destroy(with_store);

        /* A hosted map fetches nothing, so there is no request for a cache to answer. Refused
         * rather than ignored: a caller that asked for a cache and got none would be fetching
         * everything over a link it thought was cached. */
        tessella_map* hosted_store = NULL;
        printf("cache_on_hosted %d\n",
               (int)tessella_create_hosted(&cached, 51.505, -0.11, 13.0, &hosted_store));
        printf("cache_on_hosted_handle %d\n", hosted_store == NULL ? 1 : 0);
    }

    /* Marking a feature, and unmarking it. The probe's style reads no state, so what is checked
     * here is the call and its refusals rather than a changed pixel. */
    {
        static const char* const SRC = "fixture";
        static const char* const LAYER = "roads";
        static const char* const STATE = "{\"hover\": true}";
        printf("feature_state %d\n",
               (int)tessella_set_feature_state(map, (const uint8_t*)SRC, strlen(SRC),
                                               (const uint8_t*)LAYER, strlen(LAYER), 7,
                                               (const uint8_t*)STATE, strlen(STATE)));
        /* Null state is the feature no longer marked, which is a thing a host has to be able to
         * say -- not an argument fault. */
        printf("feature_state_clear_one %d\n",
               (int)tessella_set_feature_state(map, (const uint8_t*)SRC, strlen(SRC),
                                               (const uint8_t*)LAYER, strlen(LAYER), 7, NULL, 0));
        /* An array is not a state object. TESSELLA_BAD_FEATURE_STATE. */
        static const char* const NOT_AN_OBJECT = "[1, 2, 3]";
        printf("feature_state_bad %d\n",
               (int)tessella_set_feature_state(map, (const uint8_t*)SRC, strlen(SRC),
                                               (const uint8_t*)LAYER, strlen(LAYER), 7,
                                               (const uint8_t*)NOT_AN_OBJECT,
                                               strlen(NOT_AN_OBJECT)));
        printf("feature_state_null_source %d\n",
               (int)tessella_set_feature_state(map, NULL, 0, (const uint8_t*)LAYER, strlen(LAYER),
                                               7, (const uint8_t*)STATE, strlen(STATE)));
        printf("feature_state_no_map %d\n",
               (int)tessella_set_feature_state(NULL, (const uint8_t*)SRC, strlen(SRC),
                                               (const uint8_t*)LAYER, strlen(LAYER), 7,
                                               (const uint8_t*)STATE, strlen(STATE)));
        printf("feature_state_clear_all %d\n", (int)tessella_clear_feature_state(map));
        printf("feature_state_clear_twice %d\n", (int)tessella_clear_feature_state(map));
        printf("feature_state_clear_no_map %d\n", (int)tessella_clear_feature_state(NULL));
        /* And the map still ticks, which is the whole of what a refused state has to mean. */
        printf("tick_after_state %d\n", (int)tessella_tick(map));
    }

    /* A new style on a running map. The same document, so what is checked here is the call and
     * not the content: a second revision of a style with no sources needs no network. */
    printf("set_style %d\n",
           (int)tessella_set_style(map, (const uint8_t*)STYLE, strlen(STYLE)));
    /* A document that will not parse changes nothing and says so, which is what lets a host offer
     * one it is unsure of. */
    printf("set_style_bad %d\n",
           (int)tessella_set_style(map, (const uint8_t*)BAD, strlen(BAD)));
    printf("set_style_null %d\n", (int)tessella_set_style(map, NULL, 0));
    printf("set_style_no_map %d\n",
           (int)tessella_set_style(NULL, (const uint8_t*)STYLE, strlen(STYLE)));
    /* And the map still ticks, which is the whole of what "changes nothing" has to mean. */
    printf("tick_after_style %d\n", (int)tessella_tick(map));

    printf("set_camera %d\n", (int)tessella_set_camera(map, 48.85, 2.35, 11.0, 0.0, 0.0));

    /* Time passing, which is what makes a fade a fade rather than a switch. */
    printf("advance %d\n", (int)tessella_advance(map, 16.7));
    printf("advance_null %d\n", (int)tessella_advance(NULL, 16.7));

    /* A resize, through the header. Both a real one and the degenerate one a surface reports
     * while it is being torn down, which must be ignored rather than refused. */
    printf("viewport %d\n", (int)tessella_set_viewport(map, 800, 600));
    printf("viewport_zero %d\n", (int)tessella_set_viewport(map, 0, 0));
    printf("viewport_null %d\n", (int)tessella_set_viewport(NULL, 800, 600));

    /* The globe's one policy, through the declaration in the header rather than the Rust: an
     * enum whose repr disagreed would pass the wrong value with nothing to say so. */
    printf("world_copies_one %d\n",
           (int)tessella_set_world_copies(map, TESSELLA_WORLD_COPIES_ONE));
    printf("world_copies_repeated %d\n",
           (int)tessella_set_world_copies(map, TESSELLA_WORLD_COPIES_REPEATED));
    printf("world_copies_null %d\n",
           (int)tessella_set_world_copies(NULL, TESSELLA_WORLD_COPIES_ONE));
    printf("projection_globe %d\n",
           (int)tessella_set_projection(map, TESSELLA_PROJECTION_GLOBE));
    printf("projection_mercator %d\n",
           (int)tessella_set_projection(map, TESSELLA_PROJECTION_MERCATOR));
    printf("projection_null %d\n",
           (int)tessella_set_projection(NULL, TESSELLA_PROJECTION_GLOBE));

    /* The two conversions, which are the only calls that answer a question about the camera
     * rather than changing it. Driven here because the viewport is known: 800 x 600 from the
     * resize above, a flat north-up camera, and the plane. */
    {
        double latitude = 0.0;
        double longitude = 0.0;
        double x = 0.0;
        double y = 0.0;

        /* The middle of the screen is the camera's own coordinate, and projecting it back is the
         * middle of the screen. A y convention that disagreed with the Rust would come back
         * mirrored about the center, which is why the round trip is off-center. */
        printf("screen_to_geo %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 300.0, &latitude, &longitude));
        printf("screen_to_geo_center %d\n",
               (latitude > 48.84 && latitude < 48.86 && longitude > 2.34 && longitude < 2.36) ? 1
                                                                                             : 0);
        printf("geo_to_screen %d\n",
               (int)tessella_geo_to_screen(map, 48.85, 2.35, &x, &y));
        printf("geo_to_screen_center %d\n",
               (x > 399.9 && x < 400.1 && y > 299.9 && y < 300.1) ? 1 : 0);

        /* Off-center, and y down from the top: a pixel above the middle is north of the center. */
        printf("screen_to_geo_upper %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 100.0, &latitude, &longitude));
        printf("upper_is_north %d\n", latitude > 48.85 ? 1 : 0);
        printf("geo_to_screen_roundtrip %d\n",
               (int)tessella_geo_to_screen(map, latitude, longitude, &x, &y));
        printf("roundtrip_pixel %d\n", (x > 399.9 && x < 400.1 && y > 99.9 && y < 100.1) ? 1 : 0);

        /* The header's own number for the status these two answer with, so a header that drifted
         * from the Rust fails here rather than in a consumer's log. */
        printf("off_the_map_value %d\n", (int)TESSELLA_OFF_THE_MAP);

        /* Null arguments are answered rather than dereferenced, and a null handle is still not a
         * map. */
        printf("screen_to_geo_null_out %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 300.0, NULL, &longitude));
        printf("geo_to_screen_null_out %d\n",
               (int)tessella_geo_to_screen(map, 48.85, 2.35, &x, NULL));
        printf("screen_to_geo_null_map %d\n",
               (int)tessella_screen_to_geo(NULL, 400.0, 300.0, &latitude, &longitude));
        printf("geo_to_screen_null_map %d\n",
               (int)tessella_geo_to_screen(NULL, 48.85, 2.35, &x, &y));

        /* Pitched, where the top of the screen is sky. The sentinels say the out parameters were
         * left alone rather than clamped to something plausible. */
        printf("pitch %d\n", (int)tessella_set_camera(map, 48.85, 2.35, 11.0, 0.0, 75.0));
        latitude = -1000.0;
        longitude = -1000.0;
        printf("sky %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 20.0, &latitude, &longitude));
        printf("sky_untouched %d\n",
               (latitude == -1000.0 && longitude == -1000.0) ? 1 : 0);
        /* And the ground below the middle still answers. */
        printf("ground %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 500.0, &latitude, &longitude));

        /* A coordinate behind a pitched camera has no pixel either, which is the same condition
         * read in the other direction. */
        x = -1000.0;
        y = -1000.0;
        printf("behind %d\n", (int)tessella_geo_to_screen(map, 44.0, 2.35, &x, &y));
        printf("behind_untouched %d\n", (x == -1000.0 && y == -1000.0) ? 1 : 0);

        /* The globe. Same camera as the plane's checks, which is a zoom where the planet fills
         * the viewport: the center pixel is still the coordinate under the camera. */
        printf("globe %d\n", (int)tessella_set_projection(map, TESSELLA_PROJECTION_GLOBE));
        printf("globe_camera %d\n", (int)tessella_set_camera(map, 48.85, 2.35, 11.0, 0.0, 0.0));
        printf("globe_center %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 300.0, &latitude, &longitude));
        printf("globe_center_is_camera %d\n",
               (latitude > 48.84 && latitude < 48.86 && longitude > 2.34 && longitude < 2.36) ? 1
                                                                                             : 0);
        /* The far side of the planet has no pixel, which the plane has no equivalent of: a
         * Mercator map has no hidden half. */
        x = -1000.0;
        y = -1000.0;
        printf("far_side %d\n",
               (int)tessella_geo_to_screen(map, -48.85, 2.35 - 180.0, &x, &y));
        printf("far_side_untouched %d\n", (x == -1000.0 && y == -1000.0) ? 1 : 0);

        /* Zoomed out until the ball no longer fills the viewport, where a corner pixel is beside
         * the planet rather than on it. The camera is constrained at that zoom -- a map does not
         * show the world's edge -- so this asks nothing about where the center is. */
        printf("globe_out %d\n", (int)tessella_set_camera(map, 0.0, 0.0, 0.0, 0.0, 0.0));
        latitude = -1000.0;
        longitude = -1000.0;
        printf("beside_the_globe %d\n",
               (int)tessella_screen_to_geo(map, 2.0, 2.0, &latitude, &longitude));
        printf("beside_untouched %d\n",
               (latitude == -1000.0 && longitude == -1000.0) ? 1 : 0);
        /* And the middle of that same screen is on it. */
        printf("globe_out_center %d\n",
               (int)tessella_screen_to_geo(map, 400.0, 300.0, &latitude, &longitude));

        /* The regions in that same store, through the same header. Opened a second time while
         * the map above still holds its own handle to the file, which is the arrangement the
         * header describes: a region outlives every map that draws it. */
        {
            tessella_offline* offline = NULL;
            printf("offline_open %d\n",
                   (int)tessella_offline_open((const uint8_t*)cache_path, strlen(cache_path),
                                              &offline));
            printf("offline_handle %d\n", offline != NULL ? 1 : 0);

            static const char* const STYLE_URL = "https://host.invalid/style.json";
            tessella_region_spec spec;
            memset(&spec, 0, sizeof spec);
            spec.style_url = (const uint8_t*)STYLE_URL;
            spec.style_url_len = strlen(STYLE_URL);
            spec.west = 13.3;
            spec.south = 52.45;
            spec.east = 13.5;
            spec.north = 52.58;
            spec.min_zoom = 10.0;
            spec.max_zoom = 11.0;
            spec.pixel_ratio = 1.0;

            /* Sized before it is agreed to, from the style the host already has. This one has no
             * sources, so the only resource is the style document itself -- and the count is
             * exact, which is what a style with nothing to fetch a manifest for means. */
            tessella_offline_cost cost;
            memset(&cost, 0, sizeof cost);
            printf("offline_estimate %d\n",
                   (int)tessella_offline_estimate(offline, &spec, (const uint8_t*)STYLE,
                                                  strlen(STYLE), &cost));
            printf("estimate_tiles %d\n", (int)cost.tiles);
            printf("estimate_resources %d\n", cost.resources >= 1 ? 1 : 0);
            printf("estimate_precise %d\n", (int)cost.precise);

            uint64_t id = 0;
            printf("offline_define %d\n",
                   (int)tessella_offline_define(offline, &spec, &id));

            size_t listed = 0;
            uint64_t ids[4];
            printf("offline_list %d\n",
                   (int)tessella_offline_list(offline, ids, 4, &listed));
            printf("offline_listed %d\n", (int)listed);
            printf("offline_listed_id %d\n", listed == 1 && ids[0] == id ? 1 : 0);

            tessella_offline_counters counters;
            memset(&counters, 0, sizeof counters);
            printf("offline_progress %d\n",
                   (int)tessella_offline_progress(offline, id, &counters));
            printf("offline_idle %d\n", (int)counters.state);
            printf("offline_nothing_stored %d\n", counters.stored_resources == 0 ? 1 : 0);

            /* An identifier the store does not have, which is not a region at nothing percent. */
            printf("offline_no_such %d\n",
                   (int)tessella_offline_progress(offline, id + 999, &counters));

            printf("offline_delete %d\n", (int)tessella_offline_delete(offline, id));
            printf("offline_delete_twice %d\n", (int)tessella_offline_delete(offline, id));
            listed = 7;
            printf("offline_list_after %d\n",
                   (int)tessella_offline_list(offline, NULL, 0, &listed));
            printf("offline_empty %d\n", (int)listed);

            tessella_offline_close(offline);
            /* Null is accepted, as every destructor here is. */
            tessella_offline_close(NULL);
            printf("offline_closed %d\n", 1);
        }

        /* Back to the camera and the projection the rest of the probe expects. */
        printf("back_to_plane %d\n",
               (int)tessella_set_projection(map, TESSELLA_PROJECTION_MERCATOR));
        printf("back_to_camera %d\n",
               (int)tessella_set_camera(map, 48.85, 2.35, 11.0, 0.0, 0.0));
    }

    printf("tick_first %d\n", (int)tessella_tick(map));
    printf("tick_second %d\n", (int)tessella_tick(map));

    /* Ticked until the readiness settles, because a map is progressive: create parses the style
     * and stops, and the sources resolve on a worker afterwards. Reading the status straight
     * after a tick reports TESSELLA_RESOLVING and is not wrong -- it is a race, and a consumer
     * that treated one reading as final would have written the same bug.
     *
     * This is the loop a consumer runs anyway: tick at vsync, and look at the status when it
     * wants to know why nothing is on screen yet. */
    int32_t readiness = -1;
    char reason[256];
    int status = -1;
    memset(reason, 0, sizeof reason);
    for (int spin = 0; spin < 2000; spin++) {
        status = (int)tessella_tick(map);
        if (status != TESSELLA_OK) {
            break;
        }
        status = (int)tessella_status(map, &readiness, reason, sizeof reason);
        if (status != TESSELLA_OK || readiness == TESSELLA_READY ||
            readiness == TESSELLA_FAILED_TO_RESOLVE) {
            break;
        }
        {
            struct timespec pause;
            pause.tv_sec = 0;
            pause.tv_nsec = 1000000L; /* a millisecond */
            nanosleep(&pause, NULL);
        }
    }
    printf("status %d\n", status);
    printf("readiness %d\n", (int)readiness);
    printf("reason_empty %d\n", reason[0] == '\0' ? 1 : 0);

    /* The reason buffer is optional, which is the common case for a consumer that only wants to
     * know whether to keep waiting. */
    readiness = -1;
    printf("status_no_reason %d\n", (int)tessella_status(map, &readiness, NULL, 0));
    printf("readiness_again %d\n", (int)readiness);

    tessella_map_regions regions;
    memset(&regions, 0, sizeof regions);
    printf("regions %d\n", (int)tessella_regions(map, &regions));
    printf("ring_non_null %d\n", regions.ring != NULL ? 1 : 0);
    printf("ring_len_nonzero %d\n", regions.ring_len > 0 ? 1 : 0);

    /* A hosted map: the caller fetches, and the map asks. This is the browser's arrangement
     * driven from C, which is the only place the three calls can be checked as declared. */
    {
        static const char* const HOSTED_STYLE =
            "{\"version\": 8, \"sources\": {\"v\": {\"type\": \"vector\","
            " \"tiles\": [\"http://host.invalid/{z}/{x}/{y}.pbf\"],"
            " \"minzoom\": 0, \"maxzoom\": 6}},"
            " \"layers\": [{\"id\": \"w\", \"type\": \"fill\", \"source\": \"v\","
            " \"source-layer\": \"water\"}]}";

        /* The pooled map refuses the hosted calls, which is the only thing that tells a caller
         * it created the wrong kind. Checked before the hosted map exists, so a pass here cannot
         * be the hosted one answering by accident. */
        uint64_t stray = 999;
        const uint8_t* stray_url = NULL;
        size_t stray_len = 0;
        printf("take_on_pooled %d\n",
               (int)tessella_take_request(map, &stray, &stray_url, &stray_len));
        printf("answer_on_pooled %d\n", (int)tessella_answer(map, 1, 200, NULL, 0));

        tessella_config hosted_config = config;
        hosted_config.style_json = (const uint8_t*)HOSTED_STYLE;
        hosted_config.style_json_len = strlen(HOSTED_STYLE);
        tessella_map* hosted = NULL;
        printf("create_hosted %d\n",
               (int)tessella_create_hosted(&hosted_config, 51.505, -0.11, 3.0, &hosted));
        printf("hosted_non_null %d\n", hosted != NULL ? 1 : 0);

        int served = 0;
        int urls_seen = 0;
        int hosted_status = -1;
        /* Paced, and for the reason the readiness loop above is: a map is progressive, and the
         * source this one has to ask about resolves on a worker. Four hundred ticks with nothing
         * between them run to completion in microseconds and the worker never gets scheduled --
         * which passes on an idle machine and fails on a loaded CI runner, where it did. A
         * millisecond a spin gives the same two seconds the pooled loop above allows itself. */
        for (int spin = 0; spin < 2000 && hosted != NULL; spin++) {
            hosted_status = (int)tessella_tick(hosted);
            if (hosted_status != TESSELLA_OK) {
                break;
            }
            for (;;) {
                uint64_t ticket = 0;
                const uint8_t* url = NULL;
                size_t url_len = 0;
                if ((int)tessella_take_request(hosted, &ticket, &url, &url_len) != TESSELLA_OK) {
                    hosted_status = -2;
                    break;
                }
                /* Zero is what a map with nothing to fetch says, and it is the loop's exit. */
                if (ticket == 0) {
                    break;
                }
                if (url != NULL && url_len > 0) {
                    urls_seen++;
                }
                /* Answered 404, which is an answer: the tile is outside this source's coverage
                 * as far as the map is concerned, and the map draws around it. No fixture bytes
                 * are needed to check that the loop itself turns. */
                tessella_answer(hosted, ticket, 404, NULL, 0);
                served++;
            }
            if (served > 0) {
                break;
            }
            {
                struct timespec pause;
                pause.tv_sec = 0;
                pause.tv_nsec = 1000000L; /* a millisecond */
                nanosleep(&pause, NULL);
            }
        }
        printf("hosted_status %d\n", hosted_status);
        printf("hosted_served %d\n", served > 0 ? 1 : 0);
        printf("hosted_urls %d\n", urls_seen == served ? 1 : 0);

        int32_t hosted_readiness = -1;
        printf("hosted_ready %d\n",
               (int)tessella_status(hosted, &hosted_readiness, NULL, 0));
        printf("hosted_readiness %d\n", (int)hosted_readiness);
        tessella_destroy(hosted);
    }

    tessella_destroy(map);
    /* Destroying null is a no-op, which is what lets a consumer tear down without a branch. */
    tessella_destroy(NULL);
    printf("done 1\n");
    return 0;
}
