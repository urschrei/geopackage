/* Copy a layer from one GeoPackage to another, entirely from C.
 *
 * The point of this program is that it needs no schema-description API: the
 * destination layer is created from the *source stream's own Arrow schema*, so
 * the whole round trip is read_arrow -> create_layer_from_arrow_schema ->
 * write_arrow. Nothing about the layer's columns is described in C at all.
 *
 *     roundtrip <src.gpkg> <layer> <dst.gpkg>
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "geopackage.h"

static int fail(const char *what, gpkg_error_t *error) {
    fprintf(stderr, "%s: code=%d message=%s\n", what, (int)error->code,
            error->message ? error->message : "(none)");
    gpkg_error_clear(error);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: roundtrip <src.gpkg> <layer> <dst.gpkg>\n");
        return 2;
    }
    const char *src_path = argv[1];
    const char *layer_name = argv[2];
    const char *dst_path = argv[3];

    gpkg_error_t error = {GPKG_STATUS_OK, NULL};

    gpkg_t *src = gpkg_open_read_only(src_path, &error);
    if (!src) {
        return fail("gpkg_open_read_only", &error);
    }
    gpkg_layer_t *src_layer = gpkg_layer_open(src, layer_name, &error);
    if (!src_layer) {
        return fail("gpkg_layer_open", &error);
    }

    uint64_t expected = 0;
    if (gpkg_layer_count(src_layer, &expected, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_count", &error);
    }

    /* The schema comes off a stream, which is also what supplies the geometry
     * column's CRS metadata, so the destination gets the right SRS without
     * anything here knowing what it is. */
    struct ArrowArrayStream schema_stream;
    memset(&schema_stream, 0, sizeof(schema_stream));
    if (gpkg_layer_read_arrow(src_layer, &schema_stream, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_read_arrow (for schema)", &error);
    }
    struct ArrowSchema schema;
    memset(&schema, 0, sizeof(schema));
    if (schema_stream.get_schema(&schema_stream, &schema) != 0) {
        fprintf(stderr, "get_schema: %s\n", schema_stream.get_last_error(&schema_stream));
        return 1;
    }
    schema_stream.release(&schema_stream);

    gpkg_t *dst = gpkg_create(dst_path, &error);
    if (!dst) {
        schema.release(&schema);
        return fail("gpkg_create", &error);
    }
    /* The whole copy is one transaction: the SRS row, the layer DDL, its
     * spatial index and every written row commit together or not at all. This
     * is the sequence gpkg_begin exists for, and it could not work until the
     * write paths learned to join a transaction that was already open. */
    if (gpkg_begin(dst, &error) != GPKG_STATUS_OK) {
        schema.release(&schema);
        return fail("gpkg_begin", &error);
    }
    /* WGS 84. A copy of an arbitrary file would read this off the source; the
     * fixture this is run against uses 4326 throughout. */
    if (gpkg_add_epsg_srs(dst, 4326, &error) != GPKG_STATUS_OK) {
        schema.release(&schema);
        return fail("gpkg_add_epsg_srs", &error);
    }
    if (gpkg_create_layer_from_arrow_schema(dst, layer_name, &schema, true, &error) !=
        GPKG_STATUS_OK) {
        schema.release(&schema);
        return fail("gpkg_create_layer_from_arrow_schema", &error);
    }
    schema.release(&schema);

    /* Now the data. A second stream, because the first was released after its
     * schema was taken. */
    struct ArrowArrayStream data;
    memset(&data, 0, sizeof(data));
    if (gpkg_layer_read_arrow(src_layer, &data, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_read_arrow", &error);
    }

    gpkg_layer_t *dst_layer = gpkg_layer_open(dst, layer_name, &error);
    if (!dst_layer) {
        data.release(&data);
        return fail("gpkg_layer_open (dst)", &error);
    }

    uint64_t written = 0;
    /* Takes ownership of the stream: it is released by the call, and must not
     * be released here. */
    if (gpkg_layer_write_arrow(dst_layer, &data, 1000, &written, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_write_arrow", &error);
    }
    printf("copied %llu rows\n", (unsigned long long)written);

    if (written != expected) {
        fprintf(stderr, "wrote %llu rows, source has %llu\n", (unsigned long long)written,
                (unsigned long long)expected);
        return 1;
    }

    /* Nothing above is durable until here. The 1000 passed as batch_size did
     * not bound a transaction, because every batch belonged to this one. */
    if (gpkg_commit(dst, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_commit", &error);
    }

    /* Read the destination back and count, so the check is what landed on disk
     * rather than what the writer claimed. */
    uint64_t landed = 0;
    if (gpkg_layer_count(dst_layer, &landed, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_count (dst)", &error);
    }
    if (landed != expected) {
        fprintf(stderr, "destination contains %llu rows, source has %llu\n",
                (unsigned long long)landed, (unsigned long long)expected);
        return 1;
    }

    /* The other half of the pair, on the harder case: DDL. Dropping the spatial
     * index removes a virtual table, its triggers and a gpkg_extensions row,
     * and rolling back must bring all of it back. A caller who cannot undo a
     * partial write has no use for gpkg_begin. */
    bool indexed = false;
    if (gpkg_layer_has_spatial_index(dst_layer, &indexed, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_has_spatial_index", &error);
    }
    if (!indexed) {
        fprintf(stderr, "expected the copied layer to carry a spatial index\n");
        return 1;
    }
    if (gpkg_begin(dst, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_begin (rollback case)", &error);
    }
    if (gpkg_layer_drop_spatial_index(dst_layer, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_drop_spatial_index", &error);
    }
    if (gpkg_layer_has_spatial_index(dst_layer, &indexed, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_has_spatial_index (staged)", &error);
    }
    if (indexed) {
        fprintf(stderr, "the drop was not visible inside its own transaction\n");
        return 1;
    }
    if (gpkg_rollback(dst, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_rollback", &error);
    }
    if (gpkg_layer_has_spatial_index(dst_layer, &indexed, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_has_spatial_index (after rollback)", &error);
    }
    if (!indexed) {
        fprintf(stderr, "the spatial index did not come back with the rollback\n");
        return 1;
    }
    printf("rolled back a dropped spatial index\n");

    gpkg_layer_free(dst_layer);
    gpkg_layer_free(src_layer);
    if (gpkg_close(dst, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close (dst)", &error);
    }
    if (gpkg_close(src, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close (src)", &error);
    }

    printf("ok\n");
    return 0;
}
