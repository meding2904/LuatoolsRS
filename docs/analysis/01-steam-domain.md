# 01 — Steam domain services

Port specification for the Steam-facing services in the C# WPF app.

- Source snapshot: `/tmp/opencode/LuaTools` at commit `9461259` (2026-09-25).
- Source root: `src/LuaToolsGui/Services/`.
- Target: a Rust reimplementation that can be understood without reading the C#.
- This document specifies **format parsing, metadata lookup, caching, launch-option editing, and small display/hash helpers**.
- It deliberately does **not** specify the excluded Steam DRM / ownership-spoofing / crack features. See §18.

Legend:

- `Option<T>` = C# nullable reference/value (`T?`).
- `Vec<u8>` = C# `byte[]`.
- All C# `long`/`int`/`uint`/`ulong` map to `i64`/`i32`/`u32`/`u64`.
- "Ordinal" = C# `StringComparison.Ordinal` (byte/code-point comparison, case-sensitive).
- "OrdinalIgnoreCase" = C# `StringComparison.OrdinalIgnoreCase` (Unicode ordinal case folding, not culture-aware).
- "log-and-continue" means the C# catches and returns a neutral value; no exception reaches the caller.

Contents: §0 module map · §1 cross-cutting conventions · §2 SteamService · §3 SteamLibraryService · §4 BinaryVdf · §5 AppInfoFile · §6 LaunchOption · §7 LaunchModStore · §8 LaunchOptionsService · §9 ManifestFile · §10 SteamAppInfoCache · §11 SteamAppListCache · §12 SteamDepotInfo · §13 SteamLinkParser · §14 CoverCache · §15 HardwareAppIdService · §16 hashing · §17 ByteFormat · §18 exclusions · §19 ambiguities · §20 tests · §21 dependencies · §22 checklist · §23 quick reference.


---

## 0. Module map and recommended Rust layout

| C# file | Responsibility | Suggested Rust module |
|---|---|---|
| `SteamService.cs` | Steam install discovery, registry, paths, process control, shell/clipboard | `steam::paths` |
| `SteamLibraryService.cs` | Find installed games via `libraryfolders.vdf` + `appmanifest_*.acf` | `steam::library` |
| `AppInfo/BinaryVdf.cs` | Valve binary KeyValues codec | `steam::appinfo::binary_vdf` |
| `AppInfo/AppInfoFile.cs` | `appcache/appinfo.vdf` reader/writer | `steam::appinfo::file` |
| `AppInfo/LaunchOption.cs` | `config.launch.*` model and VDF mapping | `steam::appinfo::launch_option` |
| `AppInfo/LaunchModStore.cs` | Durable staged launch edits (`launchmods.json`) | `steam::appinfo::launch_store` |
| `AppInfo/LaunchOptionsService.cs` | Read/stage/apply/restore launch options | `steam::appinfo::launch_service` |
| `ManifestFile.cs` | Minimal Steam `.manifest` metadata reader + key sanity check | `steam::manifest` |
| `SteamAppInfoCache.cs` | appdetails fetch/cache, filters, overview, rate limiter | `steam::appinfo_cache` |
| `SteamAppListCache.cs` | Bulk appid → name list | `steam::applist_cache` |
| `SteamDepotInfo.cs` | Depot/branch/DLC/launch metadata from api.steamcmd.net | `steam::depot_info` |
| `SteamLinkParser.cs` | Extract appid from dragged/pasted links | `steam::link_parser` |
| `CoverCache.cs` | Download/cache Steam header images | `steam::cover_cache` |
| `HardwareAppIdService.cs` | Steam hardware appid blacklist | `steam::hardware_ids` |
| `FileHash.cs` | Soft-failing SHA-256 for fix records | `steam::hash::file_hash` |
| `AssetHash.cs` | SHA-256 for downloaded GitHub assets | `steam::hash::asset_hash` |
| `Downloads/ByteFormat.cs` | Size/rate/duration display strings | `downloads::byte_format` |

Dependency direction: `appinfo::launch_service` → `appinfo::launch_store` + `appinfo::file` + `appinfo::launch_option` + `steam::paths`. Nothing in this slice depends on the UI.

---

## 1. Cross-cutting conventions

### 1.1 Paths

The C# app writes all of its own state under:

```
%APPDATA%\LuaToolsGui\                 (Environment.SpecialFolder.ApplicationData = Roaming)
    cache.json                         (CacheService; shared app bookkeeping)
    settings.json                      (SettingsService; SteamPathOverride etc.)
    launchmods.json
    launchmods.json.tmp
    launchmods.json.bak
    launchmods.json.corrupt
    details\<appid>.json               (raw appdetails "data" object per app)
    steam-applist.json                 (appid -> name map)
    covers\<appid>.jpg
```

