/* SPDX-License-Identifier: Apache-2.0
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

static const char* const STYLE =
    "{\"version\": 8, \"sources\": {}, \"layers\": ["
    "{\"id\": \"bg\", \"type\": \"background\","
    " \"paint\": {\"background-color\": \"#101418\"}}]}";

int main(void) {
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
    static const char* const BAD = "{ this is not a style";
    bad.style_json = (const uint8_t*)BAD;
    bad.style_json_len = strlen(BAD);
    tessella_map* rejected = NULL;
    printf("bad_style %d\n", (int)tessella_create(&bad, 0.0, 0.0, 0.0, &rejected));
    printf("bad_style_handle_null %d\n", rejected == NULL ? 1 : 0);

    /* Null arguments are answered rather than dereferenced. */
    printf("null_config %d\n", (int)tessella_create(NULL, 0.0, 0.0, 0.0, &map));
    printf("null_out %d\n", (int)tessella_create(&config, 0.0, 0.0, 0.0, NULL));
    printf("null_map_tick %d\n", (int)tessella_tick(NULL));

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
        for (int spin = 0; spin < 400 && hosted != NULL; spin++) {
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
