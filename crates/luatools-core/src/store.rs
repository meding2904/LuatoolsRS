//! Store catalogue types shared by the network client, backend, and interface.
//!
//! These are public, read-only metadata records. They deliberately carry no entitlement or
//! purchase state; installing an app still happens through Steam's own protocol handler.

use serde::{Deserialize, Serialize};

use crate::ids::AppId;

/// One store search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSearchItem {
    /// Store app id.
    pub app_id: AppId,
    /// Display name.
    pub name: String,
    /// Small capsule image, falling back to the conventional CDN URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// One featured-store entry (top seller or new release).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeaturedItem {
    /// Store app id.
    pub app_id: AppId,
    /// Display name.
    pub name: String,
    /// Wide capsule image.
    pub capsule_image: String,
}

/// The two featured categories the store page renders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Featured {
    /// Best-selling games.
    #[serde(default)]
    pub top_sellers: Vec<FeaturedItem>,
    /// Newly released games.
    #[serde(default)]
    pub new_releases: Vec<FeaturedItem>,
}

impl Featured {
    /// Returns true when neither strip has any entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.top_sellers.is_empty() && self.new_releases.is_empty()
    }
}

/// Parsed store metadata for one app.
///
/// Every field is optional because Steam returns partial data for unreleased, delisted, or
/// region-restricted products. The record is display-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMetadata {
    /// Store name, when Steam supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Store product type: `game`, `dlc`, `demo`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_type: Option<String>,
    /// For DLC, the parent game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_app_id: Option<AppId>,
    /// Wide header image URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_image: Option<String>,
    /// Raw release-date text, e.g. `21 Aug, 2012`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// Parsed release year, used by the year filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_year: Option<i32>,
    /// Developer names in Steam's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub developers: Vec<String>,
    /// Publisher names in Steam's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub publishers: Vec<String>,
    /// Genre descriptions in Steam's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Short store description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    /// Steam's Metacritic score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metacritic: Option<i32>,
    /// Total recommendation count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviews: Option<i64>,
    /// Whether the app is free to play.
    #[serde(default)]
    pub is_free: bool,
    /// Whether the app is marked adult-only by Steam.
    #[serde(default)]
    pub is_adult: bool,
    /// Windows support.
    #[serde(default)]
    pub windows: bool,
    /// macOS support.
    #[serde(default)]
    pub mac: bool,
    /// Linux support.
    #[serde(default)]
    pub linux: bool,
}

impl AppMetadata {
    /// Returns the developer label joined with commas.
    #[must_use]
    pub fn developer_label(&self) -> String {
        self.developers.join(", ")
    }

    /// Returns the publisher label joined with commas.
    #[must_use]
    pub fn publisher_label(&self) -> String {
        self.publishers.join(", ")
    }

    /// Returns true when the publisher is known and differs from the developer.
    #[must_use]
    pub fn has_distinct_publisher(&self) -> bool {
        !self.publishers.is_empty()
            && !self
                .publisher_label()
                .eq_ignore_ascii_case(&self.developer_label())
    }

    /// Returns true when at least one display detail is available.
    #[must_use]
    pub fn has_anything(&self) -> bool {
        self.short_description
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
            || !self.developers.is_empty()
            || !self.genres.is_empty()
    }

    /// Returns true when this record describes a DLC.
    #[must_use]
    pub fn is_dlc(&self) -> bool {
        self.app_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("dlc"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn featured_empty_detection_checks_both_strips() {
        let mut featured = Featured::default();
        assert!(featured.is_empty());
        featured.top_sellers.push(FeaturedItem {
            app_id: AppId::new(1),
            name: "A".to_owned(),
            capsule_image: "a.jpg".to_owned(),
        });
        assert!(!featured.is_empty());
    }

    #[test]
    fn publisher_is_hidden_when_it_matches_the_developer() {
        let metadata = AppMetadata {
            developers: vec!["Valve".to_owned()],
            publishers: vec!["valve".to_owned()],
            ..AppMetadata::default()
        };
        assert_eq!(metadata.developer_label(), "Valve");
        assert!(!metadata.has_distinct_publisher());
    }

    #[test]
    fn display_detail_requires_a_non_empty_description_or_names() {
        let empty = AppMetadata::default();
        assert!(!empty.has_anything());
        let described = AppMetadata {
            short_description: Some("A game".to_owned()),
            ..AppMetadata::default()
        };
        assert!(described.has_anything());
    }
}
