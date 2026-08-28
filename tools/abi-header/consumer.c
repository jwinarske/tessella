/* A capture consumer that knows only the generated header.
 *
 * # Why this exists
 *
 * Everything that had ever read the stream was Rust, in-process, sharing the arena and the type
 * definitions with the producer. That is not a consumer so much as the producer looking at
 * itself: it cannot notice a field the header fails to describe, a layout rule that lives only
 * in a Rust doc comment, or a handle that resolves against a table nothing says how to index.
 * `probe.c` did not close the gap either -- it takes three sizeofs, and CI compiles it with
 * -fsyntax-only, so no C had ever *run* against this ABI.
 *
 * So this walks a ring and a packed slab region given nothing but two byte buffers, the header,
 * and libc. Every rule it follows is either stated in the header or is a bug in the header. It
 * prints a summary the producer's own numbers are checked against.
 *
 * # What it does not assume
 *
 * Not that the buffers are aligned: every read of a shared structure goes through memcpy, since
 * the producer promises alignment within its region and says nothing about where a consumer
 * mapped it. Not that the counters are stable: head is read once per pass, as a consumer racing
 * a live producer must. Not that a record is well-formed: a length that overruns its own record
 * is refused rather than followed.
 *
 * # Live mode
 *
 * `--live <ring> <timeout-ms>` maps the ring shared instead of reading a copy of it, and is the
 * consumer half of the process-isolation spike (plan.md 3.5). Two things separate it from the
 * file mode, and neither is cosmetic.
 *
 * It re-reads `head` with an acquiring load every pass, so it sees a group the moment the
 * producer's releasing store publishes it -- which is what makes the coupling live rather than
 * a walk over a finished buffer.
 *
 * And it publishes `tail`, which the file mode never had to. A ring is a fixed number of bytes:
 * a producer cannot write past what the consumer has consumed, and a consumer that reads
 * without publishing looks exactly like one that stalled. Across a mapping that is the whole of
 * backpressure, and it is why this mode is what proves the seam works: with a ring smaller than
 * the frames going through it, the producer only makes progress because this process says it
 * has.
 *
 * It stops on `ViewUndeclare` -- the producer tearing its view down is the end of the stream --
 * or when the timeout expires with no progress, which is what a dead producer looks like.
 */

/* mmap, clock_gettime and nanosleep are POSIX rather than C11, and this compiles as C11. */
#define _POSIX_C_SOURCE 200809L

#include <fcntl.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include "tessella_capture_abi.h"

/* The producer pads a record's fixed part to this before the payload begins. */
static size_t align_up(size_t value, size_t to) {
    return (value + to - 1) / to * to;
}

typedef struct {
    const uint8_t *bytes;
    size_t len;
} buffer;

static buffer read_file(const char *path) {
    buffer out = {NULL, 0};
    FILE *file = fopen(path, "rb");
    if (!file) {
        fprintf(stderr, "consumer: cannot open %s\n", path);
        exit(2);
    }
    if (fseek(file, 0, SEEK_END) != 0) {
        exit(2);
    }
    long size = ftell(file);
    if (size < 0) {
        exit(2);
    }
    rewind(file);
    uint8_t *bytes = (uint8_t *)malloc((size_t)size ? (size_t)size : 1);
    if (!bytes || fread(bytes, 1, (size_t)size, file) != (size_t)size) {
        fprintf(stderr, "consumer: cannot read %s\n", path);
        exit(2);
    }
    fclose(file);
    out.bytes = bytes;
    out.len = (size_t)size;
    return out;
}

/* Resolves a slab reference against the packed region.
 *
 * The handle indexes the table, and the header says so -- though only because writing this made
 * the omission visible. The rule was in the Rust doc comment on `SlabEntry` and had not reached
 * the generated header, which is the only thing a C consumer reads. Indexing by the handle was
 * the one reading the layout admitted, but a consumer should not have to infer it.
 *
 * The bounds check is not defensive clutter: a handle the table does not cover has no meaning,
 * and refusing it is what turns a producer fault into a diagnosis rather than a wild read.
 */
