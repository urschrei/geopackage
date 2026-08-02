/* The interactive-read pattern: the loop a map view or a feature table runs.
 *
 * Opens one layer projected to the columns it needs, resolves the layer's
 * coordinate reference system to a definition a projection library could
 * take, then pulls three Arrow streams through the one filtered entry point:
 * a bounding box (the canvas), the same box with a WHERE clause on top (a
 * subset string), and a single feature by id (a selection). Everything a
 * QGIS-shaped consumer does between opening a file and drawing it.
 *
 * Compiled and run against the built static library by `tests/c_smoke.rs`.
 *
 * Build by hand, once the library is built:
 *     cc -I geopackage-ffi/include geopackage-ffi/examples/query.c \
 *        target/debug/libgeopackage_ffi.a -o /tmp/query
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "geopackage.h"

/* Print and clear an error, returning 1 so callers can `return fail(...)`. */
static int fail(const char *what, gpkg_error_t *error) {
    fprintf(stderr, "%s: code=%d message=%s\n", what, (int)error->code,
            error->message ? error->message : "(none)");
    gpkg_error_clear(error);
    return 1;
}

/* Drain a stream, returning its row count, or -1 with the detail printed.
 * Pulled exactly as the C Data Interface specifies: `get_next` until a
 * released array, then release the stream. */
static long long drain(struct ArrowArrayStream *stream) {
    long long rows = 0;
    for (;;) {
        struct ArrowArray array;
        memset(&array, 0, sizeof(array));
        if (stream->get_next(stream, &array) != 0) {
            const char *detail = stream->get_last_error(stream);
            fprintf(stderr, "get_next: %s\n", detail ? detail : "(none)");
            stream->release(stream);
            return -1;
        }
        if (array.release == NULL) {
            break; /* end of stream */
        }
        rows += array.length;
        array.release(&array);
    }
    stream->release(stream);
    return rows;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: query <file.gpkg> <layer>\n");
        return 2;
    }

    gpkg_error_t error = {GPKG_STATUS_OK, NULL};

    gpkg_t *gpkg = gpkg_open_read_only(argv[1], &error);
    if (!gpkg) {
        return fail("gpkg_open_read_only", &error);
    }

    /* Projected: the feature table wants one attribute, not the geometry
     * blobs, and the stream narrows to match. The feature id is always
     * carried and need not be named. */
    const char *columns[] = {"name"};
    gpkg_layer_t *layer =
        gpkg_layer_open_with_columns(gpkg, argv[2], columns, 1, &error);
    if (!layer) {
        return fail("gpkg_layer_open_with_columns", &error);
    }

    /* The CRS, as a definition a projection library could take. The id alone
     * is what the layer stores; the definition lives in the file's
     * gpkg_spatial_ref_sys table. */
    int32_t srs_id = 0;
    if (gpkg_layer_srs_id(layer, &srs_id, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_srs_id", &error);
    }
    char *organization = NULL;
    int32_t code = 0;
    char *definition = NULL;
    if (gpkg_srs(gpkg, srs_id, NULL, &organization, &code, &definition, NULL,
                 NULL, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_srs", &error);
    }
    printf("crs: %s:%d, definition %zu bytes\n", organization, (int)code,
           strlen(definition));
    gpkg_string_free(organization);
    gpkg_string_free(definition);

    /* The canvas: everything intersecting a bounding box. A whole-world box
     * here, so the row count is checkable; a map view passes its viewport. */
    double bbox[4] = {-180.0, -90.0, 180.0, 90.0};
    struct ArrowArrayStream stream;
    memset(&stream, 0, sizeof(stream));
    if (gpkg_layer_read_arrow_filtered(layer, bbox, NULL, NULL, 0, &stream,
                                       &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_read_arrow_filtered (bbox)", &error);
    }
    long long in_view = drain(&stream);
    if (in_view < 0) {
        return 1;
    }
    printf("in view: %lld rows\n", in_view);

    /* The subset string: the same box, narrowed by a WHERE clause with a
     * bound parameter. The clause is raw SQL on the layer's own columns;
     * its placeholders are ?1 to ?N, and the read's own machinery never
     * collides with them. */
    gpkg_value_t wanted = {GPKG_VALUE_KIND_TEXT, {.text = "alpha"}};
    memset(&stream, 0, sizeof(stream));
    if (gpkg_layer_read_arrow_filtered(layer, bbox, "name = ?1", &wanted, 1,
                                       &stream, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_read_arrow_filtered (subset)", &error);
    }
    long long matching = drain(&stream);
    if (matching < 0) {
        return 1;
    }
    printf("matching subset: %lld rows\n", matching);

    /* The selection: one feature by id, which is nothing more than the
     * clause `fid = ?1`. */
    gpkg_value_t fid = {GPKG_VALUE_KIND_INTEGER, {.integer = 1}};
    memset(&stream, 0, sizeof(stream));
    if (gpkg_layer_read_arrow_filtered(layer, NULL, "fid = ?1", &fid, 1,
                                       &stream, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_read_arrow_filtered (fid)", &error);
    }
    long long selected = drain(&stream);
    if (selected < 0) {
        return 1;
    }
    printf("selected by fid: %lld rows\n", selected);

    gpkg_layer_free(layer);
    if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close", &error);
    }
    printf("ok\n");
    return 0;
}
