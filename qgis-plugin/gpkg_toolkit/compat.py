"""Names that moved between QGIS 3.28 and QGIS 4.

QGIS 3.36 and later moved many enums into ``Qgis``, and QGIS 4 (Qt 6)
removed the old spellings, along with ``QVariant.Type``. Everything version
dependent is resolved here, once, so the algorithms read the same on both.
"""

from qgis.core import (
    Qgis,
    QgsCoordinateReferenceSystem,
    QgsFeatureSink,
    QgsField,
    QgsProcessing,
    QgsProcessingParameterFile,
    QgsProcessingParameterNumber,
    QgsWkbTypes,
)


def _pick(new, old):
    """The first of two dotted lookups that resolves."""
    for root, path in (new, old):
        obj = root
        try:
            for part in path.split("."):
                obj = getattr(obj, part)
            return obj
        except AttributeError:
            continue
    raise AttributeError(f"neither {new[1]} nor {old[1]}")


SOURCE_POINT = _pick((Qgis, "ProcessingSourceType.VectorPoint"), (QgsProcessing, "TypeVectorPoint"))
SOURCE_VECTOR = _pick((Qgis, "ProcessingSourceType.Vector"), (QgsProcessing, "TypeVector"))
SOURCE_ANY_GEOMETRY = _pick(
    (Qgis, "ProcessingSourceType.VectorAnyGeometry"), (QgsProcessing, "TypeVectorAnyGeometry")
)
FILE_BEHAVIOUR = _pick(
    (Qgis, "ProcessingFileParameterBehavior.File"), (QgsProcessingParameterFile, "File")
)
NUMBER_INTEGER = _pick(
    (Qgis, "ProcessingNumberParameterType.Integer"), (QgsProcessingParameterNumber, "Integer")
)
FAST_INSERT = _pick((QgsFeatureSink, "Flag.FastInsert"), (QgsFeatureSink, "FastInsert"))
WKB_NO_GEOMETRY = _pick((Qgis, "WkbType.NoGeometry"), (QgsWkbTypes, "NoGeometry"))
WKB_POLYGON = _pick((Qgis, "WkbType.Polygon"), (QgsWkbTypes, "Polygon"))
WKT1_GDAL = _pick((Qgis, "CrsWktVariant.Wkt1Gdal"), (QgsCoordinateReferenceSystem, "WKT1_GDAL"))
WKT2_2019 = _pick(
    (Qgis, "CrsWktVariant.Wkt2_2019Simplified"),
    (QgsCoordinateReferenceSystem, "WKT2_2019_SIMPLIFIED"),
)
WKB_UNKNOWN = _pick((Qgis, "WkbType.Unknown"), (QgsWkbTypes, "Unknown"))

# `QgsField` takes `QMetaType.Type` from 3.38, and only that in QGIS 4.
_METATYPE_FIELDS = Qgis.QGIS_VERSION_INT >= 33800

# QVariant::Type and QMetaType::Type share their numbering for these.
_TYPE_CODES = {
    "bool": 1,
    "int": 2,
    "uint": 3,
    "int64": 4,
    "uint64": 5,
    "double": 6,
    "string": 10,
    "bytes": 12,
    "date": 14,
    "time": 15,
    "datetime": 16,
}


def type_code(qtype):
    """An integer for a ``QVariant.Type`` or ``QMetaType.Type``."""
    return int(getattr(qtype, "value", qtype))


def field(name, kind, length=0):
    """A ``QgsField`` of `kind`, one of the keys of ``_TYPE_CODES``."""
    code = _TYPE_CODES[kind]
    if _METATYPE_FIELDS:
        from qgis.PyQt.QtCore import QMetaType

        qtype = QMetaType.Type(code)
    else:
        from qgis.PyQt.QtCore import QVariant

        qtype = QVariant.Type(code)
    return QgsField(name, qtype, len=length)


def field_kind(qgs_field):
    """The `kind` of an existing field, or ``None`` for one not listed."""
    code = type_code(qgs_field.type())
    for kind, known in _TYPE_CODES.items():
        if known == code:
            return kind
    return None
