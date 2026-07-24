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
