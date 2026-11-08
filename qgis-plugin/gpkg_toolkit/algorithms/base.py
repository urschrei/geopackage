"""Shared parameter and output plumbing for the algorithms."""

from qgis.core import (
    QgsFeature,
    QgsFields,
    QgsProcessingAlgorithm,
    QgsProcessingException,
    QgsProcessingParameterFeatureSink,
    QgsProcessingParameterFile,
    QgsProcessingParameterString,
)

from .. import compat
from ..native import gpkg


class Algorithm(QgsProcessingAlgorithm):
    """Base class: names, groups, and the GeoPackage file parameter."""

    INPUT = "INPUT"
    OUTPUT = "OUTPUT"

    NAME = ""
    DISPLAY_NAME = ""
    GROUP = ""
    GROUP_ID = ""
    HELP = ""

    def name(self):
        return self.NAME

    def displayName(self):  # noqa: N802
        return self.DISPLAY_NAME

    def group(self):
        return self.GROUP

    def groupId(self):  # noqa: N802
        return self.GROUP_ID

    def shortHelpString(self):  # noqa: N802
        return self.HELP

    def createInstance(self):  # noqa: N802
        return type(self)()

    # -- parameters ---------------------------------------------------------

    def add_gpkg_parameter(self, name=INPUT, description="GeoPackage"):
        self.addParameter(
            QgsProcessingParameterFile(
                name, description, behavior=compat.FILE_BEHAVIOUR, extension="gpkg"
            )
        )

    def add_name_parameter(self, name, description, optional=True):
        self.addParameter(QgsProcessingParameterString(name, description, optional=optional))

    def add_table_sink(self, name=OUTPUT, description="Report"):
        self.addParameter(
            QgsProcessingParameterFeatureSink(name, description, type=compat.SOURCE_VECTOR)
        )

    # -- execution ----------------------------------------------------------

    def open(self, parameters, context, *, read_only, name=INPUT):
        """Opens the file named by parameter `name`, leniently: a file worth
        validating or repairing is by definition one with something wrong."""
        path = self.parameterAsFile(parameters, name, context)
        try:
            return gpkg.GeoPackage(path, read_only=read_only, lenient=True)
        except (gpkg.GeoPackageError, OSError) as error:
            raise QgsProcessingException(f"cannot open {path}: {error}") from error

    def sink(self, parameters, context, fields, wkb_type=None, crs=None, name=OUTPUT):
        """Creates the output sink, raising if Processing cannot."""
        from qgis.core import QgsCoordinateReferenceSystem

        sink, dest = self.parameterAsSink(
            parameters,
            name,
            context,
            fields,
            compat.WKB_NO_GEOMETRY if wkb_type is None else wkb_type,
            crs or QgsCoordinateReferenceSystem(),
        )
        if sink is None:
            raise QgsProcessingException(self.invalidSinkError(parameters, name))
        return sink, dest


def make_fields(*specs):
    """``QgsFields`` from ``(name, kind)`` pairs; see ``compat.field``."""
    fields = QgsFields()
    for name, kind in specs:
        fields.append(compat.field(name, kind))
    return fields


def add_row(sink, fields, values, geometry=None):
    feature = QgsFeature(fields)
    feature.setAttributes(list(values))
    if geometry is not None:
        feature.setGeometry(geometry)
    sink.addFeature(feature, compat.FAST_INSERT)


def wrap_errors(call, *args, **kwargs):
    """Runs `call`, turning a crate error into a Processing exception."""
    try:
        return call(*args, **kwargs)
    except gpkg.GeoPackageError as error:
        raise QgsProcessingException(str(error)) from error
