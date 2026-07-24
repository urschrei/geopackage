//! SQL identifier quoting for SQLite.

use crate::Error;

/// Quote an arbitrary identifier for safe interpolation into SQLite DDL/DML.
///
/// Uses standard SQL double-quoting, doubling embedded quotes. Rejects
/// identifiers containing NUL, which SQLite cannot represent.
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
