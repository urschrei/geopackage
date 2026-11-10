"""Tiled gridded coverage tools: value lookups, statistics, sample checks."""

from qgis.core import (
    QgsCoordinateTransform,
    QgsFeature,
    QgsFields,
    QgsGeometry,
    QgsPointXY,
    QgsProcessingException,
    QgsProcessingOutputNumber,
    QgsProcessingParameterEnum,
    QgsProcessingParameterFeatureSink,
    QgsProcessingParameterFeatureSource,
    QgsProcessingParameterNumber,
    QgsProcessingParameterString,
    QgsRectangle,
)

from .. import compat
from ..conversions import crs_for_srs
from .base import Algorithm, add_row, make_fields, wrap_errors

GROUP = "Coverages"
GROUP_ID = "coverages"

COVERAGE = "COVERAGE"
ZOOM = "ZOOM"


def resolve_coverages(gpkg, name, *, all_if_empty):
    """The coverages named by the COVERAGE parameter: the one named, or, when
    it is empty, every coverage (`all_if_empty`) or the only one."""
    names = wrap_errors(gpkg.coverage_names)
    if name:
        if name not in names:
            raise QgsProcessingException(
                f"no coverage named {name!r}; the file has {', '.join(names) or 'none'}"
            )
        return [gpkg.coverage(name)]
    if not names:
        raise QgsProcessingException("the file has no coverages")
    if all_if_empty:
        return [gpkg.coverage(n) for n in names]
    if len(names) > 1:
        raise QgsProcessingException(
            f"the file has {len(names)} coverages ({', '.join(names)}); "
            "enter the name of one"
        )
    return [gpkg.coverage(names[0])]


def zoom_parameter():
    return QgsProcessingParameterNumber(
        ZOOM,
        "Zoom level (if empty, the most detailed)",
        type=compat.NUMBER_INTEGER,
        optional=True,
        minValue=0,
    )


def zoom_value(algorithm, parameters, context):
    if parameters.get(ZOOM) in (None, ""):
        return None
    return algorithm.parameterAsInt(parameters, ZOOM, context)


def tile_polygon(coverage, zoom, column, row):
    min_x, min_y, max_x, max_y = coverage.tile_bounds(zoom, column, row)
    return QgsGeometry.fromRect(QgsRectangle(min_x, min_y, max_x, max_y))


