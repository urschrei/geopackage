# How to validate a GeoPackage and act on the findings

`GeoPackage::validate` checks a file in one pass and returns what is wrong
with it. It modifies nothing. This guide shows you how to run the check, read
the result, and apply the repairs it names.

The report covers only what these checks can see. It does not establish
conformance in every respect the spec defines. For that, the OGC executable
test suite remains the authority.

## In a script or a CI job

```console
$ gpkg validate places.gpkg
places.gpkg
  warning: spatial index on "points" is maintained by a pre-1.4 or mixed trigger set
    repair: upgrade the trigger set with Layer::repair_spatial_index

  0 errors, 1 warning, 0 advisories
```

Exit codes:

- Zero when there are no findings, or only warnings and advisories.
- Non-zero when any finding is an error. An error means a reader can get a
  wrong answer from the file.

To fail on warnings too, add `--strict`:

```console
$ gpkg validate --strict places.gpkg
$ echo $?
1
```

The command opens the file read-only and leniently, so a file with problems
is reported on rather than rejected.

## In Rust

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{GeoPackage, Severity};

let gpkg = GeoPackage::open_read_only_lenient("places.gpkg")?;
let findings = gpkg.validate()?;

for finding in &findings {
    println!("{}: {finding}", finding.severity());
    if let Some(repair) = finding.repair() {
        println!("  repair: {repair}");
    }
}

let failed = findings.iter().any(|f| f.severity() == Severity::Error);
# Ok(()) }
```

Findings are sorted most severe first, in a stable order within each
severity. If you diff the output of two runs, any differences are in the
findings, not in their order.

## Read the severities

Severity reflects the consequence of a problem, not how hard it is to fix:

- **`Severity::Error`**: a reader can get a wrong answer. For example, a
  `gpkg_contents` row naming a missing table, a spatial index that no longer
  describes its rows, or a metadata or relation row that points at something
  that no longer exists.
- **`Severity::Warning`**: the file reads correctly but does not follow the
  current spec. For example, a pre-1.2 `application_id`, a pre-1.4 index
  trigger set, an extension these checks cannot identify, or a tile pyramid
  that breaks the matrix rules.
- **`Severity::Advisory`**: a remark, not a defect. The advisory case is a
  feature table with no spatial index. It reads correctly, and someone may have chosen not to
  index the layer.

## Apply the repairs

`Finding::repair` returns the advice as text, naming the method that performs
it. `None` means this library cannot make the fix. It needs the writer that
produced the file, or a decision about the data that the library should not
make for you. To act on the findings instead of printing them, match on the
variant:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{Finding, GeoPackage};

let gpkg = GeoPackage::open_lenient("places.gpkg")?;
for finding in gpkg.validate()? {
    match finding {
        // Contents disagree with the geometries: rebuild.
        Finding::SpatialIndexOutOfStep { table_name, .. } => {
            gpkg.layer(&table_name)?.rebuild_spatial_index()?;
        }
        // Structure is a pre-1.4 or mixed trigger set: repair.
        Finding::LegacySpatialIndexTriggers { table_name } => {
            gpkg.layer(&table_name)?.repair_spatial_index()?;
        }
        // An advisory, so build one only if you want the queries indexed.
        Finding::NoSpatialIndex { table_name } => {
            gpkg.layer(&table_name)?.create_spatial_index()?;
        }
        _ => {}
    }
}
# Ok(()) }
```

The handle must be open read-write for any of these repairs. `Finding` is
`#[non_exhaustive]`, so a variant added in a later version falls through to the
catch-all arm instead of breaking the build.

To repair indexes without validating the whole file first, see
[How to repair a file's spatial indexes](repair-spatial-indexes.md).

## Then

- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  explains what an audit checks that a status cannot.
- API reference:
  [`GeoPackage::validate`](https://docs.rs/geopackage/latest/geopackage/struct.GeoPackage.html#method.validate),
  [`Finding`](https://docs.rs/geopackage/latest/geopackage/enum.Finding.html),
  [`Severity`](https://docs.rs/geopackage/latest/geopackage/enum.Severity.html).
