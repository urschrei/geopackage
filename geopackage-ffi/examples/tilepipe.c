/* The tile pipeline: build a pyramid from nothing, then copy one.
 *
 * Creates a web mercator pyramid in a new file, stores a tile in it, then
 * copies every stored tile into a second file through the cursor, which
 * walks what a pyramid stores rather than probing its declared grid. The
 * copy loop is the pattern the cursor exists for: each payload is lent, not
 * allocated, and handed straight to the destination's put.
 *
 * Compiled and run against the built static library by `tests/c_smoke.rs`.
 *
 * Build by hand, once the library is built:
 *     cc -I geopackage-ffi/include geopackage-ffi/examples/tilepipe.c \
 *        target/debug/libgeopackage_ffi.a -o /tmp/tilepipe
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

/* The smallest payload the library accepts: a PNG header declaring 256 by
 * 256 pixels. The library reads headers and decodes nothing, so this is
 * enough to be stored; a real pipeline hands over real encoder output. */
static size_t tiny_png(unsigned char *out) {
    static const unsigned char header[] = {
        0x89, 'P',  'N',  'G',  0x0d, 0x0a, 0x1a, 0x0a, /* signature */
        0x00, 0x00, 0x00, 0x0d,                         /* IHDR length */
        'I',  'H',  'D',  'R',                          /* IHDR */
        0x00, 0x00, 0x01, 0x00,                         /* width 256 */
        0x00, 0x00, 0x01, 0x00,                         /* height 256 */
        0x08, 0x02, 0x00, 0x00, 0x00,                   /* bit depth etc. */
        0x00, 0x00, 0x00, 0x00,                         /* CRC, unchecked */
    };
    memcpy(out, header, sizeof(header));
    return sizeof(header);
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: tilepipe <new.gpkg> <copy.gpkg>\n");
        return 2;
    }

    gpkg_error_t error = {GPKG_STATUS_OK, NULL};

    /* Build the source: a new file, the SRS the pyramid will name, and the
     * pyramid itself on the web mercator quad, zooms 0 to 2. */
    gpkg_t *src = gpkg_create(argv[1], &error);
    if (!src) {
        return fail("gpkg_create", &error);
    }
    if (gpkg_add_epsg_srs(src, 3857, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_add_epsg_srs", &error);
    }
    gpkg_tiles_t *tiles =
        gpkg_tiles_create_web_mercator(src, "basemap", 0, 2, &error);
    if (!tiles) {
        return fail("gpkg_tiles_create_web_mercator", &error);
    }

    unsigned char png[64];
    size_t png_len = tiny_png(png);
    if (gpkg_tiles_put(tiles, 1, 1, 0, png, png_len, &error) !=
        GPKG_STATUS_OK) {
        return fail("gpkg_tiles_put", &error);
    }
    printf("stored 1 tile\n");

    /* Build the destination the same way, then copy through the cursor. */
    gpkg_t *dst = gpkg_create(argv[2], &error);
    if (!dst) {
        return fail("gpkg_create (copy)", &error);
    }
    if (gpkg_add_epsg_srs(dst, 3857, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_add_epsg_srs (copy)", &error);
    }
    gpkg_tiles_t *copy =
        gpkg_tiles_create_web_mercator(dst, "basemap", 0, 2, &error);
    if (!copy) {
        return fail("gpkg_tiles_create_web_mercator (copy)", &error);
    }

    gpkg_tile_cursor_t *cursor = gpkg_tiles_cursor(tiles, &error);
    if (!cursor) {
        return fail("gpkg_tiles_cursor", &error);
    }
    long long copied = 0;
    for (;;) {
        int64_t zoom = 0;
        int64_t column = 0;
        int64_t row = 0;
        const uint8_t *data = NULL;
        size_t len = 0;
        if (gpkg_tile_cursor_next(cursor, &zoom, &column, &row, &data, &len,
                                  &error) != GPKG_STATUS_OK) {
            gpkg_tile_cursor_free(cursor);
            return fail("gpkg_tile_cursor_next", &error);
        }
        if (data == NULL) {
            break; /* the scan is done */
        }
        /* `data` is lent until the next call on the cursor, and the put
         * copies during the call, so no buffer changes hands. */
        if (gpkg_tiles_put(copy, zoom, column, row, data, len, &error) !=
            GPKG_STATUS_OK) {
            gpkg_tile_cursor_free(cursor);
            return fail("gpkg_tiles_put (copy)", &error);
        }
        copied++;
    }
    gpkg_tile_cursor_free(cursor);
    printf("copied %lld tiles\n", copied);

    /* Prove the copy contains what the source contains. */
    bool has = false;
    if (gpkg_tiles_has(copy, 1, 1, 0, &has, &error) != GPKG_STATUS_OK ||
        !has) {
        fprintf(stderr, "the copied tile is missing\n");
        return 1;
    }

    gpkg_tiles_free(tiles);
    gpkg_tiles_free(copy);
    if (gpkg_close(src, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close (source)", &error);
    }
    if (gpkg_close(dst, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close (copy)", &error);
    }
    printf("ok\n");
    return 0;
}
