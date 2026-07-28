/* A C consumer of libgeopackage: open a file, walk its layers, and close it.
 *
 * This is the smoke test the roadmap asks for. It is compiled and run against
 * the built static library by `tests/c_smoke.rs`, so a change that breaks the
 * header or the ABI fails the ordinary test run rather than waiting for someone
 * to try it by hand.
 *
 * Build by hand, once the library is built:
 *     cc -I geopackage-ffi/include geopackage-ffi/examples/smoke.c \
 *        target/debug/libgeopackage_ffi.a -o /tmp/smoke
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

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: smoke <file.gpkg>\n");
        return 2;
    }

    gpkg_error_t error = {GPKG_STATUS_OK, NULL};

    gpkg_t *gpkg = gpkg_open_read_only(argv[1], &error);
    if (!gpkg) {
        return fail("gpkg_open_read_only", &error);
    }

    char *version = gpkg_version(gpkg, &error);
    if (!version) {
        return fail("gpkg_version", &error);
    }
    printf("version: %s\n", version);
    gpkg_string_free(version);

    size_t warnings = gpkg_open_warning_count(gpkg);
    for (size_t i = 0; i < warnings; i++) {
        char *warning = gpkg_open_warning(gpkg, i, &error);
        if (!warning) {
            return fail("gpkg_open_warning", &error);
        }
        printf("warning: %s\n", warning);
        gpkg_string_free(warning);
    }

    size_t layers = 0;
    if (gpkg_layer_names_count(gpkg, &layers, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_layer_names_count", &error);
    }
    printf("layers: %zu\n", layers);

    for (size_t i = 0; i < layers; i++) {
        char *name = gpkg_layer_name_at(gpkg, i, &error);
        if (!name) {
            return fail("gpkg_layer_name_at", &error);
        }

        gpkg_layer_t *layer = gpkg_layer_open(gpkg, name, &error);
        if (!layer) {
            gpkg_string_free(name);
            return fail("gpkg_layer_open", &error);
        }

        uint64_t rows = 0;
        if (gpkg_layer_count(layer, &rows, &error) != GPKG_STATUS_OK) {
            gpkg_layer_free(layer);
            gpkg_string_free(name);
            return fail("gpkg_layer_count", &error);
        }
        printf("layer %s: %llu rows\n", name, (unsigned long long)rows);

        /* Closing here would be refused, because this layer handle still
         * borrows the container. Prove it, then free the handle. */
        if (gpkg_close(gpkg, &error) != GPKG_STATUS_HANDLE_IN_USE) {
            fprintf(stderr, "expected a close with a live layer handle to be refused\n");
            return 1;
        }
        gpkg_error_clear(&error);

        gpkg_layer_free(layer);
        gpkg_string_free(name);
    }

    if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close", &error);
    }

    printf("ok\n");
    return 0;
}