`%APPDATA%` is **Roaming**, not `%LOCALAPPDATA%`. On Windows the Rust port should use `dirs::data_dir()`/`SHGetKnownFolderPath(FOLDERID_RoamingAppData)`. On Linux, define a single platform data dir (C# would return `~/.config`); do not silently split it across XDG dirs.

Steam-side paths:

```
<Steam>\steam.exe
<Steam>\appcache\appinfo.vdf
<Steam>\depotcache\<depot>_<gid>.manifest        (real, Steam-visible)
<Steam>\config\depotcache\...                    (legacy, wrong, read-only fallback)
<Steam>\config\stplug-in\                        (SteamTools lua; consumer EXCLUDED, see §18)
<Steam>\steamapps\libraryfolders.vdf
<Steam>\steamapps\appmanifest_<appid>.acf
<Steam>\steamapps\common\<installdir>\
<Steam>\appcache\luatools-backup\appinfo-YYYYMMDD-HHMMSS.vdf   (next to appinfo.vdf)
```

All paths in the C# code are Windows paths (`\` separators, case-insensitive filesystem). The Rust port should keep a `PathBuf` abstraction and, if it ever runs on non-Windows, explicitly decide whether to emulate Windows case-insensitivity. Do not rely on Linux case sensitivity for Steam paths.

### 1.2 Text comparison and parsing

- C# `StringComparison.OrdinalIgnoreCase` is used for VDF keys, Steam language/OS values, dedupe, and path comparisons. In Rust, for ASCII keys use `eq_ignore_ascii_case`; for user-facing strings use a Unicode case-fold if exact C# parity matters.
- C# `long.TryParse`/`int.TryParse` use the **current culture** and `NumberStyles.Integer` (leading/trailing whitespace and a leading sign accepted). Rust `str::parse` is stricter. For Steam numeric strings, ASCII digits with optional whitespace are the real input; either trim before parsing or use a helper that mirrors C#'s permissiveness.
- JSON parsing in C# is `System.Text.Json` (`JsonDocument`). Property lookups are case-sensitive; numeric fields must be JSON numbers for `GetInt64`/`GetInt32`. `GetString()` on a non-string **throws** `InvalidOperationException`, which in several modules aborts the whole parse (see per-module notes).

### 1.3 Error philosophy

| Module | On malformed/unreadable input |
|---|---|
| `SteamService` | registry/process failures → null/false; shell open may throw |
| `SteamLibraryService` | swallow per file/library → null or skip |
| `BinaryVdf` / `AppInfoFile` | throw typed exceptions; caller decides |
| `LaunchOptionsService.Read` | propagates exceptions |
| `LaunchOptionsService.Apply` | catches all → `ApplyResult(false, message, wasRunning)` |
| `LaunchOptionsService.FindDrifted` | catches all → `[]` |
| `ManifestFile` | never throws; null/false/true as documented |
| `SteamAppInfoCache` | all IO/HTTP/parse failures → null/false |
| `SteamAppListCache` | all failures swallowed; stale fallback |
| `SteamDepotInfo` | all failures → cached null (60 s TTL) |
| `CoverCache` | all failures → null |
| `HardwareAppIdService` | all failures → keep stale set |
| `FileHash` | null on any failure |
| `AssetHash` | **throws** on IO failure |
| `ByteFormat` | no failure modes |

### 1.4 HTTP

All clients are `HttpClient` instances created by `AppHttp.Create(timeout)` except `SteamDepotInfo`, which accepts a handler/test seam. `AppHttp`:

- shares one `SocketsHttpHandler` (`PooledConnectionLifetime = 2 min`) across the app;
- installs a `ConnectCallback` that may resolve DNS via DoH (`DohResolver`) depending on a mode provider; TLS/SNI are performed against the original host name;
- does not set a default `User-Agent` or `Accept` header.

For this slice, a plain shared `reqwest::Client` (or `hyper` pool) with the per-module timeout is sufficient; the DoH behavior belongs to another slice and can be added as a connector override. The only explicit headers in this slice are added by `GithubProxy`:

```
User-Agent: LuaToolsGui
Accept: application/vnd.github+json
```

Timeouts:

| Module | Timeout |
|---|---|
| `SteamAppInfoCache` | 15 s |
| `SteamAppListCache` | 60 s |
| `SteamDepotInfo` | 15 s |
| `CoverCache` | 20 s |
| `GithubProxy` (used by `HardwareAppIdService`) | 5 min |

### 1.5 Logging

`SteamAppInfoCache` takes `ILogger<SteamAppInfoCache>` and emits exactly one warning per appid per process when an appdetails response cannot be matched:

```
appdetails: no entry with steam_appid={AppId} in the response — Steam's response shape changed again. Nothing cached for this app.
```

No other module in this slice logs. A Rust port can use `tracing::warn!` with the same fields.

### 1.6 Time and clocks

- Rate limiter and caches use `DateTime.UtcNow` / `DateTimeOffset.UtcNow` (wall clock, not monotonic). Clock jumps can cause bursts or long waits; do not replace with `Instant` unless you deliberately change behavior.
- `LaunchMod.SavedAt` is UTC and serialized as ISO-8601.
- Backup filenames use **local** time (`DateTime.Now`).
- Manifest/failure TTLs are wall-clock deltas.

---

## 2. `SteamService` — install discovery and OS integration

File: `SteamService.cs`. Class `SteamService(SettingsService settings)`. Not thread-safe concerns: all members are stateless except reading settings; process control is synchronous.

### 2.1 Types and public API

```rust
struct SteamService { settings: Arc<SettingsService> }

impl SteamService {
    pub fn new(settings: Arc<SettingsService>) -> Self;
    pub fn auto_detected_path(&self) -> Option<PathBuf>;      // re-reads registry every call
    pub fn effective_path(&self) -> Option<PathBuf>;          // override, else auto-detect
    pub fn is_overridden(&self) -> bool;
    pub fn steam_language() -> Option<String>;                // associated fn
    pub fn is_valid(&self) -> bool;
    pub fn steam_exe_path_for(steam_path: &Path) -> PathBuf;  // associated fn
    pub fn st_plug_in_dir(&self) -> Option<PathBuf>;          // <steam>\config\stplug-in
    pub fn depot_cache_dir(&self) -> Option<PathBuf>;         // <steam>\depotcache
    pub fn legacy_depot_cache_dir(&self) -> Option<PathBuf>;  // <steam>\config\depotcache
    pub fn open_url(url: &str);                               // associated fn; may throw
    pub fn reveal_in_explorer(file: &Path);                   // associated fn; may throw
    pub fn show_in_explorer(path: Option<&Path>) -> bool;     // associated fn; never throws
    pub fn copy_to_clipboard(text: Option<&str>) -> bool;     // associated fn; never throws
    pub fn is_steam_running() -> bool;                        // associated fn
    pub fn stop_steam(&self);
    pub fn start_steam(&self) -> bool;
    pub fn restart_steam(&self) -> bool;
}
```

### 2.2 Registry discovery (Windows-only)

Priority order, all opened with `RegistryView.Registry64`:

| # | Hive | Subkey | Value name |
|---|---|---|---|
| 1 | `HKCU` | `SOFTWARE\Valve\Steam` | `SteamPath` |
| 2 | `HKLM` | `SOFTWARE\WOW6432Node\Valve\Steam` | `InstallPath` |
| 3 | `HKLM` | `SOFTWARE\Valve\Steam` | `InstallPath` |

Algorithm for each entry:

1. `OpenBaseKey(hive, Registry64)`; `OpenSubKey(subkey)` (read-only).
2. `GetValue(valueName)`; accept only if the result is a `string` and is not null/whitespace. Non-string types (`REG_EXPAND_SZ` returned as string is accepted but **not expanded**; `REG_DWORD` rejected) → try next.
3. `Normalize(raw)`.
4. If `<normalized>\steam.exe` exists → return it.
5. Any exception (missing key, access denied, 32-bit OS view) → try next.

Return `None` if none match.

`Normalize(path)`:

```
path.Trim().Replace('/', '\\')
then Path.GetFullPath(...)   // makes absolute, collapses "." and "..", normalizes separators
on exception: return the trimmed/replaced string unchanged
```

`Path.GetFullPath` may throw on invalid characters or a too-long path. It does **not** trim quotes, so a registry value containing `"C:\Steam"` fails the `steam.exe` existence check.

### 2.3 Properties

`SteamPathOverride` from `SettingsService` is a user-chosen folder (Settings page folder picker). Settings store treats null/whitespace as unset.

- `auto_detected_path()` calls `DetectFromRegistry()` every time. There is no caching, so a registry change is picked up without restart.
- `effective_path()`: if `settings.steam_path_override` is non-whitespace → `Normalize(override)` **without** checking `steam.exe`; else `auto_detected_path()`.
- `is_overridden()`: override is non-whitespace.
- `is_valid()`: `effective_path().is_some() && steam.exe exists`. Note the C# evaluates `EffectivePath` twice (once for null check, once in `SteamExePathFor`), so a rare registry change between the two reads can make the result inconsistent. A Rust port can read once.
- `steam_exe_path_for(p)`: `p.join("steam.exe")`.
- `st_plug_in_dir()`: `effective_path().map(|p| p.join("config").join("stplug-in"))`; no existence check.
- `depot_cache_dir()`: `effective_path().map(|p| p.join("depotcache"))`. **This is `<Steam>\depotcache`, a sibling of `steamapps`; NOT `<Steam>\config\depotcache`.** Steam builds `<root>/depotcache/<depot>_<gid>.manifest`; a manifest in `config\depotcache` is invisible to Steam and silently breaks pinning/downloads.
- `legacy_depot_cache_dir()`: `effective_path().map(|p| p.join("config").join("depotcache"))`; read-only fallback for manifests written by older builds. Never write here.

### 2.4 Steam language

`static SteamLanguage` (property, read fresh each access):

1. Open `HKCU\SOFTWARE\Valve\Steam` with `RegistryView.Registry64`.
2. `GetValue("Language")`.
3. If it is a `string` with `Length > 0`: return `s.Trim().ToLowerInvariant()`.
4. Otherwise (missing, non-string, exception, empty string) return `None`.

Important: a whitespace-only value has `Length > 0`, so it returns `Some("")`, not `None`. Callers compare the result directly against depot `config.language` values (`"english"`, `"schinese"`, `"brazilian"`, …), so no mapping table is needed. The value is read once per depot-picker open by `BuildsViewModel` (line ~914), deliberately not cached across the app.

### 2.5 Process control

`is_steam_running()`:

```
procs = Process.GetProcessesByName("steam")   // process name without .exe, case-insensitive on Windows
return procs.Length > 0                       // dispose all in finally
```

Matches only `steam` (not `steamwebhelper`, `steamservice`, `steamerrorreporter`). A Rust port can use `sysinfo` and compare the file stem case-insensitively on Windows.

`stop_steam()`:

```
for each process named "steam":
    try { proc.Kill(entireProcessTree: true); proc.WaitForExit(8000); }
    catch { /* already gone / access denied */ }
    finally { proc.Dispose(); }
```

- Kills the whole tree (child `steamwebhelper` etc.).
- Waits up to 8 s per process; ignores the boolean result.
- Never throws out (all caught).
- Safe when Steam isn't running.
- No delay between kill and subsequent file operations beyond the wait.

`start_steam()`:

```
path = effective_path(); if None -> false
exe = path/steam.exe; if !exists -> false
try { Process.Start(new ProcessStartInfo(exe) { UseShellExecute = true }); return true; }
catch { return false; }
```

- Uses the shell, so it inherits the user's environment/working directory.
- Does not pass arguments, does not wait, does not verify the process started.

`restart_steam()`: `stop_steam(); start_steam()`. Synchronous.

Callers: Mode/unlocker/plugin/onboarding pages (excluded consumers), main window "Restart Steam", HTTP server endpoint. `LaunchOptionsService` injects its own `is_steam_running`/`stop`/`start` delegates.

### 2.6 Shell and clipboard

- `open_url(url)`: `Process.Start(new ProcessStartInfo(url){UseShellExecute=true})`. No validation; opening an unregistered scheme/URL throws (`Win32Exception`). Callers such as `FixesView` catch; others may not.
- `reveal_in_explorer(file_path)`: `Process.Start(new ProcessStartInfo("explorer.exe", $"/select,\"{file_path}\""){UseShellExecute=true})`; always `/select`, which for a directory highlights it in its **parent**, not opens it.
- `show_in_explorer(path)`: if null/whitespace → false. If `File.Exists(path)` → `reveal_in_explorer` and true. If `Directory.Exists(path)` → shell-open the directory and true. Else false (deleted since row created). Any exception → false.
- `copy_to_clipboard(text)`: null/empty → false. `System.Windows.Clipboard.SetText(text)`; catch all → false. `CLIPBRD_E_CANT_OPEN` (another process holds the clipboard) is expected and swallowed. WPF clipboard requires an STA thread; in Rust use a Windows clipboard crate on the UI thread and return false on failure.

### 2.7 Failure modes and edge cases

- No registry access / no Steam → `auto_detected_path() == None`, `effective_path() == None`, `is_valid() == false`, all Steam paths `None`.
- Override set to a non-Steam folder → `effective_path()` returns it, `is_valid()` false; all derived paths point into that folder (callers may still read/write). There is no "reject invalid override" in `SteamService`; the Settings page displays a warning when `!IsValid`.
- Registry value with forward slashes → normalized to backslashes.
- Registry value relative → `GetFullPath` resolves against the process CWD.
- Process enumeration can be slow (hundreds of processes); `IsSteamRunning` is called before each apply.
- `StopSteam` can block up to 8 s per matched process; in practice one.
- On non-Windows: registry APIs don't exist. Decide: return `None` (feature unavailable) or read `~/.steam/steam/steamapps` and `~/.local/share/Steam` as a Linux fallback. The C# has no fallback; if the port claims cross-platform support, add one explicitly and test it. Do not pretend the registry path works.

### 2.8 Tests worth adding

No existing tests cover `SteamService`. Add:

- registry priority (inject a `RegistryProvider` trait; don't hit the real registry in tests);
- string vs non-string value, missing key, access-denied;
- forward-slash normalization, trailing slash, relative path;
- override whitespace → treated as unset;
- override invalid → `is_valid` false but paths still derived;
- language: `"English"` → `"english"`, `"  "` → `Some("")`, missing → `None`, non-string → `None`;
- `DepotCacheDir` is `<root>/depotcache` (regression: it was once `<root>/config/depotcache`);
- `ShowInExplorer` file/dir/missing (abstract the shell launcher).

---

## 3. `SteamLibraryService` — installed-game discovery (ACF/VDF text)

File: `SteamLibraryService.cs`. Constructor takes `SteamService`.

### 3.1 Types and API

```rust
pub struct InstalledGame {
    pub app_id: i64,
    pub name: String,       // from "name", or appid as decimal if absent
    pub install_dir: PathBuf, // full path to steamapps/common/<installdir>
}

pub struct SteamLibraryService { steam: Arc<SteamService> }

impl SteamLibraryService {
    pub fn new(steam: Arc<SteamService>) -> Self;
    pub fn get_install_dir(&self, app_id: i64) -> Option<PathBuf>;
    pub fn enumerate_installed(&self) -> impl Iterator<Item = InstalledGame> + '_;
}
```

Both are synchronous, do real disk IO, and are safe to call from a blocking thread. `EnumerateInstalled` is a lazy iterator (C# `yield return`) so a caller can stop early; it does not cache. The comment says enumerating `.acf` files is ~39 ms across three drives; per-game work is the caller's cost.

### 3.2 Text VDF/ACF shape

`libraryfolders.vdf` (modern Steam):

```
"libraryfolders"
{
    "0"
    {
        "path"      "C:\\Program Files (x86)\\Steam"
        "label"     ""
        "contentid" "123..."
        "totalsize" "0"
        "apps"
        {
            "228980" "123456"
        }
    }
    "1"
    {
        "path"      "D:\\SteamLibrary"
        ...
    }
}
```

`appmanifest_<appid>.acf`:

```
"AppState"
{
    "appid"        "648800"
    "name"         "Raft"
    "installdir"   "Raft"
    "StateFlags"   "4"
    ...
}
```

Key properties of Valve's text KeyValues relevant here:

- Keys and string values are double-quoted.
- Backslashes are escaped as `\\`; the C# only unescapes `\\` → `\`.
- Whitespace/indentation is arbitrary; one key/value pair per line is typical but not required.
- `//` line comments and conditionals exist in the full format; the C# regex parser does not understand them.
- Values may be empty (`""`).

### 3.3 Regexes (all `RegexOptions.IgnoreCase`)

| Purpose | Pattern | Applied |
|---|---|---|
| library root | `"path"\s*"([^"]+)"` | all matches in `libraryfolders.vdf` |
| install dir | `"installdir"\s*"([^"]+)"` | first match in an ACF |
| appid | `"appid"\s*"(\d+)"` | first match in an ACF |
| name | `"name"\s*"([^"]*)"` | first match in an ACF |

Notes:

- The patterns require the literal quotes around the key, so `path` without quotes doesn't match.
- `\s*` allows newlines/tabs between key and value.
- `[^"]+` forbids an empty library/install dir; `[^"]*` allows an empty name.
- The regex is not anchored; a `"name"` value inside another nested block could be picked up, but ACF files put `name` at the top level.
- `"path"` inside a `//` comment would match.
- The old `libraryfolders.vdf` format (`"1" "D:\\SteamLibrary"` with no `"path"` key) is **not** supported by the regex; only modern format works.

### 3.4 `GetInstallDir(app_id)`

1. `steam_root = steam.effective_path()`; if `None` → `None`.
2. For each `library` in `get_library_roots(steam_root)`:
   - `acf = library/steamapps/appmanifest_<app_id>.acf`.
   - If the file does not exist → continue.
   - Read the whole file as text.
   - First `"installdir"` match; if none → continue.
   - `install_dir = unescape(capture)` (`\\` → `\`).
   - `full = library/steamapps/common/<install_dir>`.
   - If `full` is a directory → return it.
3. Return `None`.
4. Any exception in the whole loop (unreadable VDF/ACF, invalid path) → catch → `None`.

`app_id` is interpolated into the filename with C# default integer formatting (no group separators; use decimal invariant in Rust).

### 3.5 `EnumerateInstalled()`

1. `steam_root = effective_path()`; if `None` → empty.
2. For each library root:
   - `steamapps = library/"steamapps"`.
   - If `!Directory.Exists(steamapps)` → continue.
   - `Directory.GetFiles(steamapps, "appmanifest_*.acf")`; on exception → continue this library.
   - For each ACF:
     - try:
       - read all text;
       - first `"appid"` and `"installdir"` matches; if either missing → skip;
       - `i64::from_str(appid)`; on failure → skip;
       - `full = steamapps/"common"/unescape(installdir)`;
       - if `!Directory.Exists(full)` → skip;
       - first `"name"` match; name = capture if present else `app_id.to_string()`;
       - yield `InstalledGame { app_id, name, install_dir: full }`.
     - catch → skip this file only, continue the library.
3. Order: libraries in `get_library_roots` order; files in `Directory.GetFiles` order (filesystem-dependent, not sorted).

The name capture is **not** unescaped (only the install dir is). A game name containing `\\` would keep the doubled backslashes; names rarely do.

### 3.6 `GetLibraryRoots(steam_root)` (private iterator)

1. Always yield `steam_root` first ("the main install is always a library").
2. `vdf = steam_root/steamapps/libraryfolders.vdf`; if missing → stop.
3. Read all text; on exception → stop.
4. For each `"path"` match:
   - `path = unescape(capture)`;
   - if `path` equals `steam_root` with `OrdinalIgnoreCase` → skip (main root often appears in the file);
   - if `Directory.Exists(path)` → yield.
5. Duplicate non-root libraries are **not** deduped; the same library appearing twice is yielded twice, so `EnumerateInstalled` can yield duplicate games and `GetInstallDir` probes it twice.
6. Paths are not normalized before comparison (trailing slash or `/` vs `\` can defeat the equality check and the existence check).

### 3.7 `Unescape`

Only `s.Replace("\\\\", "\\")`: every adjacent pair of backslashes becomes one. It does not handle `\"`, `\n`, `\t`, or other Valve escapes. (`C:\Games` is stored as `C:\\Games`; a literal path with two consecutive backslashes would collapse, but Windows paths don't normally have that except UNC `\\server\share`, which **would be corrupted** from `\\\\server\\share` to `\\server\share`? Let's be precise: VDF stores a UNC path `\\server\share` as `\\\\server\\share`; the replacement sees pairs and produces `\\server\share` again, which is correct. Only pathological runs are ambiguous.)

### 3.8 Failure modes and edge cases

- No Steam / no override → both methods return empty/null.
- Missing `libraryfolders.vdf` → only the main root is considered.
- Malformed ACF → skipped, never throws.
- App installed in a secondary library but the primary ACF is missing → found in the secondary.
- `GetInstallDir` trusts the **filename** `appmanifest_<appId>.acf` and never checks the ACF's own `"appid"` field. `EnumerateInstalled` does the reverse: it trusts the `"appid"` inside the file and ignores the filename. A renamed/copied ACF can therefore produce inconsistent results between the two APIs.
- Game folder deleted but ACF remains → `GetInstallDir` returns null (directory check), `EnumerateInstalled` skips it.
- ACF `installdir` with `..\` or an absolute path: `Path.Combine` does **not** sanitize; a value like `..\..\Windows` would escape `common`. This is a trust boundary: ACF files are local Steam data, but a malicious/manual ACF could point outside. The C# does not validate containment. The Rust port should decide (recommend: reject paths that escape `steamapps/common`, or canonicalize and check prefix) while noting it changes behavior for pathological values.
- `EnumerateInstalled` does not catch exceptions thrown by `get_library_roots` itself (e.g. `Path.Combine` on an invalid root, or `Directory.Exists` failures inside the iterator are mostly safe). `GetInstallDir` wraps its whole loop, so it is safe.
- Case: `"AppId"` key would match because regex is IgnoreCase; values are digits.
- `GetInstallDir` returns the **game folder** (`steamapps/common/<installdir>`), not the library root and not the ACF's own folder.

### 3.9 Call sites and semantics

- `FixesViewModel` (Denuvo fix apply/revert — **EXCLUDED**, see §18) resolves a game's folder on click; if null, shows "game not found".
- `ManifestJobFactory` checks `GetInstallDir(appId)` before applying a fix (excluded pipeline) and again when reverting.
- `SteamlessService` uses `GetInstallDir` to patch executables (**EXCLUDED**: SteamStub DRM removal).
- `AppliedFixIndexService` enumerates installed games to build an index (excluded pipeline).
- The module itself is a generic "where is this Steam game installed" utility; keep it if the port retains any installed-game features.

### 3.10 Tests worth adding

No existing tests. Add fixtures with:

- main root only; modern `libraryfolders.vdf` with two libraries; one path missing on disk; one equal to root; duplicate library;
- ACF with `installdir`/`name`/`appid`; missing name → appid fallback; missing installdir → skipped; folder missing → skipped;
- escaped `\\` in `path` and `installdir`;
- unreadable ACF (permission) → other games still enumerate;
- old-format libraryfolders (numbered keys, no `"path"`) → documents current non-support (or implement a real VDF parser and support both);
- path traversal in `installdir` → decide and test rejection if hardened.

---

## 4. `AppInfo/BinaryVdf.cs` — Valve binary KeyValues codec

File: `AppInfo/BinaryVdf.cs`. Namespace `LuaToolsGui.Services.AppInfo`.

This is the codec used inside `appinfo.vdf` v29. It is also the layer that must preserve duplicate keys and non-UTF-8 bytes.

### 4.1 `VdfType` (byte markers)

| Name | Value | Payload |
|---|---:|---|
| `Table` | `0x00` | nested object, terminated by `0x08` or `0x0B` |
| `String` | `0x01` | NUL-terminated bytes |
| `Int32` | `0x02` | 4 bytes LE |
| `Float` | `0x03` | 4 bytes IEEE-754 LE |
| `Pointer` | `0x04` | 4 bytes LE (semantically an int) |
| `WString` | `0x05` | UTF-16; **defined but unsupported by both read and write** |
| `Color` | `0x06` | 4 bytes LE |
| `UInt64` | `0x07` | 8 bytes LE |
| `End` | `0x08` | object terminator |
| `Int64` | `0x0A` | 8 bytes LE |
| `EndAlt` | `0x0B` | alternative object terminator (read-only; write emits `0x08`) |

Markers are single bytes. `0x09` and any other value are unknown: the reader consumes a key and then throws when switching on the value type; the writer's switch falls through to `Can't serialize VDF type ...`.

### 4.2 `VdfProperty`

C# is a sealed class with:

```rust
struct VdfProperty {
    name: String,          // settable
    ty: VdfType,           // read-only after construction
    value: VdfValue,       // settable
}
```

`VdfValue` must be able to hold:

- `Table(VdfTable)`
- `String(Vec<u8>)` — **raw bytes**, not a Rust `String`
- `Int32(i32)`, `Float(f32)`, `Pointer(i32)`, `Color(i32)`, `UInt64(u64)`, `Int64(i64)`

Do not collapse `Pointer`/`Color` into `Int32` without also preserving the original type tag; otherwise a round-trip changes the marker byte and the appinfo SHA-1 changes. Suggested representation: `struct VdfProperty { name: String, ty: VdfType, value: VdfValue }` where `VdfValue` has distinct `Pointer(i32)` and `Color(i32)` variants.

Methods:

- `as_table() -> Option<&VdfTable>`: value if table.
- `as_text() -> String`:
  - if `String(bytes)` → `String::from_utf8_lossy(bytes)` (C# `Encoding.UTF8.GetString` replaces invalid sequences with U+FFFD);
  - else `value.to_string()` (C# `object.ToString()`).
  - Consequences: `Int32(5)` → `"5"`, `Float(1.5)` → current-culture string (C# float `ToString` is culture-sensitive; for invariant behavior use `1.5`), `Table` → the C# type's full name (a quirk; in Rust return an empty string or a marker, but do not rely on this path), null → `""`.
- `VdfProperty::text(name, value)`: `ty = String`, `value = value.as_bytes().to_vec()`.
- `VdfProperty::table(name, table)`: `ty = Table`.
- C# `Value` is `object` and settable, with no type check; a caller can create an inconsistent property. In Rust, keep `ty` and `value` consistent through constructors and make fields private.

### 4.3 `VdfTable`

**Ordered list, not a map.** The format permits the same key twice in one object; 15 of 176,869 real apps do it (e.g. app 46830 has `/silent` twice). A map silently drops duplicates and shortens the re-serialized blob, corrupting the file. Order must be preserved exactly because the blob is hashed.

```rust
struct VdfTable { items: Vec<VdfProperty> }
```

Methods and exact semantics:

| Member | Behavior |
|---|---|
| `find(name)` | first item whose name equals `name` under OrdinalIgnoreCase; `None` if absent |
| `get_table(name)` | `find(name).as_table()` (first match) |
| `get_text(name)` | `find(name).as_text()`; `None` only if no key, otherwise may return a rendered non-string value |
| `ensure_table(name)` | if `get_table(name)` exists → return it; else append a new `Table` property with that name and return it |
| `set_text(name, Option<String>)` | if value is null/empty → `remove(name)`; else if `find(name)` exists → **replace its value with UTF-8 bytes but leave its `ty` unchanged**; else append a new `String` property. The unchanged-type behavior is a C# quirk: setting a key that exists as `Int32` produces a property whose value is `Vec<u8>` but whose type tag says `Int32`, and serialization then fails with an invalid cast. Rust should either coerce `ty = String` or return an error; document the choice. |
| `remove(name)` | remove **all** items matching name OrdinalIgnoreCase, scanning backwards |
| `items` | public mutable `List<VdfProperty>`; callers append/remove directly. Preserve the list as the sole storage. |
| `clone()` | deep copy: tables recursively, byte arrays copied, primitives copied; preserves order, duplicates, type tags |

`remove` removing all matches matters: replacing `config.launch` must not leave a duplicate behind.

### 4.4 `BinaryVdf::Read(stream, strings, indexed_keys) -> Result<VdfTable, Error>`

Algorithm:

```
table = new
loop:
    marker = read_byte()
    if EOF or marker == 0x08 or marker == 0x0B:
        return table
    ty = marker as VdfType
    name = if indexed_keys:
               idx = read_u32_le()
               strings[idx as usize]      // may panic/IndexOutOfRange in C#
           else:
               read_cstring()             // UTF-8 lossy
    value = match ty:
        Table   => Read(stream, strings, indexed_keys)   // recursion
        String  => read_cstring_bytes()                  // raw Vec<u8>
        Int32 | Pointer | Color => read_i32_le()
        UInt64  => read_u64_le()
        Int64   => read_i64_le()
        Float   => read_f32_le()                         // 4 bytes
        _       => Err(InvalidData("Unsupported VDF value type 0x.."))
    table.items.push(VdfProperty { name, ty, value })
```

Details and edge cases:

- EOF (`read_byte` returns -1) is treated exactly like a terminator: **a missing terminator is silently accepted**.
- `read_cstring_bytes` reads until a `0x00` byte **or EOF**; EOF is accepted and returns whatever was accumulated. It does not error on a missing terminator.
- A NUL byte cannot appear inside a string value; an embedded NUL terminates it.
- `read_cstring` decodes raw bytes with UTF-8 replacement. This is for key names and for display only; string **values** are stored raw.
- In v29, `strings[idx]` is unchecked. A corrupt/out-of-range index throws `IndexOutOfRangeException` (or `ArgumentOutOfRange`), which propagates out of `ReadApp`. A Rust port should return a typed error.
- `WString` is not supported: reading one throws even though its marker is defined.
- Nested tables are terminated by `End` or `EndAlt`; the parent continues after the nested table's terminator.
- No depth limit. A malicious/corrupt file can recurse deeply and overflow the stack. Add a depth cap in Rust (e.g. 256) and return an error.

### 4.5 `BinaryVdf::Write(stream, table, intern, indexed_keys) -> Result<(), Error>`

Algorithm:

```
for p in table.items:
    write_u8(p.ty as u8)
    if indexed_keys:
        write_u32_le(intern(&p.name))       // new keys get a new index
    else:
        write_cstring(p.name.as_bytes())    // raw UTF-8 + 0x00
    match p.ty:
        Table   => Write(stream, p.value.as_table()?, intern, indexed_keys)
        String  => write_cstring(p.value.as_bytes()?)
        Int32 | Pointer | Color => write_i32_le(p.value.as_i32()?)
        UInt64  => write_u64_le(...)
        Int64   => write_i64_le(...)
        Float   => write_f32_le(...)
        _       => Err("Can't serialize VDF type ...")
write_u8(0x08)                              // always End, never EndAlt
```

- `intern: Fn(&str) -> u32` is ignored when `indexed_keys == false`; when true, it maps a key to its string-table index (existing or appended).
- If `ty` is `Table` but the value isn't a table, or `String` but the value isn't bytes, the C# cast throws `InvalidCastException`; Rust should return an error.
- If `ty` is `End`/`EndAlt`/`WString`/unknown in `items`, the writer has already emitted the type and key before throwing; this can leave a partial stream. `SaveAs` writes to a `MemoryStream`, so the partial data is discarded.
- String values are written raw; if a caller supplies bytes containing `0x00`, the written value will be read back truncated. C# does not validate. Rust should reject interior NUL for safety (or document).
- The writer does not deduplicate keys or sort; it writes list order exactly.
- All multi-byte integers/floats are little-endian. C# `BitConverter` is little-endian on every platform .NET supports; make it explicit in Rust (`to_le_bytes`).

### 4.6 Primitive helpers

| Helper | Behavior |
|---|---|
| `read_exactly(n)` | read exactly n bytes or throw `EndOfStreamException` |
| `read_u32` / `read_i32` / `read_u64` / `read_i64` | 4/4/8/8 bytes LE |
| `write_u32` / `write_i32` | 4 bytes LE |
| `read_cstring_bytes` | raw bytes up to NUL or EOF |
| `read_cstring` | `read_cstring_bytes` decoded as UTF-8 with replacement |
| `write_cstring(raw)` | raw bytes then one `0x00` |

No bounds checks beyond `read_exactly`; the stream's own EOF behavior governs.

### 4.7 Tests / guarantees to preserve

- Duplicate keys in one object survive read and write in order.
- Non-UTF-8 string values survive byte-for-byte through read → write.
- Table order is preserved.
- `set_text("", ...)`/null removes the key entirely.
- `find`/`remove` are case-insensitive; a second key differing only in case is a separate item for storage but `find` returns the first.
- Unknown type marker → error, not silent skip.
- Missing terminator → accepted (current behavior); decide whether Rust should warn.

---

## 5. `AppInfo/AppInfoFile.cs` — `appcache/appinfo.vdf`

File: `AppInfo/AppInfoFile.cs`.

### 5.1 Purpose and public API

Reads and rewrites Steam's appinfo cache. The real file is ~373 MB with ~177k apps; the class keeps only an `appid -> offset` index (~1.4 MB) plus the v29 string table, reads apps on demand, and streams saves.

```rust
pub const MAGIC_27: u32 = 0x0756_4427;
pub const MAGIC_28: u32 = 0x0756_4428;
pub const MAGIC_29: u32 = 0x0756_4429;

pub struct AppInfoFormatException(pub String); // Display/Debug

pub struct AppInfoEntry {
    pub app_id: i32,
    pub offset: i64,       // absolute file offset of the record's appid field
    pub size: i32,         // bytes after the size field = meta_len + blob_len
    pub change_number: u32,
}

pub struct AppInfoFile {
    // file handle, path, magic, universe, decoded strings, raw string bytes, index, apps_start
}

impl AppInfoFile {
    pub fn open(path: &Path) -> Result<Self, AppInfoError>;
    pub fn path(&self) -> &Path;
    pub fn magic_number(&self) -> u32;
    pub fn universe(&self) -> u32;
    pub fn strings(&self) -> &[String];
    pub fn indexed_keys(&self) -> bool;       // magic >= 29
    pub fn meta_length(&self) -> usize;       // 60 if magic >= 28 else 40
    pub fn entries(&self) -> &HashMap<i32, AppInfoEntry>;
    pub fn read_app(&mut self, app_id: i32) -> Option<Result<VdfTable, AppInfoError>>;
    pub fn read_app_body(&mut self, app_id: i32) -> Option<Result<VdfTable, AppInfoError>>; // appinfo child
    pub fn save_as(&mut self, dest: &Path, edits: &HashMap<i32, VdfTable>) -> Result<(), AppInfoError>;
}
```

C# specifics:

- `Entries` is `IReadOnlyDictionary<int, AppInfoEntry>`.
- `Strings` is a `List<string>` for lookup/display; raw bytes are kept privately.
- `ReadApp` returns `null` when the appid is absent; otherwise it may throw.
- `ReadAppBody` is `ReadApp(appid)?.GetTable("appinfo")`.
- `Dispose` closes the file.

### 5.2 File layout (v27/v28/v29)

```
Header:
  magic        u32 LE    0x07564427 | 0x07564428 | 0x07564429
  universe     u32 LE    typically 1
  [v29 only]   string_table_offset i64 LE   absolute file offset

App records (repeat until appid == 0):
  appid        i32 LE
  size         i32 LE    number of bytes AFTER this field = meta_len + blob_len
  meta         meta_len bytes (40 or 60; see below)
  blob         size - meta_len bytes (binary VDF; root object usually wraps "appinfo")

Terminator:
  appid        i32 LE    0

[v29 only] string table at string_table_offset:
  count        u32 LE (written as i32 by SaveAs; same bytes)
  repeated count times: NUL-terminated raw UTF-8 key bytes
```

The v29 string table is normally physically after the terminator. The reader seeks to the absolute offset, reads it, and restores the file position; it does **not** assume the table is at EOF, but `SaveAs` writes it at EOF.

`size` counts `meta + blob`, **not** the 8-byte appid/size header. Therefore a record's total length is `8 + size`, and the next record starts at `offset + 8 + size`.

### 5.3 Meta block

`meta_len` is 60 for v28/v29 and 40 for v27. Offsets within the meta block:

| Offset | Length | Field | Notes |
|---:|---:|---|---|
| 0 | 4 | `infoState` | u32; opaque, carried through untouched |
| 4 | 4 | `lastUpdated` | u32 Unix time; opaque |
| 8 | 8 | `picsToken` | u64; opaque |
| 16 | 20 | `sha1_text` | SHA-1 of Steam's own text rendering; **not reproducible from the blob**, never recomputed |
| 36 | 4 | `changeNumber` | u32; exposed as `AppInfoEntry.change_number`; carried through untouched |
| 40 | 20 | `sha1_binary` | v28+ only: `SHA1(blob)`; recomputed on edit |

For v27, the meta block is exactly bytes 0..40 (no `sha1_binary`). The C# constants are:

```
Sha1TextOffset = 16
ChangeNumberOffset = 36
Sha1BinaryOffset = 40
MetaLength = 60 (v28+) / 40 (v27)
```

All multi-byte fields are little-endian. `picsToken` is read as part of the meta but never interpreted.

### 5.4 Constructor / opening

```
file = File.Open(path, Open, Read, FileShare.ReadWrite)
magic = read_u32_le()
universe = read_u32_le()
if magic not in {MAGIC_27, MAGIC_28, MAGIC_29}:
    throw AppInfoFormatException($"Unknown appinfo version 0x{magic:X8}.")
if magic == MAGIC_29:
    table_offset = read_i64_le()
    resume = file.position
    read_string_table(table_offset)
    file.position = resume
apps_start = file.position
build_index()
```

Exception behavior (C#):

- missing file → `FileNotFoundException`
- no permission → `UnauthorizedAccessException`
- unknown magic → `AppInfoFormatException` with the hex version
- truncated header → `EndOfStreamException` from `ReadExactly`
- bad string-table offset → whatever `Seek`/read throws (often `IOException` or `ArgumentOutOfRangeException`)
- corrupt string-table count can loop for a very long time (it reads empty C-strings at EOF until the count is exhausted); a Rust port should cap/validate against file length.

`FileShare.ReadWrite` allows Steam to keep the file open while this app reads it. On Windows this does **not** allow writing while Steam has it open with a conflicting share mode; `LaunchOptionsService` stops Steam before writing.

### 5.5 String table

`read_string_table(offset)`:

```
seek(offset)
count = read_u32_le()
for i in 0..count:
    raw = read_cstring_bytes()      // NUL or EOF
    raw_bytes.push(raw)
    strings.push(utf8_lossy(raw))   // lookup/display only
```

Why raw bytes are kept: a handful of table entries are not valid UTF-8. Decoding to a `String` and re-encoding on save turns each bad byte into U+FFFD (3 bytes), which made a no-edit rebuild 12 bytes larger. `SaveAs` writes the original raw bytes for existing entries and only UTF-8-encodes newly interned keys.

- `Strings` and `raw_bytes` are parallel arrays.
- Duplicate decoded strings can exist (different raw bytes); `SaveAs`'s lookup keeps the **first** index for a decoded key.
- Indices are `u32`; the table is append-only, so existing indices in untouched blobs stay valid.
- No validation that the table offset is within the file or after the records.

### 5.6 Index construction

```
file.position = apps_start
loop:
    offset = file.position
    header = read 8 bytes
    if read < 8: return                  // silently stop (truncated file)
    app_id = i32_le(header[0..4])
    if app_id == 0: return
    size = i32_le(header[4..8])
    meta = read_exactly(meta_len)        // throws if EOF
    change = u32_le(meta[36..40])
    index[app_id] = AppInfoEntry { app_id, offset, size, change }
    file.position = offset + 8 + size
```

- `~0.5 s` for 177k apps in C#.
- The blob is skipped, never read.
- A size smaller than `meta_len` makes `position` move backwards, potentially re-reading records and looping/overwriting index entries. There is no validation. Rust should validate `size >= meta_len` and `offset + 8 + size <= file_len` (or at least not move backwards) and return a format error.
- Duplicate appids in the file: later record overwrites the earlier in the dictionary; `SaveAs` iterates index values (one per appid), so the earlier duplicate is dropped from the rewritten file. This is a silent normalization; note it.
- `appid == 0` is the terminator. A negative appid is accepted as a key (i32).
- The on-disk appid is logically a `u32`, but C# reads it as `i32`. An appid >= 2^31 therefore appears **negative**. `LaunchOptionsService.Read` receives a `long` and casts it with `(int)appId`, so the two's-complement bit pattern matches and lookup works. A Rust port must choose one representation consistently (e.g. keep `i32`/`u32` bit-cast, or normalize all appids to `i64` via `as u32 as i64`) and test an appid above `i32::MAX`.

### 5.7 Reading one app

```
read_app(app_id):
    entry = index.get(app_id) or return None
    file.position = entry.offset + 8 + meta_len
    return BinaryVdf.Read(file, strings, indexed_keys)
```

- It does **not** limit reading to `entry.size`; a blob without a terminator can read into the next record(s).
- `indexed_keys` is true only for v29; v27/v28 keys are inline strings.
- For v29, the `strings` table is used for every key; an out-of-range index throws.
- `read_app_body` returns the first `appinfo` child (OrdinalIgnoreCase). Real records wrap their content in this child.
- If the app is missing, `None`; no exception.

### 5.8 Saving: `save_as(destination, edits)`

`edits`: `appid -> full replacement root table` (the `appinfo` wrapper included). Apps not in `edits` are copied through as **raw bytes**, not reserialized. Edits whose appid is **not present** in the file's index are silently ignored (the save loop iterates the index, not the edit map). This is a correctness requirement, not only a performance choice: it makes untouched apps byte-exact by construction despite duplicate keys and non-UTF-8 strings.

Algorithm in exact order:

1. `strings = clone(raw_string_bytes)` (copy of the original raw table).
2. Build `lookup: HashMap<String, u32>` from the decoded `Strings`, **first index wins** (`TryAdd`), comparison Ordinal (case-sensitive).
3. `intern(key)`:
   - if `lookup` has key → return index;
   - else index = `strings.len()`, append `key.as_bytes().to_vec()` (UTF-8), insert into lookup, return index.
4. Open destination with `FileMode.Create` (truncate), `FileAccess.Write`, `FileShare.None`.
   - **Danger:** no check that `destination != source`. Opening the source path here truncates the file while the read handle is open, destroying it. `LaunchOptionsService` always passes a temp path. A Rust port must reject same-path destinations.
5. Write `magic` (u32 LE), `universe` (u32 LE).
6. `table_offset_pos = output.position`.
7. If `indexed_keys` (v29): write 8 zero bytes as a placeholder for the `i64` table offset.
8. Iterate `index.values()` **ordered by `offset` ascending** (not by appid, not dictionary order):
   - Seek source to `entry.offset + 8`; read `meta_len` bytes.
   - If `edits` contains `entry.app_id`:
     - `blob = BinaryVdf.Write(replacement, intern, indexed_keys)` into a `MemoryStream`.
     - `bytes = blob.to_vec()`.
     - If `magic >= MAGIC_28`: `sha1(bytes)` → copy 20 bytes into `meta[40..60]`.
       - This is required: `sha1_binary` is exactly `SHA1(blob)`. Verified against 3,000 apps and SteamEdit's writer. Leaving it stale ships a hash of pre-edit bytes.
       - `sha1_text` (meta[16..36]) and `changeNumber` (meta[36..40]) are **not** touched. `sha1_text` hashes Steam's own text rendering and cannot be reproduced from the binary blob; `changeNumber` is not bumped.
       - For v27 (no `sha1_binary`), the meta is written unchanged; the blob hash is not updated (there is no field for it).
     - Write `appid` (i32 LE), `size = meta.len() + bytes.len()` (i32 LE), `meta`, `bytes`.
   - Else:
     - Write `appid` (i32 LE), `size = entry.size` (i32 LE), `meta`, then copy `entry.size - meta_len` bytes from source to output.
       - `CopyRange` uses an 80 KiB buffer; if the source ends early it throws `EndOfStreamException("appinfo.vdf ended mid-app.")`.
       - If `entry.size < meta_len`, the count is negative and `while (count > 0)` writes **nothing**, while the header still claims `entry.size`. The output is then malformed. Rust should reject this.
9. Write `appid = 0` terminator (i32 LE).
10. If `indexed_keys`:
    - `table_offset = output.position`.
    - Write `strings.len()` as **i32 LE** (same bytes as u32 for counts < 2^31).
    - For each raw string: write bytes then `0x00`.
    - Seek to `table_offset_pos`; write `table_offset` as i64 LE.
    - Note: the stream is left positioned at the table-offset field, not EOF; the caller closes it. No flush/fsync is performed by this class.
11. No trailing padding or extra bytes are written. If the original file had bytes after the string table, they are dropped.
12. If `SaveAs` throws mid-write, the destination is left partial. The caller must write to a temp and swap.

For v27/v28 (`indexed_keys == false`), no table-offset field is written and no string table is written; the `intern` callback is unused. The destination header is 8 bytes (magic+universe).

### 5.9 Versioning / compatibility

| Magic | Version | Key encoding | Meta | String table |
|---|---|---:|---:|---|
| `0x07564427` | v27 | inline C-string | 40 | none |
| `0x07564428` | v28 | inline C-string | 60 (adds `sha1_binary`) | none |
| `0x07564429` | v29 | `u32` index into shared table | 60 | at header i64 offset |

Any other magic → `AppInfoFormatException` **before** any app parsing. This is how a future Steam version bump surfaces: `LaunchOptionsViewModel` catches `AppInfoFormatException` and shows "unsupported format" rather than crashing.

### 5.10 Threading and resource behavior

- One `FileStream` with a seek position; **not thread-safe**. Every `ReadApp`/`SaveAs` call mutates the stream position. External synchronization is required.
- `SaveAs` reads the source and writes the destination in one pass; it holds both open for the duration (~373 MB read + write, seconds).
- No async APIs; callers run it on a worker thread (`Task.Run` in the ViewModel) to avoid blocking the UI.
- No cancellation. A Rust port may add `&mut self` and optional cancellation between records, but a partially written temp is safe to delete.

### 5.11 Tests (existing `AppInfoCodecTests`) and additional cases

Existing expectations:

1. **No-edit rebuild is byte-identical**, including:
   - a v29 string table containing a non-UTF-8 entry (`[b,b,b,0xFD,k]`);
   - duplicate keys `dup` in one object (both survive, texts `first`, `second`);
   - a non-UTF-8 string value `[0x53,0x74,0xFD,0xFE,0x72]` under `legacy`;
   - a property whose **key** comes from the non-UTF-8 string-table entry;
   - meta `sha1_text = "TEXTHASH-DO-NOT-TOUCH"` (20 bytes), `changeNumber = 4242`, `sha1_binary = SHA1(blob)`.
2. Duplicate keys both survive read (`Items.Where(Name=="dup")` count 2, order preserved).
3. A non-UTF-8 string value survives an edit to **another** field (`Arguments`), byte-for-byte.
4. Edit restamps `sha1_binary` (`meta[40..60] == SHA1(blob)`, differs from original), preserves `sha1_text` (`meta[16..36]`), and preserves `changeNumber` (`4242`).
5. Launch option read/write round-trip (see §6).
6. Unknown magic `0x07564430` → `AppInfoFormatException`.

Add for Rust:

- v27 and v28 round-trip (inline keys, 40/60 meta).
- Duplicate appids: define/document last-wins and whether the rewrite drops earlier records.
- `size < meta_len`, `size` past EOF, negative `size` → format error, not loop/panic.
- Truncated blob (no terminator) → error or bounded read, not read-into-next-record.
- Same-path `save_as` rejected.
- String-table index out of range → typed error.
- Huge string-table count → bounded error.
- Non-UTF-8 **key** bytes survive (v29 table raw copy).
- New key interned at the end; existing indices unchanged.
- Edits to multiple apps, one of which is missing from the file → skipped by the caller; `SaveAs` itself only sees existing ids.

---

## 6. `AppInfo/LaunchOption.cs` — launch entries

File: `AppInfo/LaunchOption.cs`. Plain POCO (no INotifyPropertyChanged); the ViewModel wraps it.

### 6.1 Model fields, defaults, VDF mapping

| Rust field | C# default | VDF location | Read when absent |
|---|---|---|---|
| `index: String` | `"0"` | key under `config.launch` | n/a (key itself) |
| `source_index: String` | `""` | not written | set to the key read from |
| `executable: String` | `""` | `executable` | `""` |
| `arguments: String` | `""` | `arguments` | `""` |
| `working_dir: String` | `""` | `workingdir` | `""` |
| `description: String` | `""` | `description` | `""` |
| `type: String` | `"default"` | `type` | **`""`** (FromTable overwrites the default) |
| `os_list: String` | `""` | `config.oslist` | `""` |
| `os_arch: String` | `""` | `config.osarch` | `""` |
| `beta_key: String` | `""` | `config.BetaKey` (exact casing on write; read is case-insensitive) | `""` |
| `owns_dlc: String` | `""` | `config.ownsdlc` | `""` |

Field frequency in 64,117 real entries (from the source comment): `executable` 100%, `config` 91% (`oslist` 90%), `type` 82%, `description` 31%, `arguments` 10%, `workingdir` 4%. Steam **omits** unused optionals rather than writing empty strings; `ToTable` matches that.

`Summary` (C# `[JsonIgnore]`): join the non-whitespace values of `Executable`, `Arguments`, `OsList` with `"  ·  "` (two spaces, middle dot U+00B7, two spaces). Not persisted.

`Clone()` is C# `MemberwiseClone`: a shallow copy. Every field is a `String` (immutable), so it behaves as a value copy; use `#[derive(Clone)]`. The launch store and editor rely on clone-on-save so later caller mutations don't leak into the snapshot.


API signatures (Rust form; C# equivalents in §6.1):

```rust
impl LaunchOption {
    pub fn from_table(index: &str, table: &VdfTable) -> Self;
    pub fn to_table(&self, existing: Option<VdfTable>) -> VdfTable;
    pub fn read_all(app_body: &VdfTable) -> Vec<LaunchOption>;
    pub fn write_all(app_body: &mut VdfTable, options: &[LaunchOption]);
    pub fn renumber(options: &mut [LaunchOption]);
    pub fn next_index(options: &[LaunchOption]) -> String;
    pub fn clone(&self) -> Self;              // derived Clone
    pub fn summary(&self) -> String;          // computed, not serialized
}
```

### 6.2 `FromTable(index, table) -> LaunchOption`

```
config = table.get_table("config")           // first, case-insensitive
return LaunchOption {
    index: index,
    source_index: index,
    executable: table.get_text("executable").unwrap_or_default(),
    arguments: table.get_text("arguments").unwrap_or_default(),
    working_dir: table.get_text("workingdir").unwrap_or_default(),
    description: table.get_text("description").unwrap_or_default(),
    type: table.get_text("type").unwrap_or_default(),   // absent -> ""
    os_list: config.and_then(|c| c.get_text("oslist")).unwrap_or_default(),
    os_arch: config.and_then(|c| c.get_text("osarch")).unwrap_or_default(),
    beta_key: config.and_then(|c| c.get_text("BetaKey")).unwrap_or_default(),
    owns_dlc: config.and_then(|c| c.get_text("ownsdlc")).unwrap_or_default(),
}
```

- `get_text` on a non-string value returns its rendered form (e.g. an `Int32` `type` becomes `"1"`), not an error.
- `config` is looked up case-insensitively; `BetaKey` is matched case-insensitively by `VdfTable.find`, despite the capitalization.

### 6.3 `ToTable(existing: Option<VdfTable>) -> VdfTable`

Exact order and quirks:

1. `table = existing.unwrap_or_else(VdfTable::new)`.
2. `table.set_text("executable", executable)` → removes the key if the value is empty.
3. `set_text("arguments", arguments)`, `set_text("workingdir", working_dir)`, `set_text("description", description)`, `set_text("type", type)` — all remove on empty.
4. `any_config = !os_list.is_empty() || !os_arch.is_empty() || !beta_key.is_empty() || !owns_dlc.is_empty()` (C# `!string.IsNullOrEmpty`; a whitespace-only string still counts as set).
5. If `any_config`:
   - `config = table.ensure_table("config")` (appends at the end if absent);
   - `config.set_text("oslist", os_list)`, `set_text("osarch", os_arch)`, `set_text("BetaKey", beta_key)`, `set_text("ownsdlc", owns_dlc)`.
6. Else (all four managed config values empty):
   - if `table.get_table("config")` exists **and** its `items.len() == 0` → `table.remove("config")`;
   - otherwise **do nothing**. In particular, existing `oslist`/`osarch`/`BetaKey`/`ownsdlc` keys are **not cleared**.
7. Return `table`.

Consequences / edge cases:

- **Clearing a config value is not possible through this path if the entry already had one.** If the user clears `oslist` and no other managed config value is set, `any_config` is false, the old `oslist` remains, and the next write puts it back. This is a real behavioral bug in the C#. Decide whether the Rust port reproduces it or fixes it (recommended: fix by explicitly `set_text`-ing all four managed keys, which removes empties, while preserving unmodeled config keys).
- If `existing` has a `config` table with only unmodeled keys (e.g. `installscript`), and all managed values are empty, the table is kept and the managed keys are not added. This is intentional for key preservation but has the clearing consequence above.
- `set_text` on an existing non-string value leaves its type tag unchanged (§4.3); a launch key that was an `Int32` and is then set to a string can make serialization fail.
- An entry created with `LaunchOption::default()` has `type == "default"`, so `ToTable(None)` always writes `type "default"` even if the caller did not ask for it. `FromTable` of an entry with no `type` yields `""`, so a read/write round-trip of such an entry does **not** add `type`. This asymmetry is intentional/faithful.
- `owns_dlc` is a string, not a bool; it is written as text (`ownsdlc "1"` etc.).

### 6.4 `ReadAll(app_body) -> Vec<LaunchOption>`

```
launch = app_body.get_table("config")?.get_table("launch")?   // both first matches
if launch is None: return []
return launch.items
    .filter(|p| p.ty == Table)
    .map(|p| LaunchOption::from_table(&p.name, p.value.as_table()))
    .collect()
```

- Returns entries in **file order**, not sorted by key. Steam's running order comes from the key, not file position, so callers that care about order must sort/renumber.
- Non-table properties under `config.launch` are ignored.
- Duplicate keys are returned as separate entries in file order.
- `config`/`launch` are case-insensitive lookups; the written names are lowercase `config`/`launch`.

### 6.5 `WriteAll(app_body, options)`

```
config = app_body.ensure_table("config")        // creates config even if absent
previous = config.get_table("launch")           // first, maybe None
launch = VdfTable::new()
for option in options:
    lookup = if !option.source_index.is_empty() { &option.source_index } else { &option.index }
    existing = previous.and_then(|p| p.get_table(lookup)).map(|t| t.clone())
    launch.items.push(VdfProperty::table(&option.index, option.to_table(existing)))
config.remove("launch")                         // all matches
config.items.push(VdfProperty::table("launch", launch))
```

- The `source_index` lookup is load-bearing. After a reorder, the **new** index differs from the key the entry was read under. Looking up by the new index would graft a different entry's unmodeled keys (e.g. Half-Life's per-entry `vacmodulefilename`) onto the wrong entry. `FromTable` sets `source_index`; newly created entries have `""`, so their `index` is used.
- Existing tables are cloned before mutation so the caller's `previous` tree isn't partially modified.
- `config.remove("launch")` removes all duplicates, then a single new `launch` table is appended at the **end** of `config`. This can reorder `config`'s keys relative to the original; the app blob is reserialized and hashed anyway, so that is acceptable.
- If `options` is empty, the result is a `config` containing `launch { }` (empty table); all previous launch entries are gone. The app is still considered edited.
- If `app_body` had no `config`, one is created even when `options` is empty.
- `WriteAll` does **not** renumber; callers must call `Renumber` first if list order is the intended running order.
- It does not validate that `option.index` is unique or numeric. Duplicate keys can be written; Steam will see duplicates. The UI's `NextIndex` avoids this normally.

### 6.6 `Renumber(options)`

```
for (i, option) in options.iter_mut().enumerate():
    option.index = i.to_string()
```

Mutates in place, list order → `"0"`, `"1"`, … . Does **not** update `source_index`. This is what makes a reorder durable: Steam takes the running order from the key, not file position (SteamEdit reads entries `OrderBy(key)` and rewrites them as `Count.ToString()`).

### 6.7 `NextIndex(options) -> String`

```
max = -1
for option in options:
    if let Ok(n) = i32::from_str(&option.index) and n > max:
        max = n
return (max + 1).to_string()
```

- Non-numeric/overflowing indices are ignored.
- Negative indices do not raise `max` above -1, so a list of only negative indices yields `"0"`.
- Gaps are deliberately ignored: `0,2,3` → `"4"`, not `"1"`. 1,892 real apps have non-contiguous launch indices; appending past the highest avoids collisions.
- Empty list → `"0"`.
- C# `int.TryParse` accepts leading/trailing whitespace and a leading `+`/`-`; decide whether to trim in Rust.

### 6.8 Serde shape

`LaunchOption` is serialized as part of `launchmods.json` with **PascalCase** property names and all fields, including `SourceIndex`; `Summary` is `[JsonIgnore]`.

```json
{
  "Index": "0",
  "SourceIndex": "0",
  "Executable": "Game.exe",
  "Arguments": "-novid",
  "WorkingDir": "",
  "Description": "",
  "Type": "default",
  "OsList": "windows",
  "OsArch": "",
  "BetaKey": "",
  "OwnsDlc": ""
}
```

Rust: `#[derive(Serialize, Deserialize)]` with `#[serde(rename_all = "PascalCase")]`, defaults for all fields (accept older files missing `SourceIndex`/`OwnsDlc`). `Summary` should be `#[serde(skip)]`.

### 6.9 Tests (existing `AppInfoCodecTests` + `LaunchModStoreTests`)

Existing:

- Read one entry: `Executable == "Game.exe"`, `OsList == "windows"`.
- Add a new entry with `NextIndex` `"1"`, `Executable "ModLoader.exe"`, `Arguments "--profile dev"`, `WorkingDir "mods"`, `Description "Play with mods"`, `Type "option1"`, `OsList "windows"`; after write/read all fields preserved.
- `EmptyOptionalFields_AreOmittedNotWrittenBlank`: `new LaunchOption { Executable="run.sh", OsList="linux" }.ToTable()` has `executable`, no `arguments`/`workingdir`/`description`, and `config.oslist == "linux"`. (`type` is also written as `"default"`.)
- `UnmodelledKeys_SurviveAnEdit`: existing `executable=hl.exe` + `vacmodulefilename=resource\sourceinit.dat`; set `Arguments="-steam"`; `ToTable(existing)` preserves `vacmodulefilename` and adds `arguments`.
- `Reorder_RenumbersKeys_AndCarriesEachEntrysUnmodelledKeysWithIt`: two entries with distinct `vacmodulefilename`; reverse list, `Renumber`, `WriteAll`; read back order/exes, and written key `"0"` has `b.dat`, `"1"` has `a.dat`.
- `NextIndex_AppendsPastTheHighestAndIgnoresGaps`: `0,2,3` → `"4"`; empty → `"0"`.

Add:

- Read entry with no `type` → `""`; write back → no `type` key.
- Entry with `type` absent vs `"default"` vs custom.
- `WriteAll` with empty options removes all previous launch entries and leaves an empty `launch` table.
- Duplicate launch keys read as two entries; `WriteAll` collapses to one per option.
- `ToTable` clearing `oslist` on an existing entry (documents the bug).
- `set_text` on an existing non-string launch key (documents serialization failure).
- Non-ASCII arguments/description survive bytes (UTF-8).

---

## 7. `AppInfo/LaunchModStore.cs` — durable staged edits

File: `AppInfo/LaunchModStore.cs`. Purpose: survive Steam regenerating `appinfo.vdf` from PICS on login/update/store browsing. Mirrors SteamEdit's `mods.dat`: keep the desired state plus a pre-edit snapshot in our own file, and re-apply when live appinfo no longer matches.

### 7.1 Data model

```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct LaunchMod {
    pub app_id: i32,                 // "AppId"
    pub change_number: u32,          // "ChangeNumber"
    pub original: Vec<LaunchOption>, // "Original" — snapshot BEFORE our first edit
    pub desired: Vec<LaunchOption>,  // "Desired"  — what the entries should look like
    pub saved_at: DateTime<Utc>,     // "SavedAt"  — serialized as ISO-8601
}
```

Semantics:

- `Original` is a **whole snapshot**, not a diff. A diff against a file Steam rewrites at will is worthless.
- `Original` is captured only the **first** time an app is modified. Re-saving must not overwrite the original with our own previous edit.
- `ChangeNumber` is appinfo's `changeNumber` at snapshot time. If Steam later ships a different one, the app has been updated and the snapshot is re-based rather than trusted.
- `Desired` is replaced on every `Save`.

### 7.2 File / JSON shape

Path: `%APPDATA%\LuaToolsGui\launchmods.json`. Options: `WriteIndented = true`, default property naming (PascalCase), no null-ignoring.

Top-level JSON is an **array** of `LaunchMod`:

```json
[
  {
    "AppId": 603750,
    "ChangeNumber": 7777,
    "Original": [ { "Index": "0", "SourceIndex": "0", "Executable": "Game.exe", "...": "..." } ],
    "Desired":  [ { "Index": "0", "SourceIndex": "0", "Executable": "Game.exe", "...": "..." } ],
    "SavedAt": "2026-09-25T07:58:20.1234567Z"
  }
]
```

System.Text.Json details for compatibility:

- Serialized property names are exactly `AppId`, `ChangeNumber`, `Original`, `Desired`, `SavedAt`, and the `LaunchOption` names.
- `DateTime` serializes in ISO-8601 round-trip form; deserialization accepts standard ISO-8601.
- Unknown properties are ignored on read.
- Missing `Original`/`Desired` use the property initializer `[]`; missing `SavedAt` uses the initializer `DateTime.UtcNow` (because the object is constructed before properties are populated). This is a quirk; a Rust port should default missing `SavedAt` to `UtcNow` or `None` deliberately.
- The default encoder escapes some non-ASCII/HTML-sensitive characters as `\uXXXX`; semantically equivalent JSON. If the Rust port must byte-match C# files, use a compatible encoder; otherwise normal UTF-8 JSON is fine.


API signatures:

```rust
impl LaunchModStore {
    pub fn new() -> Self;
    pub fn at(path: PathBuf) -> Self;                         // test seam
    pub fn is_empty(&self) -> bool;
    pub fn all(&self) -> Vec<LaunchMod>;
    pub fn get(&self, app_id: i32) -> Option<LaunchMod>;      // clone/guard, not a live ref
    pub fn save(&self, app_id: i32, change_number: u32,
                current: &[LaunchOption], desired: &[LaunchOption]);
    pub fn remove(&self, app_id: i32);
    pub fn rebase(&self, app_id: i32, current_change_number: u32,
                  current_original: &[LaunchOption]) -> bool;
    pub fn differs(a: &[LaunchOption], b: &[LaunchOption]) -> bool; // static
}
```

### 7.3 Construction / `Load`

```rust
impl LaunchModStore {
    pub fn new() -> Self { Self::at(default_path()) }
    pub fn at(path: PathBuf) -> Self;
}
```

`Load` (called by the constructor):

1. If the file doesn't exist → empty map.
2. Try `read_to_string`, `serde_json::from_str::<Vec<LaunchMod>>`.
3. If `Some(list)` → `map = list.into_iter().map(|m| (m.app_id, m)).collect()`.
   - Duplicate `AppId` makes `ToDictionary` throw (`ArgumentException`), caught by the same catch → treated as corrupt.
4. On any exception:
   - best-effort `File.Move(path, path + ".corrupt", overwrite: true)`;
   - `map = empty`.
5. If the JSON is exactly `null` (deserializes to `None`), the C# leaves the current map (empty at construction) and does **not** move the file aside. A Rust port can treat `null` as empty without quarantining.

Notes:

- The file is kept for manual recovery; it is not overwritten on the next save (the corrupt copy is at `.corrupt`, the original path is free).
- If the `.corrupt` move itself fails, the original stays and will be retried on next start.

### 7.4 `Save(app_id, change_number, current, desired)`

Lock the store for the whole operation:

```
if !map.contains_key(app_id):
    mod = LaunchMod {
        app_id,
        change_number,
        original: current.iter().map(clone).collect(),
        desired: vec![],       // set below
        saved_at: now,
    }
    map.insert(app_id, mod)

mod.desired = desired.iter().map(clone).collect()
mod.saved_at = UtcNow
persist()
```

- `Original` is captured only on first save; a second save leaves it alone.
- `ChangeNumber` is only set on first save; a second save with a different `change_number` does **not** update it (that is `Rebase`'s job). Callers pass `state.ChangeNumber` from `Read`, which is current at staging time.
- `Desired` is always replaced (cloned).
- Cloning is important: the caller may keep mutating its list.
- `Persist` can fail silently; in-memory state still updates. The UI may think the edit is durable when it isn't.

### 7.5 `Remove(app_id)`

Lock; if `map.remove(app_id).is_some()` → `persist()`. Removing an absent id is a no-op (no file write).

### 7.6 `Rebase(app_id, current_change_number, current_original) -> bool`

```
lock
if !map.contains_key(app_id) or mod.change_number == current_change_number:
    return false
mod.change_number = current_change_number
mod.original = current_original.iter().map(clone).collect()
persist()
return true
```

- Called by `LaunchOptionsService.Read` after reading live appinfo, so a later "Restore original" writes back the **current** shipped config, not a version that no longer exists.
- `Desired` is untouched; our edit survives an app update until re-applied.
- `current_original` is the live entry list, not the previous snapshot.
- Persist failure is swallowed; the in-memory rebase still happened.

### 7.7 `All`, `Get`, `IsEmpty`

- `IsEmpty`: lock; `map.is_empty()`.
- `All`: lock; `map.values().cloned().collect()` as a snapshot list. Order is the dictionary's enumeration order (insertion order in practice, not a contract). A Rust `IndexMap` preserves file order; a `HashMap` would not. If the file's order matters for deterministic output/tests, use `IndexMap`.
- `Get`: lock; returns the stored `LaunchMod` **reference**, not a clone. Callers can mutate it without a save. Rust should return a clone or a guard.

### 7.8 `Persist` (private)

```
try:
    create_dir_all(parent)
    if map.is_empty():
        if path.exists(): remove_file(path)
        return
    tmp = path + ".tmp"
    write_text(tmp, serde_json::to_string_pretty(map.values().collect::<Vec<_>>()))
    try:
        if path.exists(): copy(path, path + ".bak", overwrite=true)
    catch: // best effort
    rename(tmp, path)          // overwrite semantics
catch:
    // Losing the record is bad but must never take the app down mid-edit.
```

- Empty store → main file deleted, so no stray `[]` file. `.bak`/`.tmp` are not cleaned.
- Atomic write via temp + rename; `.bak` holds the previous main file before replacement.
- No fsync; a crash can lose the last write.
- If serialization/IO fails, the temp may be left behind and the in-memory state is still updated.
- Directory creation failure is swallowed.

### 7.9 `Differs(a, b) -> bool` (static)

```
if a.len() != b.len(): return true
for i in 0..a.len():
    x, y = a[i], b[i]
    if x.index != y.index
       or x.executable != y.executable
       or x.arguments != y.arguments
       or x.working_dir != y.working_dir
       or x.description != y.description
       or x.type != y.type
       or x.os_list != y.os_list
       or x.os_arch != y.os_arch
       or x.beta_key != y.beta_key
       or x.owns_dlc != y.owns_dlc:
        return true
return false
```

- Compares only managed fields; ignores `source_index`, `Summary`, `saved_at`.
- Order matters; a reordered list with unchanged per-index fields is not detected unless the indices/values differ per position (which they will after `Renumber`).
- Used both for dirty detection in the editor and for drift detection (`FindDrifted`).
- String comparison is ordinal/case-sensitive; null vs empty would differ (C# properties are non-null in practice).

### 7.10 Threading

- One `object _gate` lock around every read/write of `_mods`.
- `Persist` writes the whole file while holding the lock; callers on the UI thread may block on disk IO.
- `All`/`Get` return live references/snapshots as noted.
- No async/cancellation.

### 7.11 Tests (existing `LaunchModStoreTests`)

1. `Save` captures snapshot + desired, `ChangeNumber`.
2. A second `Save` does **not** overwrite `Original`; `Original[0]` still Steam's `Game.exe`, `Desired` becomes `Mod2.exe`.
3. Persists across instances (new store reads the file).
4. `Remove` forgets the game and deletes the file; reopening is empty.
5. `Rebase` refreshes `ChangeNumber` and `Original` from the live list; `Desired` count unchanged.
6. `Rebase` with the same change number returns false and leaves `Original` unchanged.
7. `Rebase` unknown app returns false.
8. `Differs` detects a changed executable, an added entry, changed arguments, changed beta key, changed index; identical → false.
9. Corrupt file (`"{ this is not json"`) → store empty, `launchmods.json.corrupt` exists.

Add:

- Missing `SourceIndex`/`SavedAt` fields (older file) deserialize with defaults.
- Duplicate `AppId` → quarantine.
- JSON `null` → empty, no `.corrupt` (documents C#).
- Empty store deletes the file; `.bak` remains if present (document).
- Persist failure (read-only dir) leaves in-memory state updated and doesn't throw.
- `Get` returning a clone/guard in Rust (no aliasing).
- Non-ASCII paths/arguments round-trip.

---

## 8. `AppInfo/LaunchOptionsService.cs` — read / stage / apply / restore

File: `AppInfo/LaunchOptionsService.cs`. Synchronous, heavy IO; the ViewModel calls `Read` inside `Task.Run` and `Apply` on the UI thread after a confirmation dialog.

### 8.1 Types

```rust
pub struct LaunchState {
    pub app_id: i32,
    pub change_number: u32,
    pub current: Vec<LaunchOption>,
    pub install_dir: Option<String>, // raw config.installdir, not a full path
    pub is_modded: bool,
}

pub struct ApplyResult {
    pub ok: bool,
    pub error: Option<String>,
    pub steam_was_running: bool,
}
```

C# `record`s with positional fields; `ApplyResult(bool Ok, string? Error = null, bool SteamWasRunning = false)`.

### 8.2 Construction / dependencies

Public constructor:

```rust
LaunchOptionsService::new(steam: &SteamService, store: LaunchModStore)
```

wires:

- `app_info_path = || steam.effective_path().map(|p| p.join("appcache").join("appinfo.vdf"))`
- `is_steam_running = SteamService::is_steam_running`
- `stop_steam = || steam.stop_steam()`
- `start_steam = || steam.start_steam()`
- `backup_dir = None` (default computed next to appinfo)

Test seam (C# `internal`): inject `app_info_path: Fn() -> Option<PathBuf>`, `is_steam_running: Fn() -> bool`, `stop_steam: Fn()`, `start_steam: Fn()`, `store`, `backup_dir: Option<PathBuf>`. Steam process control is injected so a unit test can never kill the developer's Steam.

Public accessors:

- `Store -> &LaunchModStore`
- `AppInfoPath -> Option<PathBuf>` (calls the delegate each time)
- `IsAvailable -> bool` (`AppInfoPath.is_some() && file exists`)


API signatures:

```rust
impl LaunchOptionsService {
    pub fn new(steam: Arc<SteamService>, store: LaunchModStore) -> Self;
    pub fn store(&self) -> &LaunchModStore;
    pub fn app_info_path(&self) -> Option<PathBuf>;
    pub fn is_available(&self) -> bool;
    pub fn read(&self, app_id: i32) -> Result<Option<LaunchState>, LaunchError>;
    pub fn stage(&self, app_id: i32, change_number: u32,
                 current: &[LaunchOption], desired: &[LaunchOption]);
    pub fn stage_restore(&self, app_id: i32) -> Option<Vec<LaunchOption>>;
    pub fn reapply(&self, app_ids: &[i32], restart_steam: bool) -> ApplyResult;
    pub fn apply(&self, edits: &HashMap<i32, Vec<LaunchOption>>, restart_steam: bool) -> ApplyResult;
    pub fn find_drifted(&self) -> Vec<i32>;
}
```

C# `Read` throws on IO/format errors; the Rust `Result` is the recommended shape. `LaunchError` should distinguish `NotFound`, `UnsupportedFormat(u32)`, `Io`, `Corrupt`.

### 8.3 `Read(app_id) -> Option<LaunchState>`

```
path = AppInfoPath
if path is None or !File.Exists(path): return None

file = AppInfoFile::open(path)      // may throw (format, IO)
body = file.read_app_body(app_id)
if body is None: return None

current = LaunchOption::read_all(&body)
change_number = file.entries[app_id].change_number
store.rebase(app_id, change_number, &current)

return Some(LaunchState {
    app_id,
    change_number,
    current,
    install_dir: body.get_table("config").and_then(|c| c.get_text("installdir")),
    is_modded: store.get(app_id).is_some(),
})
```

- `install_dir` is the raw `installdir` value (e.g. `"Elden Ring"`), not `<steamapps>/common/...`.
- `is_modded` is true if **any** store entry exists, even if the staged desired state equals live.
- `Rebase` is called even when the app isn't modded; it returns false immediately if the app isn't in the store.
- `Read` does not catch exceptions; the ViewModel catches `AppInfoFormatException` and general exceptions and shows an error. A Rust API should return `Result<Option<LaunchState>, Error>`.

### 8.4 `Stage` / `StageRestore`

- `Stage(app_id, change_number, current, desired)` → `store.save(app_id, change_number, current, desired)`. Does **not** touch `appinfo.vdf`.
- `StageRestore(app_id) -> Option<Vec<LaunchOption>>` → `store.get(app_id).map(|m| m.original.clone())`.
- Applying is a separate, confirmed step because it closes Steam.

### 8.5 `Reapply(app_ids, restart_steam = true) -> ApplyResult`

```
edits = {}
for app_id in app_ids:
    if let Some(mod) = store.get(app_id):
        edits[app_id] = mod.desired.clone()   // last duplicate wins
return Apply(edits, restart_steam)
```

- Ids no longer in the store are skipped rather than failing the batch: between the drift check and the user clicking "Re-apply", an app can have been restored.
- If `app_ids` is empty or nothing is staged, `edits` is empty; `Apply` still checks the appinfo path first, then returns Ok.

### 8.6 `Apply(edits, restart_steam) -> ApplyResult`

Exact order:

1. `path = AppInfoPath`; if `None` or `!File.Exists(path)` → `ApplyResult { ok: false, error: Some("Steam's appinfo cache wasn't found."), steam_was_running: false }`.
2. If `edits.is_empty()` → `ApplyResult { ok: true, error: None, steam_was_running: false }`. **Returns before checking whether Steam is running or taking a backup.**
3. `was_running = is_steam_running()`.
4. If `was_running` → `stop_steam()`. This can block up to 8 s; failures are swallowed inside `SteamService`.
   - In production `SteamService.StopSteam` swallows every per-process failure. If a test/injected `stop_steam` action itself throws, the exception propagates before the `try/finally` below (so Steam is not restarted by this method); C# behavior, not a recommended design.
5. `tmp = path + ".luatools.tmp"`.
6. `try`:
   a. `Backup(path, backup_dir.unwrap_or(default_backup_dir(path)))`.
   b. `replacements = {}`.
   c. Open `AppInfoFile::open(path)` and, for each `(app_id, options)` in `edits`:
      - `root = file.ReadApp(app_id)`; `body = root?.get_table("appinfo")`.
      - If `root` or `body` is null → `continue` (app vanished from appinfo; skip it silently).
      - `LaunchOption::WriteAll(body, options)`.
      - `replacements[app_id] = root`.
   d. If `replacements.is_empty()` → return `ApplyResult { ok: true, error: None, steam_was_running: was_running }` **without writing the temp file**. The original is untouched. The `finally` still deletes any stale tmp and restarts Steam if needed.
   e. `file.SaveAs(tmp, &replacements)` (source still open).
7. Close the source (via RAII/`using`).
8. `File::rename(tmp, path)` with overwrite. On Windows `File.Move(tmp, path, overwrite: true)` replaces the destination; on Rust `fs::rename` replaces atomically on Unix and uses `MoveFileEx`/`ReplaceFile` semantics on Windows if implemented.
9. Return `ApplyResult { ok: true, error: None, steam_was_running: was_running }`.
10. `catch ex` → `ApplyResult { ok: false, error: Some(ex.message), steam_was_running: was_running }`.
11. `finally`:
    - if `tmp` exists → best-effort delete;
    - if `was_running && restart_steam` → `start_steam()`.

Important details:

- The write-to-temp-then-swap ensures a failure mid-write cannot leave Steam with a truncated cache. The original is untouched until the move.
- If `SaveAs` fails, the temp is deleted; if `File.Move` fails, the temp is deleted; original intact.
- The backup is taken **even if** every edited app turns out to be missing (before `replacements.is_empty()` check).
- Steam is also stopped **before** the per-app lookup. If every edited app has vanished from appinfo, the method still closes Steam (and restarts it when `restart_steam`), takes a backup, and returns `ok=true` without writing. This is wasteful but not corrupting; a Rust port may pre-check app presence before stopping Steam if desired (behavior change).
- `Apply` restarts Steam even when the apply failed (as long as it was running and `restart_steam` is true), because Steam was killed at step 4. The result's `ok=false` tells the UI it failed.
- The boolean returned by `start_steam()` is ignored; `ApplyResult.ok` reflects only the file write. A failed restart is invisible to the caller.
- `Apply` does not verify that `stop_steam()` actually stopped Steam. If Steam still holds the file, `SaveAs`/`File.Move` fails and the result is `ok=false`; the original is intact.
- `Apply` does **not** call `store.remove` or update staged state; restoring is a separate caller action.
- No retry, no cancellation, no async.
- The service assumes `edits` values are full replacement lists, not diffs.

### 8.7 `Backup(path, backup_dir)` (private, static)

```
try:
    size = metadata(path).len()
    root = Path::new(&backup_dir).canonicalize()?.root   // C# Path.GetPathRoot(Path.GetFullPath)
    drive = DriveInfo::new(root)
    if drive.available_free_space < size * 2: return
    create_dir_all(backup_dir)
    dest = backup_dir / format!("appinfo-{}.vdf", Local::now().format("%Y%m%d-%H%M%S"))
    copy(path, dest, overwrite = true)
    for old in files(backup_dir, "appinfo-*.vdf")
                 .sort_by(creation_time_utc, descending)
                 .skip(1):
        try remove(old) catch {}
catch:
    // A failed backup shouldn't block the edit: the .tmp swap is the real safety net.
```

- Backups live next to `appinfo.vdf` (`<steam>/appcache/luatools-backup` by default), on Steam's drive. Deliberately not `%APPDATA%`: the file is ~373 MB and the system drive is often tight; same-drive copy is local rather than cross-volume.
- Skips if free space `< 2 * file size` (need room for the copy plus the rewritten file).
- Keeps `MaxBackups = 1` newest by **CreationTimeUtc**, not filename. Two applies in the same second overwrite the same destination (`overwrite: true`).
- All failures swallowed.
- On non-Windows, `DriveInfo`/free-space APIs differ; use `statvfs`/`fs2::available_space` and the same 2x rule.

### 8.8 `FindDrifted() -> Vec<i32>`

```
if store.is_empty() or AppInfoPath is None or !File.Exists(path): return []
try:
    file = AppInfoFile::open(path)
    drifted = []
    for mod in store.all():
        body = file.read_app_body(mod.app_id)
        if body is None: continue
        if LaunchModStore::differs(&LaunchOption::read_all(&body), &mod.desired):
            drifted.push(mod.app_id)
    return drifted
catch:
    return []   // unreadable/locked cache: nothing actionable
```

- If nothing is staged, returns immediately without touching the 373 MB file.
- Does not re-base; a change-number update alone doesn't count as drift if the launch entries still match.
- Runs off the UI thread (caller's choice); synchronous.
- A corrupt app blob throws → catch → returns `[]` for the whole batch, not partial.

### 8.9 Call-site semantics (`LaunchOptionsViewModel`)

Useful for tests/port behavior:

- On load, the editor shows the **staged desired** list if one exists; otherwise live `current`.
- Baseline for dirty detection is built from `ToOption()` of the displayed entries and `Renumber`ed, so gaps don't read as dirty.
- `Save` renumbers desired, rejects empty `Executable` (`Launch_ExecutableRequired`), stages, then asks "apply now?"; if yes calls `Apply` with `restartSteam: true`, if no shows "staged".
- `Restore` gets `StageRestore` (snapshot), applies it, then `store.Remove(appId)`.
- `CanSave` is `state != null && error == null && isDirty`; applying closes Steam, so a no-op save is worse than useless.
- `Read` takes ~2.5 s to index 373 MB; always run it off the UI thread.
- Drift notice (App.xaml.cs `CheckLaunchOptionDriftAsync`) calls `FindDrifted`, then `Reapply` with the reported ids.

### 8.10 Failure modes summary

| Situation | Result |
|---|---|
| Steam path not found | `ok=false`, `"Steam's appinfo cache wasn't found."` |
| `edits` empty but path exists | `ok=true`, `steam_was_running=false`; no Steam touch |
| app not in appinfo | skipped; if all skipped → `ok=true`, no write |
| `AppInfoFormatException` | `ok=false`, message; original intact |
| IO/disk full/permission | `ok=false`, message; original intact (temp deleted) |
| Steam was running | stopped before write; restarted iff `restart_steam` |
| Steam not running | never started |
| backup fails / low space | ignored; apply proceeds |
| `FindDrifted` on corrupt file | `[]` |
| `Read` on corrupt file | throws; caller shows error |

### 8.11 Tests (existing `LaunchOptionsServiceTests`)

Fixture: one app 603750, v29 appinfo with launch entries `("0","Game.exe","windows")` and `("1","nw","linux")`, changeNumber 7777; store at a temp path; backup dir injected; process control counted/stubbed.

1. `Read` returns 2 entries, `Game.exe`, `linux`, `InstallDir == "Test Game"`, `ChangeNumber == 7777`, `IsModded == false`.
2. `Read(999999)` → null.
3. `StageThenApply`: staging leaves appinfo at 2 entries; `Read` then `IsModded == true`; `Apply` with desired 3 entries → `ok`, read sees 3, third `ModLoader.exe` / `--profile dev`.
4. `Apply` closes/restarts only when running:
   - not running → 0 stops, 0 starts;
   - running + `restartSteam: true` → `SteamWasRunning == true`, 1 stop, 1 start;
   - running + `restartSteam: false` → 2 stops, 1 start (cumulative).
5. Restore: stage a single `Replaced.exe`, apply, read single; `StageRestore` returns original; apply original; `store.Remove`; read 2 entries, `Game.exe`, `nw`, `IsModded == false`.
6. `Apply` backs up appinfo first: exactly one `appinfo-*.vdf` in the injected backup dir.
7. `FindDrifted` empty when nothing staged.
8. `FindDrifted` spots an overwrite: apply a desired with `Arguments = "-novid"`; immediately no drift; rewrite appinfo with original entries; drift == `[603750]`.
9. `Reapply` writes staged edits and skips unstaged ids: `Reapply([603750, 999999], restartSteam:false)` → ok, args `-novid`, drift empty.
10. `Reapply` with nothing staged → ok, 0 stops (never worth closing Steam for an empty batch).
11. `Read` re-bases the snapshot after Steam updates the app: stage same current at change 7777; rewrite appinfo with change 8888 and `Game64.exe`; `Read`; store change 8888, `Original[0].Executable == "Game64.exe"`.

Add:

- `Apply` with all edited apps missing → `ok=true`, no temp, Steam restarted if it was running.
- `Apply` when path disappears between check and open → `ok=false`, message.
- Low disk space → backup skipped, apply still works (inject a free-space provider).
- Same-second backups → one file (overwrite).
- `FindDrifted` with corrupt appinfo → `[]`.
- `Read` with v27/v28 file.
- `Apply` with `restartSteam` false leaves Steam stopped.
- Unicode installdir read.

---

## 9. `ManifestFile.cs` — Steam depot `.manifest` metadata

File: `ManifestFile.cs`. A minimal, allocation-light, local-only reader for Steam's depot manifest format. It answers three questions without contacting Steam:

- which depot the file belongs to (`DepotId`);
- its true uncompressed installed size (`SizeOnDisk` = `cb_disk_original`);
- whether its filenames are still encrypted (`FilenamesEncrypted`), and (optionally) whether a supplied depot key can decrypt the first filename.

It is **not** a general protobuf decoder and does **not** parse file entries, chunks, checksums, or hashes.

### 9.1 Types and API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestInfo {
    pub depot_id: i64,
    pub filenames_encrypted: bool,
    pub size_on_disk: i64,
    pub gid_manifest: u64,
}

pub struct ManifestFile;

impl ManifestFile {
    pub fn try_read(path: Option<&Path>) -> Option<ManifestInfo>;
    pub fn matches(path: Option<&Path>, depot_id: i64, manifest_id: &str) -> bool;
    pub fn key_looks_valid(path: Option<&Path>, key: &[u8]) -> bool;
}
```

C# `readonly record struct ManifestInfo(long DepotId, bool FilenamesEncrypted, long SizeOnDisk, ulong GidManifest)`.

All three functions are synchronous, never throw, and never allocate more than the metadata/payload needed. `TryRead` seeks over the payload rather than loading the file, because the payload can be megabytes and is only needed for the key check.

### 9.2 Container layout

The manifest is a flat run of sections:

```
[magic: u32 LE][length: u32 LE][payload: length bytes]   repeated
```

Typical order:

```
PayloadMagic   0x71F617D0   ContentManifestPayload (file mappings, chunks)
MetadataMagic  0x1F4812BE   ContentManifestMetadata
EofMagic       0x32C415AB   optional end marker
```

Constants:

| Name | Value |
|---|---:|
| `PayloadMagic` | `0x71F617D0` |
| `MetadataMagic` | `0x1F4812BE` |
| `EofMagic` | `0x32C415AB` |

- Section magic and length are **little-endian u32**.
- `length` counts the section content only, not the 8-byte header.
- The file may be wrapped in a ZIP. A zipped manifest cannot be seeked through; it is inflated whole, and the first entry is treated as the manifest. Detection is only the first two bytes `'P','K'`; if the ZIP has no entries, the original bytes are used.
- `TryRead` only needs the metadata section. A file containing just a metadata section parses successfully (the `DepotCacheMigration` tests use exactly that).

### 9.3 `TryRead(path) -> Option<ManifestInfo>`

```
if path is None/blank or !File.Exists(path): return None
try:
    fs = FileStream(path, Open, Read, FileShare.ReadWrite)
    peek 2 bytes
    if peek == "PK":
        return ParseMetadata(FindSection(Unwrap(File.ReadAllBytes(path)), MetadataMagic))
    fs.Position = 0

    header = [0u8; 8]
    while fs.Position + 8 <= fs.Length:
        if fs.Read(header, 0, 8) != 8: return None
        magic = u32_le(header[0..4])
        len = u32_le(header[4..8])

        if magic == EofMagic: break
        if len > i32::MAX or fs.Position + len > fs.Length: return None   // truncated
        if magic != MetadataMagic:
            fs.Position += len
            continue

        meta = vec![0u8; len]
        n = fs.ReadAtLeast(meta, len, throwOnEndOfStream: false)
        return if n == len { ParseMetadata(&meta) } else { None }

    return None   // no metadata section
catch:
    return None   // unreadable, truncated, not a manifest
```

Notes:

- For the zip path, `FindSection` returns the first section with `MetadataMagic`; `ParseMetadata(None)` returns `None`.
- For the non-zip path, the scanner does **not** require a payload section or an EOF marker; it returns the metadata as soon as it finds it.
- `len > int.MaxValue` check prevents a negative `(int)len`; `fs.Position + len > fs.Length` catches truncation.
- If the metadata section is the last section and has fewer than 8 bytes of header remaining, it is not seen; returns `None`.
- `FileShare.ReadWrite` lets the reader open a manifest Steam may have open.
- Soft-fail is deliberate: of 2,298 manifests in one real `depotcache`, one did not parse; a single bad file must not take down a download.

### 9.4 Section scan over a byte slice (`FindSection`)

Used for zipped manifests and in `KeyLooksValid`:

```
o = 0
while o + 8 <= data.len():
    m = u32_le(data[o..o+4])
    len = u32_le(data[o+4..o+8]) as i32
    o += 8
    if m == EofMagic: break
    if len < 0 or o + len > data.len(): break       // truncated / int overflow
    if m == magic: return Some(&data[o..o+len])
    o += len
None
```

- A `len` above `i32::MAX` casts negative → break.
- No validation of section ordering or duplicates; first match wins.

### 9.5 Metadata protobuf (subset)

`ContentManifestMetadata` fields used (numbers from DepotDownloader's `manifest.proto`):

| Field # | Wire | Name | Type | Meaning |
|---:|---:|---|---|---|
| 1 | 0 | `depot_id` | varint → i64 | depot this manifest belongs to |
| 2 | 0 | `gid_manifest` | varint → u64 | manifest id (gid) |
| 4 | 0 | `filenames_encrypted` | varint → bool (`!= 0`) | filenames still encrypted |
| 5 | 0 | `cb_disk_original` | varint → i64 | installed size (authoritative) |

All other fields are skipped. The parser is:

```
ParseMetadata(meta):
    depot=0; size=0; gid=0; encrypted=false
    o=0
    while o < meta.len():
        tag = ReadVarint(o)?; field = tag >> 3; wire = tag & 7
        if wire == 0:
            v = ReadVarint(o)?
            match field:
                1 => depot = v as i64
                2 => gid = v
                4 => encrypted = v != 0
                5 => size = v as i64
                _ => {}
        else if !SkipField(o, wire):
            return None
    return Some(ManifestInfo { depot, encrypted, size, gid })
```

Details:

- `ReadVarint`: unsigned LEB128, 7 bits per byte, little-endian groups. Returns false on running off the end or if `shift > 63` (max 10 bytes).
- `ReadTag`: tag = varint; `field = tag >> 3` (cast to i32), `wire = tag & 0x07`.
- `SkipField`:
  - wire 0 → read varint;
  - wire 1 → skip 8 bytes (check `o <= len`);
  - wire 5 → skip 4 bytes (check `o <= len`);
  - wire 2 → read length varint, skip that many bytes (check `o <= len`);
  - other (groups 3/4) → false.
- Unknown fields with wire 0 are read and ignored. Unknown wire types 1/2/5 are skipped. Groups are unsupported → `None`.
- Duplicate fields: last value wins.
- Empty metadata (`len == 0`) parses successfully to `ManifestInfo { 0, false, 0, 0 }`, **not** `None`. `Matches` then fails unless the name is `0_0`.
- `v as i64` in C# is an unchecked cast; a varint above `i64::MAX` wraps negative. Rust should use `as i64` to match or reject; document.
- `SkipField`'s length-delimited branch casts the varint length to `int`; a length above `i32::MAX` becomes negative and moves `o` backwards, which can still satisfy `o <= len`. This is a malformed-input bug; Rust must use checked/size_t arithmetic and return `None`.

### 9.6 `Matches(path, depot_id, manifest_id) -> bool`

```
match TryRead(path):
    Some(info) => info.depot_id == depot_id
                  && manifest_id.parse::<u64>().is_ok_and(|gid| info.gid_manifest == gid),
    None => false,
```

- Proves the file really is the manifest its name claims (`<depot>_<gid>.manifest`), guarding against a truncated/half-written cache entry being trusted because it exists.
- `manifest_id` is a decimal string; leading zeros parse fine; overflow/non-digits → false.
- C# `ulong.TryParse` also accepts surrounding whitespace and a leading `+`; Rust `str::parse` does not. Trim if exact parity matters.
- Does not verify `size_on_disk` or encryption.

### 9.7 `KeyLooksValid(path, key) -> bool`

Purpose: opportunistically prove a depot key is the right one, **only when the manifest still has encrypted filenames** (~1.5% of cached manifests). This is a format-level validation; the end-to-end depot-key/download pipeline is excluded (§18).

```
if path is None/blank or !File.Exists(path) or key.len() != 32: return true
if TryRead(path) is not Some(info) or !info.filenames_encrypted: return true

try:
    data = Unwrap(File.ReadAllBytes(path))
    payload = FindSection(data, PayloadMagic) or return true
    name = FirstFilename(payload) or return true

    cipher = base64_decode(name) or return true          // FormatException -> true
    if cipher.len() <= 16 or cipher.len() % 16 != 0: return true
    return TryDecryptName(cipher, key)
catch:
    return true      // never block a download on this check failing to run
```

Return semantics are deliberately asymmetric:

- `true` = "no objection / a filename decrypted cleanly / nothing to test".
- `false` = "we had something testable and it failed" (bad PKCS#7 padding, or decrypted plaintext contains control bytes).
- It is **not** a verification when filenames are not encrypted: there is nothing to test, so it reports "no objection", not "verified".
- A wrong key that happens to produce valid padding and all-printable plaintext will pass (false negative); probability is low.

### 9.8 Filename cipher (`TryDecryptName`)

Steam's manifest filename cipher:

1. The first 16 bytes of the ciphertext are an **IV encrypted with AES-256-ECB** under the depot key, no padding. Decrypt those 16 bytes to get the IV.
2. The remainder is **AES-256-CBC** under the same key with that IV and **PKCS#7** padding. Decrypt.
3. If unpadding fails → `false` (wrong key).
4. Scan the plaintext bytes: any byte `< 0x20` other than `0x00` → `false` (a correct key yields a printable path; a wrong key that happens to unpad yields control bytes). NUL is allowed.
5. Otherwise `true`.

Rust crates: `aes` + `cbc` + `cipher`; or a single `openssl`/`ring` call. Key length is exactly 32 bytes (AES-256). The IV is not validated separately.

### 9.9 `FirstFilename(payload) -> Option<String>`

Payload is a protobuf; we only need the first `FileMapping.filename`:

```
o = 0
while o < payload.len():
    tag = ReadVarint(o)?; field = tag >> 3; wire = tag & 7
    if field == 1 && wire == 2:          // repeated FileMapping mappings
        len = ReadVarint(o)?
        end = o + len
        if end > payload.len(): return None
        inner = o
        while inner < end:
            tag2 = ReadVarint(inner)?; f2 = tag2 >> 3; w2 = tag2 & 7
            if f2 == 1 && w2 == 2:       // FileMapping.filename
                n = ReadVarint(inner)?
                if inner + n > payload.len(): return None
                return Some(utf8_lossy(&payload[inner..inner+n]))
            if !SkipField(inner, w2): return None
        o = end
    else if !SkipField(o, wire):
        return None
None
```

- The filename is returned **as stored**: Base64 ASCII while encrypted; raw UTF-8 (or a path with `/`) once decrypted by Steam.
- `Encoding.UTF8.GetString` replaces invalid sequences with U+FFFD; Rust `String::from_utf8_lossy`.
- Bounds checks use `payload.len()` rather than the `FileMapping`'s `end` for the filename, and the inner loop is bounded by `end`; a filename length that crosses `end` but stays within the payload is accepted (malformed input edge case).
- No validation that the filename is non-empty; an empty name leads to Base64 decode `""` → length 0 → `true`.
- `SkipField` has the same negative-length flaw as above.

### 9.10 Call sites

- `BuildsViewModel.StartDepotDownload` (line ~947): size selection order:
  1. `ManifestFile.TryRead(path)?.size_on_disk` if `> 0` (authoritative, describes the exact build being fetched);
  2. lua's `setManifestid` size (`r.LuaSize`) if an **active** pin is what's being downloaded;
  3. app-info size (`r.Size`);
  4. lua's figure anyway (possibly 0).
- `ManifestJobFactory`:
  - `KeyLooksValid(sized.ManifestPath, key)` before a depot download; failure aborts the whole job with "bad key for depot".
  - `TryRead(...).size_on_disk > 0` overrides app-info size (shared depots have no size; token-gated apps return no depot list).
- `DepotCacheMigrationService` uses `Matches(src, depotId, gid)` to refuse moving corrupt/misnamed manifests from `config\depotcache` to the real `depotcache`. A bad file in the real folder is sticky: `LuaInstaller` skips an existing destination.
- The **download pipeline** that consumes manifests/keys is EXCLUDED (§18). The parser itself is a legitimate local format reader.

### 9.11 Edge cases and tests

Existing `DepotCacheMigrationTests` (not in the user's list but directly exercises `ManifestFile`):

- Minimal manifest = 8-byte metadata header + protobuf `{field 1 depot id, field 2 gid}`; `TryRead` parses it.
- `Matches` true only when both the depot id and gid match the filename; a valid manifest under the wrong name is rejected.
- A 4-byte/1-byte file → `TryRead` null → `Matches` false.
- Name parsing rejects: no `_`, empty depot, empty gid, non-numeric depot, non-numeric gid.

Add for Rust:

- Full section sequence payload+metadata+EOF; metadata first; metadata only; EOF only; truncated section length.
- Zipped manifest with one entry; zip with multiple entries (first wins); empty zip; zip magic but invalid archive → `None` (exception caught).
- Metadata fields out of order, unknown fields with each wire type, unknown group wire → `None`.
- Duplicate depot/gid fields → last wins.
- Varint 10-byte max; overlong/truncated varint → `None`.
- `size_on_disk` > `i64::MAX` (wrap behavior documented).
- Empty metadata → zero struct, not null.
- `Matches` with leading-zero gid; overflow gid; non-numeric.
- Key check:
  - not encrypted → true;
  - encrypted + no payload → true;
  - encrypted + first filename not Base64 → true;
  - encrypted + ciphertext length not a multiple of 16 → true;
  - encrypted + correct 32-byte key → true;
  - encrypted + wrong key → false (with a deterministic fixture);
  - key length != 32 → true;
  - encrypted + control bytes after successful unpad → false (construct a key that yields this).
- Ensure no panic on malformed lengths (the C# negative-length quirks should become errors, not loops).

---

## 10. `SteamAppInfoCache.cs` — appdetails metadata, filters, overview

File: `SteamAppInfoCache.cs`. The largest module. It provides:

- a fast synchronous name/header-image lookup from a per-appid disk blob;
- a throttled appdetails fetcher with retry;
- a background backfill;
- parsed filter/overview/game-details views.

### 10.1 Records / display models

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteamAppInfo {
    pub name: String,
    pub header_image: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppFilterData {
    pub r#type: Option<String>,          // "game" | "dlc" | "demo" | ...
    pub genres: Vec<String>,             // ["Action","RPG"]; no HTML decode
    pub windows: bool,
    pub mac: bool,
    pub linux: bool,
    pub release_year: Option<i32>,
    pub release_date: Option<DateTime<NaiveDate/NaiveDateTime>>, // parsed; for newest-first sort
    pub release_date_text: Option<String>,                        // raw Steam date, e.g. "24 Feb, 2022"
    pub is_free: bool,
    pub metacritic: Option<i32>,
    pub reviews: Option<i64>,            // recommendations.total
    pub is_adult: bool,                  // content_descriptors.ids contains 3 or 4
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppOverview {
    pub short_description: Option<String>,
    pub developers: Vec<String>,
    pub publishers: Vec<String>,
    pub genres: Vec<String>,
}
```

`AppOverview` methods (computed, not stored):

- `developer_label()`: `developers.join(", ")`.
- `publisher_label()`: `publishers.join(", ")`.
- `has_description()`: short description is non-whitespace.
- `has_developer()`: developers non-empty.
- `has_distinct_publisher()`: publishers non-empty **and** `publisher_label() != developer_label()` under OrdinalIgnoreCase. Roughly 40% of apps have the same studio as developer and publisher; showing both would read as a duplicate typo.
- `has_genres()`: genres non-empty.
- `has_anything()`: `has_description || has_developer || has_genres` (publishers alone do not count).

`GameDetails` (from `Models/ApiModels.cs`, shared with the API client):

```rust
pub struct GameDetails {
    pub name: String,             // "name"
    pub app_id: i64,              // "appid" from steam_appid, else requested id
    pub r#type: String,           // "type", "" if absent
    pub base_app_id: Option<String>, // "baseAppId" from fullgame.appid (string, or raw JSON for a number)
    pub genres: Vec<String>,      // "genres" descriptions, no HTML decode
    pub header_image: Option<String>, // "headerImage"
    pub release_date: Option<String>, // "releaseDate" raw date string
}
// IsDlc => type eq_ignore_case "dlc"
```

### 10.2 Storage and in-memory indexes

Paths:

- `Dir = %APPDATA%\LuaToolsGui`
- `DetailsDir = Dir\details`
- `DetailsPath(appid) = DetailsDir\<appid>.json`

The `details/<appid>.json` file is the **single on-disk source of truth** for an app's appdetails `data` object. There is no separate `appinfo.json`. Name/header image are derived from it.

In-memory:

```rust
struct SteamAppInfoCache {
    http: reqwest::Client,                                  // 15 s
    cache: ConcurrentHashMap<i64, Option<SteamAppInfo>>,    // fast path; None = negative cache
    filter_cache: ConcurrentHashMap<i64, AppFilterData>,
    overview_cache: ConcurrentHashMap<i64, AppOverview>,
    unresolvable: ConcurrentHashMap<i64, ()>,               // log-once set
    rate_gate: tokio::sync::Mutex<()>,                      // or Semaphore(1)
    request_times: VecDeque<DateTime<Utc>>,                 // protected by rate_gate
    interactive_waiting: AtomicUsize,
    last_persist: DateTime<Utc>,
    cache_service: Arc<CacheService>,                       // for rate-window persistence
    log: Logger,
}
```

Constructor:

- `http = AppHttp.Create(15 s)`.
- Restore the rolling window from `cache_service.get_steam_api_request_times()`: for each Unix-ms timestamp, if `now - t < 200 s`, enqueue it in the given order. This prevents a restart from bursting into a still-counting window. It does **not** sort; the persisted list is in queue order.
- `_lastPersist = DateTime.MinValue` so the first accepted request persists.
- Negative-cache values are `Option::None`; `TryGetValue` distinguishes "not looked" from "looked, none".


API signatures:

```rust
impl SteamAppInfoCache {
    pub fn new(cache: Arc<CacheService>, log: Logger) -> Self;
    pub fn guess_header_image_url(appid: i64) -> String;              // static
    pub fn get_cached(&self, appid: i64) -> Option<SteamAppInfo>;
    pub fn find_app_details_entry(root: &serde_json::Value, appid: i64) -> Option<serde_json::Value>;
    pub async fn resolve(&self, appid: i64, ct: CancellationToken) -> Option<SteamAppInfo>;
    pub fn has_full_details(&self, appid: i64) -> bool;
    pub fn get_full_details(&self, appid: i64) -> Option<String>;
    pub fn get_filter_data(&self, appid: i64) -> Option<AppFilterData>;
    pub fn get_overview(&self, appid: i64) -> Option<AppOverview>;
    pub async fn resolve_game_details(&self, appid: i64, ct: CancellationToken) -> Option<GameDetails>;
    pub fn get_game_details(&self, appid: i64) -> Option<GameDetails>;
    pub async fn ensure_full_details(&self, appid: i64, ct: CancellationToken, background: bool) -> bool;
    pub async fn backfill_full_details(&self, appids: Vec<i64>, on_progress: Option<Box<dyn Fn() + Send + Sync>>, ct: CancellationToken);
}
```

`find_app_details_entry` is C# `internal static` (visible to tests via `InternalsVisibleTo`). Returning an owned `serde_json::Value` avoids the C# `JsonElement` lifetime trap (the element points into the parsed document's buffer).

### 10.3 Fast path: `GetCached(appid) -> Option<SteamAppInfo>`

```
if cache.contains_key(appid): return cache[appid].clone()   // hit, including cached None
info = read_info_from_details(appid)
cache.insert(appid, info.clone())                           // memoize hit OR miss
return info
```

`read_info_from_details(appid)`:

```
try:
    if !File.Exists(details_path(appid)): return None
    doc = JSON parse(read_to_string(details_path))
    d = doc.root
    if d is not object: return None
    name = d.get("name").and_then(as_str)       // non-string -> C# throws -> catch -> None
    if name is None or name.trim().is_empty(): return None
    image = d.get("header_image").and_then(as_str) // non-string -> throws -> None
    return Some(SteamAppInfo { name, header_image: image })
catch:
    return None      // corrupt/partial blob -> not cached
```

- A details file containing `"{}"` (the delisted marker) has no name → `None`.
- The negative null is memoized, so a missing/empty file is read at most once per process.
- The positive/negative entry is kept for the process lifetime. Replacing or deleting `details/<appid>.json` after the first `GetCached` does **not** refresh the RAM value (the same applies to `_filterCache`/`_overviewCache`).
- **Trap:** because `ResolveAsync` starts with `if let Some(cached) = cache.get(appid) { return cached.clone() }`, calling `GetCached` on an app whose details file is absent inserts `None`, and every later `ResolveAsync(appid)` returns `None` **without making a request**. This prevents resolution for the rest of the session. Call sites commonly do `GetCached(id) ?? ResolveAsync(id)`, so this is a real bug. Decide whether the Rust port fixes it (recommended: use a separate `HashSet` for "checked disk and absent" that `ResolveAsync` ignores/clears) or reproduces it. See §19.
- `GuessHeaderImageUrl(appid)` (static): `https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/header.jpg`. Works for older/delisted apps; some newer apps 404 and fall back to the appdetails `header_image`.

### 10.4 appdetails response shape

URL (both `ResolveAsync` and `EnsureFullDetailsAsync`):

```
https://store.steampowered.com/api/appdetails?appids={appid}&cc=us&l=english
```

- `cc=us` so region-blocked/banned titles still resolve; this is metadata only, not purchasing.
- No `filters=basic`; the full payload is fetched and cached.
- No custom headers.
- Response is normally `{"<key>":{"success":true,"data":{...}}}`.

Since ~Sep 2026, for an app that has child apps (DLC/soundtrack), the outer key may be a **child** appid, while `data.steam_appid` still echoes the requested appid. Examples measured in the source: 500→1660740, 730→2678630, 620→323180, 220→323140, 243470→293061, 1091500→2441600. Childless apps and every `success:false` body are still keyed by the requested id.

#### `FindAppDetailsEntry(root, appid) -> Option<JsonElement>`

This is the critical matcher. Exact algorithm:

```
if root is not object: return None

// 1. Direct key.
if root has key appid.to_string():
    direct = root[key]
    if !try_get_data_object(direct, &data):
        return Some(direct)                       // no data object: success:false and similar
    if data_names_app(data, appid):
        return Some(direct)                       // direct key whose data names our app
    // else: direct key describes another app -> fall through to scan (do NOT trust the key)

// 2. Child-keyed shape: scan for the entry whose data.steam_appid is our app.
for (key, value) in root.properties() in document order:
    if try_get_data_object(value, &data) && data_names_app(data, appid):
        return Some(value)

return None
```

Helpers:

```
try_get_data_object(entry, out data):
    entry is object
    && entry has "data"
    && data is object

data_names_app(data, appid):
    data has "steam_appid"
    && steam_appid is JSON Number
    && steam_appid.TryGetInt64() == appid
```

Rules and consequences:

- The direct key wins over the scan, so the cheap path stays the common path and a sibling entry can never shadow the app's own.
- A `success:false` entry has no `data`; it can only be matched by the direct key. It **must** be matched so the caller can write the `"{}"` delisted marker that stops backfill retrying.
- A direct key whose `data.steam_appid` is a different number, missing, or a string is **refused**, even if it is the only entry. There is deliberately **no** "exactly one entry, take it" fallback: taking the wrong game would write its name/art/genres/release date to `details\<appid>.json`, the permanent source of truth, and resolving nothing is far cheaper than poisoning the cache.
- `steam_appid` must be a JSON **Number** and integral. `"500"` (string) is not proof; refused.
- A direct key whose value is a scalar (e.g. `"nonsense"`) has no data object and is returned by the matcher. `ResolveAsync` then calls `TryGetProperty("success")` on a non-object, which throws and is caught → `None`. So the practical result is still no data.
- Root non-object (`[]`, `"..."`, etc.) → `None`.
- Empty object `{}` → `None`.
- Scan order is JSON document order; first matching child wins.
- The matcher returns a `JsonElement` pointing into the document's buffer; callers must clone/consume before the document is disposed. In Rust, return an owned `serde_json::Value` or an index/path.

Tests (`SteamAppInfoCacheTests`) exercise exactly these:

1. `{"1660740":{success:true,data:{steam_appid:500,name:"Left 4 Dead"}}}` for 500 → entry; name L4D.
2. `{"10":{...steam_appid:10...}}` for 10 → resolves.
3. `{"999999999":{"success":false}}` for 999999999 → found by key.
4. `{"1660740":{...steam_appid:1660740,name:"Uncensored"}}` for 500 → null (refuse, don't guess).
5. Unprovable: `{}`, `[]`, data without `steam_appid`, `steam_appid:"500"`, `success:true` with no `data`, entry value `"nonsense"` → null.
6. Direct key wins over scan when both exist.
7. Direct key describing another app → null.

### 10.5 `ResolveAsync(appid, ct) -> Option<SteamAppInfo>`

```
if cache.contains_key(appid): return cache[appid].clone()    // includes negative
url = appdetails_url(appid)
max_attempts = 3
for attempt in 0..3:
    await throttle(ct, background=false)
    try:
        res = http.get(url).await
        if res.status in {429 TooManyRequests, 403 Forbidden}:
            if attempt < 2:
                await sleep(4 * (attempt+1) seconds, ct)     // 4 s, then 8 s
                continue
            return None
        if !res.status.is_success(): return None

        doc = serde_json::from_slice(res.body)               // parse
        entry = find_app_details_entry(doc.root, appid)
        if entry is None:
            log_unresolvable_once(appid)
            return None                                      // cache nothing

        if entry.success != true || entry.data is not object:
            spawn(save_full_details(appid, "{}"))            // fire-and-forget
            return None

        name = entry.data.get("name").and_then(as_str)
        if name is None or name.trim().is_empty(): return None

        image = entry.data.get("header_image").and_then(as_str)
        info = SteamAppInfo { name, header_image: image }
        cache.insert(appid, Some(info.clone()))

        spawn(save_full_details(appid, entry.data.to_raw_json()))
        return Some(info)
    catch OperationCanceledException:
        return None
    catch:
        return None
return None
```

Details:

- **Retry only on HTTP 429 and 403.** Other non-success statuses (404, 500, 502) → `None` immediately. Network errors/timeouts → `None` immediately (the catch is inside the loop, not a retry). A Rust port that adds retries changes behavior; keep this exact unless deliberately improving.
- Each retry consumes another rate-limiter slot (throttle is before the try).
- Retry delays are 4 s after attempt 0 and 8 s after attempt 1; there is no jitter.
- `res.Content.ReadAsStringAsync(ct)` is parsed; an empty/malformed body → catch → `None` (no retry).
- `success:false` (or `success` missing/non-true, or `data` missing/non-object) → spawn `"{}"` write and return `None`. The `"{}"` write is **not awaited**; a subsequent `HasFullDetails` may be false briefly.
- `name` missing/blank → return `None` **without** writing `"{}"` or updating the cache. Backfill will retry forever for such an app.
- `image` non-string → `GetString` throws → catch → `None` (the whole entry is discarded, even with a good name). In Rust, decide whether to treat non-string as `None` or fail the resolve; C# fails.
- Details save is fire-and-forget; the method returns before the file is written. A crash can lose the details while the RAM cache has them.
- The C# writes `data.GetRawText()` — the exact JSON text of the inner object, preserving Steam's property order and escaping. If the Rust port parses into `serde_json::Value` and re-serializes, the bytes differ (semantically equivalent). Use `RawValue`/a raw slice if byte fidelity or diff-based tests matter.
- `SaveFullDetailsAsync` catches all IO errors; a failed save is invisible.
- `_cache[appid] = info` warms the session RAM index.
- `ResolveAsync` does **not** consult `HasFullDetails`: if a details file exists on disk but the RAM index is empty (or was never loaded), it still fetches and overwrites the file. `GetCached` is the only reader of the disk blob on this path.
- Cancellation is **split**, and this matters: `ct` is passed to the throttle, HTTP request, retry delays, and content read, but `await ThrottleAsync(ct)` is **outside** the per-attempt `try/catch`. Cancellation (or a persistence exception) during the throttle propagates **out of** `ResolveAsync`; cancellation during the HTTP/parse/retry-delay section is caught and returns `None`. Do not assume `ResolveAsync` always returns `None` on cancel. A `"{}"` write from an earlier attempt may remain.
- `HttpClient.Timeout` (15 s) surfaces as `TaskCanceledException`/`OperationCanceledException` → `None`.

### 10.6 Full-details disk API

```rust
fn has_full_details(appid) -> bool          // File.Exists(details_path)
fn get_full_details(appid) -> Option<String> // read_to_string; any error -> None
```

- `HasFullDetails` is true for a `"{}"` marker; this is what stops backfill retrying delisted apps.
- No validation that the file is complete/parseable. A partial file (crash during write) makes `HasFullDetails` true forever, and readers return `None`/parse failures. `SaveFullDetailsAsync` is a plain `WriteAllTextAsync` (no temp/atomic swap). This is a stickiness bug worth fixing in Rust (write temp + rename).
- The details file content is the inner `data` object, not the `{"appid":{...}}` wrapper.

### 10.7 `GetFilterData(appid) -> Option<AppFilterData>`

Memoized in `_filterCache`; missing details → `None` (not memoized); parse failure → `None` (not memoized).

Exact parse:

1. `type`: `d.get("type").and_then(as_str)`; non-string → C# `GetString` throws → whole parse fails → `None`.
2. `genres`: if `genres` is an array:
   - for each item: `item.get("description").and_then(as_str)`; item non-object or description non-string → throws → whole parse `None`; missing description → skip.
   - add the string as-is (no HTML decode, no trim, may be empty).
3. `platforms`: if object:
   - `windows = platforms.windows == JSON true`
   - `mac = platforms.mac == JSON true`
   - `linux = platforms.linux == JSON true`
   - non-object → all false.
4. `release_date`:
   - if `release_date` present and `release_date.date` is a string `ds`:
     - `release_date_text = if ds.trim().is_empty() { None } else { Some(ds.trim().to_string()) }`.
     - Try `DateTime.TryParse(ds, InvariantCulture, DateTimeStyles.None)`:
       - success → `release_date = Some(parsed)`, `release_year = Some(parsed.year)`.
       - failure → regex `\b(19|20)\d{2}\b` on `ds`; first match → `release_year = Some(y)`, `release_date = Some(Jan 1 of y)` (year-only sorts as Jan 1).
     - `release_date` non-object or `date` non-string → C# throws → whole parse `None`.
   - Steam formats seen: `"24 Feb, 2022"`, `"Feb 24, 2022"`, `"2022"`, `"Q1 2022"`, `"Coming soon"`.
5. `is_free = d.is_free == JSON true`.
6. `metacritic`: `d.metacritic.score` as i32; non-object/missing/non-number → `None` (non-object throws → whole parse `None`; non-number `TryGetInt32` false → `None`).
7. `reviews`: `d.recommendations.total` as i64; same pattern.
8. `is_adult`: if `d.content_descriptors.ids` is an array:
   - any element where `TryGetInt32` succeeds and value is **3 or 4** → true.
   - `content_descriptors` non-object → throws → whole parse `None`.
   - **Discrepancy:** the record comment says "id 1, 3, or 4", but the code only tests `3 or 4`. The code excludes id 1 ("Some Nudity or Sexual Content", mainstream-M) deliberately; ids: 1 some nudity/sexual, 2 frequent violence/gore, 3 adult-only sexual, 4 frequent nudity/sexual, 5 general mature. Follow the code (`3 or 4`) unless product says otherwise; flag the comment.
9. Build `AppFilterData`, memoize, return.
10. Any exception → return `None` (no memoization).

`release_date_text` is used for display ("Released 24 Feb, 2022"); `release_date` for accurate newest-first sorting; `release_year` for the year filter.

### 10.8 `GetOverview(appid) -> Option<AppOverview>`

Memoized in `_overviewCache`.

```
raw = get_full_details(appid)
if raw is None or raw.trim().is_empty() or raw == "{}": return None
try:
    d = parse(raw)
    blurb = d.get("short_description").and_then(as_str)   // non-string -> throws -> None
    if blurb is non-whitespace:
        blurb = html_decode(blurb).trim()
    overview = AppOverview {
        short_description: if blurb non-whitespace { Some(blurb) } else { None },
        developers: read_string_array(d, "developers"),
        publishers: read_string_array(d, "publishers"),
        genres: read_descriptions(d, "genres"),
    }
    overview_cache.insert(appid, overview.clone())
    return Some(overview)
catch: return None
```

`read_string_array(d, property)`:

- if the property is an array: for each item, `item.as_str()`; non-string → throws → whole overview `None`; skip whitespace-only; `html_decode(s).trim()`; add.
- non-array/missing → empty.

`read_descriptions(d, property)`:

- if array: for each item, `item.get("description").as_str()`; missing description → skip; non-string → throws → whole overview `None`; add `html_decode(s).trim()` **even if empty** (no whitespace filter).
- non-array → empty.

Notes:

- `WebUtility.HtmlDecode` decodes named/numeric entities (`&amp;` → `&`) once. ~6% of `short_description` values carry entities. Rust: `html_escape::decode_html_entities`.
- `raw == "{}"` is an **exact** comparison. `" {} "` is not treated as the marker: it parses to an empty object and returns `Some(AppOverview { None, [], [], [] })`, not `None`. Same in `GetGameDetails`; `GetFilterData` returns default data for it. Document/decide.
- The overview memoizes even an all-empty result for a parseable non-marker object.
- Parsing failures are not memoized (retried on next call).

### 10.9 `GetGameDetails(appid) -> Option<GameDetails>`

This method is **not** memoized: every call reads `details/<appid>.json` and re-parses it (unlike `GetFilterData`/`GetOverview`, which cache their parsed results). It is used by the details flyout/API client, not a hot loop.

```
raw = get_full_details(appid)
if raw is None or raw.trim().is_empty() or raw == "{}": return None
try:
    d = parse(raw)
    name = d.get("name").and_then(as_str); if blank -> None
    genres = [item.description.as_str() for genres array]   // missing description skipped; non-string throws -> None
    base_app = if d.fullgame is object and has "appid":
                  if appid is String -> GetString
                  else -> raw JSON text (e.g. "500" for a number)
               else None
    return Some(GameDetails {
        name,
        app_id: d.steam_appid.as_i64().unwrap_or(appid),
        r#type: d.type.as_str().unwrap_or("").to_string(),  // non-string -> throws -> None
        base_app_id: base_app,
        genres,
        header_image: d.header_image.as_str(),
        release_date: d.release_date.date.as_str(),
    })
catch: return None
```

- `fullgame` is how Steam identifies the parent game for DLC; `appid` may be a string or number.
- Genres are **not** HTML-decoded here (unlike `AppOverview`), matching the old proxy's field mapping.
- `release_date` non-object → throws → `None`.
- `header_image` non-string → throws → `None`.
- `app_id` falls back to the requested id if `steam_appid` is absent/non-number.
- `GetGameDetails` reads disk; `ResolveGameDetailsAsync` first calls `EnsureFullDetailsAsync`, then this.

### 10.10 `EnsureFullDetailsAsync(appid, ct, background=false) -> bool`

```
if has_full_details(appid): return true
url = appdetails_url(appid)
for attempt in 0..3:
    await throttle(ct, background)
    try:
        res = http.get(url).await
        if status in {429,403}:
            if attempt < 2 { sleep(4*(attempt+1), ct); continue }
            return false
        if !success: return false
        doc = parse(body)
        entry = find_app_details_entry(doc.root, appid)
        if entry is None:
            log_unresolvable_once(appid)
            return false                   // deliberately NOT the "{}" marker
        if entry.success != true || entry.data not object:
            await save_full_details(appid, "{}")
            return true
        await save_full_details(appid, entry.data.raw_json)
        if !cache.contains_key(appid):
            if let Some(name) = entry.data.name.as_str(), !name.is_empty():
                image = entry.data.header_image.as_str()
                cache.insert(appid, Some(SteamAppInfo { name, image }))
        return true
    catch OperationCanceledException: return false
    catch: return false
return false
```

Differences from `ResolveAsync`:

- It awaits the save (so the file is on disk when it returns true), except the "{}" marker in the failure branch is awaited too.
- Unlike `ResolveAsync`, it does **not** require a non-empty `name`: a `success:true` + `data` object with no/blank name is still saved to disk and returns true; only the RAM warm is skipped. `ResolveAsync` returns null in that case and saves nothing.
- On an unmatched response shape it returns false and writes **nothing**; the `"{}"` marker would be permanent (`HasFullDetails` true forever, no re-fetch), and poisoning a healthy game's cache is worse than a few retried requests.
- It warms `_cache` only if the key is **absent**. If `GetCached` previously inserted `None`, `ContainsKey` is true, so the negative entry is **not replaced** and `GetCached` continues to return `None` even though details are now on disk. Bug; see §19.
- The warm block calls `GetString()` on `name` and `header_image` without a ValueKind check. A non-string value throws **after** the details file has been saved, so `EnsureFullDetailsAsync` returns false for that call even though `HasFullDetails` will be true on the next one. In Rust, treat non-strings as absent and still return true.
- A `"{}"` marker is awaited and returns true (the app is considered resolved/unavailable).
- `ResolveGameDetailsAsync` calls this with `background: false` (the default), so an interactive details request takes the next throttle slot ahead of the Manage backfill.
- `await throttle(...)` is outside the `try` here too. A cancellation/persistence exception from the throttle propagates; cancellation inside the request section returns false.
- `background=true` gives the backfill lower priority in the throttle.

### 10.11 `BackfillFullDetailsAsync(appids, on_progress, ct)`

```
for appid in appids:
    if ct.is_cancelled: return
    if has_full_details(appid): continue
    await ensure_full_details(appid, ct, background=true)
    on_progress?.call()
    try await sleep(750 ms, ct)
    catch OperationCanceledException: return
```

- Sequential, not parallel.
- `on_progress` is not called for already-cached apps (the `continue` skips it).
- `on_progress` is called after an attempt even if it failed (so progress can advance and counts refresh).
- 750 ms gap keeps backfill a trickle and yields to interactive lookups. Interactive lookups share the same throttle but are prioritized.
- Stops on cancellation at the top of the loop and during the 750 ms delay. `Ensure` is **not** wrapped in a try here; a cancellation thrown by its throttle propagates out of `BackfillFullDetailsAsync` (the outer `Task.Run` in `StartCoverPrefetch` catches `OperationCanceledException`). Cancellation inside `Ensure`'s request section returns false, then `on_progress` runs and the delay throws → return.
- The caller passes library appids "most-relevant first"; order is preserved.
- If the `appids` iterator itself throws, it propagates (not caught).

### 10.12 Rate limiter / throttle (exact)

Measured behavior: appdetails allows ~200 requests in a rolling ~200-second window per IP; exceeding it returns 429 (no `Retry-After`) until the oldest request ages out (~200 s). The implementation bursts up to **190** then paces at the window edge so it never trips 429. Window timestamps persist to `cache.json` so a restart resumes the window.

Constants:

- `Window = 200 s`
- `MaxPerWindow = 190`
- `PersistInterval = 5 s`
- Interactive/background priority as below.

```
async fn throttle(ct, background):
    if !background: interactive_waiting.fetch_add(1, SeqCst)
    try:
        rate_gate.acquire(ct).await
        held = true
        try:
            loop:
                // Background yields while any interactive request is queued.
                if background && interactive_waiting.load(SeqCst) > 0:
                    rate_gate.release(); held = false
                    await sleep(50 ms, ct)
                    rate_gate.acquire(ct).await; held = true
                    continue

                now = Utc::now()
                while request_times.front() exists && now - front > 200 s:
                    request_times.pop_front()

                if request_times.len() < 190:
                    request_times.push_back(now)
                    persist_window(now)
                    return

                // At the cap: release the gate so a higher-priority request can proceed,
                // wait for the oldest to age out, re-acquire, re-check.
                wait = front + 200 s - now
                rate_gate.release(); held = false
                await sleep(if wait > 0 { wait } else { 100 ms }, ct)
                rate_gate.acquire(ct).await; held = true
        finally:
            if held: rate_gate.release()
    finally:
        if !background: interactive_waiting.fetch_sub(1, SeqCst)
```

`persist_window(now)`:

```
if now - last_persist < 5 s: return
last_persist = now
cache_service.save_steam_api_request_times(request_times.map(|t| t.unix_millis()))
```

Properties:

- The queue is purged with a strict `>` comparison: an entry exactly 200 s old remains, so the effective window is (200 s, 200 s] depending on clock granularity.
- At the cap, multiple waiters may wake together, race for the gate, and one accepts while the others loop and wait for the next oldest. No fairness guarantee.
- Interactive priority is cooperative: a background caller holding the gate releases it and sleeps 50 ms if any interactive caller is waiting. An interactive caller already waiting on the gate may still wait behind a background caller that is between checks; the 50 ms poll bounds that.
- Every **attempt** consumes a slot, including retries, 4xx/5xx responses, and network failures, because throttle runs before the request.
- Wall clock (`DateTime.UtcNow`) is used, so a backward clock jump can stall until the "oldest" appears to age out; a forward jump can burst. Do not substitute a monotonic clock without accepting a behavior change.
- The `request_times` queue and `last_persist` are only touched under `rate_gate`; `interactive_waiting` is atomic.
- The constructor restoration filters `now - t < 200 s` but does not cap the number of restored entries. If cache.json contains 1,000 recent timestamps, all are enqueued and the first request waits until enough age out.
- `save_steam_api_request_times` rewrites all of `cache.json` under `CacheService`'s lock; it serializes the timestamps as Unix milliseconds.
- A cancelled waiter (`ct`) throws `OperationCanceledException` out of `throttle`. Because both callers invoke `throttle` **outside** their per-attempt `try/catch`, it propagates out of `ResolveAsync`/`EnsureFullDetailsAsync` (and out of `BackfillFullDetailsAsync`/`ResolveGameDetailsAsync`). Only cancellation inside the HTTP/parse section is converted to null/false. The throttle's `finally` still releases the gate/decrements the counter.
- If `persist_window` throws (disk error), the exception propagates out of `throttle`. `CacheService.SaveSteamApiRequestTimes` sets the list and calls `Save`; `SaveLocked` writes without a try/catch. Because the throttle call is outside `ResolveAsync`/`Ensure`'s per-attempt try, the exception propagates to the caller (it is **not** converted to null/false). The request slot was already enqueued but the HTTP request is never made. `CacheService.Load` catches; `Save` does not.
- `CacheService.SaveLocked` deletes cache.json if all fields are empty; with rate times present it won't be empty.

### 10.13 Log-once

```
fn log_unresolvable_once(appid):
    if unresolvable.insert(appid, ()).is_none():   // first time
        warn!("appdetails: no entry with steam_appid={appid} in the response — Steam's response shape changed again. Nothing cached for this app.")
```

- Session-only; nothing persisted. A `"{}"` marker here would permanently poison a good game's cache just to save requests, so it is deliberately not written.
- Used by both `ResolveAsync` and `EnsureFullDetailsAsync` when the entry matcher refuses.

### 10.14 Threading / async / cancellation

- Designed for concurrent UI/background callers. `ConcurrentDictionary` for caches; a single `SemaphoreSlim(1,1)` for the rate window; atomic interactive counter.
- `GetCached`, `HasFullDetails`, `GetFullDetails`, `GetFilterData`, `GetOverview`, `GetGameDetails` are synchronous and do disk IO on the calling thread. WPF callers use them on the UI thread for small files; a Rust port may use blocking IO but should avoid it on an async executor or wrap in `spawn_blocking`.
- `ResolveAsync`, `EnsureFullDetailsAsync`, `BackfillFullDetailsAsync`, `ResolveGameDetailsAsync` are async.
- Fire-and-forget saves: `_ = SaveFullDetailsAsync(...)` in `ResolveAsync` (both the `"{}"` marker and the data). `EnsureFullDetailsAsync` awaits its saves.
- Cancellation tokens flow to the throttle, HTTP, and delays. Throttle cancellation propagates (outside the per-attempt try); request/parse cancellation returns null/false. `GetFullDetails`/`GetFilterData` have no cancellation.
- No lock around `_filterCache`/`_overviewCache` beyond `ConcurrentDictionary`; two callers can parse the same app twice and both memoize (last wins, same value).
- A single instance is registered as a singleton (`App.xaml.cs`), shared by Manage/Builds/Downloads/DropInstall/Home/API client.

### 10.15 Edge cases / traps summary

1. **Negative cache blocks resolve** (§10.3).
2. **Ensure doesn't replace a negative cache entry** (§10.10).
3. **Direct-key matcher refuses mismatched data**; no one-entry fallback.
4. **Retry only 429/403**, not network/5xx.
5. **"{}" vs whitespace**: exact marker only.
6. **Partial details file is sticky** (`HasFullDetails` true, parse fails).
7. **Filter cache is never invalidated** if details are re-fetched/replaced; stale until restart.
8. **`IsAdult` code uses 3/4, comment says 1/3/4**.
9. **`GetFilterData` genres are not HTML-decoded**, `GetOverview` genres are.
10. **`GetOverview` genres include empty strings**; `ReadStringArray` skips blanks.
11. **`AppOverview.has_anything` ignores publishers.**
12. **`fullgame.appid` numeric → raw JSON text** (e.g. `"500"`), string → string.
13. **Fire-and-forget saves can be lost on crash.**
14. **Rate window persistence is wall-clock and unbounded.**
15. **`ResolveAsync` returns cached `None` immediately** even if a details file exists on disk (if the null came from `GetCached` before the file appeared).

### 10.16 Tests to add

- Rate limiter with a fake clock/queue: burst 190 accepted, 191st waits; after 200 s the oldest expires; interactive before background; cancellation releases the gate; persisted window restored and ages out.
- Retry: 429 → retry with 4 s, 8 s; 403 same; 500/404 no retry; network error no retry; timeout → None.
- `FindAppDetailsEntry`: all existing cases plus direct key with data missing `steam_appid`, direct key with `success:false` and a sibling matching entry (direct wins), scalar direct key.
- `ResolveAsync` success persists the inner `data` object; failure `success:false` writes `"{}"`; no entry writes nothing; blank name writes nothing.
- `EnsureFullDetailsAsync`: existing `"{}"` returns true without network; unmatched returns false and leaves no file; success warms cache; negative cache remains (documents bug).
- `Backfill`: skips cached apps, calls progress for attempted ones, stops on cancel, 750 ms spacing (inject a clock).
- `GetCached`: missing file memoizes None; corrupt JSON → None; `"{}"` → None; valid name/image.
- `GetFilterData`: full fixture with genres/platforms/release formats (all four), free, metacritic, recommendations, descriptors 1/2/3/4/5; malformed types.
- `GetOverview`: HTML entities, blank description, duplicate developer/publisher, empty genres, `"{}"` vs `" {} "`.
- `GetGameDetails`: DLC fullgame string and number, missing steam_appid, non-string fields.
- Partial details file: `HasFullDetails` true and parse fails (document/fix).

---

## 11. `SteamAppListCache.cs` — bulk appid → name

File: `SteamAppListCache.cs`. No constructor dependencies. Singleton.

Purpose: give every appid a name without appdetails rate limits. Primary source is the Morrenus list (a single static JSON, ~341k entries including delisted apps, no rate limit); fallback is Steam's `GetAppList`. Cached to disk and refreshed every 14 days. Covers come from elsewhere.

### 11.1 API

```rust
pub struct SteamAppListCache {
    http: reqwest::Client,                       // 60 s
    names: ConcurrentHashMap<i64, String>,
    load_started: tokio::sync::OnceCell<()>,      // C# _loadTask, single-flight
}

impl SteamAppListCache {
    pub fn new() -> Self;
    pub fn get_name(&self, appid: i64) -> Option<String>;   // None if absent
    pub async fn ensure_loaded(&self);                      // safe to call repeatedly
}
```

- `GetName` is synchronous, lock-free-ish (ConcurrentDictionary), returns null if not found.
- `EnsureLoadedAsync` returns the same `Task` on repeated calls (`_loadTask ??= LoadAsync()`). C# `??=` is not atomic; concurrent first calls can start two loads. Rust: `OnceCell`/`tokio::sync::OnceCell` or a mutex.
- No cancellation token anywhere.
- No logging; every failure is swallowed.

### 11.2 Sources / URLs

| Source | URL | JSON shape |
|---|---|---|
| Morrenus (primary) | `https://applist.morrenus.xyz/` | flat array `[{"appid":10,"name":"Counter-Strike"}, ...]` |
| Steam (fallback) | `https://api.steampowered.com/ISteamApps/GetAppList/v2/` | `{"applist":{"apps":[{"appid":10,"name":"..."}, ...]}}` |

Both use `HttpClient` with a 60 s timeout and no custom headers. Morrenus is a third-party host; treat its response as untrusted JSON.

### 11.3 Disk cache

- Path: `%APPDATA%\LuaToolsGui\steam-applist.json`.
- Content: `JsonSerializer.Serialize(_names)` where `_names` is `ConcurrentDictionary<long, string>` → JSON **object** with decimal-string keys:

```json
{"10":"Counter-Strike","20":"Team Fortress Classic","220":"Half-Life 2"}
```

- Deserialization: `Dictionary<long, string>`; numeric string keys are parsed as i64. Duplicate JSON keys: last wins (System.Text.Json behavior; Rust serde_json also last-wins by default for maps unless `preserve_order`/custom).
- `MaxAge = 14 days`, determined by the file's **LastWriteTimeUtc**, not a stored timestamp. Copying/touching the file refreshes it.
- Write is a plain `File.WriteAllTextAsync` (no temp/rename). A crash can leave truncated JSON; the next load throws → falls through to download.
- Directory is created before writing; failure swallowed.
- No atomicity, no fsync.

### 11.4 `LoadAsync` exact flow

```
// 1. Fresh disk cache.
try:
    if File.Exists(cache_file) and UtcNow - File.GetLastWriteTimeUtc(cache_file) < 14 days:
        map = deserialize<HashMap<i64,String>>(read_to_string(cache_file))
        if map is Some and !map.is_empty():
            for (k,v) in map: names[k] = v
            return
catch:
    // fall through to download

// 2. Download fresh: Morrenus first, then Steam.
if await try_download(MorrenusUrl, flat_array=true)
   or await try_download(SteamUrl, flat_array=false):
    await save()
    return

// 3. Both sources failed: fall back to a stale cache if we have one.
try:
    if names.is_empty() and File.Exists(cache_file):
        map = deserialize<HashMap<i64,String>>(read_to_string(cache_file))
        if map is Some: for (k,v) in map: names[k] = v
catch:
    // give up; names fall back to lua-parsed / appid
```

Notes:

- Short-circuit `||`: if Morrenus succeeds, Steam is not contacted.
- A "success" for a source is `!names.is_empty()`, not HTTP 200. A source that parses zero valid names returns false.
- If a source partially populates `names` and then throws, `TryDownloadAsync` returns **false** but the partial names remain. The next source adds to them. If both ultimately fail, `names.is_empty()` is false, so the stale-cache fallback is skipped; the partial set is used for the session and **not saved**.
- The stale cache is loaded **regardless of age** when both sources fail.
- If the fresh cache exists but has zero entries, it is treated as unusable and a download is attempted.
- If the fresh cache parse fails, download is attempted; if that fails, the same corrupt file is re-read in the stale fallback and fails again silently.
- If the `names` map is non-empty from a previous call (LoadAsync only runs once), the stale fallback won't overwrite; not reachable via the public API.

### 11.5 `TryDownloadAsync(url, flat_array) -> bool`

```
try:
    stream = await http.get_stream(url)
    doc = await JsonDocument.ParseAsync(stream)
    apps = if flat_array { doc.root } else { doc.root["applist"]["apps"] }

    for app in apps.as_array():
        id = app["appid"].GetInt64()      // missing/non-number -> throws -> whole source false
        name = app["name"].GetString()    // missing/non-string -> throws -> whole source false
        if name is non-whitespace: names[id] = name

    return !names.is_empty()
catch:
    return false
```

- Strict object shape: a missing/non-numeric `appid` or missing/non-string `name` aborts the whole source (names added before the bad entry remain).
- Whitespace-only names are skipped (the appid is not added; a later source can add it).
- Duplicate appids: last wins within a source.
- `JsonDocument.ParseAsync(stream)` requires the entire response in memory; ~341k entries, tens of MB. A Rust streaming parser (`serde_json::Deserializer::from_reader`) would reduce peak memory; preserve the same accept/skip rules.
- No User-Agent; Morrenus may require one? The C# sends none and works.
- `GetName` returns the stored string (which is non-whitespace by construction).

### 11.6 `SaveAsync`

```
try:
    create_dir_all(parent)
    write_all_text(cache_file, serialize(names))   // indented? No: default (compact)
catch:
    // best effort
```

- Compact JSON, no indentation.
- Writes the current map even if it was populated from a stale cache? `SaveAsync` is only called after a successful download, so it writes the download result.
- No atomic replace.

### 11.7 Call-site semantics

- `BuildsViewModel` / `ManageViewModel` / `DownloadsViewModel` / `DropInstallViewModel` call `EnsureLoadedAsync` once before resolving names, then `GetName(id)` in loops.
- Name resolution order is generally: `appList.GetName(id) ?? luaName ?? appInfo.GetCached(id)?.Name ?? id`.
- `HttpServerService` also resolves this service to serve names to the injected frontend.
- `HardwareAppIdService` mirrors this module's lazy-load + stale-fallback style but stores its list in `cache.json`.

### 11.8 Failure modes / edge cases

- Offline, both sources fail, no cache → `names` empty forever; `GetName` always null. Callers fall back to lua/appid.
- Offline, both fail, stale cache → names loaded (any age).
- Fresh cache but malformed → download attempted; if offline → stale fallback tries the same malformed file → empty.
- Huge list memory: ~341k `String`s; Rust should consider `Box<str>`/interning to reduce overhead.
- Appid 0/negative in the source: accepted (no validation); `GetName(0)` would return it. The C# does not filter.
- Names with leading/trailing spaces are stored as-is (only whitespace-only is skipped); no trim.
- The cache file can be read by another process while WriteAllText is truncating it → parse failure → download.

### 11.9 Tests to add

- Fresh cache (<14 days, non-empty) → no HTTP.
- Stale cache + Morrenus success → Morrenus used, Steam not called, cache rewritten.
- Morrenus failure + Steam success → Steam used.
- Both fail + stale cache → stale loaded.
- Both fail + no cache → empty, no throw.
- Fresh cache empty → download.
- Malformed MORRENUS entry → source false, partial names retained; Steam may fill.
- Whitespace name skipped; duplicate appid last wins.
- Missing `applist.apps` → false.
- 14-day boundary (mtime exactly 14 days old → treated stale because `< MaxAge`).
- Concurrent `ensure_loaded` only one download (with a OnceCell).
- JSON cache with string keys round-trips.

---

## 12. `SteamDepotInfo.cs` — depot / branch / DLC metadata

File: `SteamDepotInfo.cs`. Fetches from `api.steamcmd.net` (the data SteamDB shows). Memory-only, session cache with a 60 s failure TTL.

### 12.1 Records

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentDepot {
    pub id: i64,
    pub size: i64,                    // manifests.public.size, 0 if absent/invalid
    pub dlc_app_id: Option<i64>,      // dlcappid; Some => this depot ships files for that DLC
    pub is_shared: bool,              // has depotfromapp
    pub os: Option<String>,           // config.oslist
    pub language: Option<String>,     // config.dlclanguage, else config.language
    pub public_manifest_id: Option<String>, // manifests.public.gid
    pub from_app_id: Option<i64>,     // depotfromapp owner (shared redistributables)
}

impl ContentDepot {
    pub fn is_dlc(&self) -> bool { self.dlc_app_id.is_some() }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDepotInfo {
    pub app_id: i64,
    pub depots: Vec<ContentDepot>,
    pub dlc_ids: Vec<i64>,            // extended.listofdlc (full declared DLC list)
    pub launch_exes: Vec<String>,     // config.launch main Windows exes, relative, backslashes
    pub public_build_id: Option<String>, // depots.branches.public.buildid
}
```

Semantics:

- `IsShared` marks a redistributable pulled from another app (VC++/DirectX). A shared depot appears in the consuming game's app-info as a three-field stub (`config`, `depotfromapp`, `sharedinstall`) with **no manifests block**, so it has neither a gid nor a size there. Both live under the owning app; `FromAppId` is kept so the download layer can look them up.
- `DlcAppId` present means the depot ships files for that DLC. `DlcIds` includes store-only entitlements with no depot.
- `PublicManifestId` is the manifest Steam currently ships on the public branch. Compare it against a lua's `setManifestid` pin to tell "latest" from "older".
- `LaunchExes` are the exact exe(s) Steam runs, used by Steamless DRM removal (consumer **EXCLUDED**, §18). The parser itself is generic metadata.
- `PublicBuildId` lets the Builds page show how far behind a pinned lua is; null when the app declares no branches.

### 12.2 Public API

```rust
pub struct SteamDepotInfo {
    http: reqwest::Client,                                  // 15 s
    cache: ConcurrentHashMap<i64, (Option<AppDepotInfo>, DateTime<Utc>)>,
    in_flight: ConcurrentHashMap<i64, Shared<Future<Option<AppDepotInfo>>>>,
    utc_now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,  // test seam
}

impl SteamDepotInfo {
    pub fn new() -> Self;                                   // shared handler, 15 s
    pub fn with_handler(handler: Client) -> Self;           // test seam
    pub async fn get(&self, app_id: i64, ct: CancellationToken) -> Option<AppDepotInfo>;
    pub fn invalidate(&self, app_id: i64);
}
```

C#:

- `private static readonly TimeSpan FailureTtl = 60 s`.
- `ConcurrentDictionary<long, (AppDepotInfo? Info, DateTime At)> _cache` — memory only.
- `ConcurrentDictionary<long, Task<AppDepotInfo?>> _inFlight`.
- `internal Func<DateTime> UtcNow { get; init; } = () => DateTime.UtcNow` (test seam).
- `SteamDepotInfo()` uses `AppHttp.SharedHandler`; `internal SteamDepotInfo(HttpMessageHandler)` builds `new HttpClient(handler){Timeout=15s}`.

### 12.3 `GetAsync` caching / dedupe

```
if cache.get(app_id) is Some((info, at)):
    if info.is_some() or utc_now() - at < 60 s:
        return info.clone()

task = in_flight.get_or_insert_with(app_id, |id| fetch(id, ct))
if task.is_completed():
    in_flight.remove(app_id)      // sweep a synchronously-completed entry
return task.await
```

Why the sweep: `ConcurrentDictionary.GetOrAdd` stores the factory's task **after** the factory returns. If `FetchAsync` completes synchronously (stubbed handler, response already buffered), its `finally` removes the in-flight entry before it is stored; the completed task would then sit in `_inFlight` forever and be served as the result of every future fetch. The `if (task.IsCompleted)` sweep runs after `GetOrAdd` has stored the task, so it cleans up. A Rust `OnceCell`/`Shared` future does not have this race if implemented correctly, but tests should cover synchronous completion.

Semantics:

- Successes are cached for the whole session; they are never expired by `FailureTtl`.
- Failures (null) are cached for 60 s, then retried. This prevents one blip/15 s timeout from giving a game a permanent "couldn't load depot info" with no way back.
- `Invalidate(app_id)` removes both the cache entry and the in-flight entry. The Builds page's Refresh calls it so "Latest build on Steam" isn't frozen at the first fetch of the session.
- Concurrency: the first caller's `ct` is captured by the factory; concurrent callers with different tokens await that same task. If the first caller cancels, all get null. `Invalidate` during an in-flight fetch removes the cache entry, but the in-flight `FetchAsync` will still call `Cache(...)` when it completes, repopulating the cache with the pre-invalidate result; it also removes `_inFlight[app_id]` in its `finally`, which may remove a **newer** task's entry. This is a known race; a Rust port can add a generation counter to ignore stale completions.
- `UtcNow` is injectable so the TTL is testable without sleeping.

### 12.4 `FetchAsync` endpoint and parse

```
GET https://api.steamcmd.net/v1/info/{appId}
```

No headers, 15 s timeout. Response shape (only the parts used):

```json
{
  "data": {
    "386940": {
      "depots": {
        "228983": {
          "config": { "oslist": "windows", "language": "english" },
          "manifests": { "public": { "gid": "999", "size": "12345" } },
          "dlcappid": "1234",
          "depotfromapp": "228980"
        },
        "branches": { "public": { "buildid": "12345678" } },
        "baselanguages": "english,german,..."
      },
      "extended": { "listofdlc": "1234,5678" },
      "config": {
        "launch": {
          "0": { "executable": "Game.exe", "type": "default", "config": { "oslist": "windows" } },
          "1": { "executable": "Game64.exe", "config": { "oslist": "windows" } }
        }
      }
    }
  },
  "status": "success"
}
```

Fetch algorithm:

1. `GET`; if `!IsSuccessStatusCode` → `Cache(app_id, None)`.
2. Parse JSON.
3. Require `root.data` and `data[app_id.to_string()]`; missing → `Cache(None)`.
4. Depots:
   - `app.depots` must be an object (if absent/non-object → no depots, not an error).
   - For each property:
     - `entry.name` must parse as `i64`; non-numeric names (`branches`, `baselanguages`, `workshopdepot`, …) are skipped.
     - `entry.value` must be an object; otherwise skipped.
     - `is_shared = value has "depotfromapp"` (presence, regardless of value).
     - `from_app_id = if is_shared { value.depotfromapp.GetString().parse::<i64>().ok() } else { None }`.
       - **If `depotfromapp` is a JSON number, `GetString()` throws → the whole app fetch fails → cached null.** The API normally sends strings.
     - `dlc_app_id = value.dlcappid.GetString().parse::<i64>()` if present; same throw-on-number behavior.
     - `os = cfg.oslist.GetString()` if `value.config` is an object; non-string throws.
     - `language = cfg.dlclanguage.GetString()` if present, else `cfg.language.GetString()`; non-string throws.
     - `size = read_public_size(value)`:
       - if `manifests.public.size` exists and `GetString().parse::<i64>()` succeeds → that; else 0.
       - missing manifests → 0. Non-string size → throws → whole app null.
     - `public_manifest_id = read_public_manifest_id(value)`:
       - if `manifests.public.gid` exists:
         - JSON String → `GetString()`;
         - otherwise → `ToString()` (raw JSON text, e.g. `"999"` for a number).
       - else `None`.
     - Push `ContentDepot { id, size, dlc_app_id, is_shared, os, language, public_manifest_id, from_app_id }`.
5. `public_build_id`:
   - if `app.depots` is object and has `branches` and `branches` has `public` and `public` has `buildid`:
     - String → `GetString()`;
     - else → `ToString()` (raw).
   - `TryGetProperty` on a non-object `branches`/`public` **throws** (there is no ValueKind check), so a malformed branches block fails the whole app.
6. `dlc_ids`:
   - if `app.extended` exists and `extended.listofdlc` is a String `csv`:
     - split on `,`, remove empty entries, trim each, parse `i64`; invalid parts silently skipped.
   - `extended` non-object or `listofdlc` non-string → throws → whole app null. Missing → empty list.
   - Duplicates are not removed; order is preserved.
7. `launch_exes = parse_launch_exes(app)` (below).
8. `Cache(app_id, Some(AppDepotInfo { ... }))`.

Error handling:

- `catch (OperationCanceledException) { return null; }` — **not cached**, so a cancelled fetch does not poison the 60 s failure window. The `finally` removes `_inFlight`.
- `catch { return Cache(app_id, None); }` — any other exception (JSON, type, network, timeout) caches a failure for 60 s.
- `finally { _inFlight.TryRemove(app_id, out _); }`.

### 12.5 `ParseLaunchExes(app)` exact rules

```
if app.config is not object or app.config.launch is not object: return []
def = []; other = []
for (_, entry) in launch.properties():
    if entry is not object: continue
    if !entry has "executable" or entry.executable.GetString() is not non-empty: continue

    is_windows = true; has_beta = false; has_owns_dlc = false
    if entry.config is object:
        if entry.config.oslist exists and GetString() is os:
            is_windows = os.contains("windows", OrdinalIgnoreCase)
        has_beta = entry.config.betakey exists && !entry.config.betakey.GetString().trim().is_empty()
        has_owns_dlc = entry.config has "ownsdlc"          // presence only
    if !is_windows || has_beta || has_owns_dlc: continue

    norm = exe.replace('/', '\\')
    is_default = entry.type exists && entry.type.GetString().eq_ignore_case("default")
    (is_default ? def : other).push(norm)

return def.concat(other).distinct_ignore_case_preserving_order()
```

- No `config.oslist` → treated as Windows (`is_windows = true`), because older games omit it.
- `oslist` containing `"windows"` anywhere (e.g. `"windows,macos"`) → Windows.
- `betakey` non-empty → excluded (dev/QA/Denuvo branch builds).
- Any `ownsdlc` key (even empty/null) → excluded (artbook/OST launchers).
- `type == "default"` entries are preferred and listed first; other qualifying entries follow in launch-object order.
- Dedupe is case-insensitive, preserving first occurrence.
- Paths are normalized to backslashes; no validation that the exe exists or stays inside the install dir.
- `GetString()` on a non-string `executable`/`oslist`/`betakey`/`type` throws → whole app fetch fails → null.
- Returns `[]` if config/launch is missing or not an object.

**EXCLUDED consumer:** `SteamlessService.ResolveExesAsync` uses these to target SteamStub DRM removal. Do not port that. The metadata parse may be retained for UI display, but do not implement the patching.

### 12.6 Sizes and IDs

- `size` is the public-branch `cb_disk_original` (installed size), a decimal string.
- A shared depot has no `manifests` block → `size = 0` and `public_manifest_id = None`; the download layer resolves both from `FromAppId`.
- `DlcAppId`/`FromAppId` are decimal strings in the API; parsed as i64.
- `PublicManifestId` is a gid string (u64 in Steam terms) but kept as `String` because it is compared/printed as an identifier and can exceed i64 in the abstract (real gids fit u64). `ManifestFile::matches` parses it as u64.

### 12.7 Call-site semantics

- Builds page `LoadDepotsAsync` (line ~1396): `await _depotInfo.GetAsync(game.AppId)`; null → `DepotError` message; `PublicBuildId` → "Latest build: {id}".
- Depot rows: size, OS, language, DLC name, shared flag, SteamDB URL (`/depot/{id}/` or `/app/{dlc}/`), public manifest id vs lua pin.
- Download picker: shared depots are identified by `FromAppId`; `ManifestJobFactory.ResolveSharedAsync` uses the owning app's depot info to get a gid/size. Owner is usually app 228980 (VC++/DirectX), so one lookup per session across downloads.
- DLC naming: `info.Depots.Where(DlcAppId != null).Select(DlcAppId).Concat(info.DlcIds).Distinct()`, then applist/appdetails names.
- Refresh button: `Invalidate(game.AppId)` before re-fetching.

### 12.8 Threading / async / cancellation

- `GetAsync` is async and lock-free (concurrent dictionaries).
- One HTTP request per appid at a time (in-flight dedupe); successes cached for the session.
- Failures cached 60 s; `Invalidate` clears.
- Cancellation: the first caller's token governs the shared fetch; cancelled → null, not cached.
- `Invalidate` can race with an in-flight fetch as described; a stale completion may repopulate.
- No background refresh/expiry; TTL is checked lazily on `GetAsync`.
- Memory-only; no disk cache. Restarting the app re-fetches.

### 12.9 Tests (existing `SteamDepotInfoCacheTests`)

Fixture: app 386940; stub handler counts calls and returns a canned body containing both 386940 and 999999, each with depot 228983 and `manifests.public.gid = "999"`.

1. Success cached for the session: 3 `GetAsync` → 1 HTTP call.
2. `Invalidate` forces the next lookup to refetch: 1 call, invalidate, get → 2 calls.
3. `Invalidate` only drops the named app: get 386940 + 999999 (2 calls), invalidate 386940, get 999999 → still 2 calls.
4. Failure retried after TTL:
   - clock 2026-08-01T12:00Z; handler 500 → null; second get → null; 1 call (cached);
   - advance 61 s; handler 200 → not null; 2 calls.
5. Success not expired by failure TTL: get success, advance 1 hour, get → not null, still 1 call.

Add:

- In-flight dedupe: two concurrent gets → 1 call; a synchronously-completing handler doesn't leave a settled task (the `IsCompleted` sweep).
- Cancellation: first caller cancels → null, not cached, next get refetches.
- `Invalidate` during in-flight (documents the stale repopulation race).
- Parse: non-numeric depot keys skipped; shared depot (`depotfromapp`) → `is_shared`, `from_app_id`, size 0; DLC depot (`dlcappid`) → `is_dlc`; `dlclanguage` precedence over `language`; public gid number vs string; public build id number vs string; listofdlc CSV with spaces/empties/invalid; launch exes filtering (no oslist, windows, mac-only, betakey, ownsdlc, default ordering, dedupe, `/`→`\`).
- Malformed: branches non-object; size non-string; depotfromapp number; listofdlc number → app null (failure cached).
- No depots block (token-gated `_missing_token: true` + null depots) → `AppDepotInfo` with empty depots, not null; callers fall back to lua declarations.

### 12.10 Relation to excluded features

`SteamDepotInfo` is metadata; the **depot download** pipeline (DepotDownloader, keys file, `depotcache` pinning, lua `setManifestid`) is adjacent and partly excluded. The module itself is needed by the Builds page even if downloading is removed: depot lists, sizes, OS/language, DLC names, and the public build label are informational. `LaunchExes` is only for Steamless (excluded); parsing it is optional.

---

## 13. `SteamLinkParser.cs` — appid from links

File: `SteamLinkParser.cs`. Static, pure.

### 13.1 API

```rust
pub fn app_id_from(text: Option<&str>) -> Option<i64>;
```

C# `public static long? AppIdFrom(string? text)`.

### 13.2 Rules

- `MaxAppId = 2_000_000_000` (exclusive upper bound).
- Accept an id only if `id > 0 && id < MaxAppId`.
- A bare number is **not** accepted. Dragged text is often incidental, and turning any stray digits into an install is too easy by accident. The link must name an app.
- Patterns are tried in this order, all case-insensitive, each with `Regex.Match` (first match anywhere in the text):

1. `steamdb\.info/app/(\d+)` — accepts `https://steamdb.info/app/648800/`, `/depots/`, `/patchnotes/`, `http://`.
2. `store\.steampowered\.com/(?:agecheck/)?app/(\d+)` — accepts named, bare, and age-gated store pages.
3. `steamcommunity\.com/app/(\d+)` — accepts community hub/discussions links.
4. `steam://(?:store|install|run|rungameid)/(\d+)` — accepts `steam://store/...`, `install`, `run`, `rungameid`.

Why the `/app/` slot matters: `steamdb.info/depot/<id>` is a **depot** id and `/sub/<id>` is a **package** id; installing either as an appid would quietly fetch the wrong game. The regexes only accept the `/app/` slot (or a `steam://` verb).

Algorithm:

```
if text is None/whitespace: return None
for regex in [steamdb, store, community, protocol]:
    m = regex.find(text)          // first match for this regex
    if m is Some and parse_i64(capture1) is Ok(id) and 0 < id < 2_000_000_000:
        return Some(id)
return None
```

Ordering subtleties:

- The loop is by **regex**, not by earliest match position. A text containing a SteamDB link and a store link returns the SteamDB id if the SteamDB regex matches anywhere, even if the store link appears first.
- Only the **first** match of each regex is considered. If that match's id is out of range (e.g. a composite `steam://rungameid/13548193523847888896`), the parser moves to the next regex, not the next match of the same regex. A later valid `steam://store/10` in the same text would be missed if the first `steam://` match was the huge rungameid.
- `\d+` can be arbitrarily long; `i64::parse` overflow → fails the range check → move on.
- No URL validation, host anchoring, or scheme requirement: `evil.example/?u=https://steamdb.info/app/1/` matches. If that matters, anchor the pattern to a URL boundary in Rust (behavior change).
- The patterns are case-insensitive; `HTTPS://STEAMDB.INFO/APP/648800/` works.
- Trailing fragments (`#section`, query strings) are ignored because the regex stops after digits.

### 13.3 Tests (existing `SteamLinkParserTests`)

Accepted:

- `https://steamdb.info/app/648800/`, `/depots/`, `/patchnotes/`, `http://steamdb.info/app/1/`
- `https://store.steampowered.com/app/648800/Raft/`, bare, `/agecheck/app/648800/`, with `?snr=...`
- `https://steamcommunity.com/app/648800/discussions/`
- `steam://store/648800`, `steam://install/648800`

Rejected:

- `https://steamdb.info/depot/648801/` (depot id)
- `https://steamdb.info/sub/12345/` (package id)
- bare `648800`, `app 648800`
- `steam://rungameid/13548193523847888896` (composite 64-bit id, not an appid)
- `https://example.com/app/648800/` (wrong host)
- `https://steamdb.info/`, `""`, `"   "`, `null`

Other:

- Text with two links takes the first **within the winning regex** (`steamdb.info/app/648800/` newline `steamdb.info/app/220/` → 648800).
- Case-insensitive.
- Fragment `#section` ignored.

Add:

- Ordering across regex types (SteamDB wins over an earlier store link) — document or change.
- Out-of-range first match followed by valid same-regex match (currently missed).
- `steam://run/`, `steam://rungameid/` valid small id.
- Id exactly 2,000,000,000 rejected; 1,999,999,999 accepted; 0 rejected.
- Query-embedded URL (currently accepted) — decide hardening.

---

## 14. `CoverCache.cs` — header image cache

File: `CoverCache.cs`.

### 14.1 API

```rust
pub struct CoverCache {
    http: reqwest::Client,                                 // 20 s
    io_gate: tokio::sync::Mutex<()>,                       // one writer at a time
    in_flight: ConcurrentHashMap<i64, Shared<Future<Option<PathBuf>>>>,
    no_cover: ConcurrentHashMap<i64, ()>,                  // session negative cache
}

impl CoverCache {
    pub fn new() -> Self;
    pub fn get_local_path(&self, appid: i64) -> Option<PathBuf>;
    pub fn is_known_missing(&self, appid: i64) -> bool;
    pub fn mark_missing(&self, appid: i64);
    pub async fn ensure(&self, appid: i64, remote_url: &str, ct: CancellationToken) -> Option<PathBuf>;
}
```

Paths:

- `CoversDir = %APPDATA%\LuaToolsGui\covers`
- `PathFor(appid) = CoversDir\<appid>.jpg`

Cache is keyed only by appid, not URL. A cover downloaded from the CDN guess and one from appdetails `header_image` share the file; the first successful one wins permanently.

### 14.2 Valid-image rules

- `MinValidBytes = 512`. Reject bodies shorter than this. The source comment claims "we don't gate on size" but the code **does**; some legitimate covers are tiny (Undertale 391540's `header.jpg` ~5 KB), so 512 is a floor, not a quality threshold.
- JPEG magic: first three bytes `FF D8 FF`. Reject HTML/error bodies that slip through with a 200.
- Placeholder fingerprint:
  - `PlaceholderLength = 9816` bytes.
  - `PlaceholderSha256 = 732ec27f2af650fe079f1c83b0bb0c712a322dc175f383504176724675ad2700`.
  - `IsHeaderCapsulePlaceholder(bytes)` returns true iff length == 9816 **and** `SHA256(bytes)` hex equals the constant (case-insensitive).
  - This is Steam's grey "Header Capsule" placeholder, served by the predictable CDN URL for apps with no CDN header yet (typically newer releases, e.g. Silent Hill f 2947440). It is a valid JPEG, so the magic check passes; the exact fingerprint rejects it so the caller falls back to appdetails `header_image` (the real art).
- The placeholder check is deliberately exact (length + hash); don't fuzzy-match.

### 14.3 `GetLocalPath(appid)`

```
path = PathFor(appid)
if !File.Exists(path): return None
try:
    if metadata(path).len() == 9816 && IsHeaderCapsulePlaceholder(read_all(path)):
        remove_file(path)
        return None          // stale placeholder removed -> caller re-resolves via header_image
catch:
    // if we can't check/delete, just use what's there
return Some(path)
```

- A cover cached as the placeholder by an older build is purged so it re-resolves to real art. Only files whose length exactly matches 9816 are hashed, so the cost is negligible.
- If the delete fails (locked), the catch falls through and returns the path; the placeholder is served.
- If the file is deleted between `File.Exists` and `FileInfo`/`ReadAllBytes`, the catch falls through and returns `Some(path)` for a file that no longer exists. A Rust port should re-check existence or return `None`.
- A zero-byte/corrupt file of another length is returned as-is (no validation).

### 14.4 `EnsureAsync(appid, remote_url, ct)`

```
path = PathFor(appid)
if File.Exists(path): return Some(path)          // no validation, no placeholder purge
if remote_url is blank: return None
return in_flight.get_or_insert_with(appid, || DownloadAsync(appid, path, remote_url, ct)).await
```

- Existing file wins even if it is a placeholder or corrupt; callers are expected to call `GetLocalPath` first (which purges placeholders).
- One download per appid even if asked concurrently (prefetch + page view).
- The first caller's `ct` is captured; later callers share it.
- **Race:** `GetOrAdd` stores the task after the factory returns. If `DownloadAsync` completes synchronously (e.g. an already-cancelled token causes `GetByteArrayAsync` to return a cancelled task, and the `await` throws before any real suspension), its `finally` removes the in-flight entry before it is stored, leaving the completed task in `_inFlight` forever. Unlike `SteamDepotInfo`, there is no `IsCompleted` sweep. Future `EnsureAsync` calls for that appid return the stale completed `None` for the session. A correct Rust `OnceCell`/`Shared` implementation avoids this; test cancellation.
- No check of `IsKnownMissing`; callers do that.

### 14.5 `DownloadAsync(appid, path, remote_url, ct)`

```
try:
    bytes = await http.get_bytes(remote_url, ct)      // 404 throws -> caught
    if bytes.len() < 512 or !is_jpeg(bytes): return None
    if is_header_capsule_placeholder(bytes): return None

    await io_gate.acquire(ct)
    try:
        create_dir_all(CoversDir)
        write_all(path, bytes, ct)                    // async write
    finally:
        io_gate.release()
    return Some(path)
catch:
    return None      // 404 / offline / write error: caller decides on fallback
finally:
    in_flight.remove(appid)
```

- Downloads run concurrently; only the file write is serialized by the semaphore.
- `CreateDirectory` and `WriteAllBytesAsync` inside the gate.
- A write failure leaves no partial file? `File.WriteAllBytesAsync` creates/truncates and writes; on failure a partial file may remain. The next `Ensure` sees `File.Exists` and returns it without validation. This is a stickiness bug; Rust should write temp + rename.
- No `MarkMissing` here; the caller decides whether a `None` is definitive (appdetails answered with no header) or transient (rate-limited/offline).

### 14.6 Negative cache

- `IsKnownMissing(appid)`: session set.
- `MarkMissing(appid)`: add.
- In-memory only; never persisted; cleared on restart.
- Grows unbounded (one entry per appid with no cover).
- Call-site rule (`ManageViewModel.ResolveCoverFileAsync`):
  1. `GetLocalPath` (purge placeholder) → return.
  2. `IsKnownMissing` → return null.
  3. `EnsureAsync(appid, GuessHeaderImageUrl(appid))` (CDN fast path).
  4. `info = appInfo.GetCached(appid) ?? await ResolveAsync(appid)`; if info has `HeaderImage`, `EnsureAsync` that URL.
  5. If local is still null **and info is not null** (appdetails answered) → `MarkMissing`. If info is null (rate-limited/offline), do **not** mark; retry on a later view.
- `LuaToolsApiClient.GetDetailsAsync` warms the cover with `EnsureAsync(id, details.header_image, CancellationToken.None)` (best effort).

### 14.7 Failure modes

| Situation | Result |
|---|---|
| 404 from CDN | `GetByteArrayAsync` throws → caught → `None`; caller falls back |
| 200 HTML/error page | not JPEG or <512 → `None` |
| 200 placeholder JPEG | exact fingerprint → `None` |
| 200 valid JPEG | written, `Some(path)` |
| write fails (disk full/locked) | `None`; possibly a partial file remains |
| cancelled | `None`; in-flight cleanup (with the stale-task caveat) |
| no URL | `None` |
| existing file | returned without validation |

### 14.8 Tests to add

- `GetLocalPath`: missing → None; valid JPEG → Some; exact placeholder → deleted + None; placeholder-length but different bytes → Some; file locked for delete → Some with placeholder (Windows semantics; on Linux simulate delete failure with a directory/permissions).
- `Ensure`: existing file → no HTTP; blank URL → None; concurrent calls → 1 HTTP; 404 → None; short body → None; HTML body → None; placeholder body → None; valid JPEG → file written and path returned; write error → None.
- Placeholder fingerprint: length-only match rejected; hash match accepted; case-insensitive hex.
- Negative cache: `MarkMissing`/`IsKnownMissing`; not persisted.
- Cancellation: pre-cancelled token → None and a subsequent call retries (documents/fixes the stale-task bug).
- Partial-file stickiness (document/fix).
- Non-ASCII appid? appids are i64, file name decimal.

---

## 15. `HardwareAppIdService.cs` — Steam hardware blacklist

File: `HardwareAppIdService.cs`. Hides Steam Deck / Valve Index / controllers / VR headsets from the Add page's featured strips and search.

### 15.1 API

```rust
pub struct HardwareAppIdService {
    gh: Arc<GithubProxy>,
    cache: Arc<CacheService>,
    ids: RwLock<HashSet<i64>>,       // C# HashSet, seeded in ctor
    load_started: tokio::sync::OnceCell<()>,
}

impl HardwareAppIdService {
    pub fn new(gh: Arc<GithubProxy>, cache: Arc<CacheService>) -> Self; // seeds from cache
    pub fn is_blacklisted(&self, app_id: i64) -> bool;
    pub async fn ensure_fresh(&self);
}
```

C#:

- `MaxAge = 14 days`.
- `JsonOpts = new JsonSerializerOptions { PropertyNameCaseInsensitive = true }`.
- `_ids = [.. cache.GetHardwareAppIds()]` in the constructor, so `IsBlacklisted` works on the first frame, before any network call, and never throws.
- `EnsureFreshAsync() => _loadTask ??= RefreshIfStaleAsync()` — at most once per session; no retry after failure.
  C# `??=` is not atomic: two concurrent first callers can both start a refresh. Rust should use `tokio::sync::OnceCell`/`OnceLock`.
- Mirrors `SteamAppListCache`'s lazy-load + stale-fallback style, but stores in `cache.json`.

### 15.2 Source / URL

`AppConfig.HardwareAppIdListUrl`:

```
https://raw.githubusercontent.com/jsnli/steamappidlist/master/data/hardware_appid.json
```

Fetched via `GithubProxy.SendAsync(url)`:

- `GithubProxy.Candidates`: direct URL first, then (because it's `raw.githubusercontent.com`) each `GithubDownloadMirrors` entry prefixed: `https://ghproxy.net/`, `https://ghfast.top/`, `https://gh.ddlc.top/`.
- Headers: `User-Agent: LuaToolsGui`, `Accept: application/vnd.github+json`.
- 5-minute timeout.
- Returns the first successful response; if all fail, the last failed response (possibly non-null) or null if every attempt threw.
- No cancellation token (default `CancellationToken.None`).

Payload shape (array of objects; only `appid` matters):

```json
[
  { "appid": 1675200, "name": "Steam Deck" },
  { "appid": 1059530, "name": "Valve Index" }
]
```

### 15.3 `RefreshIfStaleAsync` exact flow

```
fetched_at = cache.get_hardware_app_ids_fetched_at()          // Unix ms; 0 = never
fresh = fetched_at > 0 && !ids.is_empty()
        && UtcNow - DateTimeOffset::from_unix_ms(fetched_at) < 14 days
if fresh: return                                               // cached copy recent enough

try:
    res = await gh.send(url)
    if res is None or !res.is_success: return                  // keep seeded/stale set

    body = await res.content.read_as_string()
    entries = serde_json::from_str::<Vec<HardwareApp>>(body)    // case-insensitive field names
    if entries is None or entries.is_empty(): return            // empty/garbage -> don't wipe a good cache

    ids = entries.iter().filter(|e| e.appid > 0).map(|e| e.appid).distinct().collect()
    ids_set.clear(); ids_set.extend(ids)
    cache.save_hardware_app_ids(ids, UtcNow.unix_ms())
catch:
    // offline / all mirrors down / parse error -> keep existing set
```

Details:

- "Fresh" requires **both** a nonzero timestamp and a non-empty set. A cache with a timestamp but zero ids refetches; a set with no timestamp refetches.
- TTL is 14 days; the comparison is `< MaxAge` (exactly 14 days → stale).
- `entries.Count == 0` is treated as garbage: the existing set is kept, not wiped.
- `AppId > 0` filter; `.Distinct()` preserves first-occurrence order (LINQ `Distinct` is order-preserving in practice).
- `_ids.Clear()` then add: a concurrent `IsBlacklisted` could observe a partially rebuilt set. C# `HashSet` is not thread-safe; this is a benign race in practice. Rust should replace the whole set under a lock or use `ArcSwap`.
- The timestamp saved is `UtcNow` after the fetch, not the response's date.
- On failure, `EnsureFreshAsync`'s memoized task completes successfully (it swallowed), so no retry this session.
- Cache writes go through `CacheService`, which serializes the whole `cache.json` under a lock and writes temp+rename. It stores `HardwareAppIds` (list of i64) and `HardwareAppIdsFetchedAtMs` (Unix ms).
- `CacheData` property names are PascalCase by default; the cache file is shared with other services.

### 15.4 `IsBlacklisted`

`_ids.Contains(app_id)` — synchronous, no allocation, no IO, never throws.

### 15.5 Failure modes / edge cases

- No cache, offline → empty set; `IsBlacklisted` always false. The feature is a nicety, never a hard dependency.
- Cache has ids, fetch fails → stale set remains.
- Fetch returns `[]` → stale set remains.
- Fetch returns entries with missing/non-numeric `appid` → deserialization throws → catch → stale set remains.
- Fetch returns negative/zero appids → filtered out; if all filtered, `ids` is empty but `entries` was non-empty, so the code **clears** the set and saves an empty list with a fresh timestamp. Next session: timestamp > 0 but ids empty → considered not fresh → refetch. This is a minor quirk.
- Duplicate appids → deduped.
- `GithubProxy` may add mirror prefixes that return HTML error pages; `IsSuccessStatusCode` true for a 200 HTML page → JSON parse fails → catch → keep stale.
- No logging.

### 15.6 Tests to add

- Constructor seeds from cache; `is_blacklisted` true before any network.
- Fresh cache (timestamp <14 days, non-empty) → no request.
- Stale/empty/timestamp-zero → request.
- Success replaces set and saves ids+timestamp.
- Failure keeps seeded set.
- Empty array keeps set.
- Zero/negative appids filtered; all-filtered clears set (documents quirk).
- Duplicate appids deduped.
- Case-insensitive `"AppId"`/`"appid"` JSON.
- Only one refresh per session (memoized).
- `GithubProxy` direct → mirrors order, headers (with a stub server).
- Concurrent `is_blacklisted` during refresh (Rust lock/ArcSwap).

---

## 16. Hashing helpers

### 16.1 `FileHash.cs`

Purpose: prove a file on disk is still the one a fix wrote. SHA-256 rather than something faster because it is in the BCL (no dependency), hardware-accelerated, and only runs over the handful of files a fix touches — never the whole game. Disk read dominates, so a cheaper hash buys nothing measurable.

```rust
pub struct FileHash;

impl FileHash {
    pub fn sha256(path: Option<&Path>) -> Option<String>;
    pub fn matches(path: Option<&Path>, expected: Option<&str>) -> bool;
}
```

`Sha256(path)`:

```
if path is None/whitespace: return None
try:
    fs = FileStream(path, Open, Read, FileShare.ReadWrite, buffer=64 KiB, SequentialScan)
    return hex_lower(SHA256(fs))       // read to EOF
catch:
    return None
```

- Null on any failure, never an exception: a locked or vanished file degrades to "cannot verify"; the caller decides, and a revert isn't aborted halfway.
- `FileShare.ReadWrite` allows reading a file another process holds open for read/write (common for game files). On Windows, a file opened with no sharing still fails → `None`.
- Lowercase hex, 64 chars.
- No retry.
- If the file changes during the read, the hash is of the bytes actually read (no snapshot/lock).

`Matches(path, expected)`:

```
if expected is None or expected.is_empty(): return true
return sha256(path).map_or(false, |h| h.eq_ignore_ascii_case(expected))
```

- **True when `expected` is null or empty** — an absent hash means the record predates hashing, which is "nothing to check", not "check failed". Callers rely on this to keep reverting fixes applied by older builds.
- Hex casing is ignored.
- Missing file + non-empty expected → `None == expected` false → mismatch.
- Expected is **not trimmed**: `" "` is non-empty and won't match; `"sha256:..."` is not stripped and won't match.
- A whitespace-only expected is not treated as absent.

Tests (`FileHashTests`):

- Known vector: `"abc"` → `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`.
- Missing file → null.
- null path / `"  "` → null.
- Readable while another `FileStream` with `FileAccess.ReadWrite, FileShare.ReadWrite` is open.
- Same content → matches; changed content → false.
- Hex case ignored (`ToUpperInvariant` expected still matches).
- `expected = null` and `""` → true (pre-hash records).
- Missing file + a real expected hash → false.

Call sites: `ManifestJobFactory` apply/revert (Denuvo fix pipeline — **EXCLUDED**, §18). The hashing utility itself is generic and should be ported if any file-provenance feature remains.

### 16.2 `AssetHash.cs`

C# `internal static class AssetHash` — not part of the public API. Used to verify downloaded GitHub release assets before extracting/running them.

```rust
pub(crate) struct AssetHash;

impl AssetHash {
    pub fn of_file(path: &Path) -> Result<String, std::io::Error>;
    pub fn parse_digest(digest: Option<&str>) -> Option<String>;
    pub fn matches(path: &Path, asset_digest: Option<&str>) -> Result<bool, std::io::Error>;
}
```

`of_file(path)`:

```
using s = File.OpenRead(path)          // FileShare.Read; throws if missing/locked
return hex_lower(SHA256(s))
```

- Unlike `FileHash`, this **throws** on IO failure. Callers in download services wrap it in try/catch.
- Lowercase hex.

`parse_digest(digest)`:

```
if digest is None/whitespace: return None
colon = digest.find(':')
s = if colon >= 0 { &digest[colon+1..] } else { digest }
return Some(s.trim().to_lowercase())
```

- Strips any prefix before the first colon (`sha256:`, `sha512:`, `foo:`), not just `sha256:`.
- No colon → the whole string, trimmed/lowercased.
- `"sha256:"` → `Some("")` (empty, **not** null).
- No length/hex validation.

`matches(path, asset_digest)`:

```
want = parse_digest(asset_digest)
if want is None: return true                  // nothing to check (older release)
return of_file(path).eq_ignore_ascii_case(want) ? Ok(true) : Ok(false)
```

- Absent digest → true (an older release without one is not corrupt).
- Present digest + missing file → **throws** (a deliberate difference from `FileHash`).
- `"sha256:"` (empty want) → computes the hash and compares to `""` → false.
- Case-insensitive.

Tests to add:

- Known vector for `of_file`.
- `parse_digest`: `"sha256:ABC"` → `"abc"`, `"ABC"` → `"abc"`, `"sha256: abc "` → `"abc"`, `"sha512:..."` → value, `"sha256:"` → `Some("")`, null/empty/whitespace → None, `":abc"` → `"abc"`.
- `matches`: absent digest → true without reading the file (missing file OK); matching digest → true; mismatched → false; missing file + digest → error/throw.
- Uppercase digest.

Call sites: `UnlockerService`, `PluginInstallerService`, `DepotDownloaderService`, `SteamlessService`, `SteamAutoCrackService` — all download-and-run tool pipelines, several **EXCLUDED** (§18). If the port keeps any GitHub asset download (e.g. a metadata-only tool), keep this helper.

### 16.3 Relationship to other digests

| Use | Algorithm | Output | Failure |
|---|---|---|---|
| Fix file provenance | SHA-256 | lowercase hex | null |
| GitHub asset digest | SHA-256 (prefix stripped) | lowercase hex | throws |
| appinfo `sha1_binary` | SHA-1 over the reserialized blob | 20 raw bytes in meta | exception |
| appinfo `sha1_text` | Steam's own text hash | 20 raw bytes, never recomputed | n/a |
| Cover placeholder | SHA-256 over 9816 bytes | hex compare | n/a |
| Manifest sections | no checksums parsed | n/a | n/a |

Rust crates: `sha1`, `sha2`, `hex`.

---

## 17. `Downloads/ByteFormat.cs` — display formatting

File: `Downloads/ByteFormat.cs`. Static, culture-invariant, English unit suffixes (deliberately not localized, matching the existing depot-size helpers).

```rust
pub struct ByteFormat;

impl ByteFormat {
    pub fn size(bytes: i64) -> String;
    pub fn rate(bytes_per_second: f64) -> String;
    pub fn duration(t: Duration) -> String;
}
```

### 17.1 `Size(bytes)`

```
if bytes < 0: bytes = 0
gb = bytes as f64 / 1024^3
if gb >= 1: return format_fixed(gb, "0.##") + " GB"   // at most 2 decimals, trailing zeros trimmed
mb = bytes as f64 / 1024^2
if mb >= 1: return format_fixed(mb, "0.#")  + " MB"   // at most 1 decimal, trailing zero trimmed
kb = bytes as f64 / 1024
if kb >= 1: return format_fixed(kb, "0")    + " KB"   // integer, rounded
return bytes.to_string() + " B"
```

- Never returns an empty string.
- `0` → `"0 B"`.
- Units are binary (1024), not SI (1000).
- Thresholds are `>= 1` in the next unit, so `1024^3 - 1` bytes falls into MB and can render as `"1024 MB"` (because `mb = 1023.999... >= 1` and the MB format rounds to `"1024"`). Similarly `1024^2 - 1` → `"1024 KB"`. This is the current behavior; do not add a "promote if rounded to 1024" rule unless tests/UX demand it.
- `.NET` custom numeric formats: `0.##` rounds to at most 2 fractional digits and omits trailing zeros; `0.#` at most 1; `0` no fractional digits. Use a decimal formatter with the same pattern and **golden tests** for values near rounding boundaries (e.g. 1.005, 1.25, 1.5); .NET and Rust may choose different half-way rounding.
- Culture is `InvariantCulture`: decimal point `.`, ASCII digits.
- Negative clamps to 0.

Reliable examples:

| Input | Output |
|---:|---|
| 0 | `0 B` |
| 1 | `1 B` |
| 512 | `512 B` |
| 1023 | `1023 B` |
| 1024 | `1 KB` |
| 1536 | `2 KB` |
| 1024*1024 - 1 | `1024 KB` |
| 1024*1024 | `1 MB` |
| 1.5 MB | `1.5 MB` |
| 1024^3 | `1 GB` |
| 1.1 GB | `1.1 GB` |
| 1.23 GB | `1.23 GB` |

### 17.2 `Rate(bytesPerSecond)`

```
if bytesPerSecond.is_nan() or bytesPerSecond <= 0: return ""
return Size(bytesPerSecond as i64) + "/s"
```

- Empty when the rate is not yet measurable (NaN or non-positive), so callers can hide the label.
- Cast truncates toward zero: `0.5` → `Size(0)` → `"0 B/s"` (not empty); `-1` → empty.
- Infinity is not guarded: C# `(long)double.PositiveInfinity` is an unspecified/out-of-range conversion (typically `long.MinValue`), then `Size` clamps negative → `"0 B/s"`. Rust `f64 as i64` saturates to `i64::MAX`; to match, handle non-finite explicitly (return `""` or `"0 B/s"` deliberately).
- `"/s"` is a unit symbol, unlocalized.
- Example: `4.2 MB/s` for `4.2 * 1024*1024`.

### 17.3 `Duration(TimeSpan)`

```
if t <= 0 or t.total_days() >= 1: return ""
if t.total_hours() >= 1: return format!("{}h {}m", t.total_hours() as i32, t.minutes())
if t.total_minutes() >= 1: return format!("{}m {}s", t.total_minutes() as i32, t.seconds())
return format!("{}s", max(1, ceil(t.total_seconds()) as i32))
```

- Zero, negative, or >= 1 day → empty string ("absurd duration").
- `(int)` casts truncate toward zero; `t.Minutes`/`t.Seconds` are the component values (0..59).
- Under 1 minute, seconds are **rounded up** and floored at 1 (so 0.1 s → `"1s"`, 59.5 s → `"60s"`).
- Minutes branch: 60 s → `"1m 0s"`; 90 s → `"1m 30s"`; 3599 s → `"59m 59s"`.
- Hours branch: 3600 s → `"1h 0m"`; 3661 s → `"1h 1m"`; 86399 s → `"23h 59m"`; 86400 s → `""`.
- Negative/zero → `""`.
- The C# `TimeSpan` is a duration, not a wall-clock time; Rust `std::time::Duration` cannot be negative, so accept a signed milliseconds/i64 or a custom duration for the negative case, or document that negative maps to empty before conversion.

### 17.4 Call sites / tests

- `DownloadItem.SizeLabel`: `"412 MB of 1.4 GB"` or just `"412 MB"` when total unknown; empty if `BytesRead <= 0 && TotalBytes is not > 0`.
- `DownloadItem.RateLabel`: only while `Downloading`; `ByteFormat.Rate(BytesPerSecond)`.
- `DownloadItem.EtaLabel`: only while `Downloading` and `Eta` present; `ByteFormat.Duration`; empty if the duration format is empty.
- `DownloadsViewModel` cancel dialog: `ByteFormat.Size(item.BytesRead)`.
- `ManifestJobFactory` / `BuildsViewModel`: sizes in error/confirmation text.
- `DownloadHistory.SizeLabel`: `Bytes > 0 ? Size(Bytes) : ""`.
- No dedicated `ByteFormat` tests exist (`DownloadRateTests` tests `DownloadItem` rate math, not formatting). Add golden tests for size thresholds, rate NaN/0/0.5/Infinity, and every duration branch.

### 17.5 Why separate from the depot-size formatters

`DownloadViewModel`, `BuildsViewModel`, and `DropInstallViewModel` have their own `FormatSize` helpers for **depot diff rows**: they floor at MB and return `""` for zero. That is right for a depot list but wrong for live progress, where a small lua file is a few KB and `0` must render as `"0 B"`, not vanish. `ByteFormat` is the progress formatter; do not merge the two without checking both UIs.

---

## 18. Excluded features / compliance flags

The following are **Steam DRM circumvention, ownership spoofing, or crack tooling** and must **not** be ported. They are listed here only so an implementer recognizes the boundaries and does not accidentally pull them in through a shared helper.

### 18.1 EXCLUDED — SteamTools `stplug-in` Lua / ownership spoofing

- `LuaInstaller`, `LuaVault`, `LuaFileParser` (stplug-in parsing), `LuaEditor`, `LuaBuild`, `ModeViewModel`'s lua install/apply flows.
- Any code that writes `<Steam>\config\stplug-in\<appid>.lua` or manipulates `addappid`/`setManifestid` lines to make Steam grant entitlements it does not own.
- `SteamService.StPlugInDir` is only a path helper. **Do not implement the stplug-in Lua pipeline.** If the Rust port retains a file-browser feature that displays this folder, it may keep the path, but no Lua generation/installation.
- `SteamLibraryService` is used by the Denuvo-fix page; the **Denuvo fix apply/revert pipeline** (`ManifestJobFactory` fix paths, `AppliedFixIndexService`, `FixesViewModel`) is crack tooling. The generic "find the installed game folder" logic is fine to port for non-crack features; do not port the fix application/revert.

### 18.2 EXCLUDED — Unlockers / ownership injection

- `UnlockerService` (OpenSteamTools, Bst, CloudRedirect, custom unlocker install/update).
- `CefInjectorService` / injected frontend / `HttpServerService` unlocker endpoints.
- `SteamAutoCrackService`, `SteamlessService` (SteamStub DRM removal), `CloudRedirectService`, `ModeMigration`, `ProtocolService` (if it registers unlocker protocols).
- `DonateKeysService` (depot key extraction/donation).
- `DepotDownloaderService` / `ManifestJobFactory` depot-download path that uses decrypted depot keys to fetch content the user does not own. The **format readers** (`ManifestFile`, `SteamDepotInfo`) are specified because they are local metadata parsers and are requested; do not port the key-file generation, DepotDownloader invocation, `setManifestid` pinning, or `stplug-in` integration.

### 18.3 EXCLUDED — Crack tooling

- SteamAutoCrack, Steamless (DRM stripping), Denuvo "fix" archives, Goldberg emulator injection, any executable patching, any `steam_api(64).dll` replacement.
- `AssetHash` is a generic digest helper. It appears in those services; porting the helper alone is fine, but do not port the services that download and run crack tools.
- `FileHash` is used by the Denuvo fix revert. The hashing utility is generic; the fix record/apply/revert pipeline is excluded.

### 18.4 Retained / legitimate

- Steam install discovery and installed-game lookup (for display/library management).
- appinfo **launch option** editing (SteamEdit-style: add/remove/reorder launch entries; no ownership or DRM changes). This modifies Steam's own configuration for games the user owns; it does not grant entitlements.
- appdetails metadata, names, covers, filters, overview, release dates.
- Depot/branch/DLC metadata display (sizes, languages, OS, public build id). Do not use it to fetch content.
- Bulk app name list, hardware blacklist.
- Link parser.
- Display formatting.
- Manifest metadata parsing and key **validation** (format-level). Do not build a downloader around it.
- File/asset hashing helpers.

### 18.5 Flagged but retained with caveats

| Item | Why it is flagged | Port decision |
|---|---|---|
| `ManifestFile.KeyLooksValid` | Validates a depot decryption key against an encrypted filename; only useful for depot content decryption | Specified as format validation; do not integrate with a key/download pipeline |
| `SteamDepotInfo.LaunchExes` | Exists to target Steamless DRM removal | Parse only if needed for display; do not run Steamless |
| `SteamService.StPlugInDir` | Points at SteamTools' entitlement Lua folder | Path helper only; no Lua pipeline |
| `SteamLibraryService` | Used by Denuvo fix apply/revert | Keep the generic lookup; exclude fix application |
| `SteamService.StopSteam`/`StartSteam` | Used by unlocker/plugin install flows | Generic process control; safe to keep for launch-option apply |
| `AppInfoFile.SaveAs` | Can rewrite appinfo; used by launch-option editor | Keep for launch options; do not add ownership fields |

If a requested module cannot be separated cleanly from an excluded consumer, prefer omitting the consumer and keeping the pure parser/helper.

---

## 19. Ambiguities, bugs, and decisions for the Rust port

These are places where the C# behavior is surprising, inconsistent, or likely a bug. Each needs an explicit decision; the recommendation is not automatically "fix", because tests/UX may depend on current behavior.

### 19.1 `SteamAppInfoCache` negative cache blocks resolution

- `GetCached(appid)` memoizes `None` when `details/<appid>.json` is absent.
- `ResolveAsync(appid)` starts with `if (_cache.TryGetValue(appid, out var cached)) return cached;` — so it returns `None` immediately and **never fetches**.
- Call sites do `GetCached(id) ?? await ResolveAsync(id)` (Manage tile names/covers, DropInstall), so an app first looked up via `GetCached` can never resolve in that session.
- Recommendation: distinguish "not looked" from "looked, absent" (e.g. `HashMap<i64, CacheEntry>` with a `Negative` variant that `ResolveAsync` bypasses), or have `ResolveAsync` ignore negative entries. Add a regression test: `GetCached(missing)` then `ResolveAsync` must make a request.

### 19.2 `EnsureFullDetailsAsync` doesn't replace a negative cache entry

- It warms `_cache` only `if (!_cache.ContainsKey(appid))`.
- If `GetCached` inserted `None`, `ContainsKey` is true; after a successful fetch, `GetCached` still returns `None` forever, even though `GetFilterData`/`GetOverview` read the new file.
- Recommendation: on successful save, `_cache.insert(appid, Some(info))` unconditionally (or remove the negative entry). Test both orders.

### 19.3 `LaunchOption.ToTable` cannot clear managed config keys

- When all four managed config values are empty, `any_config` is false and the code only removes `config` if it is empty. Existing `oslist`/`osarch`/`BetaKey`/`ownsdlc` remain.
- Recommendation: always `set_text` the four managed keys (which removes empties) while preserving unmodeled config keys; add a test that clearing `oslist` actually removes it. If faithful behavior is required, document it in the UI.

### 19.4 `VdfTable.SetText` leaves an existing property's type tag unchanged

- Setting a key that exists as `Int32` replaces its value with `byte[]` but leaves `Type == Int32`; `BinaryVdf.Write` then casts bytes to int and throws.
- Recommendation: coerce `Type = String` on `SetText`, or return an error if the existing type isn't `String`. Add a test.

### 19.5 `AppFilterData.IsAdult` comment vs code

- Record comment: "has a nudity/sexual Steam content descriptor (id 1, 3, or 4)".
- Code: `id is 3 or 4` only. The comment explains why id 1 is excluded (mainstream-M) and ids 2/5 are excluded.
- Recommendation: follow the code (`3 || 4`) and fix the comment. Confirm with product before changing.

### 19.6 `CoverCache` size-gate comment vs code

- Comment: "We don't gate on a size threshold".
- Code: `MinValidBytes = 512` and rejects `bytes.Length < 512`.
- Recommendation: keep the 512 floor (it rejects truncated bodies) and fix the comment; add a test for 511/512 bytes.

### 19.7 `SteamLibraryService` is a regex, not a VDF parser

- It only understands modern `"path"` keys; old numbered formats are unsupported.
- It matches `"path"`/`"installdir"`/`"name"` inside comments or unrelated nested blocks.
- It doesn't handle `\"` or other escapes.
- Recommendation: implement a small proper KeyValues text parser (quoted tokens, `//` comments, nested objects) and extract the same fields. Keep the regex behavior as a compatibility test, or decide the old format is out of scope. A proper parser is less code than debugging regex edge cases.

### 19.8 Windows-only assumptions

- Registry, `Process.GetProcessesByName`, `explorer.exe`, WPF clipboard, drive free space, case-insensitive paths.
- The C# has no Linux fallback. If the Rust port targets Linux, define:
  - Steam root detection (`~/.steam/steam`, `~/.local/share/Steam`, `STEAM_ROOT` env);
  - no registry → `None`;
  - process control via `/proc` or `sysinfo`;
  - `steam.exe` check becomes `steam.sh`/`steam` (decide);
  - paths are case-sensitive; Steam on Linux still uses Windows-style game folders under `steamapps/common`, but `libraryfolders.vdf` may contain `/` paths.
- Do not leave a half-ported registry call behind a `cfg!(windows)` that silently returns None on Linux without documenting it.

### 19.9 `SteamDepotInfo` malformed-field strictness

- `GetString()` on a non-string `depotfromapp`, `dlcappid`, `oslist`, `dlanguage`, `size`, `listofdlc`, `betakey`, `executable`, `oslist`, or `type` throws and aborts the **whole app** (cached null for 60 s).
- `branches`/`public`/`extended` non-object also throws.
- This is brittle against API shape changes. Recommendation: decide per field whether to fail the app or treat as absent. For resilience, treat non-strings as absent and keep the rest; note the behavior change. At minimum, add tests for each non-string case.

### 19.10 `SteamDepotInfo.Invalidate` in-flight race

- An in-flight `FetchAsync` can complete after `Invalidate` and re-cache the old result; its `finally` can remove a newer task's in-flight entry.
- Recommendation: generation counter per appid; only the matching generation writes cache/removes in-flight. Test with a delayed stub + invalidate + second get.

### 19.11 `CoverCache`/in-flight completed-task leak

- `_inFlight.GetOrAdd` + `finally TryRemove` has the same synchronous-completion race as `SteamDepotInfo`, but no `IsCompleted` sweep.
- A pre-cancelled token can leave a completed `None` task cached for the appid for the session.
- Recommendation: use `OnceCell`/`Shared` or sweep completed entries; test cancellation then retry.

### 19.12 Exact-marker comparisons (`"{}"`)

- `GetOverview`/`GetGameDetails` compare `raw == "{}"` exactly. A file containing `" {} "` or `"{\n}"` is parsed as an empty object and returns a non-null empty result (`GetOverview` with all-empty fields; `GetGameDetails` null because name missing).
- `GetFilterData` on `"{}"` returns default data (non-null), not null.
- Recommendation: normalize the marker (parse JSON and treat an object with no `name` as absent) or keep exact behavior for compatibility. Add tests for `"{}"`, `" {} "`, `"{}\n"`.

### 19.13 Retry semantics

- `ResolveAsync`/`EnsureFullDetailsAsync` retry only 429/403 (delays 4 s, 8 s); network errors, timeouts, 5xx, and parse errors return immediately.
- This is likely deliberate (avoid hammering a down service) but surprising. Decide whether to add bounded retries for 5xx/network; if so, keep 429/403 behavior and the interactive priority.
- Every attempt consumes a rate slot.

### 19.14 Rate limiter persistence can throw

- `PersistWindow` calls `CacheService.SaveSteamApiRequestTimes`, whose `Save` does **not** catch IO errors. A disk error propagates out of `ThrottleAsync` and then out of `ResolveAsync`/`Ensure` (the throttle call is outside their try), so the caller sees an exception, not `None`/`false`; the request isn't made, but the slot was already enqueued.
- Recommendation: make persistence best-effort (catch and log) so a cache write failure doesn't fail a metadata lookup.

### 19.15 Clock and queue assumptions

- Wall clock (not monotonic) for the 200 s window. A backward jump can stall until the "oldest" appears to age out; a forward jump can burst.
- The constructor trusts persisted timestamps' order. A corrupt/out-of-order list can make the queue's front not be the oldest, causing a long wait (`wait = front + 200 s - now` may be far future).
- Recommendation: sort restored timestamps ascending, cap to 190, and/or use a monotonic timestamp for in-process slots plus wall-clock only for persistence.

### 19.16 App list source trust / size

- Morrenus is a third-party host; the C# parses the entire response into a `JsonDocument` and trusts its shape. No signature/integrity check.
- ~341k entries; `ConcurrentDictionary<long,string>` overhead is significant. Rust: `HashMap<i64, Box<str>>` or `HashMap<i64, Arc<str>>`, pre-size to the parsed count.
- Recommendation: stream-parse, cap the number of entries / response size, and skip malformed entries individually rather than failing the whole source (behavior change; document).

### 19.17 BuildsActiveHashTests is out of scope

`/tmp/opencode/LuaTools/tests/LuaToolsGui.Tests/BuildsActiveHashTests.cs` tests `BuildsViewModel.ResolveActiveHash(liveHash, stored, editBase)` — a pure selection rule for the build switcher:

```
if liveHash is null -> null
if any stored variant hash == liveHash -> liveHash
if editBase is not null and any stored variant hash == editBase -> editBase
else -> liveHash
```

It exercises `LuaVariant`/`LuaVault`/Builds UI, **not** any module in this document. No requirement from that file applies here. Noted because it was in the requested reading list; if the port includes the Builds variant switcher, that rule belongs to the LuaVault slice.

### 19.18 `SteamService.SteamLanguage` whitespace

- A whitespace-only registry value returns `Some("")`, not `None`. `BuildsViewModel` compares it to depot language values; `""` matches nothing.
- Recommendation: return `None` for whitespace, unless parity with the current UI matters (it would only change whether a depot language is considered "selected"). Add a test.

### 19.19 `SteamService.IsValid` double read

- `EffectivePath` is evaluated twice; a registry change between reads can produce a false negative/positive. Read once in Rust.

### 19.20 Backup naming / retention

- One-second resolution filename; two applies in the same second overwrite the same backup (`overwrite: true`), so only the last is kept.
- Retention sorts by `CreationTimeUtc`, not filename; a copied/overwritten file can have an old creation time and be deleted unexpectedly.
- `MaxBackups = 1`; the real safety net is temp+swap plus the launch snapshot.
- Recommendation: use a monotonic counter/UUID or include milliseconds, and retain by name/time consistently.

### 19.21 `ManifestFile` malformed length arithmetic

- `len` is a `u32` cast to `i32`; values > `i32::MAX` become negative. In `SkipField`/`FirstFilename`, a negative length moves `o` backwards and can loop or read out of bounds.
- Recommendation: use `usize` with checked `o.checked_add(len)` and reject overflow; return `None` (soft-fail). Add fuzz tests.

### 19.22 `ManifestFile` empty metadata returns zeros

- `ParseMetadata(&[])` returns `Some(ManifestInfo { 0,false,0,0 })`, not `None`.
- `Matches(path, 0, "0")` would return true for a file with an empty metadata section and the name `0_0.manifest`. Unlikely but inconsistent with "unparseable → null".
- Recommendation: require at least one recognized field or return `None` for empty metadata; add a test.

### 19.23 `SteamAppListCache` partial source state

- A source that adds some names then throws returns false but leaves the partial set; the stale fallback is skipped if the set is non-empty; nothing is saved.
- Recommendation: parse into a temporary map per source and only merge on full success (or explicitly keep partial). Document.

### 19.24 `HardwareAppIdService` all-invalid entries

- A non-empty JSON array whose appids are all <= 0 clears the set and saves an empty list with a fresh timestamp. Next session refetches (timestamp > 0 but ids empty).
- Recommendation: if the filtered list is empty, keep the existing set; add a test.

### 19.25 `ByteFormat` rounding

- .NET custom numeric formatting and Rust's `format!("{:.2}")` may differ on half-way values and on trailing-zero trimming.
- Implement the pattern manually (round to N decimals, trim trailing zeros and a trailing `.`) and add golden tests for `1.005`, `1.25`, `1.5`, `2.5`, `1023.999`, etc.
- `Rate` casts sub-1 B/s to `0 B/s`, and Infinity behavior differs between C# and Rust; decide explicitly.

### 19.26 `AssetHash` vs `FileHash` error semantics

- Two similar helpers with opposite missing-file behavior (null vs throw). Keep them separate or unify with an explicit `Result`; do not accidentally use the wrong one in a revert path.

### 19.27 `LaunchModStore.Get` returns a live reference

- Callers can mutate the stored `LaunchMod` without persisting. Rust should return a clone or a guard; if a guard, avoid holding it across IO/await.

### 19.28 `LaunchOptionsService.Read` throws

- `Read` doesn't catch; a corrupt/unsupported appinfo propagates. The UI catches `AppInfoFormatException` separately. A Rust API should return `Result<Option<LaunchState>, ReadError>` with variants `NotFound`, `UnsupportedFormat(version)`, `Io`, `Corrupt`.
- `FindDrifted` swallows everything, so a corrupt file shows "no drift" rather than an error. Decide whether to surface it.

---

## 20. Existing test expectations index

| Test file | Modules exercised | Key expectations |
|---|---|---|
| `AppInfoCodecTests.cs` | `BinaryVdf`, `VdfTable`, `AppInfoFile`, `LaunchOption` | byte-identical no-edit round-trip; duplicate keys; non-UTF-8 values and table entries; SHA-1 binary restamp; text hash/changeNumber preserved; launch read/write; empty optionals omitted; unmodeled keys preserved; reorder renumbers + carries extras; `NextIndex`; unknown magic |
| `LaunchOptionsServiceTests.cs` | `LaunchOptionsService`, `LaunchModStore`, `AppInfoFile`, `LaunchOption` | read state; stage doesn't write; apply writes; Steam stop/restart only when running; restore; backup created; drift detection; reapply skips unstaged; no-op reapply doesn't stop Steam; read rebases after changeNumber |
| `LaunchModStoreTests.cs` | `LaunchModStore`, `LaunchOption` | snapshot only on first save; persistence; remove deletes file; rebase updates snapshot/changeNumber, not desired; same change no-op; unknown app; `Differs` fields; corrupt file quarantined |
| `SteamAppInfoCacheTests.cs` | `SteamAppInfoCache.FindAppDetailsEntry` | child-keyed matched by inner `steam_appid`; same-key resolves; failed lookup found by key; other app refused; unprovable payloads refused; direct key wins; direct key describing another app refused |
| `SteamDepotInfoCacheTests.cs` | `SteamDepotInfo` | success cached session; invalidate refetches; invalidate scoped; failure retried after 60 s; success not expired |
| `SteamLinkParserTests.cs` | `SteamLinkParser` | accepted SteamDB/store/community/steam:// links; rejected depot/sub/bare/composite/wrong-host/empty; first link; case-insensitive; fragment ignored |
| `FileHashTests.cs` | `FileHash` | `abc` vector; null on missing/empty path; readable while open; match/changed; hex case; absent hash = true; missing file + expected = false |
| `BuildsActiveHashTests.cs` | **Not this slice** (`BuildsViewModel.ResolveActiveHash`, `LuaVariant`, `LuaVault`) | exact match wins; edit base keeps row active; no base → live hash; stale base ignored; null live → null; stale edit base doesn't override exact match. See §19.17. |
| `DepotCacheMigrationTests.cs` (bonus) | `ManifestFile.TryRead`, `ManifestFile.Matches`, `SteamService` paths via migration | minimal metadata-only manifest parses; valid manifest moved; already-present not overwritten; corrupt not moved; content/name mismatch rejected; bad names rejected; unrelated files untouched; same-folder refused; leftovers keep legacy folder |
| `DownloadRateTests.cs` (adjacent) | `DownloadItem` rate/ETA math, **not** `ByteFormat` | long gaps, bursts, steady streams, ETA, reset metrics. `ByteFormat` has no dedicated tests; add them. |

Coverage gaps with **no** existing tests: `SteamService`, `SteamLibraryService`, `SteamAppListCache`, `CoverCache`, `HardwareAppIdService`, `AssetHash`, `ByteFormat`, `ManifestFile.KeyLooksValid`, `SteamAppInfoCache` fetch/throttle/filters/overview (only the matcher is tested), `SteamDepotInfo` parsing (only caching is tested). Prioritize:

1. `AppInfoFile`/`BinaryVdf` fuzz + v27/v28.
2. `LaunchOption` config-clearing/type quirks.
3. `ManifestFile` malformed sections, zip, key check.
4. `SteamAppInfoCache` matcher + fetch state machine + throttle.
5. `SteamDepotInfo` parsing + races.
6. `SteamLibraryService` VDF variants.
7. `CoverCache` placeholder/purge/cancellation.
8. `SteamService` registry/path/language (injectable providers).
9. `SteamAppListCache` source fallbacks.
10. `HardwareAppIdService` refresh preservation.
11. `ByteFormat` golden strings.
12. `FileHash`/`AssetHash` edge semantics.

---

## 21. Suggested Rust dependencies and implementation order

### 21.1 Dependencies

| Concern | Crate(s) |
|---|---|
| serde / JSON | `serde`, `serde_json` with `preserve_order` (document order matters; see §21.4) |
| HTTP/async | `reqwest` (rustls or native-tls), `tokio` |
| SHA-1 / SHA-256 | `sha1`, `sha2`, `hex` |
| AES / CBC | `aes`, `cbc`, `cipher`, `base64` |
| ZIP | `zip` |
| Regex (link parser) | `regex` |
| Dates/times | `chrono` or `time` |
| AppData / dirs | `directories` or `dirs` |
| Windows registry | `windows`/`winreg` |
| Process enumeration | `sysinfo` |
| Clipboard | `arboard` or Windows `windows` crate |
| Shell open/reveal | `open` or `windows` crate (`ShellExecuteW`) |
| HTML entities | `html-escape` |
| Logging | `tracing` |
| Errors | `thiserror` (typed) / `anyhow` at boundaries |
| Case-insensitive Unicode | `unicase` if exact C# OrdinalIgnoreCase matters for non-ASCII |
| Atomic file replace | `tempfile` + `persist`, or manual temp+rename |
| Free space | `fs2`/`sysinfo` |
| Ordered map | `indexmap` for launch store/list order if needed |

### 21.2 Implementation order (dependency-first)

1. `hash` + `byte_format` (no dependencies; easy tests).
2. `binary_vdf` (pure codec; fuzz/property tests).
3. `appinfo::file` (depends on binary_vdf; v27/28/29 fixtures).
4. `launch_option` + `launch_store` + `launch_service` (depends on appinfo; the write path can be tested entirely with fixtures).
5. `paths` (`SteamService`) with injectable registry/process/shell providers.
6. `library` (depends on paths; pure filesystem fixtures).
7. `manifest` (pure parser; fixtures + fuzz).
8. `link_parser` (pure).
9. `appinfo_cache` (HTTP + throttle; fake clock/client; disk details).
10. `applist_cache` (HTTP + disk; fake client).
11. `depot_info` (HTTP + cache; fake clock/handler).
12. `cover_cache` (HTTP + disk + placeholder).
13. `hardware_ids` (GitHub proxy + cache service).
14. Integration: wire caches/stores into the UI/service layer; ensure no excluded consumers.

### 21.3 Suggested trait seams (testability)

```rust
trait RegistryProvider {
    fn read_string(&self, hive: Hive, view: RegistryView, subkey: &str, value: &str) -> Option<String>;
}
trait SteamProcessController {
    fn is_running(&self) -> bool;
    fn stop(&self);
    fn start(&self) -> bool;
}
trait Clock { fn now_utc(&self) -> DateTime<Utc>; }
trait HttpGet { async fn get(&self, url: &str, ct: &CancellationToken) -> Result<Response, Error>; }
trait FreeSpace { fn available(&self, path: &Path) -> Option<u64>; }
trait Shell { fn open(&self, target: &Path) -> Result<(), Error>; }
trait Clipboard { fn set_text(&self, text: &str) -> bool; }
trait SettingsProvider { fn steam_path_override(&self) -> Option<String>; }
```

This mirrors the C# seams (`internal SteamDepotInfo(HttpMessageHandler)`, `internal Func<DateTime> UtcNow`, injected process delegates) and makes the tests above possible without touching the real Steam install.

The C# test project gets these via `InternalsVisibleTo("LuaToolsGui.Tests")` (also `SteamAppInfoCache.FindAppDetailsEntry`, `LaunchModStore(string)`, and the internal `LaunchOptionsService` constructor). In Rust, expose them as `pub(crate)` or behind `#[cfg(test)]` rather than widening the public API.

C# only gives `SteamDepotInfo` a handler seam; `SteamAppInfoCache`, `SteamAppListCache`, `CoverCache`, and `GithubProxy` construct `HttpClient` internally. For testability, the Rust constructors should accept a `reqwest::Client` (or a `base_url`/mock server) for every HTTP module, not just depot info.

### 21.4 Data-model notes

- `VdfTable` must be an ordered `Vec`, never a `HashMap`.
- `VdfValue::String` must be `Vec<u8>`, never `String`.
- `BTreeMap` vs `HashMap`: use `HashMap` for lookup-only caches; `IndexMap` where insertion order is user-visible (launch store list, depot order) and deterministic output matters.
- `AppInfoEntry.offset` and `size` are file-absolute/byte counts; keep them `i64`/`i32` with checked arithmetic when seeking.
- `ManifestInfo` is `Copy`; `ContentDepot`/`AppDepotInfo` cloneable.
- `LaunchOption` serde must use PascalCase and include `SourceIndex`; `Summary` skipped.
- Keep raw bytes for VDF strings and string-table entries; only decode for display/key names.
- **Enable `serde_json/preserve_order`** (or parse into an order-preserving map). C# `JsonDocument` enumerates object properties in document order; `serde_json::Map` without the feature is a sorted `BTreeMap`. Order is observable in: the appdetails child-key scan, the depot map (UI row order), `config.launch` entries (default/other ordering), and any first-match parsing. Arrays are already order-preserving.
- Use `PathBuf`, not `String`, for all filesystem paths; only convert at the shell/JSON boundary.

---

## 22. Porting checklist (definition of done)

For each module, the Rust implementation should have:

- [ ] Exact public signatures/semantics from this document (or a documented deviation in §19).
- [ ] Fixture-based tests for the formats: appinfo v27/v28/v29, binary VDF types/duplicates/non-UTF-8, manifest sections/protobuf/zip/key check, ACF/VDF, appdetails JSON shapes.
- [ ] Failure-mode tests: missing files, permissions, truncated data, malformed JSON, wrong magic/version, non-string fields, out-of-range numbers.
- [ ] Concurrency tests where the C# has shared mutable state: rate limiter, in-flight dedupe, invalidate races, negative caches, launch store locks.
- [ ] Cancellation tests for async fetches and retries.
- [ ] No excluded feature (§18) reachable from the new code: no stplug-in Lua, no unlocker install, no Steamless/SteamAutoCrack/Denuvo fix, no depot content download/key file.
- [ ] Paths centralized (AppData, Steam root, legacy depotcache) with no hardcoded user paths.
- [ ] HTTP URLs/timeouts/headers exactly as documented, with fake-client tests.
- [ ] Golden tests for `ByteFormat` and for `sha1_binary` restamping.
- [ ] Documentation of every intentional deviation (negative-cache fix, config-clearing fix, VDF parser, etc.).

---

## 23. Quick reference: URLs, paths, constants

### URLs

| Constant / use | URL |
|---|---|
| appdetails | `https://store.steampowered.com/api/appdetails?appids={appid}&cc=us&l=english` |
| CDN header guess | `https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/header.jpg` |
| Morrenus app list | `https://applist.morrenus.xyz/` |
| Steam app list | `https://api.steampowered.com/ISteamApps/GetAppList/v2/` |
| SteamCMD depot info | `https://api.steamcmd.net/v1/info/{appId}` |
| Hardware blacklist | `https://raw.githubusercontent.com/jsnli/steamappidlist/master/data/hardware_appid.json` |
| GitHub download mirrors | `https://ghproxy.net/`, `https://ghfast.top/`, `https://gh.ddlc.top/` (prefix the full URL) |
| GitHub API mirror | `https://lua.tools/api/gh/` (not used by this slice) |

### Constants

| Constant | Value |
|---|---|
| appinfo magic v27/v28/v29 | `0x07564427` / `0x07564428` / `0x07564429` |
| appinfo v27 meta | 40 bytes |
| appinfo v28/v29 meta | 60 bytes |
| appinfo sha1_text offset | 16 |
| appinfo changeNumber offset | 36 |
| appinfo sha1_binary offset | 40 |
| binary VDF End / EndAlt | `0x08` / `0x0B` |
| manifest payload magic | `0x71F617D0` |
| manifest metadata magic | `0x1F4812BE` |
| manifest EOF magic | `0x32C415AB` |
| manifest metadata fields | depot 1, gid 2, encrypted 4, size 5 |
| cover placeholder length | 9816 |
| cover placeholder SHA-256 | `732ec27f2af650fe079f1c83b0bb0c712a322dc175f383504176724675ad2700` |
| cover min valid bytes | 512 |
| appdetails window | 200 s |
| appdetails max per window | 190 |
| appdetails retry delays | 4 s, 8 s |
| appdetails max attempts | 3 |
| depot info failure TTL | 60 s |
| app list max age | 14 days |
| hardware list max age | 14 days |
| launch max backups | 1 |
| link max appid | 2,000,000,000 (exclusive) |
| hardware cache keys | `HardwareAppIds`, `HardwareAppIdsFetchedAtMs` |
| rate window cache key | `SteamApiRequestTimes` |

### Paths

| Purpose | Path |
|---|---|
| AppData root | `%APPDATA%\LuaToolsGui` |
| details | `%APPDATA%\LuaToolsGui\details\<appid>.json` |
| app list | `%APPDATA%\LuaToolsGui\steam-applist.json` |
| covers | `%APPDATA%\LuaToolsGui\covers\<appid>.jpg` |
| launch mods | `%APPDATA%\LuaToolsGui\launchmods.json` (+ `.tmp`, `.bak`, `.corrupt`) |
| shared cache | `%APPDATA%\LuaToolsGui\cache.json` |
| settings | `%APPDATA%\LuaToolsGui\settings.json` |
| Steam appinfo | `<Steam>\appcache\appinfo.vdf` |
| backup | `<Steam>\appcache\luatools-backup\appinfo-YYYYMMDD-HHMMSS.vdf` |
| manifests | `<Steam>\depotcache\<depot>_<gid>.manifest` |
| legacy manifests | `<Steam>\config\depotcache\...` (read-only fallback) |
| libraries | `<Steam>\steamapps\libraryfolders.vdf` |
| app manifests | `<library>\steamapps\appmanifest_<appid>.acf` |
| game folder | `<library>\steamapps\common\<installdir>` |
| stplug-in (EXCLUDED consumer) | `<Steam>\config\stplug-in` |

---

*End of specification.*
