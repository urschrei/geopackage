"""A QGIS vector data provider that reads and writes through the geopackage
crate, in place of OGR.

The provider key is ``geopackage_rs``, and the URI is the one OGR uses for a
GeoPackage layer, ``/path/to/file.gpkg|layername=roads``, so a layer can be
switched between the two providers by changing only the key.

What it does with a ``QgsFeatureRequest``:

- **Bounding box**: the crate's RTree query (``Layer.features(bbox=...)``),
  with QGIS's exact-intersection test on top when the request asks for one.
- **Feature ids**: one id goes through ``Layer.get`` on the provider's own
  connection; a list becomes a ``fid IN (...)`` clause.
- **Subset string**: an SQL ``WHERE`` clause, run by SQLite, as OGR's
  GeoPackage subset strings are. ``setSubsetString`` checks it by running it.
- **Attribute subset and no-geometry**: a column projection, so geometry blobs
  and unread columns are never fetched. Attributes outside the subset are
  NULL in the returned feature, as QGIS expects.
- **Filter expressions** are evaluated here, after the rows arrive, as the
  core providers do for an expression they cannot compile.

Every read but the single-id one streams from its own read-only connection on
its own thread (see ``Layer.features``), so iterators are safe on QGIS's
rendering threads and never contend with an edit in progress on the
provider's connection. Reads see committed data only, which is what an edit
buffer commit produces.

Editing covers adding, deleting and changing features (attributes and
geometry). Adding, deleting and renaming fields are not offered: the crate
has no schema changes yet.
"""

import os

from qgis.core import (
    QgsAbstractFeatureIterator,
    QgsAbstractFeatureSource,
    QgsCoordinateTransform,
    QgsCsException,
    QgsExpressionContext,
    QgsExpressionContextUtils,
    QgsFeature,
    QgsFeatureIterator,
    QgsFeatureRequest,
    QgsFields,
    QgsGeometry,
    QgsProject,
    QgsProviderMetadata,
    QgsProviderRegistry,
    QgsRectangle,
    QgsVectorDataProvider,
    QgsWkbTypes,
)

from . import compat
from .conversions import crs_for_srs, field_kind_for, from_value, to_value, wkb_type
from .native import gpkg

PROVIDER_KEY = "geopackage_rs"
PROVIDER_DESCRIPTION = "GeoPackage (GeoPackage Toolkit)"

# Rows per chunk the reader thread hands over. Large enough that the per-chunk
# crossing is noise, small enough that the first features of a canvas render
# arrive quickly.
CHUNK = 2000


def decode_uri(uri):
    """``{"path": ..., "layerName": ...}`` for ``path|layername=name``."""
    path, _, rest = uri.partition("|")
    parts = {"path": path}
    for option in rest.split("|") if rest else []:
        key, _, value = option.partition("=")
        if key.strip().lower() == "layername":
            parts["layerName"] = value
    return parts


def encode_uri(parts):
    uri = parts.get("path", "")
    if parts.get("layerName"):
        uri += f"|layername={parts['layerName']}"
    return uri


def _in_clause(column, fids):
    ids = ",".join(str(int(fid)) for fid in fids)
    return f"{column} IN ({ids})"


def _quote(name):
    return '"' + name.replace('"', '""') + '"'


