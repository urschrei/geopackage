"""Translation between QGIS objects and geopackage_rs values."""

import datetime
import json

from qgis.core import QgsCoordinateReferenceSystem, QgsWkbTypes

from . import compat

# A field's kind (see `compat.field_kind`) to the GeoPackage column type it is
# written as. Anything else is written as TEXT, as GDAL does.
COLUMN_TYPES = {
    "bool": "BOOLEAN",
    "int": "INTEGER",
    "uint": "INTEGER",
    "int64": "INTEGER",
    "uint64": "INTEGER",
    "double": "DOUBLE",
    "string": "TEXT",
    "bytes": "BLOB",
    "date": "DATE",
    "datetime": "DATETIME",
}

# GeoPackage column types to the QGIS field kind a reader shows them as.
FIELD_KINDS = {
    "BOOLEAN": "bool",
    "TINYINT": "int",
    "SMALLINT": "int",
    "MEDIUMINT": "int",
    "INT": "int64",
    "INTEGER": "int64",
    "FLOAT": "double",
    "DOUBLE": "double",
    "REAL": "double",
    "TEXT": "string",
    "BLOB": "bytes",
    "DATE": "date",
    "DATETIME": "datetime",
}


def column_type(qgs_field):
    """The GeoPackage column type for a ``QgsField``."""
    kind = compat.field_kind(qgs_field)
    column = COLUMN_TYPES.get(kind, "TEXT")
    if column == "TEXT" and kind == "string" and qgs_field.length() > 0:
        return f"TEXT({qgs_field.length()})"
    return column


def field_kind_for(column_type_name):
    """The QGIS field kind for a GeoPackage column type, such as
    ``"TEXT(20)"``."""
    base = (column_type_name or "TEXT").split("(")[0].strip().upper()
    return FIELD_KINDS.get(base, "string")


def geometry_type(wkb_type):
    """The GeoPackage geometry type name and z/m flags for a QGIS WKB type,
    or ``None`` for no geometry."""
    if wkb_type in (compat.WKB_NO_GEOMETRY,):
        return None
    flat = QgsWkbTypes.flatType(wkb_type)
    if flat == compat.WKB_UNKNOWN:
        name = "GEOMETRY"
    else:
        name = QgsWkbTypes.displayString(flat).upper()
    z = "mandatory" if QgsWkbTypes.hasZ(wkb_type) else "prohibited"
    m = "mandatory" if QgsWkbTypes.hasM(wkb_type) else "prohibited"
    return name, z, m


def _is_null(value):
    if value is None:
        return True
    # QGIS 3 represents NULL as a null QVariant; a null QDate or QDateTime is
    # an invalid one, which is NULL too.
    is_null = getattr(value, "isNull", None)
    return callable(is_null) and is_null()


def to_value(value):
    """A QGIS attribute value as a geopackage_rs value."""
    if _is_null(value):
        return None
    if isinstance(value, (bool, int, float, str, bytes, datetime.date)):
        return value
    name = type(value).__name__
    if name == "QDateTime":
        result = value.toPyDateTime()
        if value.timeSpec() == 1 or value.offsetFromUtc() == 0:  # Qt::UTC
            result = result.replace(tzinfo=datetime.timezone.utc)
        return result
    if name == "QDate":
        return value.toPyDate()
    if name == "QTime":
        return value.toString("HH:mm:ss.zzz")
    if name == "QByteArray":
        return bytes(value)
    if isinstance(value, (list, dict)):
        return json.dumps(value, default=str)
    return str(value)


def crs_for_srs(srs):
    """A ``QgsCoordinateReferenceSystem`` for a geopackage_rs ``Srs``, or an
    invalid one for an undefined SRS."""
    if srs is None:
        return QgsCoordinateReferenceSystem()
    if srs.authid:
        crs = QgsCoordinateReferenceSystem(srs.authid)
        if crs.isValid():
            return crs
    for wkt in (srs.definition_wkt2, srs.definition):
        if wkt and wkt != "undefined":
            crs = QgsCoordinateReferenceSystem.fromWkt(wkt)
            if crs.isValid():
                return crs
    return QgsCoordinateReferenceSystem()


# Custom SRS ids start here, clear of the EPSG codes that EPSG rows use as
# their srs_id.
CUSTOM_SRS_BASE = 100000


def ensure_srs(gpkg, crs, geopackage_error):
    """Adds `crs` to the file if it is not there, and returns its srs_id.

    An EPSG CRS is added from the crate's EPSG registry under its own code. A
    CRS without one, or one the registry does not know, is added from QGIS's
    WKT under a new id from ``CUSTOM_SRS_BASE``. An invalid CRS is the
    GeoPackage's "undefined Cartesian" row, 0.
    """
    if not crs.isValid():
        return 0
    authid = crs.authid()
    if authid.upper().startswith("EPSG:"):
        code = int(authid.split(":", 1)[1])
        try:
            gpkg.add_epsg_srs(code)
            return code
        except geopackage_error:
            pass
        organization, org_id = "EPSG", code
    else:
        organization, org_id = "NONE", None

    existing = {s.srs_id for s in gpkg.srs_list()}
    wkt1 = crs.toWkt(compat.WKT1_GDAL)
    wkt2 = crs.toWkt(compat.WKT2_2019)
    for srs in gpkg.srs_list():
        if srs.definition == wkt1 or (wkt2 and srs.definition_wkt2 == wkt2):
            return srs.srs_id
    srs_id = max([CUSTOM_SRS_BASE - 1, *existing]) + 1
    gpkg.add_srs(
        srs_id,
        crs.description() or "custom",
        organization,
        org_id if org_id is not None else srs_id,
        wkt1 or "undefined",
        definition_wkt2=wkt2 or None,
    )
    return srs_id
