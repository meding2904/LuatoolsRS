//! Depot, branch, and DLC metadata shared by the network client and the interface.
//!
//! The records mirror the public `api.steamcmd.net` view of an app's depots. They are display
//! metadata only: no depot key is stored, and nothing in this module can fetch depot content.

use serde::{Deserialize, Serialize};

use crate::ids::{AppId, DepotId, ManifestId};

/// One content depot declared by an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentDepot {
    /// Depot id.
    pub id: DepotId,
    /// Public manifest size in bytes, or zero when unknown.
    #[serde(default)]
    pub size: u64,
    /// DLC app that owns this depot, when the depot ships DLC files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dlc_app_id: Option<AppId>,
    /// True when the depot is a shared redistributable owned by another app.
    #[serde(default)]
    pub is_shared: bool,
    /// Declared OS list, e.g. `windows`, `linux,macos`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Declared depot language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Manifest currently shipped on the public branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_manifest_id: Option<ManifestId>,
    /// Owning app for a shared depot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_app_id: Option<AppId>,
}

impl ContentDepot {
    /// Returns true when this depot ships files for a DLC.
    #[must_use]
    pub const fn is_dlc(&self) -> bool {
        self.dlc_app_id.is_some()
    }
}

/// Depot metadata for one app.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppDepotInfo {
    /// App the metadata belongs to.
    pub app_id: AppId,
    /// Content depots declared by the app.
    #[serde(default)]
    pub depots: Vec<ContentDepot>,
    /// Full declared DLC list, including store-only entitlements with no depot.
    #[serde(default)]
    pub dlc_ids: Vec<AppId>,
    /// Main Windows launch executables declared by the app.
    #[serde(default)]
    pub launch_exes: Vec<String>,
    /// Current public branch build id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_build_id: Option<String>,
}

impl AppDepotInfo {
    /// Sum of every known depot size.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.depots.iter().map(|depot| depot.size).sum()
    }

    /// Returns the depot with the given id.
    #[must_use]
    pub fn depot(&self, id: DepotId) -> Option<&ContentDepot> {
        self.depots.iter().find(|depot| depot.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_ignore_unknown_sizes() {
        let info = AppDepotInfo {
            app_id: AppId::new(730),
            depots: vec![
                ContentDepot {
                    id: DepotId::new(731),
                    size: 10,
                    dlc_app_id: None,
                    is_shared: false,
                    os: None,
                    language: None,
                    public_manifest_id: None,
                    from_app_id: None,
                },
                ContentDepot {
                    id: DepotId::new(732),
                    size: 0,
                    dlc_app_id: None,
                    is_shared: false,
                    os: None,
                    language: None,
                    public_manifest_id: None,
                    from_app_id: None,
                },
            ],
            ..AppDepotInfo::default()
        };
        assert_eq!(info.total_size(), 10);
        assert!(info.depot(DepotId::new(732)).is_some());
    }
}