class GpkgFeatureIterator(QgsAbstractFeatureIterator):
    """Turns one ``QgsFeatureRequest`` into one crate read."""

    def __init__(self, source, request):
        super().__init__(request)
        self._source = source
        self._request = request if request is not None else QgsFeatureRequest()
        self._rows = None
        self._buffer = []
        self._closed = False
        self._single = None

        self._transform = QgsCoordinateTransform()
        if self._request.destinationCrs().isValid() and self._request.destinationCrs() != source.crs:
            self._transform = QgsCoordinateTransform(
                source.crs, self._request.destinationCrs(), self._request.transformContext()
            )
        try:
            self._rect = self.filterRectToSourceCrs(self._transform)
        except QgsCsException:
            self.close()
            return

        self._exact_engine = None
        if not self._rect.isNull() and self._request.flags() & compat.REQUEST_EXACT_INTERSECT:
            self._exact_engine = QgsGeometry.createGeometryEngine(
                QgsGeometry.fromRect(self._rect).constGet()
            )
            self._exact_engine.prepareGeometry()

        self._distance_engine = None
        if (
            self._request.spatialFilterType() == compat.SPATIAL_DISTANCE_WITHIN
            and not self._request.referenceGeometry().isEmpty()
        ):
            reference = self._request.referenceGeometry()
            self._distance_engine = QgsGeometry.createGeometryEngine(reference.constGet())
            self._distance_engine.prepareGeometry()

        self._expression = None
        if self._request.filterType() == compat.FILTER_EXPRESSION:
            self._expression = self._request.filterExpression()
            self._context = QgsExpressionContext(source.expression_context)
            self._expression.prepare(self._context)

        self._columns = self._columns_to_read()
        self._positions = (
            None if self._columns is None else [source.field_index[c] for c in self._columns]
        )
        self._no_geometry = bool(self._request.flags() & compat.REQUEST_NO_GEOMETRY)
        self._has_transform = self._transform.isValid()
        self._want_geometry = (
            source.has_geometry
            and (
                not (self._request.flags() & compat.REQUEST_NO_GEOMETRY)
                or not self._rect.isNull()
                or self._exact_engine is not None
                or self._distance_engine is not None
                or (self._expression is not None and self._expression.needsGeometry())
            )
        )

    # -- planning -----------------------------------------------------------

    def _columns_to_read(self):
        """The value columns to read, or ``None`` for all of them."""
        if self._expression is not None:
            return None  # the expression may read any column
        if not (self._request.flags() & compat.REQUEST_SUBSET_OF_ATTRIBUTES):
            return None
        wanted = set(self._request.subsetOfAttributes())
        names = self._source.field_names
        return [names[i] for i in sorted(wanted) if 0 <= i < len(names)]

    def _start(self):
        source = self._source
        clauses = []
        if source.subset:
            clauses.append(f"({source.subset})")
        filter_type = self._request.filterType()
        if filter_type == compat.FILTER_FID:
            if not source.subset and self._rect.isNull():
                # The common case for Identify and attribute-table edits: one
                # row, read synchronously, with no reader thread to start.
                self._single = source.layer.get(int(self._request.filterFid()))
                # `get` returns every column, not the projection.
                self._columns = None
                self._positions = None
                self._rows = iter(())
                return
            clauses.append(_in_clause(source.pk_sql, [self._request.filterFid()]))
        elif filter_type == compat.FILTER_FIDS:
            fids = list(self._request.filterFids())
            if not fids:
                self._rows = iter(())
                return
            clauses.append(_in_clause(source.pk_sql, fids))
        bbox = None
        if not self._rect.isNull() and source.has_geometry:
            bbox = (
                self._rect.xMinimum(),
                self._rect.yMinimum(),
                self._rect.xMaximum(),
                self._rect.yMaximum(),
            )
        self._rows = source.layer.features(
            bbox=bbox,
            where=" AND ".join(clauses) or None,
            columns=self._columns,
            geometry=self._want_geometry,
            chunk_size=CHUNK,
        )

    # -- iteration ----------------------------------------------------------

    def _next_row(self):
        if self._single is not None:
            row, self._single = self._single, None
            return row
        if not self._buffer:
            if self._rows is None:
                self._start()
                if self._single is not None:
                    return self._next_row()
            next_chunk = getattr(self._rows, "next_chunk", None)
            if next_chunk is None:
                return None
            self._buffer = next_chunk()
            self._buffer.reverse()
            if not self._buffer:
                return None
        return self._buffer.pop()

    def fetchFeature(self, f):  # noqa: N802 - QGIS's name
        if self._closed:
            f.setValid(False)
            return False
        source = self._source
        # Per-feature work is the provider's cost, so everything the request
        # fixes is looked up once, here, rather than per row.
        convert = source.converted
        fields = source.fields
        exact = self._exact_engine
        expression = self._expression
        distance = self._distance_engine
        no_geometry = self._no_geometry
        width = len(source.field_names)
        convert_set = source.converted_set
        while True:
            try:
                row = self._next_row()
            except gpkg.GeoPackageError as error:
                source.report(f"cannot read features: {error}")
                row = None
            if row is None:
                f.setValid(False)
                return False
            fid, wkb, values = row
            # Read after the row: the first read can change the projection
            # (a single-id lookup returns every column).
            columns = self._columns
            positions = self._positions
            f.setFields(fields, False)
            f.setId(fid)
            if columns is None:
                if convert:
                    for i in convert:
                        values[i] = from_value(values[i])
                attributes = values
            else:
                attributes = [None] * width
                for index, value in zip(positions, values):
                    attributes[index] = from_value(value) if index in convert_set else value
            f.setAttributes(attributes)
            if wkb is not None and not (no_geometry and exact is None and distance is None
                                        and expression is None):
                geometry = QgsGeometry()
                geometry.fromWkb(wkb)
                f.setGeometry(geometry)
            else:
                f.clearGeometry()

            if exact is not None and not (
                f.hasGeometry() and exact.intersects(f.geometry().constGet())
            ):
                continue
            if expression is not None:
                self._context.setFeature(f)
                if not expression.evaluate(self._context):
                    continue
            if self._has_transform:
                self.geometryToDestinationCrs(f, self._transform)
            if distance is not None and (
                not f.hasGeometry()
                or distance.distance(f.geometry().constGet()) > self._request.distanceWithin()
            ):
                continue
            if no_geometry and f.hasGeometry():
                f.clearGeometry()
            f.setValid(True)
            return True

    def __iter__(self):
        return self

    def __next__(self):
        f = QgsFeature()
        if not self.nextFeature(f):
            raise StopIteration
        return f

    def rewind(self):
        if self._closed:
            return False
        self._discard()
        return True

    def close(self):
        self._discard()
        self._closed = True
        return True

    def _discard(self):
        rows = self._rows
        self._rows, self._buffer, self._single = None, [], None
        closer = getattr(rows, "close", None)
        if closer is not None:
            closer()