class SampleCoverageAlgorithm(Algorithm):
    NAME = "samplecoverage"
    DISPLAY_NAME = "Sample coverage at points"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Reads the value of a coverage (gridded data stored as tiles, such "
        "as an elevation model) at each input point, and writes it to a new "
        "field. The points are transformed into the coverage's CRS first. "
        "A point gets NULL if it is outside the coverage, in a tile that the "
        "coverage does not store, or on a cell that has no value. A "
        "multipart geometry is sampled at its centroid.\n\n"
        "<b>Nearest</b> takes the value of the cell that contains the point. "
        "<b>Bilinear</b> interpolates between the four nearest cell values. "
        "On a coverage whose cells each have one value over their whole area "
        "(grid cell encoding 'grid-value-is-area'), bilinear gives the same "
        "values as nearest."
    )

    POINTS = "POINTS"
    INTERPOLATION = "INTERPOLATION"
    FIELD = "FIELD"
    INTERPOLATIONS = ["nearest", "bilinear"]

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_name_parameter(
            COVERAGE, "Coverage name (if empty, the file's only coverage)"
        )
        self.addParameter(
            QgsProcessingParameterFeatureSource(
                self.POINTS, "Points", [compat.SOURCE_POINT]
            )
        )
        self.addParameter(zoom_parameter())
        self.addParameter(
            QgsProcessingParameterEnum(
                self.INTERPOLATION,
                "Interpolation",
                options=self.INTERPOLATIONS,
                defaultValue=0,
            )
        )
        self.addParameter(
            QgsProcessingParameterString(self.FIELD, "Name of the new field", "value")
        )
        self.addParameter(
            QgsProcessingParameterFeatureSink(
                self.OUTPUT, "Sampled points", compat.SOURCE_POINT
            )
        )

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        source = self.parameterAsSource(parameters, self.POINTS, context)
        if source is None:
            raise QgsProcessingException(
                self.invalidSourceError(parameters, self.POINTS)
            )
        field_name = self.parameterAsString(parameters, self.FIELD, context) or "value"
        interpolation = self.INTERPOLATIONS[
            self.parameterAsEnum(parameters, self.INTERPOLATION, context)
        ]
        zoom = zoom_value(self, parameters, context)
        name = self.parameterAsString(parameters, COVERAGE, context).strip()

        fields = QgsFields(source.fields())
        if fields.indexOf(field_name) >= 0:
            raise QgsProcessingException(
                f"the points already have a field named {field_name!r}; enter another name"
            )
        fields.append(compat.field(field_name, "double"))
        sink, dest = self.sink(
            parameters, context, fields, source.wkbType(), source.sourceCrs()
        )

        with self.open(parameters, context, read_only=True) as gpkg:
            (coverage,) = resolve_coverages(gpkg, name, all_if_empty=False)
            coverage_crs = crs_for_srs(gpkg.srs(coverage.srs_id))
            transform = None
            if coverage_crs.isValid() and source.sourceCrs().isValid():
                transform = QgsCoordinateTransform(
                    source.sourceCrs(), coverage_crs, context.transformContext()
                )

            # Gather positions first so the crate sees them in one call, with
            # its tile cache warm across neighbours.
            features, positions, slots = [], [], []
            for feature in source.getFeatures():
                if feedback.isCanceled():
                    break
                features.append(QgsFeature(feature))
                geometry = feature.geometry()
                if geometry is None or geometry.isEmpty():
                    slots.append(None)
                    continue
                point = (
                    geometry.asPoint()
                    if not geometry.isMultipart()
                    else geometry.centroid().asPoint()
                )
                if transform is not None:
                    point = transform.transform(QgsPointXY(point))
                slots.append(len(positions))
                positions.append((point.x(), point.y()))

            # Sort spatially (by tile row, then column) so lookups that share
            # tiles run together; the answers are put back in input order.
            order = sorted(
                range(len(positions)), key=lambda i: (-positions[i][1], positions[i][0])
            )
            values = wrap_errors(
                coverage.values_at,
                [positions[i] for i in order],
                zoom,
                interpolation,
            )
            by_position = [None] * len(positions)
            for i, value in zip(order, values):
                by_position[i] = value

            for i, feature in enumerate(features):
                out = QgsFeature(fields)
                out.setGeometry(feature.geometry())
                slot = slots[i]
                out.setAttributes(
                    feature.attributes() + [None if slot is None else by_position[slot]]
                )
                sink.addFeature(out, compat.FAST_INSERT)
                feedback.setProgress(100 * (i + 1) / max(len(features), 1))
        return {self.OUTPUT: dest}


