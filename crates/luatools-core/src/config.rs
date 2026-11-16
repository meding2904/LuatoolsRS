//! Compile-time service endpoints and public client constants.
//!
//! These are the non-secret values the desktop client uses. Authentication credentials are not
//! compiled in; the sign-in flow reads its endpoint from settings or environment variables.

/// Public Steam store origin.
pub const STEAM_STORE_BASE: &str = "https://store.steampowered.com";

/// Public Steam store search endpoint.
pub const STEAM_STORE_SEARCH_URL: &str = "https://store.steampowered.com/api/storesearch/";

/// Public Steam featured-categories endpoint.
pub const STEAM_FEATURED_URL: &str = "https://store.steampowered.com/api/featuredcategories";

/// Public Steam appdetails endpoint.
pub const STEAM_APPDETAILS_URL: &str = "https://store.steampowered.com/api/appdetails";

/// Public SteamCMD depot metadata endpoint, used as `{base}/{appid}`.
pub const STEAMCMD_INFO_BASE: &str = "https://api.steamcmd.net/v1/info";

/// Steam hardware/accessory appid blacklist.
pub const HARDWARE_APPID_LIST_URL: &str =
    "https://raw.githubusercontent.com/jsnli/steamappidlist/master/data/hardware_appid.json";

/// Primary update feed repository.
pub const UPDATE_REPOS: [&str; 1] = ["meding2904/LuatoolsRS"];

/// Steam store plugin release owner.
pub const PLUGIN_RELEASES_OWNER: &str = "madoiscool";

/// Steam store plugin release repository.
pub const PLUGIN_RELEASES_REPO: &str = "LTSP";

/// GitHub API mirrors, tried after the direct URL.
pub const GITHUB_API_MIRRORS: [&str; 1] = ["https://lua.tools/api/gh/"];

/// GitHub release-download mirrors, tried after the direct URL.
pub const GITHUB_DOWNLOAD_MIRRORS: [&str; 3] = [
    "https://ghproxy.net/",
    "https://ghfast.top/",
    "https://gh.ddlc.top/",
];

/// User agent sent by LuaTools HTTP requests.
pub const USER_AGENT: &str = concat!("LuaTools/", env!("CARGO_PKG_VERSION"));

/// First-party app list source used to name apps that are not installed.
pub const APP_LIST_URL_PRIMARY: &str = "https://applist.morrenus.xyz/";

/// Steam's own app-list fallback.
pub const APP_LIST_URL_STEAM: &str = "https://api.steampowered.com/ISteamApps/GetAppList/v2/";

/// Self-hosted Umami analytics origin.
pub const UMAMI_HOST: &str = "https://analytics.lua.tools";

/// Umami website id for the desktop client.
pub const UMAMI_WEBSITE_ID: &str = "820d782c-a434-424f-9f90-dee83dc6032e";

/// Hostname override shown in the analytics dashboard.
pub const UMAMI_HOSTNAME: &str = "desktop.lua.tools";
