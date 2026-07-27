//! Write-path throughput benchmarks (M2 acceptance criterion 4).
//!
//! Measures [`geopackage::Layer::write_all`] for point, linestring, and polygon
//! layers across the three index configurations:
//!
//! - `unindexed`:  no spatial index; plain batched inserts.
//! - `triggered`:  an empty 1.4 spatial index present, maintained per row by
//!   the RTree triggers as each row lands (`never_bulk`).
//! - `bulk`:       an empty 1.4 spatial index present, built by writing the
//!   shadow tables after the rows are inserted with triggers dropped
//!   (`always_bulk`).
//!
//! Row count defaults to 1,000,000; override with `GPKG_BENCH_ROWS`. Data is
//! generated once per geometry kind and cloned into each measured setup, so the
//! timed region is the write, not the generation or the temp-file plumbing.
//!
//! Run: `cargo bench -p geopackage --bench write`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use geo_types::{Coord, Geometry, LineString, Point, Polygon};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BulkIndexOptions, ColumnConstraint, ColumnSpec, ConstraintKind, DataColumn, GeoPackage,
    GeometrySpec, NewFeature, OpenOptions, TableSchemaBuilder, Value,
};

/// Number of rows written per benchmark. 1,000,000 by default; override with
/// `GPKG_BENCH_ROWS` (used to keep quick verification runs small).
fn rows() -> usize {
    std::env::var("GPKG_BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000)
}

/// A deterministic, well-spread coordinate for row `i`, over roughly the WGS 84
/// domain. The golden-ratio and pi multipliers scatter successive rows so the
/// RTree does not see a monotone insertion order.
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    (x, y)
}

/// The three geometry kinds benchmarked.
#[derive(Clone, Copy)]
enum GeomKind {
    Point,
    Line,
    Polygon,
}

impl GeomKind {
    fn label(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Line => "linestring",
            Self::Polygon => "polygon",
        }
    }

    fn geometry_type(self) -> GeometryType {
        match self {
            Self::Point => GeometryType::Point,
            Self::Line => GeometryType::LineString,
            Self::Polygon => GeometryType::Polygon,
        }
    }

    /// Build one geometry for row `i`. Lines carry four vertices and polygons a
    /// closed five-vertex ring, both anchored on [`coord`].
    fn geometry(self, i: usize) -> Geometry<f64> {
        let (x, y) = coord(i);
        match self {
            Self::Point => Geometry::Point(Point::new(x, y)),
            Self::Line => Geometry::LineString(LineString::from(vec![
                (x, y),
                (x + 0.001, y + 0.002),
                (x + 0.003, y - 0.001),
                (x + 0.004, y + 0.004),
            ])),
            Self::Polygon => {
                let ring = LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.01, y },
                    Coord {
                        x: x + 0.01,
                        y: y + 0.01,
                    },
                    Coord { x, y: y + 0.01 },
                    Coord { x, y },
                ]);
                Geometry::Polygon(Polygon::new(ring, Vec::new()))
            }
        }
    }

    /// Generate `n` attribute-free features of this kind.
    fn generate(self, n: usize) -> Vec<NewFeature<Geometry<f64>>> {
        (0..n)
            .map(|i| NewFeature::new(self.geometry(i), Vec::new()))
            .collect()
    }
}

/// The spatial-index configuration under test.
#[derive(Clone, Copy)]
enum Mode {
    Unindexed,
    Triggered,
    Bulk,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Unindexed => "unindexed",
            Self::Triggered => "triggered",
            Self::Bulk => "bulk",
        }
    }

    /// The bulk-vs-triggered choice forced for this mode's `write_all`.
    fn options(self) -> BulkIndexOptions {
        match self {
            // Unindexed never reaches the bulk decision (no index); the value is
            // irrelevant, so keep the honest triggered/never choice.
            Self::Unindexed | Self::Triggered => BulkIndexOptions::never_bulk(),
            Self::Bulk => BulkIndexOptions::always_bulk(),
        }
    }
}

/// A prepared, but not-yet-written, benchmark target: a fresh GeoPackage with an
/// empty layer (and, for the indexed modes, an empty spatial index), plus the
/// cloned features to write into it.
struct Target {
    _dir: tempfile::TempDir,
    gpkg: GeoPackage,
    table: String,
    features: Vec<NewFeature<Geometry<f64>>>,
    options: BulkIndexOptions,
}