class GpkgFeatureSource(QgsAbstractFeatureSource):
    """A snapshot of what an iterator needs, safe to use on another thread."""

    def __init__(self, provider):
        super().__init__()
        self.provider = provider
        self.layer = provider._layer
        self.fields = QgsFields(provider._fields)
        self.field_names = list(provider._field_names)
        self.field_index = {name: i for i, name in enumerate(self.field_names)}
        # The columns whose values need converting for QGIS: dates and
        # datetimes. The rest cross as they are.
        self.converted = [
            i
            for i, f in enumerate(self.fields)
            if compat.field_kind(f) in ("date", "datetime")
        ]
        self.converted_set = frozenset(self.converted)
        self.pk_sql = provider._pk_sql
        self.subset = provider._subset
        self.crs = provider._crs
        self.has_geometry = provider._layer.geometry_column is not None
        self.expression_context = QgsExpressionContext()
        self.expression_context.appendScope(QgsExpressionContextUtils.globalScope())
        self.expression_context.appendScope(
            QgsExpressionContextUtils.projectScope(QgsProject.instance())
        )
        self.expression_context.setFields(self.fields)

    def report(self, message):
        self.provider.pushError(message)

    def getFeatures(self, request):  # noqa: N802
        return QgsFeatureIterator(GpkgFeatureIterator(self, request))


