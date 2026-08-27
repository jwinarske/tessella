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
 * mapped it. Not that the counters are stable: head is read once up front, as a consumer racing
 * a live producer must. Not that a record is well-formed: a length that overruns its own record
 * is refused rather than followed.
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: consumer <ring.bin> <slabs.bin>\n");
        return 2;
    }
    buffer ring = read_file(argv[1]);
    buffer slabs = read_file(argv[2]);

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

    /* Read once, as a consumer racing a live producer must: a head that advanced mid-walk would
     * otherwise let the loop run past the bytes that were published when it started. */
    uint64_t head = control.head;
    uint64_t cursor = control.tail;

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
        case TSL_ENVELOPE_KIND_VIEW_USE:
            drawables++;
            break;
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
    return unresolved == 0 ? 0 : 1;
}