/// Create a fresh file, layer, and (for indexed modes) empty index, and clone
/// the pre-generated features into it. Not part of the timed region.
fn prepare(geom: GeomKind, mode: Mode, master: &[NewFeature<Geometry<f64>>]) -> Target {
    let dir = tempfile::tempdir().expect("tempdir");
    let gpkg = GeoPackage::create(dir.path().join("bench.gpkg")).expect("create gpkg");
    let table = geom.label().to_owned();
    let builder = TableSchemaBuilder::new(table.clone())
        .geometry(GeometrySpec::new(geom.geometry_type(), 4326))
        // Each arm decides for itself, so the unindexed one stays unindexed.
        .spatial_index(false);
    {
        let layer = gpkg.create_layer(&builder).expect("create layer");
        if matches!(mode, Mode::Triggered | Mode::Bulk) {
            // Building the index on the empty table installs the 1.4 triggers and
            // an empty RTree, the starting state for both indexed write paths.
            layer.create_spatial_index().expect("create spatial index");
        }
    }
    Target {
        _dir: dir,
        gpkg,
        table,
        features: master.to_vec(),
        options: mode.options(),
    }
}

/// The timed region: write every prepared feature into the layer.
fn run(target: Target) -> usize {
    let layer = target.gpkg.layer(&target.table).expect("open layer");
    let fids = layer
        .write_all_with(target.features, 0, target.options)
        .expect("write_all");
    fids.len()
}

fn bench_writes(c: &mut Criterion) {
    let n = rows();
    let mut group = c.benchmark_group("write");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(12));
    group.throughput(Throughput::Elements(
        u64::try_from(n).expect("row count fits u64"),
    ));

    for geom in [GeomKind::Point, GeomKind::Line, GeomKind::Polygon] {
        let master = geom.generate(n);
        for mode in [Mode::Unindexed, Mode::Triggered, Mode::Bulk] {
            let name = format!("{}/{}", geom.label(), mode.label());
            group.bench_function(&name, |b| {
                b.iter_batched(
                    || prepare(geom, mode, &master),
                    |target| black_box(run(target)),
                    BatchSize::PerIteration,
                );
            });
        }
    }
    group.finish();
}

/// A layer whose columns carry `gpkg_schema` constraints, and the rows to
/// write into it. Both arms get the identical file; only the option differs,
/// so what is measured is the checking rather than the having.
fn prepare_constrained(enforce: bool, master: &[NewFeature<Geometry<f64>>]) -> Target {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.gpkg");
    {
        let gpkg = GeoPackage::create(&path).expect("create gpkg");
        gpkg.create_layer(
            &TableSchemaBuilder::new("sites")
                .column(ColumnSpec::new("code", ColumnType::Text(None)))
                .column(ColumnSpec::new("year", ColumnType::Integer))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                .spatial_index(false),
        )
        .expect("create layer");
        gpkg.add_column_constraint(&ColumnConstraint {
            name: "codes".to_owned(),
            kind: ConstraintKind::Glob("[A-Z][A-Z]-*".to_owned()),
            description: None,
        })
        .expect("glob constraint");
        gpkg.add_column_constraint(&ColumnConstraint {
            name: "years".to_owned(),
            kind: ConstraintKind::Range {
                min: 0.0,
                min_is_inclusive: true,
                max: 1e9,
                max_is_inclusive: true,
            },
            description: None,
        })
        .expect("range constraint");
        for (column, constraint) in [("code", "codes"), ("year", "years")] {
            gpkg.set_data_column(
                "sites",
                &DataColumn {
                    column_name: column.to_owned(),
                    name: None,
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: Some(constraint.to_owned()),
                },
            )
            .expect("describe column");
        }
        gpkg.close().expect("close");
    }
    let gpkg = OpenOptions::new()
        .enforce_column_constraints(enforce)
        .open(&path)
        .expect("open gpkg");
    Target {
        _dir: dir,
        gpkg,
        table: "sites".to_owned(),
        features: master.to_vec(),
        options: BulkIndexOptions::never_bulk(),
    }
}

/// What enforcing `gpkg_schema` constraints costs on the write path.
///
/// Two constrained columns of the two forms a value can be checked against: a
/// glob over text and a range over an integer. Every row satisfies both, so the
/// measurement is the check itself rather than the cost of failing.
fn bench_constraint_enforcement(c: &mut Criterion) {
    let n = rows();
    let master: Vec<NewFeature<Geometry<f64>>> = (0..n)
        .map(|i| {
            let (x, y) = coord(i);
            NewFeature::new(
                Geometry::Point(Point::new(x, y)),
                vec![
                    Value::Text(format!("IE-{i:07}")),
                    Value::Integer(i as i64 % 1_000_000),
                ],
            )
        })
        .collect();

    let mut group = c.benchmark_group("constraints");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(12));
    group.throughput(Throughput::Elements(
        u64::try_from(n).expect("row count fits u64"),
    ));
    for (label, enforce) in [("unenforced", false), ("enforced", true)] {
        group.bench_function(label, |b| {
            b.iter_batched(
                || prepare_constrained(enforce, &master),
                |target| black_box(run(target)),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_writes, bench_constraint_enforcement);
criterion_main!(benches);