class GpkgDataProvider(QgsVectorDataProvider):
    @classmethod
    def providerKey(cls):  # noqa: N802
        return PROVIDER_KEY

    @classmethod
    def description(cls):
        return PROVIDER_DESCRIPTION

    @classmethod
    def createProvider(cls, uri, options, flags=None):  # noqa: N802
        return cls(uri, options, flags)

    def __init__(self, uri="", options=None, flags=None):
        # The options and flags are QGIS's to pass; this provider reads
        # neither, so None stands in for a default rather than a shared
        # instance built once at import.
        super().__init__(uri)
        self._uri = uri
        self._valid = False
        self._subset = ""
        self._extent = None
        self._count = None
        self._gpkg = None
        self._layer = None
        self._fields = QgsFields()
        self._field_names = []
        self._writable = False
        self._crs = crs_for_srs(None)
        self._wkb_type = compat.WKB_NO_GEOMETRY
        self._pk_sql = "rowid"
        try:
            self._open(uri)
        except (gpkg.GeoPackageError, KeyError, OSError, ValueError) as error:
            self.pushError(f"{PROVIDER_DESCRIPTION}: cannot open {uri}: {error}")

    def _open(self, uri):
        parts = decode_uri(uri)
        path = parts["path"]
        self._writable = os.access(path, os.W_OK)
        self._gpkg = gpkg.GeoPackage(path, read_only=not self._writable, lenient=True)
        name = parts.get("layerName")
        if not name:
            names = self._gpkg.layer_names() + self._gpkg.attribute_table_names()
            if not names:
                raise ValueError("the file has no layers or tables")
            if len(names) > 1:
                raise ValueError(
                    "the file has more than one layer or table; add "
                    f"|layername=<name> to the source, with one of: {', '.join(names)}"
                )
            name = names[0]
        if name in self._gpkg.layer_names():
            self._layer = self._gpkg.layer(name)
        else:
            self._layer = self._gpkg.attributes(name)

        for field in self._layer.fields:
            kind = field_kind_for(field.column_type or field.declared_type)
            length = 0
            declared = field.declared_type.upper()
            if declared.startswith("TEXT(") and declared.endswith(")"):
                length = int(declared[5:-1] or 0)
            qgs_field = compat.field(field.name, kind, length)
            qgs_field.setTypeName(field.declared_type)
            self._fields.append(qgs_field)
            self._field_names.append(field.name)
        self._pk_sql = _quote(self._layer.primary_key) if self._layer.primary_key else "rowid"

        geometry = self._layer.geometry_column
        if geometry is not None:
            self._wkb_type = wkb_type(geometry)
            self._crs = crs_for_srs(self._gpkg.srs(geometry.srs_id))
        self._valid = True

    # -- QgsDataProvider ------------------------------------------------------

    def name(self):
        return PROVIDER_KEY

    def isValid(self):  # noqa: N802
        return self._valid

    def crs(self):
        return self._crs

    def dataSourceUri(self, expandAuthConfig=False):  # noqa: N802
        return self._uri

    def storageType(self):  # noqa: N802
        return PROVIDER_DESCRIPTION

    def extent(self):
        if self._extent is None:
            self._extent = QgsRectangle()
            self._extent.setNull()
            if self._valid and self._layer.geometry_column is not None:
                bounds = None
                if not self._subset:
                    try:
                        bounds = self._layer.extent()
                    except gpkg.GeoPackageError:
                        bounds = None
                if bounds is not None:
                    self._extent = QgsRectangle(*bounds)
                else:
                    # A subset, or no recorded extent: measure the rows.
                    request = QgsFeatureRequest().setSubsetOfAttributes([])
                    for feature in self.getFeatures(request):
                        if feature.hasGeometry():
                            self._extent.combineExtentWith(feature.geometry().boundingBox())
        return QgsRectangle(self._extent)

    def updateExtents(self):  # noqa: N802
        self._extent = None

    # -- QgsVectorDataProvider -------------------------------------------------

    def featureSource(self):  # noqa: N802
        return GpkgFeatureSource(self)

    def getFeatures(self, request=None):  # noqa: N802
        request = request if request is not None else QgsFeatureRequest()
        return QgsFeatureIterator(GpkgFeatureIterator(GpkgFeatureSource(self), request))

    def wkbType(self):  # noqa: N802
        return self._wkb_type

    def fields(self):
        return self._fields

    def featureCount(self):  # noqa: N802
        if not self._valid:
            return 0
        if self._count is None:
            if not self._subset:
                self._count = self._layer.count()
            else:
                rows = self._layer.features(
                    where=self._subset, columns=[], geometry=False, chunk_size=10_000
                )
                total = 0
                while chunk := rows.next_chunk():
                    total += len(chunk)
                self._count = total
        return self._count

    def subsetString(self):  # noqa: N802
        return self._subset

    def supportsSubsetString(self):  # noqa: N802
        return True

    def subsetStringDialect(self):  # noqa: N802
        return "SQLite WHERE clause"

    def setSubsetString(self, subset, updateFeatureCount=True):  # noqa: N802
        subset = (subset or "").strip()
        if subset == self._subset:
            return True
        if subset:
            # Run it once, so a typo fails here rather than as an empty layer.
            try:
                probe = self._layer.features(where=subset, columns=[], geometry=False, chunk_size=1)
                probe.next_chunk()
                probe.close()
            except gpkg.GeoPackageError as error:
                self.pushError(f"invalid subset string {subset!r}: {error}")
                return False
        self._subset = subset
        self._invalidate()
        self.dataChanged.emit()
        return True

    def capabilities(self):
        caps = compat.CAP_SELECT_AT_ID | compat.CAP_CIRCULAR_GEOMETRIES
        if self._valid and self._writable:
            caps |= (
                compat.CAP_ADD_FEATURES
                | compat.CAP_DELETE_FEATURES
                | compat.CAP_CHANGE_ATTRIBUTES
            )
            if self._layer.geometry_column is not None:
                caps |= compat.CAP_CHANGE_GEOMETRIES
        return caps

    def _invalidate(self):
        self._extent = None
        self._count = None
        self.clearMinMaxCache()

    # -- editing ---------------------------------------------------------------

    def _wkb_for(self, geometry):
        """ISO WKB for `geometry`, coerced to the column's multi and z/m form,
        or ``None`` for an empty or missing geometry."""
        if geometry is None or geometry.isNull() or geometry.isEmpty():
            return None
        geometry = QgsGeometry(geometry)
        target = self._wkb_type
        if QgsWkbTypes.isMultiType(target) and not geometry.isMultipart():
            geometry.convertToMultiType()
        abstract = geometry.get()
        if not QgsWkbTypes.hasZ(target) and abstract.is3D():
            abstract.dropZValue()
        elif QgsWkbTypes.hasZ(target) and not abstract.is3D():
            abstract.addZValue(0)
        if not QgsWkbTypes.hasM(target) and abstract.isMeasure():
            abstract.dropMValue()
        elif QgsWkbTypes.hasM(target) and not abstract.isMeasure():
            abstract.addMValue(0)
        return bytes(geometry.asWkb())

    def addFeatures(self, features, flags=None):  # noqa: N802
        rows = []
        width = len(self._field_names)
        for feature in features:
            attributes = list(feature.attributes())[:width]
            attributes += [None] * (width - len(attributes))
            wkb = self._wkb_for(feature.geometry()) if self._layer.geometry_column else None
            rows.append((None, wkb, [to_value(v) for v in attributes]))
        try:
            fids = self._layer.insert(rows)
        except gpkg.GeoPackageError as error:
            self.pushError(f"cannot add features: {error}")
            return False, features
        added = []
        for feature, fid in zip(features, fids):
            feature.setId(fid)
            added.append(feature)
        self._invalidate()
        return True, added

    def deleteFeatures(self, fids):  # noqa: N802
        if not fids:
            return True
        try:
            self._layer.delete([int(fid) for fid in fids])
        except gpkg.GeoPackageError as error:
            self.pushError(f"cannot delete features: {error}")
            return False
        self._invalidate()
        return True

    def changeAttributeValues(self, changes):  # noqa: N802
        try:
            for fid, values in changes.items():
                named = {
                    self._field_names[index]: to_value(value)
                    for index, value in values.items()
                    if 0 <= index < len(self._field_names)
                }
                if named:
                    self._layer.update_attributes(int(fid), named)
        except gpkg.GeoPackageError as error:
            self.pushError(f"cannot change attribute values: {error}")
            return False
        self._invalidate()
        return True

    def changeGeometryValues(self, changes):  # noqa: N802
        try:
            for fid, geometry in changes.items():
                wkb = self._wkb_for(geometry)
                if wkb is None:
                    self.pushError("this provider cannot remove a geometry from a feature")
                    return False
                self._layer.update_geometry(int(fid), wkb)
        except gpkg.GeoPackageError as error:
            self.pushError(f"cannot change geometries: {error}")
            return False
        self._invalidate()
        return True

    def handlePostCloneOperations(self, source):  # noqa: N802
        self._subset = source._subset


class GpkgProviderMetadata(QgsProviderMetadata):
    """Metadata with URI encoding, so that projects store the path in a form
    QGIS can make relative and resolve again."""

    def __init__(self):
        super().__init__(PROVIDER_KEY, PROVIDER_DESCRIPTION, GpkgDataProvider.createProvider)

    def decodeUri(self, uri):  # noqa: N802
        return decode_uri(uri)

    def encodeUri(self, parts):  # noqa: N802
        return encode_uri(parts)

    def providerCapabilities(self):  # noqa: N802
        return QgsProviderMetadata.FileBasedUris


_METADATA = None


def register():
    """Registers the provider once per process. QGIS cannot unregister a
    provider, so a plugin reload finds it registered already."""
    global _METADATA
    registry = QgsProviderRegistry.instance()
    if PROVIDER_KEY in registry.providerList():
        return
    _METADATA = GpkgProviderMetadata()
    registry.registerProvider(_METADATA)
