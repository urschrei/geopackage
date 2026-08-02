# How to validate a GeoPackage and act on the findings

`GeoPackage::validate` makes one pass over a file and returns what is wrong
with it, without modifying anything. This guide runs that check, reads the
result, and applies the repairs it names.

What this reports is what these checks can see, not conformance in every
respect the spec defines. The OGC executable test suite remains the authority
on that.

## In a script or a CI job

```console
$ gpkg validate places.gpkg
places.gpkg
  warning: spatial index on "points" is maintained by a pre-1.4 or mixed trigger set
    repair: upgrade the trigger set with Layer::repair_spatial_index

  0 errors, 1 warning, 0 advisories
```

Exit codes:

- Zero when there are no findings, and when the findings are warnings and
  advisories only.
- Non-zero when any finding is an error, the severity meaning a reader can get
  a wrong answer from the file.

If warnings should fail the job too, add `--strict`:

```console
$ gpkg validate --strict places.gpkg
$ echo $?
1
```

The file is opened read-only and leniently, so one with something wrong with
it is reported rather than rejected.

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

Findings come back most severe first, and stably ordered within a severity, so
a diff of the output is a diff of the findings rather than of their order.

## Read the severities

Severity is about consequence, not about how hard something is to fix:

- **`Severity::Error`**: a reader can get a wrong answer. A `gpkg_contents`
  row naming a table that is not there, a spatial index that no longer
  describes its rows, a metadata or relation row pointing at something that
  has gone.
- **`Severity::Warning`**: the file is out of step with the current spec but
  reads correctly. A pre-1.2 `application_id`, a pre-1.4 index trigger set, an
  extension these checks cannot identify, a tile pyramid that breaks the
  matrix rules.
- **`Severity::Advisory`**: a remark rather than a defect. A feature table
  with no spatial index is the case: it reads correctly, and not indexing a
  layer is a decision someone may have taken deliberately.

## Apply the repairs

`Finding::repair` returns the advice as text, naming the method that performs
it. `None` means the fix is outside this library: it needs the writer that
produced the file, or a decision about the data that should not be taken on
your behalf. To act on the findings rather than print them, match on the
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

Note that the handle has to be open read-write for any of these, and that
`Finding` is `#[non_exhaustive]`: a variant added in a later version reaches
the catch-all arm rather than breaking the build.

For the index repairs in particular, see
[How to repair a file's spatial indexes](repair-spatial-indexes.md), which
covers doing the same work without validating the whole file first.

## Then

- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  covers what an audit checks that a status cannot.
- API reference:
  [`GeoPackage::validate`](https://docs.rs/geopackage/latest/geopackage/struct.GeoPackage.html#method.validate),
  [`Finding`](https://docs.rs/geopackage/latest/geopackage/enum.Finding.html),
  [`Severity`](https://docs.rs/geopackage/latest/geopackage/enum.Severity.html).