/* Records an id in the declared set.
 *
 * Both `tsl_geometry_add` and `tsl_mesh_add` land here, and that is the ABI's rule rather than a
 * convenience: the header says a mesh's id "is in the same space as tsl_geometry_add's, so
 * tsl_view_use, tsl_view_release and tsl_geometry_remove bind, release and drop a mesh exactly
 * as they do geometry". A consumer keeping two tables resolves a use of a mesh against neither. */
static int declare_id(uint64_t **ids, size_t *count, size_t *cap, uint64_t id) {
    if (*count == *cap) {
        size_t grown = *cap ? *cap * 2 : 64;
        uint64_t *bigger = (uint64_t *)realloc(*ids, grown * sizeof(uint64_t));
        if (!bigger) {
            return 0;
        }
        *ids = bigger;
        *cap = grown;
    }
    (*ids)[(*count)++] = id;
    return 1;
}

static const uint8_t *resolve(buffer region, tsl_slab_ref ref, uint64_t *length_out) {
    tsl_slab_region header;
    if (region.len < sizeof header) {
        return NULL;
    }
    memcpy(&header, region.bytes, sizeof header);
    if (header.abi_rev != TSL_ABI_REV || ref.slab >= header.count) {
        return NULL;
    }

    size_t at = sizeof(tsl_slab_region) + (size_t)ref.slab * sizeof(tsl_slab_entry);
    if (at + sizeof(tsl_slab_entry) > region.len) {
        return NULL;
    }
    tsl_slab_entry entry;
    memcpy(&entry, region.bytes + at, sizeof entry);

    if (entry.offset > region.len || entry.length > region.len - entry.offset) {
        return NULL;
    }
    if ((uint64_t)ref.offset + ref.length > entry.length) {
        return NULL;
    }
    *length_out = ref.length;
    return region.bytes + entry.offset + ref.offset;
}

