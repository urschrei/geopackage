//! Normative table definition SQL (spec Annex C) and required seed rows.

/// `gpkg_spatial_ref_sys` (Annex C.1).
pub const CREATE_GPKG_SPATIAL_REF_SYS: &str = "\
CREATE TABLE gpkg_spatial_ref_sys (
  srs_name TEXT NOT NULL,
  srs_id INTEGER PRIMARY KEY,
  organization TEXT NOT NULL,
  organization_coordsys_id INTEGER NOT NULL,
  definition  TEXT NOT NULL,
  description TEXT
)";

/// `gpkg_contents` (Annex C.2).
pub const CREATE_GPKG_CONTENTS: &str = "\
CREATE TABLE gpkg_contents (
  table_name TEXT NOT NULL PRIMARY KEY,
  data_type TEXT NOT NULL,
  identifier TEXT UNIQUE,
  description TEXT DEFAULT '',
  last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  min_x DOUBLE,
  min_y DOUBLE,
  max_x DOUBLE,
  max_y DOUBLE,
  srs_id INTEGER,
  CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id)
)";

/// `gpkg_geometry_columns` (Annex C.3). Created lazily with the first feature table.
pub const CREATE_GPKG_GEOMETRY_COLUMNS: &str = "\
CREATE TABLE gpkg_geometry_columns (
  table_name TEXT NOT NULL,
  column_name TEXT NOT NULL,
  geometry_type_name TEXT NOT NULL,
  srs_id INTEGER NOT NULL,
  z TINYINT NOT NULL,
  m TINYINT NOT NULL,
  CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name),
  CONSTRAINT uk_gc_table_name UNIQUE (table_name),
  CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),
  CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id)
)";

/// `gpkg_tile_matrix_set` (Annex C.4). Created lazily with the first tile pyramid.
pub const CREATE_GPKG_TILE_MATRIX_SET: &str = "\
CREATE TABLE gpkg_tile_matrix_set (
  table_name TEXT NOT NULL PRIMARY KEY,
  srs_id INTEGER NOT NULL,
  min_x DOUBLE NOT NULL,
  min_y DOUBLE NOT NULL,
  max_x DOUBLE NOT NULL,
  max_y DOUBLE NOT NULL,
  CONSTRAINT fk_gtms_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),
  CONSTRAINT fk_gtms_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id)
)";

/// `gpkg_tile_matrix` (Annex C.5). Created lazily with the first tile pyramid.
pub const CREATE_GPKG_TILE_MATRIX: &str = "\
CREATE TABLE gpkg_tile_matrix (
  table_name TEXT NOT NULL,
  zoom_level INTEGER NOT NULL,
  matrix_width INTEGER NOT NULL,
  matrix_height INTEGER NOT NULL,
  tile_width INTEGER NOT NULL,
  tile_height INTEGER NOT NULL,
  pixel_x_size DOUBLE NOT NULL,
  pixel_y_size DOUBLE NOT NULL,
  CONSTRAINT pk_ttm PRIMARY KEY (table_name, zoom_level),
  CONSTRAINT fk_tmm_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name)
)";

/// `gpkg_extensions` (Annex C.8). Created lazily when the first extension is registered.
pub const CREATE_GPKG_EXTENSIONS: &str = "\
CREATE TABLE gpkg_extensions (
  table_name TEXT,
  column_name TEXT,
  extension_name TEXT NOT NULL,
  definition TEXT NOT NULL,
  scope TEXT NOT NULL,
  CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name)
)";

/// `gpkg_data_columns` (Annex F.9, Requirement 103), the first of the two
/// tables the `gpkg_schema` extension defines.
///
/// Verbatim from the spec, less the comments: the spec source writes them with
/// `//`, which SQLite does not accept.
///
/// The primary key is `(table_name, column_name)`, so a column has at most one
/// row. Before 1.2.1 `table_name` carried a foreign key to `gpkg_contents`;
/// that was relaxed, but files written under the old definition still have it.
pub const CREATE_GPKG_DATA_COLUMNS: &str = "\
CREATE TABLE gpkg_data_columns (
  table_name TEXT NOT NULL,
  column_name TEXT NOT NULL,
  name TEXT,
  title TEXT,
  description TEXT,
  mime_type TEXT,
  constraint_name TEXT,
  CONSTRAINT pk_gdc PRIMARY KEY (table_name, column_name),
  CONSTRAINT gdc_tn UNIQUE (table_name, name)
)";

/// `gpkg_data_column_constraints` (Annex F.9, Requirement 107), the second of
/// the `gpkg_schema` tables.
///
/// Verbatim, less the `//` comments, as [`CREATE_GPKG_DATA_COLUMNS`].
///
/// One constraint occupies one row for `range` and `glob`, and one row per
/// member for `enum`, which is why the unique constraint spans
/// `(constraint_name, constraint_type, value)` rather than the name alone.
/// `min` and `max` are NUMERIC, the spec's only exception to its own rule that
/// column types come from Table 1.
///
/// In GeoPackage 1.0 the inclusivity columns were named `minIsInclusive` and
/// `maxIsInclusive`; files written then still use those names.
pub const CREATE_GPKG_DATA_COLUMN_CONSTRAINTS: &str = "\
CREATE TABLE gpkg_data_column_constraints (
  constraint_name TEXT NOT NULL,
  constraint_type TEXT NOT NULL,
  value TEXT,
  min NUMERIC,
  min_is_inclusive BOOLEAN,
  max NUMERIC,
  max_is_inclusive BOOLEAN,
  description TEXT,
  CONSTRAINT gdcc_ntv UNIQUE (constraint_name, constraint_type, value)
)";

/// The three `gpkg_spatial_ref_sys` records required by Requirement 11:
/// EPSG:4326, undefined Cartesian (−1), undefined geographic (0).
pub const SEED_SPATIAL_REF_SYS: [&str; 3] = [
    "INSERT INTO gpkg_spatial_ref_sys \
     (srs_name, srs_id, organization, organization_coordsys_id, definition, description) VALUES (\
     'WGS 84 geodetic', 4326, 'EPSG', 4326, \
     'GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4326\"]]', \
     'longitude/latitude coordinates in decimal degrees on the WGS 84 spheroid')",
    "INSERT INTO gpkg_spatial_ref_sys \
     (srs_name, srs_id, organization, organization_coordsys_id, definition, description) VALUES (\
     'Undefined Cartesian SRS', -1, 'NONE', -1, 'undefined', \
     'undefined Cartesian coordinate reference system')",
    "INSERT INTO gpkg_spatial_ref_sys \
     (srs_name, srs_id, organization, organization_coordsys_id, definition, description) VALUES (\
     'Undefined geographic SRS', 0, 'NONE', 0, 'undefined', \
     'undefined geographic coordinate reference system')",
];
