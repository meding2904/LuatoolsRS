//! Steam data formats and local library services.
//!
//! The crate is deliberately free of UI, network, and platform-shell code:
//!
//! - [`vdf`] — Valve text KeyValues (`libraryfolders.vdf`, `appmanifest_*.acf`),
//! - [`binary_vdf`] — the binary KeyValues blob inside `appinfo.vdf`,
//! - [`appinfo`] — `appcache/appinfo.vdf` indexing and record parsing,
//! - [`acf`] — app manifest fields and install paths,
//! - [`library`] — Steam discovery, library enumeration, recursive sizes,
//! - [`manifest`] — depot `.manifest` sections and file mappings,
//! - [`launch_options`] — Play-button entries plus the durable edit overlay,
//! - [`links`] — SteamDB/store/community/`steam://`/`luatools://` link parsing,
//! - [`covers`] — cover path resolution.
//!
//! Everything that can fail returns [`luatools_core::Result`] with a contextual error; no
//! function panics on malformed input.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![deny(unsafe_code)]

pub mod acf;
pub mod appinfo;
pub mod binary_vdf;
pub mod covers;
pub mod launch_options;
pub mod library;
pub mod links;
pub mod manifest;
pub mod vdf;

pub use acf::{AppManifest, STATE_FULLY_INSTALLED, parse_app_manifest, read_app_manifest};
pub use appinfo::{AppInfo, AppInfoCache, AppType, SteamAppEntry};
pub use binary_vdf::{Bvdf, BvdfProperty, BvdfValue, parse as parse_binary_vdf};
pub use covers::CoverCache;
pub use launch_options::{
    LaunchMod, LaunchModStore, LaunchOption, filter_for_os, read_launch_options,
};
pub use library::{SteamLibrary, discover_libraries, find_steam_root, installed_size};
pub use links::{MAX_APP_ID, SteamLink, extract_app_id, parse_steam_link};
pub use manifest::{Manifest, ManifestFile, matches, parse_manifest};
pub use vdf::{Vdf, VdfValue, parse as parse_vdf, parse_file, to_string_pretty};
