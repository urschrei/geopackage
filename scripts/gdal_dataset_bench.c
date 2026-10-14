/*
 * The GDAL counterpart of geopackage/examples/dataset_bench.rs. Driven by
 * scripts/bench_datasets.sh, which compiles it against the GDAL that
 * `gdal-config` names.
 *
 * Each subcommand does the same work as the subcommand of the same name in
 * dataset_bench, through the OGR C API, and prints `<key>=<value>` lines,
 * including `elapsed_ms` measured inside the process.
 *
 *   scan <file> <layer>
 *       Read every feature with OGR_L_GetNextFeature, which builds an
 *       OGRGeometry for each one.
 *   arrow <file> <layer>
 *       Consume every batch of OGR_L_GetArrowStream. OGR_GPKG_NUM_THREADS in
 *       the environment sets the thread count.
 *   index <file> <layer>
 *       Build the spatial index with SELECT CreateSpatialIndex(...).
 *   bbox <file> <layer> <minx> <miny> <maxx> <maxy>
 *       Read the features that OGR_L_SetSpatialFilterRect selects.
 *   write <src> <srclayer> <dst> <yes|no>
 *       Read the source's Arrow batches into memory (not timed), then time
 *       OGR_L_WriteArrowBatch into a new file, in one transaction, with or
 *       without a spatial index.
 *
 * All writes use synchronous = OFF (OGR_SQLITE_SYNCHRONOUS), as dataset_bench
 * does.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "cpl_conv.h"
#include "cpl_string.h"
#include "gdal.h"
#include "ogr_api.h"
#include "ogr_recordbatch.h"

#define MAX_BATCHES 65536

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1.0e6;
}

static void fail(const char *what)
{
    fprintf(stderr, "%s: %s\n", what, CPLGetLastErrorMsg());
    exit(2);
}

static GDALDatasetH open_ds(const char *path, int update)
{
    unsigned int flags = GDAL_OF_VECTOR | (update ? GDAL_OF_UPDATE : GDAL_OF_READONLY);
    GDALDatasetH ds = GDALOpenEx(path, flags, NULL, NULL, NULL);
    if (ds == NULL)
    {
        fail(path);
    }
    return ds;
}

static OGRLayerH layer_by_name(GDALDatasetH ds, const char *name)
{
    OGRLayerH layer = GDALDatasetGetLayerByName(ds, name);
    if (layer == NULL)
    {
        fail(name);
    }
    return layer;
}

static int cmd_scan(const char *path, const char *name)
{
    GDALDatasetH ds = open_ds(path, 0);
    OGRLayerH layer = layer_by_name(ds, name);

    long long rows = 0;
    long long geometries = 0;
    const double start = now_ms();
    OGRFeatureH feature;
    while ((feature = OGR_L_GetNextFeature(layer)) != NULL)
    {
        rows += 1;
        if (OGR_F_GetGeometryRef(feature) != NULL)
        {
            geometries += 1;
        }
        OGR_F_Destroy(feature);
    }
    const double elapsed = now_ms() - start;
    GDALClose(ds);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("rows=%lld\n", rows);
    printf("geometries=%lld\n", geometries);
    return 0;
}

static int cmd_arrow(const char *path, const char *name)
{
    GDALDatasetH ds = open_ds(path, 0);
    OGRLayerH layer = layer_by_name(ds, name);

    const double start = now_ms();
    struct ArrowArrayStream stream;
    memset(&stream, 0, sizeof(stream));
    if (!OGR_L_GetArrowStream(layer, &stream, NULL))
    {
        fail("OGR_L_GetArrowStream");
    }
    struct ArrowSchema schema;
    memset(&schema, 0, sizeof(schema));
    long long columns = -1;
    if (stream.get_schema(&stream, &schema) == 0)
    {
        columns = (long long)schema.n_children;
        schema.release(&schema);
    }

    long long rows = 0;
    long long batches = 0;
    for (;;)
    {
        struct ArrowArray array;
        memset(&array, 0, sizeof(array));
        if (stream.get_next(&stream, &array) != 0)
        {
            fail("get_next");
        }
        if (array.release == NULL)
        {
            break;
        }
        rows += (long long)array.length;
        batches += 1;
        array.release(&array);
    }
    stream.release(&stream);
    const double elapsed = now_ms() - start;
    GDALClose(ds);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("rows=%lld\n", rows);
    printf("batches=%lld\n", batches);
    printf("columns=%lld\n", columns);
    return 0;
}

static int cmd_index(const char *path, const char *name)
{
    GDALDatasetH ds = open_ds(path, 1);
    OGRLayerH layer = layer_by_name(ds, name);
    char sql[1024];
    snprintf(sql, sizeof(sql), "SELECT CreateSpatialIndex('%s', '%s')", name,
             OGR_L_GetGeometryColumn(layer));

    const double start = now_ms();
    OGRLayerH result = GDALDatasetExecuteSQL(ds, sql, NULL, NULL);
    if (result != NULL)
    {
        GDALDatasetReleaseResultSet(ds, result);
    }
    const double elapsed = now_ms() - start;
    if (CPLGetLastErrorType() >= CE_Failure)
    {
        fail("CreateSpatialIndex");
    }
    GDALClose(ds);

    printf("elapsed_ms=%.3f\n", elapsed);
    return 0;
}

static int cmd_bbox(const char *path, const char *name, char **bounds)
{
    GDALDatasetH ds = open_ds(path, 0);
    OGRLayerH layer = layer_by_name(ds, name);

    long long hits = 0;
    const double start = now_ms();
    OGR_L_SetSpatialFilterRect(layer, atof(bounds[0]), atof(bounds[1]),
                               atof(bounds[2]), atof(bounds[3]));
    OGRFeatureH feature;
    while ((feature = OGR_L_GetNextFeature(layer)) != NULL)
    {
        hits += 1;
        OGR_F_Destroy(feature);
    }
    const double elapsed = now_ms() - start;
    GDALClose(ds);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("hits=%lld\n", hits);
    return 0;
}

static int cmd_write(const char *source, const char *name, const char *target,
                     int with_index)
{
    /* Not timed: read the source's batches into memory. */
    GDALDatasetH src = open_ds(source, 0);
    OGRLayerH src_layer = layer_by_name(src, name);

    struct ArrowArrayStream stream;
    memset(&stream, 0, sizeof(stream));
    if (!OGR_L_GetArrowStream(src_layer, &stream, NULL))
    {
        fail("OGR_L_GetArrowStream");
    }
    struct ArrowSchema schema;
    memset(&schema, 0, sizeof(schema));
    if (stream.get_schema(&stream, &schema) != 0)
    {
        fail("get_schema");
    }
    static struct ArrowArray batches[MAX_BATCHES];
    int n_batches = 0;
    long long rows = 0;
    for (;;)
    {
        if (n_batches == MAX_BATCHES)
        {
            fprintf(stderr, "more than %d batches\n", MAX_BATCHES);
            return 2;
        }
        memset(&batches[n_batches], 0, sizeof(struct ArrowArray));
        if (stream.get_next(&stream, &batches[n_batches]) != 0)
        {
            fail("get_next");
        }
        if (batches[n_batches].release == NULL)
        {
            break;
        }
        rows += (long long)batches[n_batches].length;
        n_batches += 1;
    }
    stream.release(&stream);

    /* Not timed: create the target layer from the source definition. */
    GDALDriverH driver = GDALGetDriverByName("GPKG");
    GDALDatasetH dst = GDALCreate(driver, target, 0, 0, 0, GDT_Unknown, NULL);
    if (dst == NULL)
    {
        fail(target);
    }
    char **options = NULL;
    options = CSLSetNameValue(options, "SPATIAL_INDEX", with_index ? "YES" : "NO");
    options = CSLSetNameValue(options, "GEOMETRY_NAME", OGR_L_GetGeometryColumn(src_layer));
    options = CSLSetNameValue(options, "FID", OGR_L_GetFIDColumn(src_layer));
    OGRLayerH dst_layer = GDALDatasetCreateLayer(
        dst, name, OGR_L_GetSpatialRef(src_layer), OGR_L_GetGeomType(src_layer), options);
    CSLDestroy(options);
    if (dst_layer == NULL)
    {
        fail("GDALDatasetCreateLayer");
    }
    OGRFeatureDefnH defn = OGR_L_GetLayerDefn(src_layer);
    for (int i = 0; i < OGR_FD_GetFieldCount(defn); ++i)
    {
        if (OGR_L_CreateField(dst_layer, OGR_FD_GetFieldDefn(defn, i), TRUE) != OGRERR_NONE)
        {
            fail("OGR_L_CreateField");
        }
    }

    const double start = now_ms();
    if (GDALDatasetStartTransaction(dst, FALSE) != OGRERR_NONE)
    {
        fail("StartTransaction");
    }
    for (int i = 0; i < n_batches; ++i)
    {
        if (!OGR_L_WriteArrowBatch(dst_layer, &schema, &batches[i], NULL))
        {
            fail("OGR_L_WriteArrowBatch");
        }
    }
    if (GDALDatasetCommitTransaction(dst) != OGRERR_NONE)
    {
        fail("CommitTransaction");
    }
    GDALClose(dst);
    const double elapsed = now_ms() - start;

    for (int i = 0; i < n_batches; ++i)
    {
        if (batches[i].release != NULL)
        {
            batches[i].release(&batches[i]);
        }
    }
    schema.release(&schema);
    GDALClose(src);

    printf("elapsed_ms=%.3f\n", elapsed);
    printf("rows=%lld\n", rows);
    printf("batches=%d\n", n_batches);
    printf("index=%s\n", with_index ? "yes" : "no");
    return 0;
}

int main(int argc, char **argv)
{
    const char *usage = "usage: gdal_dataset_bench <scan|arrow|index|bbox|write> <file> <layer> [...]";
    if (argc < 4)
    {
        fprintf(stderr, "%s\n", usage);
        return 2;
    }
    CPLSetConfigOption("OGR_SQLITE_SYNCHRONOUS", "OFF");
    GDALAllRegister();

    const char *command = argv[1];
    if (strcmp(command, "scan") == 0)
    {
        return cmd_scan(argv[2], argv[3]);
    }
    if (strcmp(command, "arrow") == 0)
    {
        return cmd_arrow(argv[2], argv[3]);
    }
    if (strcmp(command, "index") == 0)
    {
        return cmd_index(argv[2], argv[3]);
    }
    if (strcmp(command, "bbox") == 0 && argc >= 8)
    {
        return cmd_bbox(argv[2], argv[3], &argv[4]);
    }
    if (strcmp(command, "write") == 0 && argc >= 6)
    {
        return cmd_write(argv[2], argv[3], argv[4], strcmp(argv[5], "yes") == 0);
    }
    fprintf(stderr, "%s\n", usage);
    return 2;
}
