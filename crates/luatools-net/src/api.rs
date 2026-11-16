//! A small client for Steam's public store metadata endpoints.
//!
//! Only store search, app details, and featured categories are implemented. Those are public,
//! read-only catalogue endpoints; anything that touches entitlements or downloads belongs to a
//! different subsystem and is deliberately absent.

use luatools_core::{AppId, Error, Result};
pub use luatools_core::{Featured, FeaturedItem, StoreSearchItem};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

/// The public Steam store origin.
pub const STEAM_STORE_BASE: &str = "https://store.steampowered.com";

/// Maximum number of search results returned by [`ApiClient::search_store`].
pub const MAX_SEARCH_RESULTS: usize = 8;

/// Maximum number of entries kept in each featured category.
pub const MAX_FEATURED_ITEMS: usize = 20;

/// The raw `data` object from Steam's appdetails response.
///
/// Steam sometimes keys the response by a child app id rather than the requested one, so the
/// parser anchors on `data.steam_appid`. Keeping the whole object lets callers read fields that
/// do not have a typed accessor yet without another request.
#[derive(Debug, Clone)]
pub struct AppDetailsRaw {
    /// The app id that was requested and matched by `data.steam_appid`.
    pub app_id: AppId,
    /// The raw `data` object.
    pub data: Value,
}