/* Maps a file shared, so writes by the other process are seen by this one. */
static buffer map_shared(const char *path, uint8_t **writable) {
    buffer out = {NULL, 0};
    int fd = open(path, O_RDWR);
    if (fd < 0) {
        fprintf(stderr, "consumer: cannot open %s\n", path);
        exit(2);
    }
    struct stat info;
    if (fstat(fd, &info) != 0 || info.st_size <= 0) {
        fprintf(stderr, "consumer: cannot size %s\n", path);
        exit(2);
    }
    void *base = mmap(NULL, (size_t)info.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (base == MAP_FAILED) {
        fprintf(stderr, "consumer: cannot map %s\n", path);
        exit(2);
    }
    *writable = (uint8_t *)base;
    out.bytes = (const uint8_t *)base;
    out.len = (size_t)info.st_size;
    return out;
}

/* Milliseconds on a clock that does not step. */
static uint64_t now_ms(void) {
    struct timespec at;
    clock_gettime(CLOCK_MONOTONIC, &at);
    return (uint64_t)at.tv_sec * 1000u + (uint64_t)(at.tv_nsec / 1000000);
}

int main(int argc, char **argv) {
    int live = 0;
    uint64_t timeout_ms = 0;
    if (argc == 5 && strcmp(argv[1], "--live") == 0) {
        live = 1;
        timeout_ms = strtoull(argv[4], NULL, 10);
    } else if (argc != 3) {
        fprintf(stderr, "usage: consumer <ring.bin> <slabs.bin>\n");
        fprintf(stderr, "       consumer --live <ring> <slabs> <timeout-ms>\n");
        return 2;
    }

    uint8_t *shared = NULL, *unused = NULL;
    buffer ring = live ? map_shared(argv[2], &shared) : read_file(argv[1]);
    /* The slab region is mapped too, and is the producer's arena rather than a copy of it: a
     * producer allocating out of the shared region has the bytes in place before the record
     * naming them can be published, so a handle read off a visible record resolves (plan.md
     * 11.3). Resolving here is what tests that — `unresolved` is nonzero if it is not true. */
    buffer slabs = live ? map_shared(argv[3], &unused) : read_file(argv[2]);

    tsl_ring_control control;
    if (ring.len < sizeof control) {
        fprintf(stderr, "consumer: ring is shorter than its control block\n");
        return 1;
    }
    memcpy(&control, ring.bytes, sizeof control);
    if (control.abi_rev != TSL_ABI_REV) {
        fprintf(stderr, "consumer: ring is rev %u, this build speaks rev %u\n",
                control.abi_rev, (unsigned)TSL_ABI_REV);
        return 1;
    }
    if (control.capacity == 0 || (control.capacity & (control.capacity - 1)) != 0) {
        fprintf(stderr, "consumer: capacity %llu is not a power of two\n",
                (unsigned long long)control.capacity);
        return 1;
    }
    if (ring.len < sizeof control + control.capacity) {
        fprintf(stderr, "consumer: ring is shorter than control block plus capacity\n");
        return 1;
    }

    const uint8_t *data = ring.bytes + sizeof(tsl_ring_control);
    uint64_t records = 0, skips = 0;
    uint64_t counts[16] = {0};
    uint64_t geometries = 0, drawables = 0, order_entries = 0;
    uint64_t vertices = 0, attributes = 0, resolved_bytes = 0;
    uint64_t unresolved = 0;
    /* Uniform buffers, which are what a mirror actually binds. DR-16 consolidates one per
     * (view, layer) and indexes it by the order entry's ubo_index, so a consumer that could
     * read geometry and not these could register a scene and draw none of it. */
    uint64_t ubos = 0, ubo_bytes = 0, ubo_frame_wide = 0, ubo_truncated = 0;
    /* Textures and stencil tiles, which carry the two payload shapes nothing else here does: a
     * fixed array with a separate count, and a span whose count is in elements rather than in
     * bytes. Both are places a header is insufficient by omission rather than by error -- the
     * struct is right and the rule for reading it is not written down -- so a consumer that
     * walks them is what proves the header enough for a real mirror. */
    uint64_t textures = 0, texture_rects = 0, texture_bytes = 0, texture_bad = 0;
    uint64_t whole_texture_uploads = 0;
    uint64_t stencils = 0, stencil_tiles = 0, stencil_bad = 0;
    /* The rest of the twelve kinds. A mirror has to act on every one of these -- a retirement
     * frees GPU resources, a view declaration opens a scene -- and a consumer that walked only
     * the records it found interesting would leak, or draw one view's geometry into another. */
    uint64_t removes = 0, remove_unknown = 0, releases = 0;
    uint64_t declares = 0, undeclares = 0, view_bad = 0;
    uint64_t meshes = 0, mesh_bytes = 0, mesh_unknown_format = 0;
    /* Every geometry id declared, so a use naming one that was not can be reported. The ABI
     * says a consumer "looks an id up and finds whichever kind of thing it added", and that a
     * use of an id it never met is a protocol fault. A consumer that counted uses without
     * resolving them would never notice -- which is what this one did until it was asked to. */
    uint64_t *declared = NULL;
    size_t declared_count = 0, declared_cap = 0;
    uint64_t dangling = 0;
    uint64_t cameras = 0, camera_bad = 0;
    /* Carried out so the producer's own numbers can be checked against them. */
    double first_proj0 = 0.0, first_pitch = -1.0, first_intensity = -1.0;
    uint64_t first_epoch = 0, first_cutoff = 0;

    /* The counters live in the shared region, not in the snapshot above: `control` was copied
     * before the producer had written anything more, and its head and tail are already stale.
     * In file mode there is no other process and the copy is the whole truth. */
    uint64_t *head_at = NULL, *tail_at = NULL;
    if (live) {
        head_at = (uint64_t *)(void *)(shared + offsetof(tsl_ring_control, head));
        tail_at = (uint64_t *)(void *)(shared + offsetof(tsl_ring_control, tail));
    }

    /* Read once per pass, as a consumer racing a live producer must: a head that advanced
     * mid-walk would otherwise let the loop run past the bytes that were published when it
     * started. The acquire is what the header asks for -- "producer releases, consumer
     * acquires" -- and without it the records a published head points at are not guaranteed
     * visible to this thread, whatever the counter says. */
    uint64_t head = live ? __atomic_load_n(head_at, __ATOMIC_ACQUIRE) : control.head;
    uint64_t cursor = live ? __atomic_load_n(tail_at, __ATOMIC_RELAXED) : control.tail;
    uint64_t started = now_ms(), progressed = started;
    int ended = !live;

    while (!ended || cursor < head) {
    while (cursor < head) {
        size_t offset = (size_t)(cursor & (control.capacity - 1));
        tsl_record_header record;
        if (control.capacity - offset < sizeof record) {
            fprintf(stderr, "consumer: a record header straddles the wrap at %llu\n",
                    (unsigned long long)cursor);
            return 1;
        }
        memcpy(&record, data + offset, sizeof record);

        if (record.total_len < sizeof record || record.total_len > control.capacity) {
            fprintf(stderr, "consumer: record at %llu claims %u bytes\n",
                    (unsigned long long)cursor, record.total_len);
            return 1;
        }
        if (record.flags & TSL_RECORD_FLAG_SKIP) {
            skips++;
            cursor += record.total_len;
            continue;
        }

        size_t body = align_up(record.record_len, TSL_PAYLOAD_ALIGN);
        if (sizeof record + body + record.payload_len > record.total_len) {
            fprintf(stderr, "consumer: record at %llu overruns itself\n",
                    (unsigned long long)cursor);
            return 1;
        }
        const uint8_t *fixed = data + offset + sizeof(tsl_record_header);
        const uint8_t *payload = fixed + body;

        records++;
        if (record.kind < 16) {
            counts[record.kind]++;
        }

        switch (record.kind) {
        case TSL_ENVELOPE_KIND_GEOMETRY_ADD: {
            tsl_geometry_add add;
            if (record.record_len < sizeof add) {
                break;
            }
            memcpy(&add, fixed, sizeof add);
            geometries++;
            if (!declare_id(&declared, &declared_count, &declared_cap, add.geometry)) {
                fprintf(stderr, "consumer: out of memory\n");
                return 2;
            }
            vertices += add.vertex_count;

            for (uint32_t i = 0; i < add.attrs.count; i++) {
                size_t at = add.attrs.offset + (size_t)i * sizeof(tsl_attribute_desc);
                if (at + sizeof(tsl_attribute_desc) > record.payload_len) {
                    break;
                }
                tsl_attribute_desc desc;
                memcpy(&desc, payload + at, sizeof desc);
                attributes++;

                uint64_t length = 0;
                if (resolve(slabs, desc.source, &length)) {
                    resolved_bytes += length;
                } else if (desc.source.length != 0) {
                    unresolved++;
                }
            }
            break;
        }
        case TSL_ENVELOPE_KIND_VIEW_USE: {
            drawables++;
            tsl_view_use use;
            if (record.record_len < sizeof use) {
                break;
            }
            memcpy(&use, fixed, sizeof use);
            /* Linear: a frame declares tens of geometries, and the records arrive in protocol
             * order so every add precedes every use of it. */
            int found = 0;
            for (size_t i = 0; i < declared_count; i++) {
                if (declared[i] == use.geometry) {
                    found = 1;
                    break;
                }
            }
            if (!found) {
                dangling++;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_GEOMETRY_REMOVE: {
            tsl_geometry_remove gone;
            if (record.record_len < sizeof gone) {
                break;
            }
            memcpy(&gone, fixed, sizeof gone);
            removes++;
            /* A retirement of something that was never declared is the same protocol fault a
             * dangling use is, seen from the other end -- and it is the one a consumer notices
             * *late*, because it frees nothing and then leaks the geometry that really was
             * added. Found by the same linear scan, and the entry is struck so a second remove
             * of one id is caught too. */
            int known = 0;
            for (size_t i = 0; i < declared_count; i++) {
                if (declared[i] == gone.geometry) {
                    declared[i] = declared[declared_count - 1];
                    declared_count--;
                    known = 1;
                    break;
                }
            }
            if (!known) {
                remove_unknown++;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_VIEW_RELEASE: {
            tsl_view_release release;
            if (record.record_len < sizeof release) {
                break;
            }
            memcpy(&release, fixed, sizeof release);
            releases++;
            break;
        }
        case TSL_ENVELOPE_KIND_VIEW_DECLARE: {
            tsl_view_declare declare;
            if (record.record_len < sizeof declare) {
                break;
            }
            memcpy(&declare, fixed, sizeof declare);
            declares++;
            /* The view a frame's records belong to. A consumer that ignored this would put
             * every view's drawables in one scene, which is the failure the whole per-view
             * split exists to prevent. */
            if (declare.view >= TSL_MAX_VIEWS) {
                view_bad++;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_VIEW_UNDECLARE: {
            tsl_view_undeclare undeclare;
            if (record.record_len < sizeof undeclare) {
                break;
            }
            memcpy(&undeclare, fixed, sizeof undeclare);
            undeclares++;
            break;
        }
        case TSL_ENVELOPE_KIND_MESH_ADD: {
            tsl_mesh_add mesh;
            if (record.record_len < sizeof mesh) {
                break;
            }
            memcpy(&mesh, fixed, sizeof mesh);
            meshes++;
            /* A mesh is bytes in a slab and a format saying what they are. The header is
             * explicit that a consumer meeting a format it does not know must *skip* the mesh
             * rather than guess at the bytes, so that is what this does -- and counts, because
             * silently skipping every mesh is how a consumer draws an empty map and reports
             * success. */
            if (mesh.format != TSL_MESH_FORMAT_GLB) {
                mesh_unknown_format++;
                break;
            }
            uint64_t bytes = 0;
            if (!resolve(slabs, mesh.bytes, &bytes)) {
                unresolved++;
                break;
            }
            mesh_bytes += bytes;
            /* Same table as geometry, per the header's own sentence. */
            if (!declare_id(&declared, &declared_count, &declared_cap, mesh.mesh)) {
                fprintf(stderr, "consumer: out of memory\n");
                return 2;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_UBO_UPDATE: {
            tsl_ubo_update update;
            if (record.record_len < sizeof update) {
                break;
            }
            memcpy(&update, fixed, sizeof update);
            ubos++;
            /* A span's offset and count are into the payload region, and the header says to
             * validate both against payload_len before trusting either. For a ubo the count is
             * a byte count rather than an element count -- the struct says so, and reading it
             * as elements would multiply by a stride that does not exist. */
            if ((uint64_t)update.data.offset + update.data.count > record.payload_len) {
                ubo_truncated++;
                break;
            }
            ubo_bytes += update.data.count;
            if (update.layer_index < 0) {
                ubo_frame_wide++;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_CAMERA_UPDATE: {
            tsl_camera_update camera;
            if (record.record_len < sizeof camera) {
                camera_bad++;
                break;
            }
            memcpy(&camera, fixed, sizeof camera);
            if (cameras == 0) {
                first_proj0 = camera.proj_matrix[0];
                first_pitch = camera.pitch;
                first_intensity = camera.light.intensity;
                first_epoch = camera.order_epoch;
                first_cutoff = camera.opaque_pass_cutoff;
            }
            /* A projection whose first column is zero is not a projection. Cheap, and it is the
             * shape of failure a wrong offset produces: the read lands in padding and every
             * double comes back zero. */
            if (camera.proj_matrix[0] == 0.0 && camera.proj_matrix[5] == 0.0) {
                camera_bad++;
            }
            cameras++;
            break;
        }
        case TSL_ENVELOPE_KIND_TEXTURE_UPDATE: {
            tsl_texture_update texture;
            if (record.record_len < sizeof texture) {
                break;
            }
            memcpy(&texture, fixed, sizeof texture);
            textures++;
            /* The rectangles are a fixed array with a count beside it, not a span, and the
             * count is the only thing that says how much of the array means anything. Reading
             * the whole array would take whatever the tail of it happens to hold as damage. */
            if (texture.rect_count > TSL_TEXTURE_RECT_CAP) {
                texture_bad++;
                break;
            }
            texture_rects += texture.rect_count;
            if ((uint64_t)texture.pixels.offset + texture.pixels.count > record.payload_len) {
                texture_bad++;
                break;
            }
            texture_bytes += texture.pixels.count;

            /* How many bytes the pixels *should* be, which is the check that needs the header to
             * say more than the format's name. A count that does not match the area is an
             * upload that will run off the end of the surface or leave part of it stale, and
             * nothing else in the record contradicts it. */
            uint32_t pixel = tsl_texture_pixel_size(texture.format);
            if (pixel == 0) {
                texture_bad++;
                break;
            }
            if (texture.rect_count == 0) {
                /* A whole-texture upload. The header says rect_count of zero means this, and a
                 * consumer that took it as "no damage" would upload nothing and sample a blank
                 * atlas -- which is a map with no labels and no error anywhere. */
                whole_texture_uploads++;
                uint64_t want = (uint64_t)texture.size.width * texture.size.height * pixel;
                if (want != texture.pixels.count) {
                    texture_bad++;
                }
                break;
            }
            /* Every rectangle has to fall inside the texture it damages, and together they have
             * to account for the bytes. A rect that does not fit is an upload past the end of
             * the surface, which is the shape of failure a wrong offset produces here: the array
             * is read from the wrong place and the coordinates come back as neighbouring
             * fields. */
            uint64_t want = 0;
            for (unsigned r = 0; r < texture.rect_count; r++) {
                uint32_t right = (uint32_t)texture.rects[r].x + texture.rects[r].w;
                uint32_t bottom = (uint32_t)texture.rects[r].y + texture.rects[r].h;
                if (right > texture.size.width || bottom > texture.size.height) {
                    texture_bad++;
                }
                want += (uint64_t)texture.rects[r].w * texture.rects[r].h * pixel;
            }
            if (want != texture.pixels.count) {
                texture_bad++;
            }
            break;
        }
        case TSL_ENVELOPE_KIND_STENCIL_TILES: {
            tsl_stencil_tiles stencil;
            if (record.record_len < sizeof stencil) {
                break;
            }
            memcpy(&stencil, fixed, sizeof stencil);
            stencils++;
            /* A span of structs rather than of bytes, which is the case a header gets wrong by
             * omission: the offset is in bytes and the count is in *elements*, so a consumer
             * that validated `offset + count` would accept a list running fifteen sixteenths
             * past the end of the payload. */
            uint64_t span_bytes = (uint64_t)stencil.tiles.count * sizeof(tsl_stencil_tile);
            if ((uint64_t)stencil.tiles.offset + span_bytes > record.payload_len) {
                stencil_bad++;
                break;
            }
            stencil_tiles += stencil.tiles.count;
            for (uint32_t t = 0; t < stencil.tiles.count; t++) {
                tsl_stencil_tile entry;
                memcpy(&entry, payload + stencil.tiles.offset + t * sizeof entry, sizeof entry);
                /* A tile matrix that is entirely zero never came from a camera. */
                if (entry.matrix[0] == 0.0f && entry.matrix[5] == 0.0f
                    && entry.matrix[15] == 0.0f) {
                    stencil_bad++;
                }
            }
            break;
        }
        case TSL_ENVELOPE_KIND_ORDER_UPDATE: {
            tsl_order_update update;
            if (record.record_len < sizeof update) {
                break;
            }
            memcpy(&update, fixed, sizeof update);
            order_entries += update.entries.count;
            break;
        }
        default:
            break;
        }

        cursor += record.total_len;
        if (record.kind == TSL_ENVELOPE_KIND_VIEW_UNDECLARE) {
            ended = 1;
        }
    }

    if (!live) {
        break;
    }
    /* The bytes this pass consumed are the producer's to reuse. Released, so the producer's
     * acquiring load of tail cannot see the space before it sees that the reads are done. */
    __atomic_store_n(tail_at, cursor, __ATOMIC_RELEASE);
    if (ended) {
        break;
    }
    uint64_t reached = __atomic_load_n(head_at, __ATOMIC_ACQUIRE);
    if (reached != head) {
        head = reached;
        progressed = now_ms();
        continue;
    }
    if (now_ms() - progressed > timeout_ms) {
        fprintf(stderr, "consumer: no progress for %llums; the producer is gone\n",
                (unsigned long long)timeout_ms);
        return 3;
    }
    /* A sleep rather than a spin: this is a spike proving the coupling, not a latency
     * measurement, and burning a core to shave microseconds off it would prove nothing. */
    struct timespec pause = {0, 200000};
    nanosleep(&pause, NULL);
    head = __atomic_load_n(head_at, __ATOMIC_ACQUIRE);
    }

    printf("records %llu\n", (unsigned long long)records);
    printf("skips %llu\n", (unsigned long long)skips);
    printf("geometries %llu\n", (unsigned long long)geometries);
    printf("drawables %llu\n", (unsigned long long)drawables);
    printf("order_entries %llu\n", (unsigned long long)order_entries);
    printf("vertices %llu\n", (unsigned long long)vertices);
    printf("attributes %llu\n", (unsigned long long)attributes);
    printf("resolved_bytes %llu\n", (unsigned long long)resolved_bytes);
    printf("unresolved %llu\n", (unsigned long long)unresolved);
    printf("ubos %llu\n", (unsigned long long)ubos);
    printf("ubo_bytes %llu\n", (unsigned long long)ubo_bytes);
    printf("ubo_frame_wide %llu\n", (unsigned long long)ubo_frame_wide);
    printf("ubo_truncated %llu\n", (unsigned long long)ubo_truncated);
    printf("cameras %llu\n", (unsigned long long)cameras);
    printf("camera_bad %llu\n", (unsigned long long)camera_bad);
    /* Scaled to integers so the harness can compare them without parsing doubles: the point is
     * that C read the same number the producer wrote, not that it round-trips a decimal. */
    printf("camera_proj0_micro %lld\n", (long long)(first_proj0 * 1000000.0));
    printf("camera_pitch_milli %lld\n", (long long)(first_pitch * 1000.0));
    printf("camera_light_milli %lld\n", (long long)(first_intensity * 1000.0));
    printf("camera_epoch %llu\n", (unsigned long long)first_epoch);
    printf("camera_cutoff %llu\n", (unsigned long long)first_cutoff);
    printf("textures %llu\n", (unsigned long long)textures);
    printf("texture_rects %llu\n", (unsigned long long)texture_rects);
    printf("texture_bytes %llu\n", (unsigned long long)texture_bytes);
    printf("texture_bad %llu\n", (unsigned long long)texture_bad);
    printf("whole_texture_uploads %llu\n", (unsigned long long)whole_texture_uploads);
    printf("stencils %llu\n", (unsigned long long)stencils);
    printf("stencil_tiles %llu\n", (unsigned long long)stencil_tiles);
    printf("stencil_bad %llu\n", (unsigned long long)stencil_bad);
    printf("removes %llu\n", (unsigned long long)removes);
    printf("remove_unknown %llu\n", (unsigned long long)remove_unknown);
    printf("releases %llu\n", (unsigned long long)releases);
    printf("declares %llu\n", (unsigned long long)declares);
    printf("undeclares %llu\n", (unsigned long long)undeclares);
    printf("view_bad %llu\n", (unsigned long long)view_bad);
    printf("meshes %llu\n", (unsigned long long)meshes);
    printf("mesh_bytes %llu\n", (unsigned long long)mesh_bytes);
    printf("mesh_unknown_format %llu\n", (unsigned long long)mesh_unknown_format);
    printf("dangling_uses %llu\n", (unsigned long long)dangling);
    free(declared);
    return (unresolved == 0 && ubo_truncated == 0 && camera_bad == 0) ? 0 : 1;
}
