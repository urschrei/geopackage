//! SQL identifier quoting for SQLite.

use crate::Error;

/// Quotes an identifier for safe interpolation into SQLite DDL or DML.
///
/// Uses standard SQL double-quoting, doubling embedded quotes.
///
/// # Errors
///
/// Returns [`Error::InvalidIdentifier`] if the identifier is empty or contains
/// NUL, which SQLite cannot represent.
///
/// # Examples
///
/// ```
/// use geopackage_core::ident::quote;
///
/// assert_eq!(quote("roads")?, "\"roads\"");
/// assert_eq!(quote("we\"ird")?, "\"we\"\"ird\"");
/// assert!(quote("").is_err());
/// # Ok::<(), geopackage_core::Error>(())
/// ```
pub fn quote(ident: &str) -> Result<String, Error> {
    if ident.is_empty() || ident.contains('\0') {
        return Err(Error::InvalidIdentifier(ident.to_owned()));
    }
    let mut out = String::with_capacity(ident.len() + 2);
    out.push('"');
    for c in ident.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote("roads").unwrap(), "\"roads\"");
        assert_eq!(quote("we\"ird").unwrap(), "\"we\"\"ird\"");
        quote("").unwrap_err();
        quote("a\0b").unwrap_err();
    }
}
