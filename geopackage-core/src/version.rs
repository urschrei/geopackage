//! GeoPackage file identity: SQLite `application_id` and `user_version` pragmas.

use std::fmt;

/// `application_id` for GeoPackage 1.2+: "GPKG".
pub const APPLICATION_ID_GPKG: u32 = 0x4750_4B47;
/// Legacy `application_id` for GeoPackage 1.0: "GP10".
pub const APPLICATION_ID_GP10: u32 = 0x4750_3130;
/// Legacy `application_id` for GeoPackage 1.1: "GP11".
pub const APPLICATION_ID_GP11: u32 = 0x4750_3131;

/// GeoPackage specification version, as declared by a file's pragmas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum GpkgVersion {
    /// GeoPackage 1.0 (`application_id` "GP10").
    V1_0,
    /// GeoPackage 1.1 (`application_id` "GP11").
    V1_1,
    /// GeoPackage 1.2.x (`user_version` 102xx).
    V1_2,
    /// GeoPackage 1.3.x (`user_version` 103xx).
    V1_3,
    /// GeoPackage 1.4.x (`user_version` 104xx).
    V1_4,
}

impl GpkgVersion {
    /// Classifies a file from its `application_id` and `user_version` pragmas.
    ///
    /// Returns `None` if the pragmas do not identify a GeoPackage.
    /// `user_version` values above the newest known 104xx range map to
    /// `V1_4` (per spec, clients should attempt to read newer minor versions).
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::GpkgVersion;
    /// use geopackage_core::version::APPLICATION_ID_GPKG;
    ///
    /// let version = GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, 10400);
    /// assert_eq!(version, Some(GpkgVersion::V1_4));
    /// ```
    pub fn from_pragmas(application_id: u32, user_version: u32) -> Option<Self> {
        match application_id {
            APPLICATION_ID_GP10 => Some(Self::V1_0),
            APPLICATION_ID_GP11 => Some(Self::V1_1),
            APPLICATION_ID_GPKG => match user_version / 100 {
                102 => Some(Self::V1_2),
                103 => Some(Self::V1_3),
                v if v >= 104 => Some(Self::V1_4),
                _ => None,
            },
            _ => None,
        }
    }

    /// Returns the `user_version` this crate writes for the given version
    /// (`None` for 1.0/1.1, which predate `user_version` use).
    pub fn user_version(self) -> Option<u32> {
        match self {
            Self::V1_0 | Self::V1_1 => None,
            Self::V1_2 => Some(10200),
            Self::V1_3 => Some(10300),
            Self::V1_4 => Some(10400),
        }
    }

    /// Returns the version as written in the specification, such as `"1.4"`.
    ///
    /// The minor-patch part is not included because the pragmas do not record
    /// it: a `user_version` of 10401 and one of 10400 both classify as
    /// [`Self::V1_4`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_0 => "1.0",
            Self::V1_1 => "1.1",
            Self::V1_2 => "1.2",
            Self::V1_3 => "1.3",
            Self::V1_4 => "1.4",
        }
    }
}

impl fmt::Display for GpkgVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(
            GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, 10400),
            Some(GpkgVersion::V1_4)
        );
        assert_eq!(
            GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, 10201),
            Some(GpkgVersion::V1_2)
        );
        // future minor versions read as newest-known
        assert_eq!(
            GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, 10500),
            Some(GpkgVersion::V1_4)
        );
        assert_eq!(
            GpkgVersion::from_pragmas(APPLICATION_ID_GP10, 0),
            Some(GpkgVersion::V1_0)
        );
        assert_eq!(GpkgVersion::from_pragmas(0x1234_5678, 10400), None);
        assert_eq!(GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, 0), None);
    }
}
