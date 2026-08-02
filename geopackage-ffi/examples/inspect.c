/* The fail-fast pattern: everything a C consumer can learn about an unknown
 * GeoPackage before touching a row of it.
 *
 * Opens the file read-only and tolerantly, reports what the open forgave,
 * enumerates the feature layers and tile pyramids, walks the extensions
 * catalogue with the support level the library claims for each row, and runs
 * validation. A consumer that does all of this before writing meets no
 * surprise refusal mid-write; the extension walk is the "fail fast" the
 * GeoPackage specification's catalogue exists for.
 *
 * Compiled and run against the built static library by `tests/c_smoke.rs`.
 *
 * Build by hand, once the library is built:
 *     cc -I geopackage-ffi/include geopackage-ffi/examples/inspect.c \
 *        target/debug/libgeopackage_ffi.a -o /tmp/inspect
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
        fprintf(stderr, "usage: inspect <file.gpkg>\n");
        return 2;
    }

    gpkg_error_t error = {GPKG_STATUS_OK, NULL};

    /* Tolerant and read-only: the right way in for a file from elsewhere.
     * Whatever the open had to forgive is kept as warnings, not lost. */
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
    printf("open warnings: %zu\n", warnings);
    for (size_t i = 0; i < warnings; i++) {
        char *warning = gpkg_open_warning(gpkg, i, &error);
        if (!warning) {
            return fail("gpkg_open_warning", &error);
        }
        printf("  %s\n", warning);
        gpkg_string_free(warning);
    }

    /* The feature layers, with a row count each. */
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
            return fail("gpkg_layer_open", &error);
        }
        uint64_t rows = 0;
        if (gpkg_layer_count(layer, &rows, &error) != GPKG_STATUS_OK) {
            return fail("gpkg_layer_count", &error);
        }
        printf("  %s: %llu rows\n", name, (unsigned long long)rows);
        gpkg_layer_free(layer);
        gpkg_string_free(name);
    }

    /* The tile pyramids, by name. */
    size_t pyramids = 0;
    if (gpkg_tiles_names_count(gpkg, &pyramids, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_tiles_names_count", &error);
    }
    printf("pyramids: %zu\n", pyramids);
    for (size_t i = 0; i < pyramids; i++) {
        char *name = gpkg_tiles_name_at(gpkg, i, &error);
        if (!name) {
            return fail("gpkg_tiles_name_at", &error);
        }
        printf("  %s\n", name);
        gpkg_string_free(name);
    }

    /* The extensions catalogue, with the support level per row. A row
     * reporting "unrecognised" names a table this library will refuse to
     * write, so this loop is where a writer decides to decline the file
     * rather than fail mid-write. */
    size_t extensions = 0;
    if (gpkg_extensions_count(gpkg, &extensions, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_extensions_count", &error);
    }
    printf("extensions: %zu\n", extensions);
    int unsupported = 0;
    for (size_t i = 0; i < extensions; i++) {
        char *name = NULL;
        char *table = NULL;
        char *support = NULL;
        if (gpkg_extension_at(gpkg, i, &name, &table, NULL, NULL, &support,
                              &error) != GPKG_STATUS_OK) {
            return fail("gpkg_extension_at", &error);
        }
        printf("  %s on %s: %s\n", name, table ? table : "(whole file)",
               support);
        if (strcmp(support, "unrecognised") == 0) {
            unsupported = 1;
        }
        gpkg_string_free(name);
        gpkg_string_free(table);
        gpkg_string_free(support);
    }
    if (unsupported) {
        printf("would decline writes: an extension is unrecognised\n");
    }

    /* Validation: severity, description, and the repairing call where one
     * exists. Zero findings is a clean file; the call succeeding says only
     * that the checks ran. */
    gpkg_findings_t *findings = NULL;
    if (gpkg_validate(gpkg, &findings, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_validate", &error);
    }
    size_t count = gpkg_findings_count(findings);
    printf("findings: %zu\n", count);
    for (size_t i = 0; i < count; i++) {
        char *severity = NULL;
        char *text = NULL;
        char *repair = NULL;
        if (gpkg_finding_at(findings, i, &severity, &text, &repair, &error) !=
            GPKG_STATUS_OK) {
            gpkg_findings_free(findings);
            return fail("gpkg_finding_at", &error);
        }
        printf("  [%s] %s\n", severity, text);
        if (repair) {
            printf("          repair: %s\n", repair);
        }
        gpkg_string_free(severity);
        gpkg_string_free(text);
        gpkg_string_free(repair);
    }
    gpkg_findings_free(findings);

    if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
        return fail("gpkg_close", &error);
    }
    printf("ok\n");
    return 0;
}
