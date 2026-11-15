//! Strongly typed identifiers and the small value objects they carry.

use std::fmt;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $inner:ty) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        #[repr(transparent)]
        pub struct $name(pub $inner);

        impl $name {
            /// Wraps the raw value.
            #[must_use]
            pub const fn new(raw: $inner) -> Self {
                Self(raw)
            }

            /// Returns the raw value.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<$inner> for $name {
            fn from(raw: $inner) -> Self {
                Self(raw)
            }
        }
    };
}

id_type!(
    /// A Steam application id.
    AppId,
    u32
);
id_type!(
    /// A Steam depot id.
    DepotId,
    u32
);
id_type!(
    /// A Steam manifest id (a `u64` GID).
    ManifestId,
    u64
);
id_type!(
    /// Identifies one entry in the download queue.
    DownloadId,
    u64
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_round_trips_through_raw_value() {
        let id = AppId::new(730);
        assert_eq!(id.get(), 730);
        assert_eq!(id.to_string(), "730");
        assert_eq!(AppId::from(730), id);
    }

    #[test]
    fn manifest_id_keeps_full_gid_precision() {
        let raw = 7_649_880_572_861_247_491_u64;
        assert_eq!(ManifestId::from(raw).get(), raw);
    }
}
