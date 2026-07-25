/*
 * GDAL's Arrow read path over a GeoPackage, timed, for comparison against this
 * crate's `read_arrow` (M3 acceptance criterion 3). Driven by
 * scripts/compare_gdal_arrow.sh.
 *
 * Written in C against the OGR C API rather than driven from Python, so that
 * nothing but GDAL is in the measured loop. It consumes the Arrow C stream
 * exactly as any other consumer would: get_schema once, then get_next until the
 * stream is exhausted, releasing each array.
 *
 * Subcommands, each printing `<key>=<value>` lines so the driver script can
 * read them:
 *
 *   noop <file>   open and close, the startup floor to subtract
 *   read <file>   consume the whole Arrow stream, the timed operation
 *
 * Thread count is GDAL's own business: it is set by OGR_GPKG_NUM_THREADS in the
 * environment, which the driver script sets explicitly rather than leaving to
 * the default of min(4, CPUs). A comparison that let one side use four cores
 * and the other one would be worthless.
 *
 * Build: see scripts/compare_gdal_arrow.sh, which uses gdal-config.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "gdal.h"
#include "ogr_api.h"
#include "ogr_recordbatch.h"

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1.0e6;
}

static GDALDatasetH open_ro(const char *path)
{
    GDALDatasetH ds = GDALOpenEx(path, GDAL_OF_VECTOR | GDAL_OF_READONLY, NULL,
                                 NULL, NULL);
    if (ds == NULL)
    {
        fprintf(stderr, "cannot open %s\n", path);
        exit(2);
    }
    return ds;
}

/* Open and close, nothing else: the floor to subtract from a timed read. */
static int cmd_noop(const char *path)
{
    const double start = now_ms();
    GDALDatasetH ds = open_ro(path);
    GDALClose(ds);
    printf("elapsed_ms=%.3f\n", now_ms() - start);
    return 0;
}

/* Consume the whole Arrow stream, reporting rows, batches and schema width. */
static int cmd_read(const char *path)
{
    GDALDatasetH ds = open_ro(path);
    OGRLayerH layer = GDALDatasetGetLayer(ds, 0);
    if (layer == NULL)
    {
        fprintf(stderr, "no layer 0 in %s\n", path);
        return 2;
    }

    const double start = now_ms();

    struct ArrowArrayStream stream;
    memset(&stream, 0, sizeof(stream));
    if (!OGR_L_GetArrowStream(layer, &stream, NULL))
    {
        fprintf(stderr, "GetArrowStream failed\n");
        return 2;
    }

    struct ArrowSchema schema;
    memset(&schema, 0, sizeof(schema));
    long long columns = -1;
    if (stream.get_schema(&stream, &schema) == 0)
    {
        columns = (long long)schema.n_children;
        if (schema.release != NULL)
        {
            schema.release(&schema);
        }
    }

    long long rows = 0;
    long long batches = 0;
    for (;;)
    {
        struct ArrowArray array;
        memset(&array, 0, sizeof(array));
        if (stream.get_next(&stream, &array) != 0)
        {
            fprintf(stderr, "get_next failed\n");
            return 2;
        }
        if (array.release == NULL)
        {
            break; /* end of stream */
        }
        rows += (long long)array.length;
        batches += 1;
        array.release(&array);
    }
    if (stream.release != NULL)
    {
        stream.release(&stream);
    }

    const double elapsed = now_ms() - start;
    GDALClose(ds);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("rows=%lld\n", rows);
    printf("batches=%lld\n", batches);
    printf("columns=%lld\n", columns);
    return 0;
}

int main(int argc, char **argv)
{
    if (argc < 3)
    {
        fprintf(stderr, "usage: %s <noop|read> <file.gpkg>\n", argv[0]);
        return 2;
    }
    GDALAllRegister();

    if (strcmp(argv[1], "noop") == 0)
    {
        return cmd_noop(argv[2]);
    }
    if (strcmp(argv[1], "read") == 0)
    {
        return cmd_read(argv[2]);
    }
    fprintf(stderr, "unknown subcommand %s\n", argv[1]);
    return 2;
}
