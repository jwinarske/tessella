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
    tessella_config config;
    config.style_json = STYLE;
    config.width = 1024;
    config.height = 768;
    config.ring_capacity = 1u << 22;

    tessella_map* map = NULL;
    printf("create %d\n", (int)tessella_create(&config, 51.505, -0.11, 13.0, &map));
    printf("handle_non_null %d\n", map != NULL ? 1 : 0);
    if (map == NULL) {
        return 1;
    }

    /* A style that will not parse must fail at create, and must not hand back a handle. */
    tessella_config bad = config;
    bad.style_json = "{ this is not a style";
    tessella_map* rejected = NULL;
    printf("bad_style %d\n", (int)tessella_create(&bad, 0.0, 0.0, 0.0, &rejected));
    printf("bad_style_handle_null %d\n", rejected == NULL ? 1 : 0);

    /* Null arguments are answered rather than dereferenced. */
    printf("null_config %d\n", (int)tessella_create(NULL, 0.0, 0.0, 0.0, &map));
    printf("null_out %d\n", (int)tessella_create(&config, 0.0, 0.0, 0.0, NULL));
    printf("null_map_tick %d\n", (int)tessella_tick(NULL));

    printf("set_camera %d\n", (int)tessella_set_camera(map, 48.85, 2.35, 11.0, 0.0, 0.0));

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

    tessella_destroy(map);
    /* Destroying null is a no-op, which is what lets a consumer tear down without a branch. */
    tessella_destroy(NULL);
    printf("done 1\n");
    return 0;
}