class CoverageStatisticsAlgorithm(Algorithm):
    NAME = "coveragestatistics"
    DISPLAY_NAME = "Coverage tile statistics"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Reads every stored tile of a coverage at one zoom level, and writes "
        "the outline of each tile with two sets of statistics. The rec_* "
        "fields are the statistics that the file records for the tile, in "
        "the table gpkg_2d_gridded_tile_ancillary. The other fields are "
        "calculated from the tile's values, after scale and offset are "
        "applied and without cells that have no value. If the two sets do "
        "not agree, the recorded statistics are out of date. If a tile cannot "
        "be read, the 'error' field gives the reason."
    )

    FIELDS = (
        ("coverage", "string"),
        ("zoom", "int"),
        ("tile_column", "int"),
        ("tile_row", "int"),
        ("rec_min", "double"),
        ("rec_max", "double"),
        ("rec_mean", "double"),
        ("rec_std_dev", "double"),
        ("min", "double"),
        ("max", "double"),
        ("mean", "double"),
        ("std_dev", "double"),
        ("count", "int64"),
        ("error", "string"),
    )

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_name_parameter(
            COVERAGE, "Coverage name (if empty, the file's only coverage)"
        )
        self.addParameter(zoom_parameter())
        self.addParameter(
            QgsProcessingParameterFeatureSink(
                self.OUTPUT, "Tile statistics", compat.SOURCE_ANY_GEOMETRY
            )
        )

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        name = self.parameterAsString(parameters, COVERAGE, context).strip()
        fields = make_fields(*self.FIELDS)
        with self.open(parameters, context, read_only=True) as gpkg:
            (coverage,) = resolve_coverages(gpkg, name, all_if_empty=False)
            zoom = zoom_value(self, parameters, context)
            if zoom is None:
                zoom = max(coverage.zoom_levels)
            crs = crs_for_srs(gpkg.srs(coverage.srs_id))
            sink, dest = self.sink(parameters, context, fields, compat.WKB_POLYGON, crs)
            stats = wrap_errors(coverage.tile_statistics, zoom)
            for i, tile in enumerate(stats):
                if feedback.isCanceled():
                    break
                calculated = tile["calculated"] or {}
                add_row(
                    sink,
                    fields,
                    [
                        coverage.name,
                        tile["zoom"],
                        tile["column"],
                        tile["row"],
                        tile["min"],
                        tile["max"],
                        tile["mean"],
                        tile["std_dev"],
                        calculated.get("min"),
                        calculated.get("max"),
                        calculated.get("mean"),
                        calculated.get("std_dev"),
                        calculated.get("count", 0),
                        tile["error"],
                    ],
                    tile_polygon(coverage, tile["zoom"], tile["column"], tile["row"]),
                )
                feedback.setProgress(100 * (i + 1) / max(len(stats), 1))
        return {self.OUTPUT: dest}


class CheckCoverageSamplesAlgorithm(Algorithm):
    NAME = "checkcoveragesamples"
    DISPLAY_NAME = "Find NaN and infinite values in coverages"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Reads every stored tile of one coverage, or of every coverage in "
        "the file, and reports each tile that contains a NaN or infinite "
        "value or that cannot be read. The GeoPackage standard does not "
        "allow NaN or infinite values in a coverage.\n\n"
        "Validate GeoPackage does not make this check, because it reads only "
        "the header of each tile.\n\n"
        "The output has the outline of each failing tile, in the coverage's "
        "CRS. If the output is empty, every tile passed."
    )

    FAILURES = "FAILURES"
    FIELDS = (
        ("coverage", "string"),
        ("zoom", "int"),
        ("tile_column", "int"),
        ("tile_row", "int"),
        ("message", "string"),
    )

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_name_parameter(COVERAGE, "Coverage name (if empty, every coverage)")
        self.addParameter(
            QgsProcessingParameterFeatureSink(
                self.OUTPUT, "Failing tiles", compat.SOURCE_ANY_GEOMETRY
            )
        )
        self.addOutput(QgsProcessingOutputNumber(self.FAILURES, "Failing tiles"))

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        name = self.parameterAsString(parameters, COVERAGE, context).strip()
        fields = make_fields(*self.FIELDS)
        failures = 0
        with self.open(parameters, context, read_only=True) as gpkg:
            coverages = resolve_coverages(gpkg, name, all_if_empty=True)
            # One sink, so one CRS: the first coverage's. Coverages in other
            # CRSs are still checked and reported, and a warning says so.
            crs = crs_for_srs(gpkg.srs(coverages[0].srs_id))
            sink, dest = self.sink(parameters, context, fields, compat.WKB_POLYGON, crs)
            for coverage in coverages:
                same_crs = coverage.srs_id == coverages[0].srs_id
                if not same_crs:
                    feedback.reportError(
                        f"{coverage.name} has a different CRS from the first "
                        "coverage; its failing tiles are listed without outlines",
                        fatalError=False,
                    )
                found = wrap_errors(coverage.check_samples)
                for zoom, column, row, message in found:
                    failures += 1
                    feedback.pushWarning(
                        f"{coverage.name}, zoom {zoom}, column {column}, row {row}: {message}"
                    )
                    add_row(
                        sink,
                        fields,
                        [coverage.name, zoom, column, row, message],
                        tile_polygon(coverage, zoom, column, row) if same_crs else None,
                    )
                if not found:
                    feedback.pushInfo(f"{coverage.name}: every tile passed")
        return {self.OUTPUT: dest, self.FAILURES: failures}
