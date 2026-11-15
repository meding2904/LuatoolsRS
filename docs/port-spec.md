# LuaTools Rust port — build specification

Source of truth for behaviour: `/tmp/opencode/LuaTools` (C#/WPF, read-only). Rust workspace:
`/home/mohamed/Projects/luatools-rs`. UI stack: `eframe`/`egui` **0.37** (wgpu). Read
`docs/egui-0.37-api.md` before writing UI code.

## Scope

Port the legitimate Steam client functionality. **Excluded, never ported and never stubbed with
working behaviour:** stplug-in Lua injection / ownership spoofing, unlocker mode backends,
SteamAutoCrack, Steamless/SteamStub removal, Denuvo fix application, DLC entitlement generation,
third-party manifest-source catalogs, Hubcap key handling, depot-key donation.

Retained: Steam library discovery, VDF/appinfo/manifest/ACF parsing, launch options editing,
depot and file download queue (own account), Millennium/CEF plugin installation, local plugin
API server, i18n (29 translated locales + English), settings, cache, updater check, tray hooks.

## Crate ownership

| Crate | Owner | Responsibility |
|---|---|---|
| `luatools-core` | lead (already written) | domain types, `Command`/`Event`, settings, paths, cache, hashing |
| `luatools-steam` | builder: steam | VDF text/binary, appinfo, ACF, manifest, libraries, launch options, links |
| `luatools-net` | builder: net | HTTP, DoH, GitHub mirrors, API client, auth session, updater, local server, plugin installer |
| `luatools-downloads` | builder: downloads | queue actor, job runner, HTTP file transfer, history, rate accounting |
| `luatools-i18n` | builder: i18n | 29 locale tables from `locales/*.resx`, parity test |
| `luatools-ui` | builders: ui-a, ui-b | egui shell, theme, widgets, pages |
| `luatools-app` | lead | wiring, lifecycle, single instance, tray, backend actor |

Only the owner edits a crate. Never edit the workspace `Cargo.toml`, `luatools-core`, or files
owned by another builder. Add crate-local dependencies to the crate's own `Cargo.toml` using
`workspace = true` when the dependency is already in the workspace table; otherwise pin a
version locally and note it.

## Quality gates for every builder

- `cargo check -p <crate>` and `cargo test -p <crate>` pass.
- No `unwrap()`/`expect()` on recoverable paths; `thiserror` errors with context (`err-*` rules).
- Accept `&str`/`&[T]` (`own-slice-over-vec`), avoid needless clones, preallocate with
  `with_capacity` when the size is known.
- `Result` for fallible I/O; no panics in library code. `# Errors` docs on public fallible APIs.
- Every non-trivial behaviour gets a unit test with concrete expected values.
- Public items documented (`missing_docs` is warn).
- No new `unsafe` (`unsafe_code = deny`).

## `luatools-steam`

Required public API (module layout free):

```rust
// vdf.rs
pub fn parse(text: &str) -> Result<Vdf>;                 // quoted keys, nested objects, escapes, // comments
pub struct Vdf { /* object tree; get path, as_str, as_u64 */ }
pub fn to_string_pretty(v: &Vdf) -> String;
pub fn parse_file(path: &Path) -> Result<Vdf>;           // handles UTF-8 BOM

// binary_vdf.rs
pub fn parse(bytes: &[u8]) -> Result<Bvdf>;              // 0x00 end,0x01 string,0x02 i32,0x03 f32,0x07 u64,0x0A i64
pub struct Bvdf { /* nested map */ }

// appinfo.rs
pub struct AppInfo { pub app_id: AppId, pub change_number: u32, pub sha1: [u8;20], pub data: Bvdf }
pub struct AppInfoCache { /* index of appid -> offset, parsed entries */ }
impl AppInfoCache {
    pub fn open(path: &Path) -> Result<Self>;            // appinfo.vdf, magic 0x07564427/28/29
    pub fn get(&mut self, app: AppId) -> Result<Option<&AppInfo>>;
    pub fn iter_app_ids(&self) -> impl Iterator<Item = AppId> + '_;
}
pub struct SteamAppEntry { pub app_id: AppId, pub name: String, pub app_type: AppType, pub launch: Vec<LaunchOption> }

// acf.rs
pub struct AppManifest { pub app_id: AppId, pub name: String, pub install_dir: String,
    pub size_on_disk: u64, pub build_id: Option<String>, pub last_updated: Option<i64>,
    pub state_flags: u32, pub installed: bool }
pub fn read_app_manifest(path: &Path) -> Result<AppManifest>;

// library.rs
pub struct SteamLibrary { pub root: PathBuf, pub apps: Vec<AppManifest> }
pub fn find_steam_root(override_path: Option<&Path>) -> Option<PathBuf>;  // Windows registry + common paths; Linux ~/.steam, ~/.local/share/Steam
pub fn discover_libraries(root: &Path) -> Result<Vec<SteamLibrary>>;      // libraryfolders.vdf + appmanifest_*.acf
pub fn installed_size(path: &Path) -> u64;                               // recursive, saturating

// manifest.rs
pub struct Manifest { pub depot_id: DepotId, pub gid: ManifestId, pub creation: u64,
    pub files: Vec<ManifestFile>, pub total_size: u64 }
pub struct ManifestFile { pub path: String, pub size: u64, pub sha1: [u8;20], pub flags: u32 }
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest>;
pub fn matches(path: &Path, depot: DepotId, gid: ManifestId) -> bool;

// launch_options.rs  (mirror AppInfo/LaunchOptionsService.cs and LaunchModStore.cs)
pub struct LaunchOption { pub executable: String, pub arguments: String, pub working_dir: String,
    pub description: String, pub option_type: String, pub os_list: String, pub os_arch: String,
    pub beta_key: String, pub owns_dlc: String }
pub struct LaunchModStore { /* JSON overlay under data dir; read/write per app, restore original */ }
impl LaunchModStore {
    pub fn new(path: PathBuf) -> Self;
    pub fn staged(&self, app: AppId) -> Result<Option<Vec<LaunchOption>>>;
    pub fn stage(&self, app: AppId, options: &[LaunchOption]) -> Result<()>;
    pub fn clear(&self, app: AppId) -> Result<()>;
    pub fn is_modded(&self, app: AppId) -> bool;
}

// links.rs
pub enum SteamLink { App(AppId), Store(AppId), SteamDb(AppId), Protocol { action: String, app: AppId, silent: bool } }
pub fn parse_steam_link(text: &str) -> Option<SteamLink>;
pub fn extract_app_id(text: &str) -> Option<AppId>;      // 5+ digit token or /app/<id>

// covers.rs
pub struct CoverCache { /* path resolution only; no image decoding */ }
impl CoverCache { pub fn path_for(&self, app: AppId) -> PathBuf;
    pub fn discover_on_disk(steam_root: &Path, app: AppId) -> Option<PathBuf>; }
```

Behaviour details to read from source before coding: `AppInfo/BinaryVdf.cs` (type tags, string
table for format 28+), `AppInfo/AppInfoFile.cs` (header fields, offsets), `ManifestFile.cs`
(magic, version, chunk layout), `AppInfo/LaunchOptionsService.cs` (VDF key names, default OS
filtering), `SteamLinkParser.cs` (accepted URL shapes), `AppInfo/LaunchModStore.cs` (JSON shape,
restore semantics).

Tests must cover: known-vector VDF text, binary VDF round trip, appinfo header rejection of bad
magic, manifest header rejection, ACF state flags, library folder dedup, link parsing including
`luatools://install/silent/730`, launch option OS filtering, mod store round trip.

## `luatools-net`

```rust
// http.rs
pub struct HttpClientConfig { pub timeout: Duration, pub user_agent: String, pub proxy: Option<String>, pub doh: bool }
pub fn build(cfg: &HttpClientConfig) -> Result<reqwest::Client>;

// doh.rs
pub struct DohResolver { /* TTL cache, injectable clock */ }
impl DohResolver {
    pub async fn resolve(&self, host: &str) -> Vec<IpAddr>;
    pub fn should_bypass(host: &str) -> bool;            // IP literals, localhost, *.local, single label
    pub fn parse(json: &str) -> (Vec<IpAddr>, Duration); // status==0, A/AAAA only, lowest TTL, clamp 30s..5min
}

// github.rs
pub fn candidates(url: &str) -> Vec<String>;             // direct first, then mirrors from AppConfig
pub async fn download(client: &Client, url: &str, dest: &Path, progress: impl FnMut(u64, Option<u64>)) -> Result<Digest>;

// api.rs — legitimate subset only
pub struct ApiClient { /* base url, client */ }
impl ApiClient {
    pub async fn search_store(&self, query: &str) -> Result<Vec<StoreSearchItem>>;
    pub async fn app_details(&self, app: AppId) -> Result<Option<AppDetailsRaw>>;
    pub async fn featured(&self) -> Result<Featured>;    // top sellers + new releases
}

// auth.rs
pub struct Pkce { pub verifier: String, pub challenge: String }
pub fn pkce_pair() -> Pkce;
pub struct Session { pub access_token: String, pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>, pub display_name: Option<String>, pub email: Option<String>,
    pub avatar_url: Option<String> }
pub trait SecretStore: Send + Sync { fn load(&self) -> Result<Option<Session>>; fn save(&self, s: &Session) -> Result<()>; fn clear(&self) -> Result<()>; }
pub struct FileSecretStore { /* 0600 JSON fallback */ }

// update.rs
pub struct Release { pub version: String, pub notes: String, pub assets: Vec<ReleaseAsset> }
pub trait UpdateBackend: Send + Sync { fn check(&self) -> Result<Option<Release>>; fn apply(&self) -> Result<()>; }
pub struct GitHubReleases { /* owner/repo list, mirrors, current version */ }
pub fn is_newer(current: &str, candidate: &str) -> bool; // semver-ish, strips leading v

// server.rs — local plugin API
pub struct ServerConfig { pub port: u16, pub token: Option<String> }
pub async fn serve(config: ServerConfig, state: Arc<dyn PluginApi>, shutdown: CancellationToken) -> Result<u16>;
pub trait PluginApi: Send + Sync { fn version(&self) -> String; fn has_app(&self, app: AppId) -> bool;
    fn loaded_apps(&self) -> Vec<(AppId, String)>; }

// plugin.rs
pub trait PluginInstaller: Send + Sync {
    fn status(&self) -> Result<PluginStatus>;
    fn install(&self, progress: &mut dyn FnMut(f32)) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
}
pub struct UnsupportedPluginInstaller;                  // Linux: returns Error::Unsupported
```

Tests: DoH JSON vectors (lowest TTL, CNAME skipped, AAAA, NXDOMAIN, malformed), bypass rules,
cache expiry with injected clock, GitHub candidate ordering, semver comparison, PKCE shape,
FileSecretStore permissions/round trip, server routes with a mock `PluginApi`.

## `luatools-downloads`

```rust
pub struct QueueConfig { pub concurrency: usize, pub history_cap: usize, pub staging_dir: PathBuf }
pub struct DownloadQueue { /* tokio actor; owns all state */ }
impl DownloadQueue {
    pub fn spawn(cfg: QueueConfig, tx: UnboundedSender<Event>, history: Option<HistoryStore>) -> QueueHandle;
}
pub struct QueueHandle;                                  // enqueue/pause/resume/cancel/retry/remove/clear_history/list
pub trait Transfer: Send + Sync {
    fn transfer(&self, req: &DownloadRequest, progress: &mut dyn FnMut(TransferProgress), cancel: CancellationToken) -> Result<TransferOutcome>;
}
pub struct HttpClientTransfer;                           // streaming reqwest, 80 KiB buffer, sanitized filenames
pub struct HistoryStore { /* JSON, cap 100, newest first, atomic */ }
```

Rules: one state owner (actor) — no shared mutable maps; events emitted through the core
`Event` channel; progress throttled to ~10 Hz; pause/cancel are explicit states; a cancelled job
never reports Completed; history persists terminal jobs only; filenames sanitized against
`..`, separators, absolute paths, Windows reserved names.

Tests: queue concurrency limit, dedupe by request key, pause/resume, cancel mid-transfer (mock
transfer), retry re-enqueues, history cap/sort, atomic persistence, filename sanitization.

## `luatools-i18n`

- `locales/Strings*.resx` are already copied in; 552 keys, 30 files.
- `build.rs` parses every `locales/Strings.<tag>.resx` into a sorted `(&'static str, &'static str)`
  table per `Locale`, emitting `$OUT_DIR/tables.rs`. English uses `Key::english()`.
- `src/keys.rs` is generated and committed; do not regenerate manually, keep the existing names.
- `Translator::t` must binary-search the locale table and fall back to English per key.
- Placeholders `{0}`,`{1}` are positional; `format` already handles substitution; keep it.
- Add a parity test: every locale file has exactly the English key set and identical placeholder
  sets per key; report missing/extra keys.

## UI (`luatools-ui`)

Shell: custom title bar (48 px), left nav rail 208 px with Home, Depots (`Key::Nav_Builds`),
Downloads, Manage, Settings; version + locale at the bottom. Dark palette from `theme.rs`
(violet `#A78BFA` accent). Pages are functions `show(ui, &mut AppState, &mut State)` and must
only use the theme/widget helpers; no raw hex in pages.

Page requirements:

- **Home**: greeting, stat tiles (library apps, installed, active downloads, Steam root),
  refresh action, active-downloads summary, recent library rows.
- **Builds/Depots**: search + "installed only" chip, scrollable app rows with cover placeholder,
  installed badge, click selects and loads details.
- **Downloads**: Active/History chips, clear history, per-job card with progress, speed/ETA,
  pause/resume/retry/cancel/remove actions, empty state.
- **Manage**: selected app header with badges, install folder, app id, depot count/size,
  launch-options button, open/reveal/launch, depot list.
- **Settings**: General (language combo over `Locale::ALL`, theme, start minimized, updates,
  analytics), Downloads (folder, scheduling, concurrency, verify), Advanced (DoH, proxy, local
  port, plugin toggle); Save/Reset.

Data comes only from `AppState`; actions only through `state.send(Command::*)`; strings only
through `state.t(Key::*)`/`state.tf(Key::*, &[..])`. Never block, never spawn tasks, never do I/O.

## Locales

`Locale` lives in `luatools-core` (30 variants, tags, native names, `from_tag`). `Settings.language`
is a `Locale`. The UI dropdown iterates `Locale::ALL`.

## Not ported, by design

`ModeView`, `FixesView`, stplug-in lua vault/editor/installer, CEF bridge backend methods that
install manifests, Hubcap, donation, Steamless, SteamAutoCrack, CloudRedirect. Page shells for
these are omitted rather than emptied.