impl AppDetailsRaw {
    /// Returns a top-level string field.
    #[must_use]
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(Value::as_str)
    }

    /// The game's display name.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.str_field("name")
    }

    /// Steam's product type, e.g. `game` or `dlc`.
    #[must_use]
    pub fn app_type(&self) -> Option<&str> {
        self.str_field("type")
    }

    /// True when the product type is `dlc`.
    #[must_use]
    pub fn is_dlc(&self) -> bool {
        self.app_type()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("dlc"))
    }

    /// The wide header image URL.
    #[must_use]
    pub fn header_image(&self) -> Option<&str> {
        self.str_field("header_image")
    }

    /// The release date as Steam renders it. Some unreleased apps return an empty string.
    #[must_use]
    pub fn release_date(&self) -> Option<&str> {
        self.data
            .get("release_date")
            .and_then(|release| release.get("date"))
            .and_then(Value::as_str)
    }

    /// Genre descriptions in Steam's order.
    #[must_use]
    pub fn genres(&self) -> Vec<String> {
        self.data
            .get("genres")
            .and_then(Value::as_array)
            .map(|genres| {
                genres
                    .iter()
                    .filter_map(|genre| genre.get("description").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// For DLC, the parent game's app id from the `fullgame` object.
    #[must_use]
    pub fn base_app_id(&self) -> Option<AppId> {
        let value = self.data.get("fullgame")?.get("appid")?;
        match value {
            Value::Number(number) => number
                .as_u64()
                .and_then(|raw| u32::try_from(raw).ok())
                .map(AppId::from),
            Value::String(text) => text.parse::<u32>().ok().map(AppId::from),
            _ => None,
        }
    }

    /// Parses the display metadata used by the library and store pages.
    #[must_use]
    pub fn metadata(&self) -> luatools_core::AppMetadata {
        let release_date = self.release_date().map(str::to_owned);
        let release_year = release_date.as_deref().and_then(release_year);
        let platforms = self.data.get("platforms");
        let content_ids = self
            .data
            .get("content_descriptors")
            .and_then(|descriptors| descriptors.get("ids"))
            .and_then(Value::as_array);
        let is_adult = content_ids.is_some_and(|ids| {
            ids.iter()
                .any(|id| id.as_i64().is_some_and(|id| id == 3 || id == 4))
        });
        luatools_core::AppMetadata {
            name: self.name().map(str::to_owned),
            app_type: self.app_type().map(str::to_owned),
            base_app_id: self.base_app_id(),
            header_image: self.header_image().map(str::to_owned),
            release_date,
            release_year,
            developers: string_array(&self.data, "developers"),
            publishers: string_array(&self.data, "publishers"),
            genres: self.genres(),
            short_description: self.str_field("short_description").map(str::to_owned),
            metacritic: self
                .data
                .get("metacritic")
                .and_then(|value| value.get("score"))
                .and_then(Value::as_i64)
                .and_then(|score| i32::try_from(score).ok()),
            reviews: self
                .data
                .get("recommendations")
                .and_then(|value| value.get("total"))
                .and_then(Value::as_i64),
            is_free: self
                .data
                .get("is_free")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_adult,
            windows: platforms
                .and_then(|value| value.get("windows"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            mac: platforms
                .and_then(|value| value.get("mac"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            linux: platforms
                .and_then(|value| value.get("linux"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

fn release_year(text: &str) -> Option<i32> {
    text.split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| part.len() == 4)
        .find_map(|part| part.parse::<i32>().ok())
        .filter(|year| (1900..=2100).contains(year))
}

fn string_array(data: &Value, key: &str) -> Vec<String> {
    data.get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A client bound to one store origin.
#[derive(Debug, Clone)]
pub struct ApiClient {
    client: Client,
    base: String,
}

impl ApiClient {
    /// Builds a client against [`STEAM_STORE_BASE`].
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client,
            base: STEAM_STORE_BASE.trim_end_matches('/').to_owned(),
        }
    }

    /// Builds a client against another origin, for a mirror or a test double.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when `base` is not an absolute HTTP(S) URL.
    pub fn with_base_url(client: Client, base: impl AsRef<str>) -> Result<Self> {
        let parsed = url::Url::parse(base.as_ref())
            .map_err(|error| Error::InvalidInput(format!("api base url: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::InvalidInput(
                "api base url must be http or https".to_owned(),
            ));
        }
        Ok(Self {
            client,
            base: parsed.as_str().trim_end_matches('/').to_owned(),
        })
    }

    /// The origin requests are built against.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Searches the store for `query`, returning at most [`MAX_SEARCH_RESULTS`] usable hits.
    ///
    /// A blank query returns an empty list without a request.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Network`] for transport failures, [`Error::Http`] for an unsuccessful
    /// status, and [`Error::Parse`] when the body is not the expected JSON.
    pub async fn search_store(&self, query: &str) -> Result<Vec<StoreSearchItem>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        let url = format!(
            "{}/api/storesearch/?term={encoded}&l=english&cc=US",
            self.base
        );
        let body = self.get_text(&url).await?;
        parse_store_search(&body)
    }

    /// Fetches details for one app, or `Ok(None)` when Steam has no data for it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Network`] for transport failures, [`Error::Http`] for an unsuccessful
    /// status, and [`Error::Parse`] when the body is not the expected JSON.
    pub async fn app_details(&self, app: AppId) -> Result<Option<AppDetailsRaw>> {
        let url = format!(
            "{}/api/appdetails?appids={}&cc=us&l=english",
            self.base,
            app.get()
        );
        let body = self.get_text(&url).await?;
        parse_app_details(&body, app)
    }

    /// Fetches the top-seller and new-release strips.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Network`] for transport failures, [`Error::Http`] for an unsuccessful
    /// status, and [`Error::Parse`] when the body is not the expected JSON.
    pub async fn featured(&self) -> Result<Featured> {
        let url = format!("{}/api/featuredcategories?cc=us&l=english", self.base);
        let body = self.get_text(&url).await?;
        parse_featured(&body)
    }

    async fn get_text(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| Error::Network {
                context: format!("GET {url}"),
                message: error.to_string(),
            })?;
        if !response.status().is_success() {
            return Err(Error::Http {
                status: response.status().as_u16(),
                url: url.to_owned(),
            });
        }
        response.text().await.map_err(|error| Error::Network {
            context: format!("reading {url}"),
            message: error.to_string(),
        })
    }
}

/// Parses a store-search response.
///
/// Entries without a positive id or a name are dropped. A missing `tiny_image` falls back to the
/// small capsule URL on the Steam CDN.
///
/// # Errors
///
/// Returns [`Error::Parse`] when `json` is not a store-search object.
pub fn parse_store_search(json: &str) -> Result<Vec<StoreSearchItem>> {
    let response: StoreSearchResponse = serde_json::from_str(json)
        .map_err(|error| Error::parse("steam storesearch", error.to_string()))?;

    let mut items = Vec::with_capacity(response.items.len().min(MAX_SEARCH_RESULTS));
    for item in response.items {
        if items.len() == MAX_SEARCH_RESULTS {
            break;
        }
        let Some(raw_id) = item.id.filter(|id| *id > 0) else {
            continue;
        };
        let Some(name) = item
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let icon = item
            .tiny_image
            .filter(|image| !image.is_empty())
            .or_else(|| Some(tiny_capsule_url(raw_id)));
        items.push(StoreSearchItem {
            app_id: AppId::from(raw_id),
            name,
            icon,
        });
    }
    Ok(items)
}

/// Parses the two featured categories.
///
/// Keeps only real games (`type == 0`) that have a wide capsule image, removes duplicate app ids
/// while preserving Steam's order, and caps each list at [`MAX_FEATURED_ITEMS`].
///
/// # Errors
///
/// Returns [`Error::Parse`] when `json` is not a featuredcategories object.
pub fn parse_featured(json: &str) -> Result<Featured> {
    let response: FeaturedResponse = serde_json::from_str(json)
        .map_err(|error| Error::parse("steam featuredcategories", error.to_string()))?;
    Ok(Featured {
        top_sellers: clean_featured(response.top_sellers),
        new_releases: clean_featured(response.new_releases),
    })
}

/// Parses an appdetails response and anchors on the requested app id.
///
/// Steam may key the response by a child app id. The direct key is accepted only when its `data`
/// object names the requested app (or has no data object at all, which is the `success: false`
/// shape). Otherwise every entry is scanned for a matching `data.steam_appid`; an entry that
/// merely happens to be the only one is never accepted. Guessing would poison caches with another
/// product's metadata.
///
/// # Errors
///
/// Returns [`Error::Parse`] when `json` is not a JSON object.
pub fn parse_app_details(json: &str, app: AppId) -> Result<Option<AppDetailsRaw>> {
    let root: Value = serde_json::from_str(json)
        .map_err(|error| Error::parse("steam appdetails", error.to_string()))?;
    let Some(object) = root.as_object() else {
        return Err(Error::parse(
            "steam appdetails",
            "response is not an object",
        ));
    };

    let key = app.get().to_string();
    let selected = match object.get(&key) {
        Some(entry) => match entry.get("data") {
            // `success: false` carries no data object; Steam has nothing for this app.
            None => None,
            Some(data) if data.is_object() && data_names_app(data, app) => Some(entry),
            Some(_) => find_by_data(object, app),
        },
        None => find_by_data(object, app),
    };

    let Some(entry) = selected else {
        return Ok(None);
    };
    if entry.get("success").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let Some(data) = entry.get("data").filter(|data| data.is_object()) else {
        return Ok(None);
    };
    Ok(Some(AppDetailsRaw {
        app_id: app,
        data: data.clone(),
    }))
}

fn data_names_app(data: &Value, app: AppId) -> bool {
    data.get("steam_appid")
        .and_then(Value::as_u64)
        .and_then(|raw| u32::try_from(raw).ok())
        == Some(app.get())
}

fn find_by_data(object: &serde_json::Map<String, Value>, app: AppId) -> Option<&Value> {
    object.values().find(|entry| {
        entry
            .get("data")
            .filter(|data| data.is_object())
            .is_some_and(|data| data_names_app(data, app))
    })
}

fn clean_featured(category: Option<FeaturedCategory>) -> Vec<FeaturedItem> {
    let Some(category) = category else {
        return Vec::new();
    };
    let mut items = Vec::with_capacity(category.items.len().min(MAX_FEATURED_ITEMS));
    for item in category.items {
        if items.len() == MAX_FEATURED_ITEMS {
            break;
        }
        if item.item_type != 0 {
            continue;
        }
        let Some(raw_id) = item.id.filter(|id| *id > 0) else {
            continue;
        };
        let Some(capsule_image) = item.large_capsule_image.filter(|image| !image.is_empty()) else {
            continue;
        };
        if items
            .iter()
            .any(|existing: &FeaturedItem| existing.app_id.get() == raw_id)
        {
            continue;
        }
        let name = item.name.unwrap_or_default().trim().to_owned();
        if name.is_empty() {
            continue;
        }
        items.push(FeaturedItem {
            app_id: AppId::from(raw_id),
            name,
            capsule_image,
        });
    }
    items
}

fn tiny_capsule_url(app: u32) -> String {
    format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{app}/capsule_sm_120.jpg")
}

#[derive(Debug, Deserialize)]
struct StoreSearchResponse {
    #[serde(default)]
    items: Vec<StoreSearchRaw>,
}

#[derive(Debug, Deserialize)]
struct StoreSearchRaw {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tiny_image: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FeaturedResponse {
    #[serde(default)]
    top_sellers: Option<FeaturedCategory>,
    #[serde(default)]
    new_releases: Option<FeaturedCategory>,
}

#[derive(Debug, Default, Deserialize)]
struct FeaturedCategory {
    #[serde(default)]
    items: Vec<FeaturedRaw>,
}

#[derive(Debug, Deserialize)]
struct FeaturedRaw {
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    large_capsule_image: Option<String>,
    #[serde(default, rename = "type")]
    item_type: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_search_keeps_usable_entries_and_caps_the_list() {
        let mut items = String::new();
        for id in 1..=10 {
            if id > 1 {
                items.push(',');
            }
            items.push_str(&format!(
                r#"{{"id":{id},"name":"Game {id}","tiny_image":"https://cdn/{id}.jpg"}}"#
            ));
        }
        let json = format!(r#"{{"total":10,"items":[{items}]}}"#);
        let parsed = parse_store_search(&json).expect("valid search body");
        assert_eq!(parsed.len(), MAX_SEARCH_RESULTS);
        assert_eq!(parsed[0].app_id, AppId::from(1));
        assert_eq!(parsed[0].icon.as_deref(), Some("https://cdn/1.jpg"));
    }

    #[test]
    fn store_search_drops_blank_entries_and_fills_missing_icons() {
        let json = r#"{"items":[
            {"id":0,"name":"bad id","tiny_image":"x"},
            {"id":10,"name":"","tiny_image":"x"},
            {"id":20,"name":" Good ","tiny_image":""},
            {"id":30,"name":"No image"}
        ]}"#;
        let parsed = parse_store_search(json).expect("valid search body");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "Good");
        assert_eq!(
            parsed[0].icon.as_deref(),
            Some("https://cdn.cloudflare.steamstatic.com/steam/apps/20/capsule_sm_120.jpg")
        );
        assert_eq!(parsed[1].app_id, AppId::from(30));
    }

    #[test]
    fn store_search_rejects_malformed_json() {
        assert!(parse_store_search("not json").is_err());
    }

    #[test]
    fn featured_filters_duplicates_and_non_games() {
        let json = r#"{
            "top_sellers":{"items":[
                {"id":1,"name":"A","type":0,"large_capsule_image":"a.jpg"},
                {"id":2,"name":"Bundle","type":1,"large_capsule_image":"b.jpg"},
                {"id":3,"name":"No art","type":0},
                {"id":1,"name":"A duplicate","type":0,"large_capsule_image":"a2.jpg"},
                {"id":4,"name":"D","type":0,"large_capsule_image":"d.jpg"}
            ]},
            "new_releases":{"items":[
                {"id":5,"name":"E","type":0,"large_capsule_image":"e.jpg"}
            ]}
        }"#;
        let featured = parse_featured(json).expect("valid featured body");
        assert_eq!(
            featured
                .top_sellers
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "D"]
        );
        assert_eq!(featured.new_releases.len(), 1);
        assert_eq!(featured.new_releases[0].capsule_image, "e.jpg");
    }

    #[test]
    fn featured_missing_categories_are_empty() {
        let featured = parse_featured("{}").expect("empty object is valid");
        assert!(featured.top_sellers.is_empty());
        assert!(featured.new_releases.is_empty());
    }

    #[test]
    fn app_details_reads_the_direct_entry() {
        let json = r#"{"730":{"success":true,"data":{
            "steam_appid":730,
            "name":"Counter-Strike 2",
            "type":"game",
            "header_image":"https://cdn/730.jpg",
            "release_date":{"date":"21 Aug, 2012"},
            "genres":[{"description":"Action"},{"description":"Free To Play"}]
        }}}"#;
        let details = parse_app_details(json, AppId::from(730))
            .expect("valid body")
            .expect("app exists");
        assert_eq!(details.name(), Some("Counter-Strike 2"));
        assert_eq!(details.header_image(), Some("https://cdn/730.jpg"));
        assert_eq!(details.release_date(), Some("21 Aug, 2012"));
        assert_eq!(details.genres(), vec!["Action", "Free To Play"]);
        assert!(!details.is_dlc());
    }

    #[test]
    fn app_details_follows_a_child_keyed_response() {
        let json = r#"{"1660740":{"success":true,"data":{
            "steam_appid":500,
            "name":"Left 4 Dead",
            "type":"game"
        }}}"#;
        let details = parse_app_details(json, AppId::from(500))
            .expect("valid body")
            .expect("child-keyed entry must resolve");
        assert_eq!(details.app_id, AppId::from(500));
        assert_eq!(details.name(), Some("Left 4 Dead"));
    }

    #[test]
    fn app_details_refuses_a_mismatched_direct_entry() {
        let json = r#"{"500":{"success":true,"data":{
            "steam_appid":999,
            "name":"Wrong Game",
            "type":"game"
        }}}"#;
        assert!(
            parse_app_details(json, AppId::from(500))
                .expect("valid body")
                .is_none()
        );
    }

    #[test]
    fn app_details_prefers_matching_entry_in_a_child_keyed_object() {
        let json = r#"{
            "500":{"success":true,"data":{"steam_appid":999,"name":"Wrong","type":"game"}},
            "1660740":{"success":true,"data":{"steam_appid":500,"name":"Right","type":"game"}}
        }"#;
        let details = parse_app_details(json, AppId::from(500))
            .expect("valid body")
            .expect("matching entry");
        assert_eq!(details.name(), Some("Right"));
    }

    #[test]
    fn app_details_success_false_is_none() {
        let json = r#"{"999":{"success":false}}"#;
        assert!(
            parse_app_details(json, AppId::from(999))
                .expect("valid body")
                .is_none()
        );
    }

    #[test]
    fn app_details_rejects_malformed_json() {
        assert!(parse_app_details("nope", AppId::from(1)).is_err());
        assert!(parse_app_details("[]", AppId::from(1)).is_err());
    }

    #[test]
    fn dlc_details_expose_the_parent_app() {
        let json = r#"{"123":{"success":true,"data":{
            "steam_appid":123,
            "name":"A DLC",
            "type":"dlc",
            "fullgame":{"appid":"456"}
        }}}"#;
        let details = parse_app_details(json, AppId::from(123))
            .expect("valid body")
            .expect("dlc exists");
        assert!(details.is_dlc());
        assert_eq!(details.base_app_id(), Some(AppId::from(456)));
    }
}
