/*
 * GDAL's Arrow write path into a GeoPackage, timed, for comparison against this
 * crate's `write_arrow` (M3 acceptance criterion 3, write side). Driven by
 * scripts/compare_gdal_arrow_write.sh.
 *
 * The counterpart of scripts/gdal_arrow_read.c. Both arms of the comparison do
 * the same thing: read a source GeoPackage's Arrow stream into memory first,
 * untimed, then time only the writing of those batches into a fresh file. The
 * read is deliberately outside the measurement, because otherwise the figure
 * would be a read plus a write and would say nothing about either. That is the
 * mistake the M2 GDAL comparison had to withdraw.
 *
 * Note that GDAL's GeoPackage driver has no specialised WriteArrowBatch: its
 * slide 11 lists only GeoParquet and GeoArrow, so this exercises the generic
 * implementation built on CreateFeature. That is the honest comparison, since it
 * is what a GDAL user writing a GeoPackage actually gets.
 *
 * Usage: gdal_arrow_write <source.gpkg> <target.gpkg> <index:yes|no>
 * Prints `<key>=<value>` lines including elapsed_ms for the write alone.
 *
 * The spatial-index flag matters and is not cosmetic: this driver creates one by
 * default, and the Rust side does not, so leaving it alone would compare a write
 * that builds an index against one that does not.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "cpl_string.h"
#include "gdal.h"
#include "ogr_api.h"
#include "ogr_recordbatch.h"

#define MAX_BATCHES 4096

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1.0e6;
}

int main(int argc, char **argv)
{
    if (argc < 4)
    {
        fprintf(stderr, "usage: %s <source.gpkg> <target.gpkg> <index:yes|no>\n",
                argv[0]);
        return 2;
    }
    const int wantIndex = (strcmp(argv[3], "yes") == 0);
    GDALAllRegister();

    /* --- read the source into memory, untimed --- */
    GDALDatasetH src = GDALOpenEx(argv[1], GDAL_OF_VECTOR | GDAL_OF_READONLY,
                                  NULL, NULL, NULL);
    if (src == NULL)
    {
        fprintf(stderr, "cannot open %s\n", argv[1]);
        return 2;
    }
    OGRLayerH srcLayer = GDALDatasetGetLayer(src, 0);

    struct ArrowArrayStream stream;
    memset(&stream, 0, sizeof(stream));
    if (!OGR_L_GetArrowStream(srcLayer, &stream, NULL))
    {
        fprintf(stderr, "GetArrowStream failed\n");
        return 2;
    }
    struct ArrowSchema schema;
    memset(&schema, 0, sizeof(schema));
    if (stream.get_schema(&stream, &schema) != 0)
    {
        fprintf(stderr, "get_schema failed\n");
        return 2;
    }

    static struct ArrowArray batches[MAX_BATCHES];
    int nBatches = 0;
    long long rows = 0;
    for (; nBatches < MAX_BATCHES; ++nBatches)
    {
        memset(&batches[nBatches], 0, sizeof(struct ArrowArray));
        if (stream.get_next(&stream, &batches[nBatches]) != 0)
        {
            fprintf(stderr, "get_next failed\n");
            return 2;
        }
        if (batches[nBatches].release == NULL)
        {
            break;
        }
        rows += (long long)batches[nBatches].length;
    }
    stream.release(&stream);

    /* --- create the target layer from the source's definition --- */
    GDALDriverH driver = GDALGetDriverByName("GPKG");
    GDALDatasetH dst = GDALCreate(driver, argv[2], 0, 0, 0, GDT_Unknown, NULL);
    if (dst == NULL)
    {
        fprintf(stderr, "cannot create %s\n", argv[2]);
        return 2;
    }
    char **papszOptions = NULL;
    papszOptions = CSLSetNameValue(papszOptions, "SPATIAL_INDEX",
                                   wantIndex ? "YES" : "NO");
    OGRLayerH dstLayer =
        GDALDatasetCreateLayer(dst, "features", OGR_L_GetSpatialRef(srcLayer),
                               wkbPolygon, papszOptions);
    CSLDestroy(papszOptions);
    if (dstLayer == NULL)
    {
        fprintf(stderr, "cannot create target layer\n");
        return 2;
    }
    /* Fields from the source definition, so both arms write the same columns. */
    OGRFeatureDefnH srcDefn = OGR_L_GetLayerDefn(srcLayer);
    for (int i = 0; i < OGR_FD_GetFieldCount(srcDefn); ++i)
    {
        if (OGR_L_CreateField(dstLayer, OGR_FD_GetFieldDefn(srcDefn, i), TRUE) !=
            OGRERR_NONE)
        {
            fprintf(stderr, "cannot create field %d\n", i);
            return 2;
        }
    }

    /* --- the timed part: write the batches --- */
    const double start = now_ms();
    int written = 0;
    for (int i = 0; i < nBatches; ++i)
    {
        if (!OGR_L_WriteArrowBatch(dstLayer, &schema, &batches[i], NULL))
        {
            fprintf(stderr, "WriteArrowBatch failed on batch %d\n", i);
            return 2;
        }
        written += 1;
    }
    GDALClose(dst);
    const double elapsed = now_ms() - start;

    for (int i = 0; i < nBatches; ++i)
    {
        if (batches[i].release != NULL)
        {
            batches[i].release(&batches[i]);
        }
    }
    if (schema.release != NULL)
    {
        schema.release(&schema);
    }
    GDALClose(src);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("rows=%lld\n", rows);
    printf("batches=%d\n", written);
    printf("index=%s\n", wantIndex ? "yes" : "no");
    return 0;
}
