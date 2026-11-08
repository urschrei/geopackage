//! Conversions between crate values and Python objects.
//!
//! | Crate | Python |
//! |---|---|
//! | `Null` | `None` |
//! | `Integer` | `int` |
//! | `Float` | `float` |
//! | `Boolean` | `bool` |
//! | `Text` | `str` |
//! | `Blob` | `bytes` |
//! | `Date` | `datetime.date` |
//! | `DateTime` | `datetime.datetime`, aware when the stored text has an offset |
//!
//! The date types go through the `datetime` module rather than PyO3's
//! `PyDate`, because the latter is outside the limited API that abi3 builds
//! against.

use geopackage::Value;
use geopackage::core::datetime::{Date, DateTime};
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyByteArray, PyBytes, PyFloat, PyInt, PyMemoryView, PyString};

use crate::{IntoPyResult, PyGpkgResult};

/// The `datetime` module's classes, looked up once per batch of conversions.
pub(crate) struct DateTypes<'py> {
    date: Bound<'py, PyAny>,
    datetime: Bound<'py, PyAny>,
    timezone: Bound<'py, PyAny>,
    timedelta: Bound<'py, PyAny>,
}

impl<'py> DateTypes<'py> {
    pub(crate) fn new(py: Python<'py>) -> PyResult<Self> {
        let module = py.import("datetime")?;
        Ok(Self {
            date: module.getattr("date")?,
            datetime: module.getattr("datetime")?,
            timezone: module.getattr("timezone")?,
            timedelta: module.getattr("timedelta")?,
        })
    }
}

/// Converts one crate value to a Python object.
pub(crate) fn value_to_py<'py>(
    py: Python<'py>,
    types: &DateTypes<'py>,
    value: &Value,
) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Integer(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::Boolean(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Text(s) => PyString::new(py, s).into_any(),
        Value::Blob(b) => PyBytes::new(py, b).into_any(),
        Value::Date(d) => date_to_py(types, *d)?,
        Value::DateTime(dt) => {
            let tzinfo = match dt.offset_minutes {
                Some(minutes) => {
                    let delta = types.timedelta.call((0, i64::from(minutes) * 60), None)?;
                    types.timezone.call1((delta,))?
                }
                None => py.None().into_bound(py),
            };
            types.datetime.call1((
                dt.date.year(),
                dt.date.month(),
                dt.date.day(),
                dt.hour,
                dt.minute,
                dt.second,
                dt.nanosecond / 1_000,
                tzinfo,
            ))?
        }
        // `Value` is `#[non_exhaustive]`; `Null` and anything a later version
        // adds read as `None` rather than failing a whole batch.
        _ => py.None().into_bound(py),
    })
}

fn date_to_py<'py>(types: &DateTypes<'py>, date: Date) -> PyResult<Bound<'py, PyAny>> {
    types.date.call1((date.year(), date.month(), date.day()))
}

/// Converts a Python object to a crate value, for writes and query
/// parameters.
///
/// `bool` is tested before `int`, since a Python `bool` is an `int`. An aware
/// `datetime` is converted to UTC; a naive one is taken to be UTC already,
/// which is what a GeoPackage `DATETIME` is by definition.
pub(crate) fn py_to_value(object: &Bound<'_, PyAny>) -> PyGpkgResult<Value> {
    if object.is_none() {
        return Ok(Value::Null);
    }
    if object.is_instance_of::<PyBool>() {
        return Ok(Value::Boolean(object.extract()?));
    }
    if object.is_instance_of::<PyInt>() {
        return Ok(Value::Integer(object.extract()?));
    }
    if object.is_instance_of::<PyFloat>() {
        return Ok(Value::Float(object.extract()?));
    }
    if object.is_instance_of::<PyString>() {
        return Ok(Value::Text(object.extract()?));
    }
    if let Ok(bytes) = object.cast::<PyBytes>() {
        return Ok(Value::Blob(bytes.as_bytes().to_vec()));
    }
    if object.is_instance_of::<PyByteArray>() || object.is_instance_of::<PyMemoryView>() {
        let bytes = object.py().get_type::<PyBytes>().call1((object,))?;
        return Ok(Value::Blob(bytes.cast::<PyBytes>()?.as_bytes().to_vec()));
    }
    let types = DateTypes::new(object.py())?;
    // `datetime` before `date`: a `datetime` is a `date`.
    if object.is_instance(&types.datetime)? {
        let utc = types.timezone.getattr("utc")?;
        let text: String = if object.getattr("tzinfo")?.is_none() {
            object.call_method1("isoformat", ())?.extract()?
        } else {
            object
                .call_method1("astimezone", (utc,))?
                .call_method0("replace")?
                .call_method1("isoformat", ())?
                .extract()?
        };
        // `isoformat` writes `+00:00` for UTC; the lenient parser takes
        // either form, and the writer stores the canonical `Z`.
        let text = text.trim_end_matches("+00:00");
        return Ok(Value::DateTime(
            DateTime::parse_lenient(&format!("{text}Z")).py()?,
        ));
    }
    if object.is_instance(&types.date)? {
        let year: u16 = object.getattr("year")?.extract()?;
        let month: u8 = object.getattr("month")?.extract()?;
        let day: u8 = object.getattr("day")?.extract()?;
        return Ok(Value::Date(Date::new(year, month, day).py()?));
    }
    Err(PyTypeError::new_err(format!(
        "cannot store a {} in a GeoPackage column",
        object.get_type().name()?
    )))
}

/// Converts a Python sequence of values.
pub(crate) fn py_to_values(objects: &Bound<'_, PyAny>) -> PyGpkgResult<Vec<Value>> {
    objects.try_iter()?.map(|o| py_to_value(&o?)).collect()
}

/// Quotes an SQL identifier.
pub(crate) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
