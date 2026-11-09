"""Bulk layer export through the crate's writer."""

import os

from qgis.core import (
    QgsGeometry,
    QgsProcessingException,
    QgsProcessingOutputNumber,
    QgsProcessingOutputVectorLayer,
    QgsProcessingParameterBoolean,
    QgsProcessingParameterFeatureSource,
    QgsProcessingParameterFileDestination,
    QgsProcessingParameterString,
    QgsWkbTypes,
)

from .. import compat
from ..conversions import column_type, ensure_srs, geometry_type, to_value
from ..native import gpkg
from .base import Algorithm, wrap_errors


class Cancelled(Exception):
    pass


class ExportLayerAlgorithm(Algorithm):
    NAME = "exportlayer"
    DISPLAY_NAME = "Export layer to GeoPackage"
    GROUP = "Import and export"
    GROUP_ID = "export"
    HELP = (
        "Writes a vector layer into a GeoPackage through the geopackage "
        "crate: a new file, or a new table in an existing one. Rows are "
        "written in batches with the crate's bulk RTree build, and the "
        "result is a GeoPackage 1.4 table with the 1.4 trigger set.\n\n"
        "An integer field named 'fid' supplies the feature ids, as GDAL "
        "treats it; otherwise ids are assigned. Field types map to their "
        "GeoPackage equivalents, and anything without one is written as "
        "TEXT. Single geometries in a multi layer are promoted to multi. "
        "The layer's CRS is added from the EPSG registry when it has an EPSG "
        "code, and from its WKT otherwise."
    )

    LAYER_NAME = "LAYER_NAME"
    SPATIAL_INDEX = "SPATIAL_INDEX"
    OUTPUT_LAYER = "OUTPUT_LAYER"
    WRITTEN = "WRITTEN"
    BATCH = 10_000

    def initAlgorithm(self, config=None):  # noqa: N802
        self.addParameter(
            QgsProcessingParameterFeatureSource(
                self.INPUT, "Input layer", [compat.SOURCE_VECTOR]
            )
        )
        self.addParameter(
            QgsProcessingParameterFileDestination(
                self.OUTPUT, "GeoPackage", fileFilter="GeoPackage (*.gpkg)"
            )
        )
        self.addParameter(
            QgsProcessingParameterString(
                self.LAYER_NAME, "Table name (the input's name if empty)", optional=True
            )
        )
        self.addParameter(
            QgsProcessingParameterBoolean(
                self.SPATIAL_INDEX, "Build a spatial index", True
            )
        )
        self.addOutput(
            QgsProcessingOutputVectorLayer(self.OUTPUT_LAYER, "Exported layer")
        )
        self.addOutput(QgsProcessingOutputNumber(self.WRITTEN, "Rows written"))

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        source = self.parameterAsSource(parameters, self.INPUT, context)
        if source is None:
            raise QgsProcessingException(
                self.invalidSourceError(parameters, self.INPUT)
            )
        path = self.parameterAsFileOutput(parameters, self.OUTPUT, context)
        table = self.parameterAsString(parameters, self.LAYER_NAME, context).strip()
        if not table:
            layer = self.parameterAsVectorLayer(parameters, self.INPUT, context)
            table = (layer.name() if layer else "") or "layer"
        spatial_index = self.parameterAsBool(parameters, self.SPATIAL_INDEX, context)

        fields = source.fields()
        fid_index = next(
            (
                i
                for i, f in enumerate(fields)
                if f.name().lower() == "fid"
                and compat.field_kind(f) in ("int", "int64", "uint")
            ),
            None,
        )
        value_indexes = [i for i in range(len(fields)) if i != fid_index]
        names = {fields[i].name().lower() for i in value_indexes}
        geometry_column = "geom" if "geom" not in names else "geometry"
        columns = [(fields[i].name(), column_type(fields[i])) for i in value_indexes]

        wkb_type = source.wkbType()
        geometry = geometry_type(wkb_type)
        promote = geometry is not None and QgsWkbTypes.isMultiType(wkb_type)

        try:
            if os.path.exists(path):
                handle = gpkg.GeoPackage(path, lenient=True)
            else:
                handle = gpkg.GeoPackage.create(path)
        except gpkg.GeoPackageError as error:
            raise QgsProcessingException(f"cannot open {path}: {error}") from error

        with handle:
            if geometry is not None:
                srs_id = wrap_errors(
                    ensure_srs, handle, source.sourceCrs(), gpkg.GeoPackageError
                )
                name, z, m = geometry
                target = wrap_errors(
                    handle.create_layer,
                    table,
                    name,
                    srs_id,
                    columns,
                    z=z,
                    m=m,
                    spatial_index=spatial_index,
                    geometry_column=geometry_column,
                )
            else:
                target = wrap_errors(handle.create_layer, table, None, 0, columns)

            total = source.featureCount() or 0
            step = 100.0 / total if total > 0 else 0

            def rows():
                for n, feature in enumerate(source.getFeatures()):
                    if feedback.isCanceled():
                        raise Cancelled
                    attributes = feature.attributes()
                    fid = (
                        to_value(attributes[fid_index])
                        if fid_index is not None
                        else None
                    )
                    wkb = None
                    if geometry is not None and feature.hasGeometry():
                        geom = QgsGeometry(feature.geometry())
                        if promote and not geom.isMultipart():
                            geom.convertToMultiType()
                        wkb = bytes(geom.asWkb())
                    yield fid, wkb, [to_value(attributes[i]) for i in value_indexes]
                    if n % 1000 == 0:
                        feedback.setProgress(n * step)

            try:
                written = len(wrap_errors(target.insert, rows(), batch=self.BATCH))
            except Cancelled:
                feedback.reportError(
                    "Cancelled: the batches before it were kept.", False
                )
                written = target.count()

            if geometry is not None:
                wrap_errors(target.recompute_extent)

        uri = f"{path}|layername={table}"
        context.addLayerToLoadOnCompletion(
            uri,
            context.LayerDetails(table, context.project(), self.OUTPUT_LAYER),
        )
        return {self.OUTPUT: path, self.OUTPUT_LAYER: uri, self.WRITTEN: written}
