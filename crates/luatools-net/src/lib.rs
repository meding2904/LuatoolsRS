//! HTTP client, API client, auth, DoH resolver, updater, and local API server.
//!
//! This crate owns every outbound network concern that is not a bulk file transfer, plus the
//! loopback API consumed by the Steam store plugin:
//!
//! - [`http`] builds the shared [`reqwest::Client`] and optionally installs the DoH resolver.
//! - [`doh`] resolves names through a dns-json endpoint with a TTL cache.
//! - [`github`] retries GitHub requests through capability-matched mirrors and hashes downloads.
//! - [`api`] reads Steam's public store search, app details, and featured categories.
//! - [`auth`] supplies PKCE helpers, the session shape, and the file secret store.
//! - [`update`] compares versions and checks GitHub Releases.
//! - [`server`] serves the token-guarded loopback plugin API.
//! - [`plugin`] defines the store-plugin installer interface.
//!
//! The scope is deliberately limited to legitimate Steam client functionality. Manifest-source
//! catalogs, DRM circumvention, and entitlement generation are not implemented.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod api;
pub mod appdetails;
pub mod applist;
pub mod auth;
pub mod depot_info;
pub mod doh;
pub mod github;
pub mod hardware;
pub mod http;
pub mod plugin;
pub mod server;
pub mod update;

pub use api::{ApiClient, AppDetailsRaw, Featured, FeaturedItem, StoreSearchItem};
pub use appdetails::AppDetailsCache;
pub use applist::AppListCache;
pub use auth::{
    AuthClient, AuthConfig, FileSecretStore, Pkce, SecretStore, Session, code_challenge,
    parse_session, pkce_pair, try_pkce_pair,
};
pub use depot_info::SteamDepotInfo;
pub use doh::DohResolver;
pub use github::{GithubMirrors, candidates, download};
pub use hardware::HardwareAppIdService;
pub use http::{HttpClientConfig, build};
pub use plugin::{PluginInstaller, UnsupportedPluginInstaller};
pub use server::{PluginApi, ServerConfig, serve};
pub use update::{GitHubReleases, Release, ReleaseAsset, UpdateBackend, is_newer};
