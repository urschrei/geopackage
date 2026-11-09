"""Type stubs for the native module. Docstrings live on the Rust side."""

import datetime
from collections.abc import Iterable, Iterator, Mapping, Sequence
from os import PathLike
from typing import Any, Literal, Optional, Union

from typing_extensions import TypeAlias

__version__: str

Value: TypeAlias = Union[None, bool, int, float, str, bytes, datetime.date, datetime.datetime]
Bounds: TypeAlias = tuple[float, float, float, float]
Row: TypeAlias = tuple[int, Optional[bytes], list[Value]]
ZmFlag: TypeAlias = Union[Literal["prohibited", "mandatory", "optional"], int]

class GeoPackageError(Exception): ...

class Srs:
    srs_id: int
    name: str
    organization: str
    organization_coordsys_id: int
    definition: str
    definition_wkt2: Optional[str]
    description: Optional[str]
    epoch: Optional[float]
    authid: Optional[str]

class Finding:
    severity: Literal["error", "warning", "advisory"]
    code: str
    message: str
    table_name: Optional[str]
    repair: Optional[str]

class Extension:
    name: str
    table_name: Optional[str]
    column_name: Optional[str]
    definition: str
    scope: str
    support: Literal["implemented", "known", "removed", "unrecognised"]

class Field:
    name: str
    declared_type: str
    column_type: Optional[str]
    not_null: bool
    default_value: Optional[str]

class GeometryColumn:
    column_name: str
    geometry_type: str
    srs_id: int
    z: str
    m: str

class FeatureIterator(Iterator[Row]):
    columns: list[str]
    def __iter__(self) -> FeatureIterator: ...
    def __next__(self) -> Row: ...
    def next_chunk(self) -> list[Row]: ...
    def close(self) -> None: ...

class Layer:
    name: str
    kind: Literal["features", "attributes"]
    fields: list[Field]
    field_names: list[str]
    primary_key: Optional[str]
    geometry_column: Optional[GeometryColumn]
    def count(self) -> int: ...
    def extent(self) -> Optional[Bounds]: ...
    def recompute_extent(self) -> Optional[Bounds]: ...
    def spatial_index_status(self) -> Literal["current", "legacy", "stale", "absent"]: ...
    def has_spatial_index(self) -> bool: ...
    def create_spatial_index(self) -> None: ...
    def repair_spatial_index(self) -> None: ...
    def rebuild_spatial_index(self) -> None: ...
    def repair_wkb_type_codes(self) -> int: ...
    def repair_datetimes(self) -> int: ...
    def drop_spatial_index(self) -> None: ...
    def audit_spatial_index(self) -> dict[str, Union[int, bool]]: ...
    def extensions(self) -> list[Extension]: ...
    def features(
        self,
        bbox: Optional[Bounds] = None,
        where: Optional[str] = None,
        params: Optional[Sequence[Value]] = None,
        columns: Optional[Sequence[str]] = None,
        *,
        geometry: bool = True,
        chunk_size: int = 1000,
    ) -> FeatureIterator: ...
    def get(self, fid: int) -> Optional[Row]: ...
    def insert(
        self,
        rows: Iterable[tuple[Optional[int], Optional[bytes], Sequence[Value]]],
        *,
        batch: int = 10000,
    ) -> list[int]: ...
    def update_attributes(self, fid: int, values: Mapping[str, Value]) -> bool: ...
    def update_geometry(self, fid: int, wkb: bytes) -> bool: ...
    def delete(self, fids: Sequence[int]) -> int: ...

class TileMatrix:
    zoom_level: int
    matrix_width: int
    matrix_height: int
    tile_width: int
    tile_height: int
    pixel_x_size: float
    pixel_y_size: float

class Coverage:
    name: str
    datatype: Literal["integer", "float"]
    scale: float
    offset: float
    precision: Optional[float]
    data_null: Optional[float]
    uom: Optional[str]
    field_name: Optional[str]
    quantity_definition: Optional[str]
    grid_cell_encoding: Optional[str]
    srs_id: int
    extent: Bounds
    matrices: list[TileMatrix]
    zoom_levels: list[int]
    def tile_count(self, zoom: Optional[int] = None) -> int: ...
    def tile_bounds(self, zoom: int, column: int, row: int) -> Bounds: ...
    def validate(self) -> None: ...
    def values_at(
        self,
        points: Sequence[tuple[float, float]],
        zoom: Optional[int] = None,
        interpolation: Literal["nearest", "bilinear"] = "nearest",
    ) -> list[Optional[float]]: ...
    def tile(self, zoom: int, column: int, row: int) -> Optional[bytes]: ...
    def samples(
        self, zoom: int, column: int, row: int
    ) -> Optional[tuple[int, int, list[Optional[float]]]]: ...
    def tile_statistics(self, zoom: Optional[int] = None) -> list[dict[str, Any]]: ...
    def check_samples(self, zoom: Optional[int] = None) -> list[tuple[int, int, int, str]]: ...
    def put_tile(
        self, zoom: int, column: int, row: int, data: bytes, *, statistics: bool = True
    ) -> None: ...

class GeoPackage:
    path: str
    read_only: bool
    is_open: bool
    version: str
    def __init__(
        self,
        path: Union[str, PathLike[str]],
        *,
        read_only: bool = False,
        lenient: bool = False,
    ) -> None: ...
    @staticmethod
    def create(path: Union[str, PathLike[str]]) -> GeoPackage: ...
    def close(self) -> None: ...
    def __enter__(self) -> GeoPackage: ...
    def __exit__(self, *args: object) -> bool: ...
    def open_warnings(self) -> list[str]: ...
    def contents(self) -> list[dict[str, Any]]: ...
    def layer_names(self) -> list[str]: ...
    def attribute_table_names(self) -> list[str]: ...
    def layer(self, name: str) -> Layer: ...
    def attributes(self, name: str) -> Layer: ...
    def layers(self) -> list[Layer]: ...
    def create_layer(
        self,
        name: str,
        geometry_type: Optional[str] = None,
        srs_id: int = 0,
        columns: Sequence[Union[tuple[str, str], tuple[str, str, bool]]] = ...,
        *,
        z: Optional[ZmFlag] = None,
        m: Optional[ZmFlag] = None,
        spatial_index: bool = True,
        primary_key: Optional[str] = None,
        geometry_column: Optional[str] = None,
    ) -> Layer: ...
    def add_epsg_srs(self, code: int) -> bool: ...
    def add_srs(
        self,
        srs_id: int,
        name: str,
        organization: str,
        organization_coordsys_id: int,
        definition: str,
        *,
        description: Optional[str] = None,
        definition_wkt2: Optional[str] = None,
    ) -> bool: ...
    def srs(self, srs_id: int) -> Optional[Srs]: ...
    def srs_list(self) -> list[Srs]: ...
    def validate(self) -> list[Finding]: ...
    def extensions(self) -> list[Extension]: ...
    def coverage_names(self) -> list[str]: ...
    def coverage(self, name: str) -> Coverage: ...
    def create_coverage(
        self,
        name: str,
        srs_id: int,
        extent: Bounds,
        matrices: Sequence[tuple[int, int, int, int, int]],
        *,
        datatype: Literal["integer", "float"] = "integer",
        scale: float = 1.0,
        offset: float = 0.0,
        data_null: Optional[float] = None,
        uom: Optional[str] = None,
        grid_cell_encoding: Optional[str] = None,
    ) -> Coverage: ...
    def integrity_check(self) -> list[str]: ...
