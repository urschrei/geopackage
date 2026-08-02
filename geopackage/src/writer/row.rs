use geo_traits::GeometryTrait;

use crate::{Result, Value};

use super::feature_writer::FeatureWriter;

/// A new row for [`crate::Layer::write_all`]: an optional explicit feature id, an
/// optional geometry, and the value-column values in column order.
///
/// Construct with [`NewFeature::new`] (a geometry) or
/// [`NewFeature::attributes`] (none); set an explicit id with
/// [`NewFeature::with_fid`].
#[derive(Debug, Clone)]
pub struct NewFeature<G> {
    /// Explicit feature id, or `None` to let SQLite assign one.
    pub fid: Option<i64>,
    /// The geometry, or `None` for a NULL geometry / attribute row.
    pub geometry: Option<G>,
    /// The value-column values, in the layer's value-column order: every
    /// column except the geometry and the primary key.
    pub values: Vec<Value>,
}

impl<G> NewFeature<G> {
    /// Creates a feature with a geometry and its values (auto-assigned id).
    pub fn new(geometry: G, values: Vec<Value>) -> Self {
        Self {
            fid: None,
            geometry: Some(geometry),
            values,
        }
    }

    /// Creates a row with no geometry (a NULL geometry, or an attribute
    /// row).
    pub fn attributes(values: Vec<Value>) -> Self {
        Self {
            fid: None,
            geometry: None,
            values,
        }
    }

    /// Sets an explicit feature id.
    #[must_use]
    pub fn with_fid(mut self, fid: i64) -> Self {
        self.fid = Some(fid);
        self
    }
}

/// A row that [`crate::Layer::write_all`] and its bulk counterpart can write.
///
/// Implemented by [`NewFeature`], whose geometry is an object to be encoded, and
/// by the columnar write path, whose geometry is already ISO WKB and only needs
/// a header. Both paths share the batching, the bulk-index decision and the
/// transaction handling; they differ only in how one row reaches the database,
/// which is what this trait names.
pub(crate) trait WritableRow {
    /// Writes this row through `writer`, returning its assigned feature id
    /// and
    /// the XY envelope of its geometry, or `None` when it has no indexable
    /// geometry.
    fn write(self, writer: &mut FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)>;
}

impl<G: GeometryTrait<T = f64>> WritableRow for NewFeature<G> {
    fn write(self, writer: &mut FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        match &self.geometry {
            Some(geometry) => writer.insert_returning_envelope(self.fid, geometry, &self.values),
            None => writer
                .insert_row_owned(self.fid, &self.values)
                .map(|fid| (fid, None)),
        }
    }
}
