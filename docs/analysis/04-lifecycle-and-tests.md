# 04 — Application lifecycle, models, i18n, release/CI and test matrix

Port specification for the LuaTools C#/WPF application (`.NET 8`, `net8.0-windows`, Velopack 1.2.0,
Microsoft.Extensions.Hosting 10.0.9) into the Rust workspace under
`/home/mohamed/Projects/luatools-rs`.

Source of truth (read-only): `/tmp/opencode/LuaTools`.

This document covers only the slice listed below; other analysis documents cover the Steam library,
downloads, HTTP server, plugin injection and UI pages in detail.

Source files examined for this slice:

- `src/LuaToolsGui/Program.cs`, `App.xaml`, `App.xaml.cs`, `AppConfig.cs`, `AssemblyInfo.cs`,
  `LuaToolsGui.csproj`, `MainWindow.xaml`, `MainWindow.xaml.cs`
- `src/LuaToolsGui/Models/ApiModels.cs`, `Models/FixModels.cs`, `Models/ModeModels.cs`
- `src/LuaToolsGui/Services/ProtocolService.cs`, `UpdateService.cs`, `SettingsService.cs`,
  `CacheService.cs`, `ModeMigration.cs`, `DepotCacheMigrationService.cs`, `DohResolver.cs`,
  `PluginLog.cs`, `HttpServerService.cs` (lifecycle parts), `CefInjectorService.cs` (lifecycle parts),
  `Downloads/DownloadQueue.cs` and `Downloads/DownloadItem.cs` (lifecycle/rate parts),
  `ViewModels/SettingsViewModel.cs` (language/startup parts), `ViewModels/OnboardingViewModel.cs`,
  `ViewModels/MainViewModel.cs`
- `Resources/Strings.resx`, `Strings.Designer.cs`, `Resources/README.md`, all 29
  `Strings.<tag>.resx` files (every file parsed for key/placeholder parity; German read in full as the
  worked example; `es`/`es-419`/`pt-BR`/`pt-PT`/`zh-Hans`/`zh-Hant` sampled for regional differences)
- all 17 files in `tests/LuaToolsGui.Tests/` plus `LuaToolsGui.Tests.csproj`
- `.github/workflows/i18n-check.yml`, `scripts/check-i18n.py`, `CONTRIBUTING.md`, `README.md`,
  `.gitignore`, `LuaToolsGui.sln`

---

## 0. Short version

The application is a single-instance, tray-capable WinExe with a custom `Main`. Startup order is
load-bearing: Velopack hooks → UI culture → protocol registration → argument scan → named-mutex
single-instance test → WPF `App.Run`. The DI container is built in the `App` constructor and started
from `OnStartup`. There is no global unhandled-exception handler and no file log for general errors;
only the plugin/HTTP backend services write `plugin-backend.log`.

The localization system is a hand-maintained, strongly typed RESX layer: 552 keys, 29 translated
files plus the English base, 95 keys with positional placeholders, full key parity enforced by a
Python script in CI. `x:Static` resolves strings once at XAML parse time, which is why changing the
language requires a relaunch.

The test suite is 281 xUnit cases across 17 files. Most tests are table-driven pure-logic tests with
injectable clocks/handlers. A large block of them belongs to lua/depot unlocking, unlocker-mode
migration and depot-cache rescue, and should be dropped with those features in the Rust port. Tests
for launch-options editing, DoH, rate metering, Steam cache lookups and language detection are
feature-agnostic and should be reproduced.

---

## 1. Startup sequence

### 1.1 Entry point order (`Program.Main`)

`Program.Main` is `[STAThread]` and is set as `<StartupObject>LuaToolsGui.Program</StartupObject>`.
The SDK is prevented from generating a competing `Main` by removing `App.xaml` from
`ApplicationDefinition` and adding it as a `Page`.

Exact order:

1. `VelopackApp.Build().Run()`. Must run before any WPF/UI work. It handles install/update hooks and
   no-ops on a normal launch.
2. `ApplyUiCulture()`. Sets UI culture before any WPF element exists, because `x:Static` resource
   lookups are resolved once at parse time. See §1.4.
3. `Services.ProtocolService.Register()`. Writes the `luatools://` handler to `HKCU` on every launch,
   including launches that will immediately exit as a second instance.
4. Scan `args` (case-insensitive):
   - any argument that starts with `luatools://` becomes the protocol URL; if several do, the last one
     wins;
   - `--minimized` sets `StartMinimized = true`;
   - `--tray-locked` sets `trayLocked = true`;
   - everything else is ignored.
5. Create the single-instance mutex:
   `new Mutex(initiallyOwned: true, "LuaToolsGui.SingleInstance", out bool isFirst)`. The name has no
   `Global\` prefix, so it is per-session, not per-machine.
6. If `isFirst == false`, run the second-instance path (§1.2) and return.
7. First instance stores `StartupUrl`, `StartMinimized`, `SessionTrayLock` in static fields.
8. Create three named `EventWaitHandle`s with `EventResetMode.AutoReset`, initially unsignaled:
   - `LuaToolsGui.ShowWindow`
   - `LuaToolsGui.EnableTrayLock`
   - `LuaToolsGui.RecheckUpdates`
9. `var app = new App(); app.InitializeComponent(); app.Run();`
10. After `Run` returns, dispose the three event handles. The mutex is disposed by `using`.

`App.InitializeComponent()` loads `App.xaml`, whose only content is `ShutdownMode="OnExplicitShutdown"`,
WPF-UI `ThemesDictionary Theme="Dark"`, `ControlsDictionary`, and the converter resources. There is no
`StartupUri`; the main window is resolved from DI and shown manually.

### 1.2 Second-instance path

When the mutex is not acquired:

| Input condition | Action |
|---|---|
| a `luatools://` URL was on the command line | `ProtocolService.WritePending(url)` writes `%AppData%\LuaToolsGui\protocol_url.tmp` (raw URL), so the live instance can pick it up |
| `--minimized` was NOT passed | open `LuaToolsGui.ShowWindow` and `Set()` it, best effort; the live instance restores/focuses its window |
| `--minimized` WAS passed | do not signal the window. The Steam DLL loader launches duplicates during one Steam boot; surfacing on those would be wrong |
| `--tray-locked` was passed | open `LuaToolsGui.EnableTrayLock` and `Set()` it (turns on close-to-tray for the live session), then open `LuaToolsGui.RecheckUpdates` and `Set()` it (re-runs the silent app+plugin update flow). Both are best effort. The update poke is deliberately gated to `--tray-locked` so manual/protocol relaunches never trigger updates |

The second process then returns without creating a WPF `App`.

### 1.3 First instance: handoff to `App`

Static fields used by `App.OnStartup`:

- `Program.StartupUrl` (string?) — protocol URL from the command line.
- `Program.StartMinimized` (bool) — `--minimized`.
- `Program.SessionTrayLock` (bool) — `--tray-locked`; in-memory only, never persisted.
- `Program.ShowWindowSignal`, `EnableTrayLockSignal`, `RecheckUpdatesSignal` (`EventWaitHandle?`).

### 1.4 UI culture selection

`ApplyUiCulture()` wraps everything in `try/catch`; a culture problem must never prevent launch.

1. `ResolveLanguageTag()`:
   - `ReadSavedLanguage()` reads just the `Language` property from
     `%AppData%\LuaToolsGui\settings.json` with `JsonDocument`. The property name is matched exactly
     (`TryGetProperty("Language")`), and whitespace-only values become null. Any exception returns
     null.
   - If saved is non-null and `IsSupported(saved)` (case-insensitive membership in
     `SupportedLanguages`), use the saved string verbatim.
   - Otherwise map `CultureInfo.InstalledUICulture` (the OS display language), not
     `CurrentUICulture`.
2. Build `new CultureInfo(tag)` and set:
   - `CultureInfo.DefaultThreadCurrentUICulture`
   - `CultureInfo.DefaultThreadCurrentCulture`
   - `Thread.CurrentThread.CurrentUICulture`
   - `Thread.CurrentThread.CurrentCulture`
3. Override `FrameworkElement.LanguageProperty` metadata to
   `XmlLanguage.GetLanguage(culture.IetfLanguageTag)`, so WPF date/number formatting follows the UI
   culture.

`SupportedLanguages` (exact list and order in `Program.cs`):

```
en, zh-Hans, zh-Hant, ja, ko,
es, es-419, pt-BR, pt-PT, fr, de, it, nl, pl,
ru, uk, tr, ar,
cs, hu, ro, el, bg, th, vi, id, da, fi, nb, sv
```

That is 30 tags including English, i.e. 29 translations; `Strings.en` is the base `Strings.resx`
(no `Strings.en.resx`).

### 1.5 `MatchOsLanguage` exact algorithm

Input: a `CultureInfo`. Output: one supported tag or `"en"`.

Order:

1. **Exact supported tag**: if `os.Name` is in `SupportedLanguages` (case-insensitive), return
   `os.Name` unchanged. This covers `pt-BR`, `pt-PT`, `es-419`, `zh-Hans`, `zh-Hant`, `en`, etc.
2. **Chinese** (`TwoLetterISOLanguageName == "zh"`):
   - name contains `Hant` (case-insensitive) → `zh-Hant`
   - name contains `Hans` → `zh-Hans`
   - else `zh-TW`, `zh-HK`, `zh-MO` → `zh-Hant`; every other Chinese locale → `zh-Hans`
3. **Portuguese** (`two == "pt"`) → `pt-PT`. There is no neutral `pt` resource; European is the
   default.
4. **Spanish** (`two == "es"`):
   - `IsLatinAmericanSpanish(name)` = name starts with `es-` (case-insensitive) AND is not `es-ES`
   - Latin → `es-419`; otherwise → `es`
   - consequently `es-MX`, `es-AR`, `es-CO`, `es-CL`, `es-US`, etc. → `es-419`; `es-ES` and bare `es`
     → `es`.
5. **Norwegian** (`two is "nb" or "nn" or "no"`) → `nb`.
6. **Generic neutral parent**: if the two-letter name is supported, return it. This is what maps
   `es-ES`→`es`, `fr-FR`→`fr`, `de-DE`→`de`, `ja-JP`→`ja`, etc. (Spanish is already handled above,
   but the generic path would produce the same result for Spain.)
7. Fallback `"en"`.

`IsSupported` uses `StringComparer.OrdinalIgnoreCase`; the returned `os.Name` is not lowercased. A
hand-edited settings value of e.g. `PT-br` is accepted as "supported" and handed to `CultureInfo`,
which normalizes it, but the saved value itself is not canonicalized.

### 1.6 DI container: registration order and lifetimes

`App` constructor: `Host.CreateDefaultBuilder()` (no command-line args) → `ConfigureServices` →
`Build()`. No `appsettings.json` is present in the repo. Default logging providers are therefore the
host defaults (console/debug/EventSource/EventLog on Windows); the app never configures a file sink.

Registrations, in order:

| Service | Lifetime | Notes |
|---|---|---|
| `SettingsService` | singleton | loads `settings.json` in constructor |
| `CacheService` | singleton | loads `cache.json` in constructor |
| `SteamService` | singleton | Steam discovery |
| `SteamAppListCache` | singleton | |
| `SteamAppInfoCache` | singleton | |
| `CoverCache` | singleton | |
| `ToastService` | singleton | presenter attached later to window snackbar |
| `SteamDepotInfo` | singleton | 60 s failure TTL |
| `LuaVault` | singleton | |
| `Services.AppInfo.LaunchModStore` | singleton | `launchmods.json` |
| `Services.AppInfo.LaunchOptionsService` | singleton | |
| `LuaInstaller` | singleton | raises `Installed` |
| `SteamLibraryService` | singleton | |
| `DonateKeysService` | singleton | |
| `AnalyticsService` | singleton | |
| `GithubProxy` | singleton | |
| `HardwareAppIdService` | singleton | |
| `SteamlessService` | singleton | DRM-removal feature |
| `SteamAutoCrackService` | singleton | DRM feature |
| `CloudRedirectService` | singleton | unlocker add-on |
| `DepotDownloaderService` | singleton | |
| `DepotCacheMigrationService` | singleton | |
| `AppliedFixIndexService` | singleton | Denuvo fix index |
| `UnlockerService` | singleton | OST/BST/Custom |
| `PluginInstallerService` | singleton | |
| `DropInstallViewModel` | **transient** | one per page (Home, Add) |
| `AuthService` | singleton | |
| `LuaToolsApiClient` | singleton | |
| `HubcapService` | singleton | |
| `UpdateService` | singleton | constructs one `UpdateManager` per release repo |
| `Downloads.DownloadQueue` | singleton | |
| `Downloads.DownloadQueue` as `IHostedService` | hosted | factory resolves the same singleton |
| `Downloads.ManifestJobFactory` | singleton | |
| `PluginAddService` | singleton | |
| `HttpServerService` | singleton | |
| `HttpServerService` as `IHostedService` | hosted | same instance |
| `CefInjectorService` | singleton | |
| `CefInjectorService` as `IHostedService` | hosted | same instance |
| `DownloadViewModel` | singleton | |
| `SettingsViewModel` | singleton | |
| `ManageViewModel` | singleton | |
| `BuildsViewModel` | singleton | |
| `LaunchOptionsViewModel` | **transient** | one per dialog |
| `HomeViewModel` | singleton | |
| `ModeViewModel` | singleton | |
| `FixesViewModel` | singleton | |
| `DownloadsViewModel` | singleton | |
| `PluginViewModel` | singleton | |
| `OnboardingViewModel` | singleton | |
| `MainViewModel` | singleton | |
| `HomeView` | singleton | pages resolved by NavigationView |
| `DownloadView` | singleton | |
| `DownloadsView` | singleton | |
| `ManageView` | singleton | |
| `BuildsView` | singleton | |
| `ModeView` | singleton | |
| `FixesView` | singleton | |
| `PluginView` | singleton | |
| `SettingsView` | singleton | |
| `MainWindow` | singleton | |

`MainWindow` is constructed by DI with `(MainViewModel, IServiceProvider, SettingsService)`.
Its constructor calls `InitializeComponent`, sets `DataContext`, calls
`RootNavigation.SetServiceProvider(services)` so `NavigationView` can resolve the singleton page
views, initializes the tray icon, subscribes `Closing`, and on `Loaded` navigates to `HomeView` then
awaits `MainViewModel.InitializeAsync()` (auth restore; caught so offline still shows the guest UI).

Hosted services start in registration order:

1. `DownloadQueue` — restores history from `cache.json` and starts a background pump task.
2. `HttpServerService` — binds `http://127.0.0.1:6767/`; on `HttpListenerException` it tries
   `netsh http add urlacl url=http://127.0.0.1:6767/ user=Everyone` with `runas` and a 3 s wait, then
   retries once; failure is logged and startup continues.
3. `CefInjectorService` — loads `luatools.js` + polyfill (or inline polyfill) and starts the CDP
   injection loop (150 ms tick, full tab refresh/inject every 7 ticks ≈ 1 s; CDP port 8080).

On host stop, .NET stops hosted services in reverse order: `CefInjectorService`,
`HttpServerService`, `DownloadQueue`.

### 1.7 `App.OnStartup` sequence

`OnStartup` is `async void`. Order:

1. `base.OnStartup(e)`.
2. Resolve `SettingsService`; set `AppHttp.ModeProvider = () => dnsSettings.DnsMode`. The HTTP handler
   calls this per connection, not once, so ordering is not load-bearing; setting it first makes the
   first request honor the user's choice.
3. Fire-and-forget `Task.Run` cleanup:
   - delete `%USERPROFILE%\Downloads\LuaTools` recursively if it exists (legacy staging folder);
   - `HttpFileDownloader.SweepStale()` for the current `%TEMP%` staging area.
   All failures swallowed.
4. `await _host.StartAsync()` (hosted services above).
5. `ModeMigration.Apply(SettingsService)`. If it returns true, set
   `CacheService.OnboardingComplete = false`. This runs before anything reads `SelectedMode`, because
   `UnlockerService.SelectedMode` would otherwise parse a legacy value to null.
6. `UnlockerService.EnsureLuaPathRegistered()` (only for Ost/Bst; edits `opensteamtool.toml`
   `[lua] paths` to include `config/stplug-in`).
7. Resolve `MainViewModel` and `SettingsViewModel`.
8. Wire `settingsVm.RequestRestart = RelaunchApp`.
9. Resolve `MainWindow` (its constructor builds the tray icon and subscribes `Closing`/`Loaded`).
10. Wire `settingsVm.RequestShowWindow = () => Dispatcher.Invoke(window.RestoreFromTray)`.
11. Register `ThreadPool.RegisterWaitForSingleObject` listeners (AutoReset, infinite timeout,
    `executeOnlyOnce: false`) for the three named events:
    - ShowWindow → on UI thread: read pending protocol URL; if it parses `Silent`, do not restore the
      window; otherwise `window.RestoreFromTray()`; then `HandleProtocolUrl(pending)` if any.
    - EnableTrayLock → `Program.SessionTrayLock = true`.
    - RecheckUpdates → `_ = RunUpdateFlowAsync()`.
12. `App.RunUpdateFlow = RunUpdateFlowAsync` so the HTTP `POST /check-updates` runs the identical flow.
13. `settingsVm.RequestSignIn = () => main.SignInCommand.ExecuteAsync(null)`.
14. Build `navigateToSignIn`: on UI thread set `settingsVm.LoginRequiredMessage =
    Strings.Settings_LoginRequired` and `window.NavigateToSettings()`; assign to
    `DownloadViewModel.RequestSignIn` and `FixesViewModel.RequestSignIn`.
15. `toast.Attach(window.RootSnackbar)`.
16. `settingsVm.RequestRestartPrompt` shows a persistent action toast:
    `Lang_Changed_Title`, `Lang_Changed_Body`, action `Lang_Changed_Restart` → restart.
17. Resolve `DownloadViewModel` and `ManageViewModel`.
18. `manage.NavigateToAdd`: navigate Add and `download.SeedSearch(appId)`.
19. `manage.NavigateToBuilds`: navigate Builds and `builds.SelectAppAsync(appId)`.
20. `manage.OpenLaunchOptions`: construct `LaunchOptionsDialog` with a transient VM and owner window,
    `ShowDialog()`.
21. Start the launch-option drift check: `_ = CheckLaunchOptionDriftAsync()`. It returns immediately
    when `LaunchOptionsService.Store.IsEmpty`; otherwise `Task.Run(launch.FindDrifted)`; if any, show
    an action toast `Launch_Drift_Title` / `string.Format(Launch_Drift_Body, count)` /
    `Launch_Drift_Action`. The action asks `MessageBox` OK/Cancel with `Launch_ApplyNow_Title` and
    `Launch_ApplyNow_Body`, then runs `Task.Run(launch.Reapply(drifted))` and toasts
    `Launch_Applied` or `Launch_Applied_Restarted` / `string.Format(Launch_ApplyFailed, error)`.
22. Resolve `HomeViewModel` and define `openInManage(appId)` = navigate Manage then
    `manage.OpenDetailForAppIdAsync(appId)`; assign `home.NavigateToGame = openInManage`.
23. Resolve `DownloadsViewModel`; `RevealItem` → navigate Add.
24. `download.NavigateToGame = openInManage`; `builds.NavigateToManage = openInManage`;
    `builds.RequestShowDownloads = () => Dispatcher.Invoke(window.NavigateToDownloads)`.
25. Define `installByAppId(appId)` = `Dispatcher.Invoke(() => HandleProtocolUrl($"luatools://install/{appId}"))`;
    assign to `home.Drop.InstallByAppId` and `download.Drop.InstallByAppId`, so a dropped link and a
    protocol URL share one path.
26. Home dashboard cells → `NavigateToPlugin`, `NavigateToManage`, `NavigateToSettings`,
    `NavigateToMode`; `main.Onboarding.RefreshHome = () => Dispatcher.Invoke(() => home.LoadAsync())`.
27. Subscribe `LuaInstaller.Installed`: on UI thread, `manage.LoadAsync()`, `builds.LoadAsync()`,
    `await home.RefreshLibraryAsync()`, then `await appInfo.EnsureFullDetailsAsync(appId)` and refresh
    home again if details resolved. This warms content-hashed header art for newer titles.
28. Compute startup URL: `Program.StartupUrl ?? ProtocolService.TryReadPending()`.
29. `silentStartup = (url is not null && ProtocolService.Parse(url).Silent) || Program.StartMinimized`.
30. `_exitAfterSilentInstall = silentStartup && Program.StartupUrl is not null && !settingsVm.MinimizeToTray`
    (cold launch only; a second-instance handoff never auto-exits).
31. If `silentStartup`:
    - `window.StartSilent()` makes the tray icon visible; the window is never shown.
    - `await main.InitializeAsync()` (restores auth) inside try/catch; offline proceeds as guest.
    Else:
    - `window.Show()`. The window's `Loaded` handler navigates Home and calls `main.InitializeAsync()`.
    - First-run onboarding:
      - if `!cache.OnboardingComplete`:
        - `configured = unlocker.SelectedMode is (Ost or Bst) && installer.IsInstalledLocally()`;
          `Custom` deliberately does not count.
        - if configured, set `cache.OnboardingComplete = true`.
        - else `main.Onboarding.IsOpen = true`.
32. If `url is not null`, `HandleProtocolUrl(url)`.
33. If `Program.SessionTrayLock`, `_ = RunUpdateFlowAsync()` (silent app-then-plugin update).
34. Fire-and-forget: `DonateKeysService.SendPendingKeysIfEnabledAsync()`,
    `AnalyticsService.TrackAppLaunchAsync()`, `HardwareAppIdService.EnsureFreshAsync()`,
    `DepotCacheMigrationService.RunAsync()`.
### 1.8 First-run onboarding behavior

`OnboardingComplete` lives in `cache.json`, not settings (app bookkeeping, not a preference).

- The overlay is shown only when `cache.OnboardingComplete` is false and the user is not already
  configured. "Configured" means a mode of Ost or Bst AND `PluginInstallerService.IsInstalledLocally()`.
- `MainViewModel.Onboarding` is a singleton `OnboardingViewModel`; its `IsOpen` drives the overlay.
- Defaults: `ApplyRecommended = true`, `InstallPlugin = true`.
- `Finish`:
  - sets `cache.OnboardingComplete = true` immediately;
  - sets `IsOpen = false` before any slow work;
  - if either choice is on, starts `ApplyChoicesAsync` in the background.
- `ApplyChoicesAsync`:
  - toast `Onboarding_Title` / `Onboarding_Applying`;
  - `Task.Run(steam.StopSteam)` once;
  - if recommended: `Settings.FastFetch = true`, then `UnlockerService.InstallAsync(UnlockerMode.Bst)`;
    failure toasts `result.Error` (fallback empty string);
  - if plugin: `PluginInstallerService.InstallAsync(null)`; failure toasts the error;
  - `Task.Run(steam.StartSteam)` once;
  - finally `RefreshHome` (best effort).
- `ModeMigration` is the only code that clears `OnboardingComplete` again (when a retired mode is
  found).
- Sign-in inside onboarding calls `AuthService.SignInAsync`; error text goes to `StatusLine`.

### 1.9 Logging and unhandled exceptions

- General logging uses `Microsoft.Extensions.Logging` from the default host. `ILogger<T>` is injected
  into eight services (`HttpServerService`, `CefInjectorService`, `DownloadQueue`,
  `DepotCacheMigrationService`, `SteamAppInfoCache`, `DepotDownloaderService`,
  `AppliedFixIndexService`, `SteamAutoCrackService`, etc.). There is no `AddFile`, no rolling log, no
  log level configuration in the repo, and no `appsettings.json`.
- `PluginLog` is the only explicit file log:
  `%AppData%\LuaToolsGui\plugin-backend.log`, lines `[HH:mm:ss.fff] message` + `Environment.NewLine`,
  guarded by a lock, every failure swallowed. It is used by `HttpServerService` to log every request except paths starting `/add-status/` and
  `/has/` as `HTTP <METHOD> <path>`, and by `PluginAddService` for plugin-add state transitions
  (`PluginAdd.Start`, `.Pick`, `.Check`, `.Download`, exceptions).
- There is **no** `DispatcherUnhandledException`, `AppDomain.CurrentDomain.UnhandledException`,
  `TaskScheduler.UnobservedTaskException`, or `Application.DispatcherUnhandledException` handler.
  A grep over the whole app finds none. `OnStartup` and `OnExit` are `async void`; exceptions after
  the first await would be unobserved/crash the dispatcher unless caught locally.
- `ApplyUiCulture`, protocol registration, settings/cache/auth load, migration, cleanup, analytics,
  donation, hardware-list refresh, drift check, update check and nearly every service call are
  individually wrapped in `try/catch` and fail silently or log only at debug/information level.
- `MainWindow.Loaded` wraps `main.InitializeAsync()` in try/catch; auth restore failure leaves the UI
  as guest.

### 1.10 Shutdown

Three paths:

1. **Window close with close-to-tray active** — `MinimizeToTray == true` OR `Program.SessionTrayLock`:
   `Closing` is cancelled, `Hide()` is called, tray icon becomes visible. The process stays alive.
2. **Tray → Exit** — `ExitApp()` sets `_reallyExiting = true`, hides the tray icon, calls `Close()`.
   `Closing` sees `_reallyExiting` and does not cancel; it disposes the tray icon and calls
   `Application.Current.Shutdown()` (necessary because `ShutdownMode` is `OnExplicitShutdown`).
3. **Programmatic relaunch** — `RelaunchApp()` starts
   `cmd.exe /c timeout /t 2 /nobreak >nul & start "" "<exe>"` with `CreateNoWindow = true`,
   `UseShellExecute = false`, then `Shutdown()`. The 2 s delay exists because the new process must not
   see this process's mutex still held.

`OnExit`:

1. If `UpdateService.HasStagedUpdate`, call `Updates.ApplyOnExit()`
   (`WaitExitThenApplyUpdates(_staged, silent: true, restart: false)`).
2. `await _host.StopAsync()`.
3. `_host.Dispose()`.
4. `base.OnExit(e)`.

Hosted service stop order is reverse registration (`CefInjectorService`, `HttpServerService`,
`DownloadQueue`). `DownloadQueue.StopAsync` cancels all active items, marks them `Cancelled`, writes
a history entry with `Downloads_Err_Interrupted` when no message exists, settles their completion
tasks, persists history, and waits up to 3 s for the pump. `CefInjectorService.StopAsync` cancels the
loop and disposes cached CDP sockets. `HttpServerService.StopAsync` cancels and stops the listener.

The three named event handles are disposed after `app.Run()` returns; the mutex is disposed by its
`using`.

---

### 1.11 Compiled-in configuration (`AppConfig.cs`)

`AppConfig` is a static class of `const`/`static readonly` values; there are no environment-variable
or settings-file overrides. Except where noted, these are public client values that already ship in
the web bundle.

| Constant | Value | Use |
|---|---|---|
| `SupabaseUrl` | `https://db.lua.tools` | Supabase REST/auth base |
| `SupabaseAnonKey` | a long Supabase **anon** JWT (public client value; see the source file) | `apikey` header for Supabase REST |
| `ApiBaseUrl` | `https://lua.tools` | lua.tools API base |
| `BotAccountEmailDomain` | `@bot.lua.tools` | detects Discord-bot placeholder accounts for the re-link banner |
| `HubcapBaseUrl` | `https://hubcapmanifest.com` | Hubcap API base; called with the user's own key |
| `OAuthCallbackPort` | `53789` | local OAuth callback listener |
| `OAuthCallbackUrl` | `http://localhost:53789/callback` | must be registered in Supabase Auth → Redirect URLs |
| `DailyDownloadLimit` | `25` | lua.tools standard daily cap; Hubcap-keyed downloads exempt |
| `SteamStoreSearchUrl` | `https://store.steampowered.com/api/storesearch/` | Add-page search |
| `SteamFeaturedUrl` | `https://store.steampowered.com/api/featuredcategories` | Add-page featured strips |
| `HardwareAppIdListUrl` | `https://raw.githubusercontent.com/jsnli/steamappidlist/master/data/hardware_appid.json` | hardware appid blacklist, ~14-day cache |
| `SteamlessRepo` | `atom0s/Steamless` | SteamStub DRM removal tool (DRM feature) |
| `CloudRedirectRepo` | `Selectively11/CloudRedirect` | CloudRedirect GUI manager (unlocker add-on) |
| `SteamAutoCrackRepo` | `SteamAutoCracks/Steam-auto-crack` | GUI-only, framework-dependent `net10.0-windows` (DRM feature) |
| `DepotDownloaderRepo` | `mendy-tools/DepotDownloaderMod` | self-hosted re-pack, flat zip, self-contained win-x64 (depot feature) |
| `ManifestBackendUrl` | `http://167.235.229.108` | plain-HTTP manifest backend |
| `ManifestBackendUserAgent` | `secretgoonpoon` | User-Agent gate for that backend |
| `DonateKeysUserAgent` | `discord(dot)gg/luatools` | different User-Agent for the donate-keys endpoint (403 otherwise) |
| `UmamiHost` | `https://analytics.lua.tools` | anonymous launch analytics |
| `UmamiWebsiteId` | `820d782c-a434-424f-9f90-dee83dc6032e` | Umami website id |
| `UmamiHostname` | `desktop.lua.tools` | Umami hostname override |
| `GithubReleasesRepos[0]` | `https://github.com/madoiscool/LuaTools` | primary Velopack feed |
| `GithubReleasesRepos[1]` | `https://github.com/mendy-tools/LuaTools` | backup feed; assets re-uploaded manually only if primary dies |
| `GithubReleasesRepo` | first element | convenience property |
| `PluginReleasesOwner` | `madoiscool` | plugin repo owner |
| `PluginReleasesRepo` | `LTSP` | plugin releases: `plugin.zip` + `winmm.dll`, tag = version |
| `GithubApiMirrors` | `https://lua.tools/api/gh/` | API-only self-hosted proxy, server-side PAT + cache |
| `GithubDownloadMirrors` | `https://ghproxy.net/`, `https://ghfast.top/`, `https://gh.ddlc.top/` | download-only public proxies |

Mirror semantics: any GitHub request is tried direct first, then prefixed onto the **matching**
mirror class (`<mirror><full-github-url>`). API metadata must use `GithubApiMirrors` (public download
proxies 403 the REST API); release/raw downloads must use `GithubDownloadMirrors` (the self-hosted API
route 400s downloads). `GithubProxy.Candidates` selects by URL type.

Port note: the Rust equivalent should be `luatools-core/src/config.rs` with `&'static str` constants.
Do not copy the Supabase anon JWT into a public Rust source file unless it is confirmed to be the same
public value; prefer a build-time `env!`/`option_env!` or a redacted constant and document the
rotation procedure.

## 2. Tray icon, window state, updates and protocol registration

### 2.1 Tray icon

`MainWindow` (a `Wpf.Ui.Controls.FluentWindow`) creates a WinForms `NotifyIcon` in its constructor:

- `Text = "LuaTools"`, `Visible = false`.
- Icon loaded from `pack://application:,,,/icon.ico`; on failure the icon is left null and the menu
  still works.
- `DoubleClick` → `RestoreFromTray()`.
- Context menu (`ContextMenuStrip`):
  - `Strings.Tray_Open` → `RestoreFromTray()`
  - separator
  - `Strings.Tray_Exit` → `ExitApp()`
- The menu labels are resolved at window construction; a language change therefore also requires the
  relaunch already prompted by Settings.
- There is no left-click handler and no minimize event handler; "minimize to tray" is implemented
  only on window **close**, not on `WindowState.Minimized`.

`RestoreFromTray()`:

1. `Show()`
2. `WindowState = WindowState.Normal`
3. hide tray icon
4. `Activate()`
5. `Topmost = true` then `Topmost = false` (bounce to defeat Windows foreground rules when the request
   comes from another process)
6. `Focus()`

`StartSilent()` only makes the tray icon visible; it never shows the window.

`ShowInstallNotification(message, error)` makes the tray icon visible and shows a balloon:
5,000 ms, title `"LuaTools"`, `ToolTipIcon.Error` when `error`, otherwise `Info`.

### 2.2 Window state persistence

There is **none**. The application does not persist window position, size, maximized state or
monitor. `MainWindow.xaml` declares:

- `Width="1080"`, `Height="720"`
- `MinWidth="760"`, `MinHeight="560"`
- `WindowStartupLocation="CenterScreen"`
- `ExtendsContentIntoTitleBar="True"`, `WindowBackdropType="Mica"`, `WindowCornerPreference="Round"`
- `Title="LuaTools"`, `Icon="icon.ico"`

The only persisted lifecycle-adjacent UI state is `MinimizeToTray` (settings) and the
`StartWithWindows` Run-key entry. The Rust port should not assume geometry persistence; if adding it,
that is a new feature, not a parity requirement. (The Rust workspace already enables eframe's
`persistence` feature; using it would be an intentional deviation.)

`MinimizeToTray` defaults false. `SettingsViewModel.OnMinimizeToTrayChanged` writes the setting and,
when turned **off**, calls `RequestShowWindow` to restore the window if hidden.

`StartWithWindows` defaults false. When enabled, `SettingsViewModel` writes
`HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Run`, value name `LuaTools`, data
`"<Environment.ProcessPath>" --minimized`. Disabling deletes the value. Failures are swallowed; the
setting is still saved even when the registry write fails.

### 2.3 Auto-update check and install flow (Velopack)

`UpdateService` constructs one `UpdateManager` per repo in `AppConfig.GithubReleasesRepos`, in order:

1. `https://github.com/madoiscool/LuaTools` (primary)
2. `https://github.com/mendy-tools/LuaTools` (backup; populated manually only if the primary dies)

Each manager uses
`new GithubSource(repo, accessToken: null, prerelease: false, downloader: new ProxiedFileDownloader())`.
The proxied downloader routes feed/package requests direct first, then capability-matched mirrors
(API mirrors for `api.github.com`, download mirrors for release assets).

`CheckAndStageAsync()`:

- If `_managers` is empty or `_managers[0].IsInstalled == false`, return. This makes `dotnet run` and
  unpacked builds no-op. `IsInstalled` is a property of the Velopack installation, not the repo.
- For each manager in order:
  - `CheckForUpdatesAsync()`. If it returns null, the repo is reachable and we are current: **stop**;
    do not fall through to a backup, which may lag and would offer no/older version.
  - `DownloadUpdatesAsync(info)` (delta download per Velopack).
  - remember `_stagedMgr` and `_staged`, raise `UpdateReady` (currently no subscriber), return.
  - any exception (unreachable repo, failed download) → try the next repo.
- Every repo failed → silent no-op; retry next launch/Steam-open.

`HasStagedUpdate` is `_staged is not null`.

`ApplyAndRestart(string[]? restartArgs)` calls `_stagedMgr.ApplyUpdatesAndRestart(_staged, restartArgs)`.
The app-restart flow passes exactly `["--minimized", "--tray-locked"]`, so the relaunched instance
keeps tray/loader semantics.

`ApplyOnExit()` calls `_stagedMgr.WaitExitThenApplyUpdates(_staged, silent: true, restart: false)`.

Trigger points:

- **Steam-open / loader launch**: `OnStartup` only calls `RunUpdateFlowAsync()` when
  `Program.SessionTrayLock` is true. Manual, protocol and silent-install launches never auto-update.
- **Recheck event**: a `--tray-locked` second instance sets `LuaToolsGui.RecheckUpdates`, causing the
  live app to re-run the flow.
- **HTTP `POST /check-updates`**: `App.RunUpdateFlow` (wired in `OnStartup`) so the store plugin's
  "Check for updates" button uses the same app+plugin flow. If not wired yet, it falls back to
  `UpdateService.CheckAndStageAsync()` + `PluginInstallerService.AutoUpdateAsync()`.
- **Exit**: `OnExit` applies a staged update without restart if one is still pending.

`RunUpdateFlowAsync` is serialized by `_updateFlowGate = new SemaphoreSlim(1,1)` and uses `Wait(0)`;
a second overlapping run drops out immediately. Exact order:

1. `try { await Updates.CheckAndStageAsync(); } catch { }`.
2. If `HasStagedUpdate`, `Dispatcher.Invoke(() => ApplyAndRestart(["--minimized","--tray-locked"]))`
   and return. This process ends; the relaunched current instance re-enters the flow via OnStartup.
3. Else plugin update:
   - `PluginInstallerService.GetStatusAsync(force: true)`; if `UpdateAvailable`:
     - if `!DllMatches`, toast `"LuaTools"` / `"Updating plugin. Steam will restart."` (hardcoded
       English; a localization defect);
     - `await installer.InstallAsync(progress: null)`.
   - all errors swallowed; retry on next Steam-open.

App-before-plugin ordering is deliberate: the app's compiled code (e.g. the CDP port constant) may
depend on the plugin version, and the two ship from separate repos/cadences.

Velopack only updates to a **strictly higher** version, so every release must bump `--packVersion`.

### 2.4 Protocol registration and URL grammar

`ProtocolService.Register()` (called on every launch, before the single-instance check):

```
HKCU\Software\Classes\luatools
    (Default)      = "URL:LuaTools Protocol"
    "URL Protocol" = ""                       (empty string)
HKCU\Software\Classes\luatools\shell\open\command
    (Default)      = "\"<Environment.ProcessPath>\" \"%1\""
```

No `FriendlyTypeName`, no icon, no `DefaultIcon`, no uninstall cleanup. Errors swallowed.

Pending URL file: `%AppData%\LuaToolsGui\protocol_url.tmp`.
`WritePending` creates the directory, writes the raw URL (no newline added).
`TryReadPending` reads all text, `Trim()`s it, deletes the file, and returns null for empty; all
errors swallowed.

`Parse(url)`:

1. Null/whitespace → `(null, null, false)`.
2. `new Uri(url)`; invalid → `(null, null, false)`.
3. Scheme must equal `luatools` case-insensitively.
4. `action = uri.Authority.ToLowerInvariant()`; must be exactly `game`, `install`, `manage` or `fix`.
5. `id = uri.AbsolutePath.TrimStart('/')`.
6. Only for `install`, if `id` starts with `silent/` (case-insensitive), set `Silent = true` and strip
   that prefix.
7. `long.TryParse(id)` on the entire remainder; failure → `(null, null, false)`.

Consequences: a trailing slash (`luatools://install/648800/`) fails because the id becomes
`648800/`; query strings are not part of `AbsolutePath` and are ignored; userinfo/port are not used.

Supported forms:

| URL | Action | AppId | Silent |
|---|---|---|---|
| `luatools://game/648800` | game | 648800 | false |
| `luatools://install/648800` | install | 648800 | false |
| `luatools://install/silent/648800` | install | 648800 | true |
| `luatools://manage/648800` | manage | 648800 | false |
| `luatools://fix/648800` | fix | 648800 | false |

`App.HandleProtocolUrl` dispatch:

| Action | Behavior |
|---|---|
| `game` | navigate Add; `DownloadViewModel.SeedSearch(appId)` (no install) |
| `install`, silent | headless: `download.ProtocolInstall(appId, callback)`; callback on UI thread calls `window.ShowInstallNotification(msg, error)`; if `_exitAfterSilentInstall && queue.ActiveCount == 0`, wait 6,000 ms then `Dispatcher.Invoke(Shutdown)`. No navigation, no window. |
| `install`, not silent | navigate Add; `download.ProtocolInstall(appId)` |
| `manage` | navigate Manage; `manage.OpenDetailForAppIdAsync(appId)` |
| `fix` | navigate Fixes; `fixes.OpenForAppIdAsync(appId)` |

If `action` or `appId` is null, `HandleProtocolUrl` returns silently.

Silent-startup rules from §1.7: `ProtocolService.Parse(url).Silent || Program.StartMinimized`; the
window is never shown, the tray icon is visible, and auth restore is done explicitly because the
window `Loaded` handler will not run.

---

## 3. Models (`src/LuaToolsGui/Models/`)

### 3.1 Serialization conventions

- All models are plain classes/records with public settable properties. There are **no** validation
  attributes, no `required`, no data annotations, and no custom converters.
- `System.Text.Json` reads with `PropertyNameCaseInsensitive = true` in the API/auth readers; the
  attribute names below are nevertheless the exact wire keys. Unknown JSON properties are ignored.
- Missing properties take the CLR default from the initializer (`""`, `[]`, `0`, `false`, `null`).
- A present property with the wrong JSON kind (e.g. string where a number is declared) throws
  `JsonException`; callers generally catch and treat the whole response as unavailable.
- Classes with no `[JsonPropertyName]` serialize as **PascalCase** property names. This matters for
  `StoredAuth` (persisted on disk) and, if ever serialized, `SteamSearchResult` and `StandardUsage`.
- Enums are serialized by name where they are serialized at all. `UnlockerMode` is stored as a string
  in `settings.json`; `ModeKind`/`ModeStatus` are UI/query state and are not persisted.
- Timestamps that come from the wire are strings (`createdAt`, `appliedAt`, `api_key_expires_at`,
  `releaseDate`) except GitHub's `published_at`, which is a `DateTimeOffset?`. `StoredAuth.ExpiresAt`
  is a `DateTimeOffset` serialized by System.Text.Json as ISO 8601 with offset.
- Record positional parameters serialize with their PascalCase parameter names if serialized.

### 3.1.1 Validation

The model layer performs **no** validation of its own. There are no `[Required]`, `[Range]`,
`[StringLength]` or custom validators; deserialization only enforces CLR types and nullability.
Domain validation is spread across services and callers:

- `ProtocolService.Parse` requires a valid `Uri`, an allowed action, and a fully numeric appid
  (`long.TryParse`); otherwise it returns nulls.
- `AppInfoFile` requires magic 27/28/29 and a structurally valid header/string table; anything else
  throws `AppInfoFormatException`.
- `DepotCacheMigrationService` requires `<depot>_<gid>.manifest` with numeric `long`/`ulong` parts and
  content that matches the name (`ManifestFile.Matches`), or it rejects the file.
- `ModeMigration` deliberately uses case-sensitive `Enum.TryParse<UnlockerMode>`; lowercase values are
  treated as unknown and trigger re-onboarding.
- `SettingsService.DnsMode` is a whitelist: only `Always` and `Never` are returned as-is; anything
  else (including null) becomes `Auto`.
- `LaunchOptionsService.Read` returns null for an app not present in appinfo; the view model surfaces
  `Error` and `CanSave == false`.
- `SteamAppInfoCache.FindAppDetailsEntry` refuses an entry whose `data.steam_appid` is missing,
  non-numeric, or different from the requested appid; it never guesses.

The Rust port should keep validation at the same layer boundaries (codec/service), not add
`serde(deny_unknown_fields)` or model-level validators that would change tolerance of missing/extra
fields.

### 3.2 `ApiModels.cs`

#### Steam search / store

| Type | Field | JSON name | CLR type | Notes |
|---|---|---|---|---|
| `SteamStoreSearchResponse` | `Items` | `items` | `List<SteamStoreItem>` | Steam `storesearch` response |
| `SteamStoreItem` | `Id` | `id` | `long` | appid |
| | `Name` | `name` | `string` | default `""` |
| | `TinyImage` | `tiny_image` | `string?` | |
| `SteamSearchResult` | `AppId` | `AppId` (PascalCase, no attribute) | `long` | internal UI DTO, **not** read from Steam JSON |
| | `Name` | `Name` | `string` | |
| | `Icon` | `Icon` | `string?` | falls back to `https://cdn.cloudflare.steamstatic.com/steam/apps/<id>/capsule_sm_120.jpg` |
| `SteamFeaturedResponse` | `TopSellers` | `top_sellers` | `SteamFeaturedCategory?` | |
| | `NewReleases` | `new_releases` | `SteamFeaturedCategory?` | |
| `SteamFeaturedCategory` | `Items` | `items` | `List<SteamFeaturedItem>` | |
| `SteamFeaturedItem` | `Id` | `id` | `long` | |
| | `Name` | `name` | `string` | |
| | `LargeCapsuleImage` | `large_capsule_image` | `string?` | 616×353 capsule |
| | `Type` | `type` | `int` | `0` = game; non-zero bundles/etc. are filtered out |

`LuaToolsApiClient.SearchAsync` caps results at 8 (`Take(8)`).
`GetFeaturedAsync` keeps only `Type == 0`, `Id > 0`, non-empty `LargeCapsuleImage`, de-duplicates by
`Id` keeping first occurrence, caps each list at 20.

#### Game details / DLC

| Type | Field | JSON name | CLR type | Notes |
|---|---|---|---|---|
| `GameDetails` | `Name` | `name` | `string` | |
| | `AppId` | `appid` | `long` | |
| | `Type` | `type` | `string` | `"dlc"`, `"game"`, etc. |
| | `BaseAppId` | `baseAppId` | `string?` | camelCase on the wire |
| | `Genres` | `genres` | `List<string>` | website API emits names, not ids |
| | `HeaderImage` | `headerImage` | `string?` | |
| | `ReleaseDate` | `releaseDate` | `string?` | display text, not parsed |
| | `IsDlc` | — | computed `bool` | `[JsonIgnore]`; case-insensitive `Type == "dlc"` |
| `DlcDepot` | `DepotId` | `depotId` | `string` | not a number |
| | `Language` | `language` | `string?` | |
| | `OsList` | `oslist` | `string?` | |
| | `Included` | `included` | `bool` | |
| | `Meta` | — | computed `string` | `[JsonIgnore]`; `(Language ?? "default")` plus `" · " + OsList` only when OsList non-empty |
| `DlcInfo` | `AppId` | `appid` | `string` | |
| | `Name` | `name` | `string` | |
| | `Type` | `type` | `string` | |
| | `DepotCount` | `depotCount` | `int` | |
| | `HaveCount` | `haveCount` | `int` | |
| | `MissingCount` | `missingCount` | `int` | |
| | `Depots` | `depots` | `List<DlcDepot>` | |

`DlcDepot.Meta` examples: language `english`, oslist `windows` → `english · windows`; no language →
`default`; no oslist → just `english`.

#### Hubcap / errors / usage

| Type | Field | JSON name | CLR type | Notes |
|---|---|---|---|---|
| `HubcapStats` | `UserId` | `user_id` | `string` | |
| | `DailyUsage` | `daily_usage` | `int` | |
| | `DailyLimit` | `daily_limit` | `int` | |
| | `CanMakeRequests` | `can_make_requests` | `bool` | |
| | `ApiKeyExpiresAt` | `api_key_expires_at` | `string?` | date string |
| `HubcapManifestStatus` | `Status` | `status` | `string` | |
| | `ManifestFileExists` | `manifest_file_exists` | `bool` | |
| `ApiError` | `Error` | `error` | `string?` | |
| `StandardUsage` | `Used` | `Used` (PascalCase) | `int` | positional record, constructed locally |
| | `Limit` | `Limit` | `int` | always `AppConfig.DailyDownloadLimit` = 25 |
| `SupporterStatus` | `IsSupporter` | `isSupporter` | `bool` | camelCase on the wire |

#### Auth / Supabase

| Type | Field | JSON name | CLR type | Notes |
|---|---|---|---|---|
| `CodeRedeemResponse` | `UserId` | `user_id` | `string` | Discord bot code redeem |
| | `Token` | `token` | `string` | magic-link token |
| `SupabaseSession` | `AccessToken` | `access_token` | `string` | |
| | `RefreshToken` | `refresh_token` | `string` | |
| | `ExpiresIn` | `expires_in` | `int` | seconds |
| | `User` | `user` | `SupabaseUser?` | |
| `SupabaseUser` | `Id` | `id` | `string` | UUID |
| | `Email` | `email` | `string?` | bot accounts end with `@bot.lua.tools` |
| | `Metadata` | `user_metadata` | `UserMetadata?` | |
| `UserMetadata` | `FullName` | `full_name` | `string?` | |
| | `Name` | `name` | `string?` | |
| | `AvatarUrl` | `avatar_url` | `string?` | |
| | `CustomClaims` | `custom_claims` | `CustomClaims?` | |
| `CustomClaims` | `GlobalName` | `global_name` | `string?` | |
| `StoredAuth` | `RefreshToken` | `RefreshToken` (PascalCase) | `string` | persisted, see below |
| | `AccessToken` | `AccessToken` | `string` | |
| | `ExpiresAt` | `ExpiresAt` | `DateTimeOffset` | ISO 8601 |
| | `DisplayName` | `DisplayName` | `string?` | |
| | `Email` | `Email` | `string?` | |
| | `AvatarUrl` | `AvatarUrl` | `string?` | |

`StoredAuth` is serialized with default options (PascalCase, no indentation override), encrypted with
Windows DPAPI `ProtectedData.Protect(..., DataProtectionScope.CurrentUser)`, and written to
`%AppData%\LuaToolsGui\auth.dat`. `LoadStored` calls `ProtectedData.Unprotect` with the same scope.
A Rust port needs DPAPI (`CryptProtectData`/`CryptUnprotectData`) to read existing sessions, or must
accept a one-time re-login.

#### Source metadata

`SourceMeta` is a static table, not a wire DTO:

| Key | DisplayName | DiscordUrl | RequiresUserKey |
|---|---|---|---|
| `Ryuu` | null (use key) | `https://discord.gg/manifests` | false |
| `TwentyTwo Cloud` | null | `https://discord.gg/RrukXPyv5b` | false |
| `Sushi` | null | `https://discord.gg/hMdv5dQhcN` | false |
| `Skyflare` | null | `https://discord.gg/luatools` | false |
| `Sadie (Morrenus)` | `Sadie (Hubcap)` | `https://discord.gg/hubcapsmanifest` | true |

`SourceMeta.Get(name)` returns a default `Meta(null, null, false)` for unknown names. The record is
`Meta(string? DisplayName = null, string? DiscordUrl = null, bool RequiresUserKey = false)`.

### 3.3 `FixModels.cs` (Denuvo fixes — DRM feature; drop in the Rust port)

All names are exact wire keys. This entire file exists to download and apply Denuvo bypass fixes and
to record/revert them. If the Rust port excludes DRM fixes (recommended), none of these types are
needed.

| Type | Field | JSON name | CLR type | Notes |
|---|---|---|---|---|
| `DenuvoListingsResponse` | `Games` | `games` | `List<DenuvoGameListing>` | `/api/denuvo/listings` |
| | `Tags` | `tags` | `List<DenuvoTag>` | |
| `DenuvoGameListing` | `AppId` | `appid` | `string` | |
| | `Name` | `name` | `string` | |
| | `HeaderImage` | `header_image` | `string?` | |
| | `FixCount` | `fixCount` | `int` | camelCase |
| | `Tags` | `tags` | `List<DenuvoTag>` | |
| `DenuvoTag` | `Id` | `id` | `string` | |
| | `Name` | `name` | `string` | |
| | `Slug` | `slug` | `string` | |
| | `Color` | `color` | `string?` | |
| `DenuvoFixesResponse` | `AppId` | `appid` | `string` | |
| | `Name` | `name` | `string` | |
| | `HeaderImage` | `header_image` | `string?` | |
| | `Fixes` | `fixes` | `List<DenuvoFix>` | |
| `DenuvoFix` | `Id` | `id` | `string` | |
| | `Title` | `title` | `string` | |
| | `Description` | `description` | `string?` | |
| | `Tags` | `tags` | `List<DenuvoTag>` | |
| | `HasManifest` | `hasManifest` | `bool` | camelCase |
| | `HasFix` | `hasFix` | `bool` | camelCase |
| | `ManifestFilename` | `manifestFilename` | `string?` | camelCase |
| | `FixFilename` | `fixFilename` | `string?` | camelCase |
| | `CreatedAt` | `createdAt` | `string?` | camelCase |
| `DenuvoDownloadResponse` | `Url` | `url` | `string` | signed URL |
| `DenuvoFixRecord` | `AppId` | `appId` | `long` | on-disk `.luatools-fix/<key>.json` |
| | `FixId` | `fixId` | `string` | |
| | `AppliedAt` | `appliedAt` | `string` | |
| | `Files` | `files` | `List<DenuvoFixRecordEntry>` | |
| `DenuvoFixRecordEntry` | `RelativePath` | `relativePath` | `string` | relative to game folder |
| | `Action` | `action` | `string` | exactly `"modified"` or `"added"` |
| | `BackupPath` | `backupPath` | `string?` | relative to `.luatools-fix/` |
| | `HashAfter` | `hashAfter` | `string?` | SHA-256 as left by the fix |
| | `HashBefore` | `hashBefore` | `string?` | null for `"added"` entries |
| `AppliedFixIndex` | `Entries` | `entries` | `List<AppliedFixIndexEntry>` | `%AppData%\LuaToolsGui\applied-fixes.json`; a hint only |
| `AppliedFixIndexEntry` | `AppId` | `appId` | `long` | camelCase |
| | `FixId` | `fixId` | `string` | |
| | `GameName` | `gameName` | `string` | |
| | `InstallDir` | `installDir` | `string` | |
| | `AppliedAt` | `appliedAt` | `string` | |
| | `FileCount` | `fileCount` | `int` | |

Semantics encoded in comments and relied on by fix apply/revert:

- The on-disk `JsonPropertyName` values above are the contract and must not be renamed even if the
  type names change.
- Backups chain when fixes are stacked. Reverts must proceed newest-first conceptually, but the actual
  guard is `hashAfter`: if the file on disk does not match what the fix wrote, the revert stops rather
  than clobbering later edits.
- An absent `hashAfter`/`hashBefore` means "cannot verify", not "verification failed". Older records
  must remain revertible. `FileHash.Matches(path, null)` returns true for this reason.
- `AppliedFixIndex` may be deleted/regenerated; the per-game record is authoritative.

### 3.4 `ModeModels.cs` (unlocker modes — drop in the Rust port)

Enums and their persisted/display values:

| Enum | Members | Wire/on-disk values | Notes |
|---|---|---|---|
| `UnlockerMode` | `Ost` | `"Ost"` | OpenSteamTools nightly; repo `madoiscool/OST-Nightly` |
| | `Bst` | `"Bst"` | BetterSteamTools fork; repo `madoiscool/BetterSteamTools` |
| | `Custom` | `"Custom"` | user manages their own unlocker |
| `ModeKind` | `Zip` | `"Zip"` | download/verify/extract a release zip |
| | `Manual` | `"Manual"` | place nothing |
| `ModeStatus` | `Unknown` | `"Unknown"` | offline / GitHub unreachable / Steam not located |
| | `NotInstalled` | `"NotInstalled"` | |
| | `UpToDate` | `"UpToDate"` | |
| | `UpdateAvailable` | `"UpdateAvailable"` | |
| | `UserManaged` | `"UserManaged"` | Custom mode: nothing to check |

The enum names are deliberately not the old `SteamTools`, `OpenSteamTools`,
`OpenSteamToolsNightly`, `CloudRedirect` values. No legacy string parses as a current member; that is
what makes `ModeMigration` one-shot without a marker flag.

Records/classes:

| Type | Members (types) | Notes |
|---|---|---|
| `ModeDefinition` | `Mode: UnlockerMode`, `DisplayName: string`, `Description: string`, `Kind: ModeKind`, `Owner: string`, `Repo: string`, `FixedTag: string?`, `PlaceFiles: string[]`, `ZipAssetPattern: string?`, `UpdateManifestUrl: string? = null`, `HiddenUnlessFile: string? = null` | positional record. `FixedTag "ST"` example; null means latest release. `{version}` placeholder in `ZipAssetPattern`. `HiddenUnlessFile` hides a card unless the file exists in the Steam root (or the mode is active). |
| `UpdateManifest` | `Version: string`, `File: string`, `Sha256: string` | parsed from `latest.toml`; `Version` is a release tag like `v1.0.0` and is also used to build the zip URL; `File` is a bare filename; `Sha256` lower-case hex. |
| `GithubRelease` | `TagName: string` (`tag_name`), `PublishedAt: DateTimeOffset?` (`published_at`), `Assets: List<GithubAsset>` (`assets`) | sort by `published_at`; `/releases` sorts by `created_at`, which can tie. |
| `GithubAsset` | `Name: string` (`name`), `DownloadUrl: string` (`browser_download_url`), `Digest: string?` (`digest`, `"sha256:<hex>"`), `Size: long` (`size`) | size lets a tool download show real bytes rather than a bare fraction. |
| `ModeState` | `Mode: UnlockerMode`, `Status: ModeStatus`, `IsActive: bool`, `LatestVersion: string?` | UI card state. |
| `CloudRedirectAddonState` | `Installed: bool`, `Enabled: bool`, `UpdateAvailable: bool`, `LatestVersion: string?` | derived from `cloud_redirect.dll` presence and `[cloud] enabled` in `opensteamtool.toml`. |
| `ModeInstallResult` | `Success: bool`, `Error: string?`, `Failed: IReadOnlyList<string>` | `Ok()` → `(true, null, [])`; `Fail(error)` → `(false, error, [])`. Mirrors `LuaInstaller.InstallResult`. |

Current `ModeDefinition` values (from `UnlockerService`, useful for exact model semantics):

| Mode | DisplayName | Kind | Owner/Repo | PlaceFiles | ZipAssetPattern | UpdateManifestUrl | HiddenUnlessFile |
|---|---|---|---|---|---|---|---|
| Ost | `OpenSteamTools` | Zip | `madoiscool/OST-Nightly` | `dwmapi.dll`, `xinput1_4.dll`, `OpenSteamTool.dll` | `OpenSteamTool-{version}-Release.zip` | null (GitHub releases API) | null |
| Bst | `BetterSteamTools` | Zip | `madoiscool/BetterSteamTools` | same three | same pattern | `https://raw.githubusercontent.com/madoiscool/BetterSteamTools/refs/heads/updates/opensteamtool/latest.toml` | null |
| Custom | localized `Mode_Name_Custom` | Manual | empty | empty | null | null | null |

`latest.toml` parsing is line-oriented: the service reads `version`, `path`, `sha256` from the text
and lowercases the hash; `Path.GetFileName(path)` strips any directory. The zip URL for a
manifest-driven mode is
`https://github.com/<Owner>/<Repo>/releases/download/<Version>/<pattern with {version replaced}>`.

### 3.5 Other lifecycle-relevant on-disk contracts

These are not in `Models/` but are part of the lifecycle and model layer.

#### `%AppData%\LuaToolsGui\settings.json` (`AppSettings`, PascalCase)

| Property | Type | Default / interpretation |
|---|---|---|
| `SteamPathOverride` | `string?` | null = auto-detect from registry |
| `SelectedMode` | `string?` | `"Ost"` / `"Bst"` / `"Custom"`; legacy values migrated |
| `AutoUpdateApps` | `bool?` | null → true |
| `DonateKeys` | `bool?` | null → true |
| `ManagePageSize` | `int?` | null → 24; 0 = All |
| `FixesPageSize` | `int?` | null → 24; 0 = All |
| `BuildsPageSize` | `int?` | null → 10; 0 = All |
| `Language` | `string?` | BCP-47 tag; null = follow Windows |
| `HubcapApiKey` | `string?` | `smm_…`; stored locally |
| `StartWithWindows` | `bool?` | null → false |
| `MinimizeToTray` | `bool?` | null → false |
| `FastFetch` | `bool?` | null → false |
| `DnsMode` | `string?` | `"Auto"` / `"Always"` / `"Never"`; null or anything else → `"Auto"` |

Persistence details:

- The serializer uses `WriteIndented = true` and the default null handling, so null-valued
  properties are written explicitly. Do not add `skip_serializing_if = "Option::is_none"` if
  byte-level compatibility with existing files matters.
- Directory `%AppData%\LuaToolsGui`; primary `settings.json`, temp `settings.json.tmp`, backup
  `settings.json.bak`.
- Load: primary → if unparseable/missing, **move a present non-empty primary to `.corrupt`** → try
  `.bak` → empty defaults. A present-but-corrupt file is never silently reset in place.
- Save: if every persisted field is null, delete primary, backup and temp and return. Otherwise
  serialize with `WriteIndented = true`, write `.tmp`, copy the old primary to `.bak` (best effort),
  atomically `File.Move(.tmp, primary, overwrite: true)`.
- **Known defect to decide on**: the "empty" test lists `SteamPathOverride`, `SelectedMode`,
  `AutoUpdateApps`, `DonateKeys`, `ManagePageSize`, `FixesPageSize`, `BuildsPageSize`, `Language`,
  `HubcapApiKey`, `StartWithWindows`, `MinimizeToTray`, `FastFetch` — but **not `DnsMode`**. A user
  whose only change is the DNS mode gets `settings.json` deleted on that save. The port should fix
  this, but should record the compatibility decision.

#### `%AppData%\LuaToolsGui\cache.json` (`CacheData`, PascalCase)

| Property | Type | Notes |
|---|---|---|
| `OpenSteamToolsInstalledVersion` | `string?` | mode fingerprint |
| `OpenSteamToolsInstalledZipDigest` | `string?` | sha256 of installed zip |
| `SteamApiRequestTimes` | `List<long>` | Unix-ms rolling rate-limit window |
| `DonatedAppIds` | `List<string>` | permanent; backend accepts each depot once per IP |
| `HardwareAppIds` | `List<long>` | Steam hardware appids to hide |
| `HardwareAppIdsFetchedAtMs` | `long` | Unix-ms, 0 = never |
| `LoadedAppIds` | `List<long>` | "recently loaded" notification list |
| `OnboardingComplete` | `bool` | first-run flag |
| `DownloadHistory` | `List<DownloadHistoryRecord>` | newest first, capped 100 |
| `DepotDownloaderVersion` | `string?` | tool fingerprint |
| `DepotDownloaderCheckedAtMs` | `long` | 0 = never |
| `SteamlessVersion` | `string?` | |
| `SteamlessCheckedAtMs` | `long` | |
| `SteamAutoCrackVersion` | `string?` | |
| `SteamAutoCrackCheckedAtMs` | `long` | |

`CacheService` serializes `Save` under one lock (callers mutate fields, then call `Save`), writes
`cache.json.tmp` then atomic-moves over `cache.json`, and deletes the file when every field is empty.
It does **not** keep a `.bak`; a parse failure silently resets to `new CacheData()`, which can wipe
`DonatedAppIds`.

#### Launch-option state

- `%AppData%\LuaToolsGui\launchmods.json`: a JSON array of `LaunchMod` objects with PascalCase
  properties: `AppId` (`int`), `ChangeNumber` (`uint`), `Original` (`List<LaunchOption>`),
  `Desired` (`List<LaunchOption>`), `SavedAt` (`DateTime`, UTC). `LaunchOption` also has no JSON
  attributes, so its on-disk fields are PascalCase: `Index`, `SourceIndex`, `Executable`,
  `Arguments`, `WorkingDir`, `Description`, `Type`, `OsList`, `OsArch`, `BetaKey`, `OwnsDlc`
  (`Summary` and `Clone` are `[JsonIgnore]`/methods).
- `%AppData%\LuaToolsGui\luavault\<appid>\index.json`: `VaultIndex` with `AppId` (`long`),
  `Variants` (`List<LuaVariant>`), `EditBaseHash` (`string?`); byte files are
  `<sha256-lowercase>.lua` in the same folder. `LuaVariant` record fields: `Hash`, `Kind`, `BuildId`,
  `Label`, `CapturedAt`, `Source`, `ManifestIds`, `DepotCount`, `DlcCount`; `IsPinned` and
  `DisplayLabel` are computed.
- `LuaVariantKind` values are lower-case strings: `"default"`, `"build"`, `"preset"`. These are
  stored values matched in code and must never be localized.
- Vault index save uses a `.bak` fallback (`index.json.bak`), like settings.

---

## 4. i18n architecture

### 4.1 File set and inventory

| Item | Value |
|---|---|
| English source | `src/LuaToolsGui/Resources/Strings.resx` |
| Translated files | 29: `Strings.<tag>.resx` |
| Total RESX files | 30 (English base + 29 translations) |
| Keys per file | **552 in every file** (verified by parsing all 30) |
| Keys with placeholders | 95 use `{0}`; 21 also use `{1}`; 3 also use `{2}`; no `{3}`+ |
| Escaped braces | none (`{{`/`}}` never appear) |
| `<comment>` elements | none in `Strings.resx`; the schema permits them but the CI regex does not tolerate them |
| File formatting | every `<data name="Key" xml:space="preserve"><value>` opening is contiguous and every `</value></data>` closes it; values may contain embedded newlines (confirmation dialogs do). No `<comment>`. |
| `Strings.Designer.cs` | hand-maintained; exactly 552 static accessors plus `Get(string key)` |
| XAML references | 240 distinct keys via `{x:Static res:Strings.Key}` |
| C# references | 418 occurrences of `Strings.*` across the app |

The three keys using `{2}` are `Settings_HubcapKeyOkExpiry`, `Plugin_Err_MissingAssets` and
`Builds_Select_Space`. Placeholders are always positional integers starting at 0; there is no ICU
plural syntax, no named placeholders, and no HTML markup in values.

### 4.2 RESX schema

Each file is UTF-8 XML with this shape:

1. `<?xml version="1.0" encoding="utf-8"?>`
2. `<root>` containing the standard .NET RESX XSD block (`xsd:schema id="root"` with `metadata`,
   `assembly`, `data`, `resheader` elements). This block is identical in every file.
3. Four `<resheader>` entries:
   - `resmimetype` = `text/microsoft-resx`
   - `version` = `2.0`
   - `reader` = `System.Resources.ResXResourceReader, System.Windows.Forms, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089`
   - `writer` = `System.Resources.ResXResourceWriter, System.Windows.Forms, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089`
4. A language marker comment at the top of translated files (`<!-- de -->`).
5. Section comments such as `<!-- ── Navigation (MainWindow) ── -->`, `<!-- ── Settings ── -->`.
6. 552 `<data name="..." xml:space="preserve"><value>...</value></data>` entries. The opening tag and
   `<value>` are contiguous on one line, and `</value></data>` closes on the final line; the value
   itself may contain embedded newlines (e.g. `Manage_Delete_Body`, `Depot_Cancel_Body`). The key set
   is identical in every file, but the ordering is not: translated files reorder or append newer keys
   (roughly 490 positions differ from English in most languages). Never rely on positional alignment.

There is no `<assembly>` alias, no `type=` attribute, no `mimetype`, no `<metadata>`, and no
`<comment>` in the actual data. XML special characters are escaped (`&amp;`, `&lt;`, `&gt;`); the
translator README calls this out explicitly.

### 4.3 Key naming grammar

Keys are ASCII, case-sensitive, and follow:

```
<Area>_<SubArea>_<Detail>
```

with PascalCase components after each underscore. Examples: `Nav_Home`, `Settings_Section_Startup`,
`Builds_Action_SaveAsPreset`, `Downloads_Status_AwaitingConfirm`, `Err_VerifyFailedFile`,
`Fixes_Revert_Conflict_Body`.

Key prefixes and counts in the English source:

| Prefix | Count | Typical purpose |
|---|---:|---|
| `Manage` | 77 | Manage page: filters, sorts, actions, delete/reveal, depots, Steamless strings |
| `Settings` | 61 | Settings page: account, Hubcap, language, DNS, Steam path, startup, toggles |
| `Builds` | 55 | Builds/Depots page: variants, editor, depot lock/enable, download selection |
| `Mode` | 45 | Unlocker mode cards, statuses, CloudRedirect add-on |
| `Downloads` | 45 | Download queue/history statuses, actions, depot/SAC progress |
| `Fixes` | 42 | Denuvo fixes grid/detail/revert |
| `Plugin` | 42 | Store plugin install/update/status/errors |
| `Add` | 40 | Add/search/featured/download page |
| `Launch` | 33 | Launch-options editor |
| `Err` | 16 | Service errors surfaced in toasts/MessageBoxes |
| `Home` | 14 | Home dashboard |
| `Nav` | 13 | Navigation rail/footer |
| `Drop` | 13 | Drag-and-drop zone |
| `Depot` | 12 | Depot download errors/cancel |
| `Onboarding` | 11 | First-run overlay |
| `Confirm` | 7 | Shared confirm overlay |
| `Common` | 6 | Search placeholder, loading, appid fallback, copy/reveal |
| `Auth` | 6 | Auth flow errors |
| `Api` | 4 | API errors |
| `Hubcap` | 4 | Hubcap key errors |
| `Lang` | 3 | Language-changed toast |
| `Tray` | 2 | `Tray_Open`, `Tray_Exit` |
| `Main` | 1 | `Main_RestartSteam_Ask` |
| **Total** | **552** | |

Lifecycle-relevant keys, exact:

- `Nav`: `Nav_Home`, `Nav_Add`, `Nav_Manage`, `Nav_Builds`, `Nav_Mode`, `Nav_Fixes`,
  `Nav_RestartSteam`, `Nav_Settings`, `Nav_SignInDiscord`, `Nav_WaitingForBrowser`,
  `Nav_Footer_Guest`, `Nav_Footer_LoggedIn`, `Nav_Downloads`.
- `Tray`: `Tray_Open`, `Tray_Exit`.
- `Main`: `Main_RestartSteam_Ask`.
- `Lang`: `Lang_Changed_Title`, `Lang_Changed_Body`, `Lang_Changed_Restart`.
- `Onboarding`: `Onboarding_Title`, `Onboarding_SignIn_Blurb`, `Onboarding_SignedIn`,
  `Onboarding_Recommended_Title`, `Onboarding_Recommended_Detail`, `Onboarding_Plugin_Title`,
  `Onboarding_Plugin_Detail`, `Onboarding_Yes`, `Onboarding_No`, `Onboarding_Go`,
  `Onboarding_Applying`.
- `Settings` lifecycle subset: `Settings_Title`, `Settings_SignOut`, `Settings_BrowsingAsGuest`,
  `Settings_GuestHint`, `Settings_SignInDiscord`, `Settings_BotCode_*`, `Settings_BotLink_*`,
  `Settings_LoginRequired`, `Settings_Language`, `Settings_Language_Hint`,
  `Settings_Language_SystemDefault`, `Settings_Dns`, `Settings_Dns_Hint`, `Settings_Dns_Auto`,
  `Settings_Dns_Always`, `Settings_Dns_Never`, `Settings_Section_Startup`,
  `Settings_StartWithWindows`, `Settings_StartWithWindows_Hint`, `Settings_MinimizeToTray`,
  `Settings_MinimizeToTray_Hint`.

A notable naming/UX mismatch: `Nav_Builds` and `Builds_Title` both have the English value `Depots`;
the internal name still says "Builds". Preserve the keys, not the assumptions.

The full 552-key inventory, grouped by prefix, is in Appendix A.

### 4.4 Culture list and native labels

`Program.SupportedLanguages` (membership and order) and `SettingsViewModel.LanguageOptions`
(display order, with a leading "System default" entry whose tag is null):

| Tag | Native label | RESX file |
|---|---|---|
| *(null)* | localized `Settings_Language_SystemDefault` | — follows OS |
| `en` | English | `Strings.resx` (base) |
| `zh-Hans` | 简体中文 | `Strings.zh-Hans.resx` |
| `zh-Hant` | 繁體中文 | `Strings.zh-Hant.resx` |
| `ja` | 日本語 | `Strings.ja.resx` |
| `ko` | 한국어 | `Strings.ko.resx` |
| `es` | Español (España) | `Strings.es.resx` |
| `es-419` | Español (Latinoamérica) | `Strings.es-419.resx` |
| `pt-BR` | Português (Brasil) | `Strings.pt-BR.resx` |
| `pt-PT` | Português (Portugal) | `Strings.pt-PT.resx` |
| `fr` | Français | `Strings.fr.resx` |
| `de` | Deutsch | `Strings.de.resx` |
| `it` | Italiano | `Strings.it.resx` |
| `nl` | Nederlands | `Strings.nl.resx` |
| `pl` | Polski | `Strings.pl.resx` |
| `ru` | Русский | `Strings.ru.resx` |
| `uk` | Українська | `Strings.uk.resx` |
| `ar` | العربية | `Strings.ar.resx` |
| `cs` | Čeština | `Strings.cs.resx` |
| `hu` | Magyar | `Strings.hu.resx` |
| `ro` | Română | `Strings.ro.resx` |
| `tr` | Türkçe | `Strings.tr.resx` |
| `el` | Ελληνικά | `Strings.el.resx` |
| `bg` | Български | `Strings.bg.resx` |
| `th` | ไทย | `Strings.th.resx` |
| `vi` | Tiếng Việt | `Strings.vi.resx` |
| `id` | Bahasa Indonesia | `Strings.id.resx` |
| `da` | Dansk | `Strings.da.resx` |
| `fi` | Suomi | `Strings.fi.resx` |
| `nb` | Norsk | `Strings.nb.resx` |
| `sv` | Svenska | `Strings.sv.resx` |

The two lists differ only in ordering: `Program.cs` places `tr` before `ar`; `SettingsViewModel`
places `ar` before `cs` and `tr` after `ro`. Membership is identical. Keep a single shared constant
in Rust to avoid drift.

### 4.5 Fallback and regional-variant resolution

Two layers:

1. **Tag selection** (`Program.MatchOsLanguage`, §1.5; or an explicitly saved `Language` from the
   dropdown).
2. **Per-key resource lookup** by .NET `ResourceManager` for that `CultureInfo`.

The RESX satellite fallback chain is exact-tag → neutral parent → invariant (English):

| Requested culture | Lookup chain | Consequence |
|---|---|---|
| `es` | `es` → invariant | Spanish (Spain) |
| `es-419` | `es-419` → `es` → invariant | Latin American Spanish; missing keys fall back to Spain Spanish first, then English |
| `pt-BR` | `pt-BR` → `pt` (not shipped) → invariant | **never** falls back to `pt-PT`; missing keys go to English |
| `pt-PT` | `pt-PT` → `pt` (not shipped) → invariant | same |
| `zh-Hans` | `zh-Hans` → `zh` (not shipped) → invariant | no cross-fallback to Traditional |
| `zh-Hant` | `zh-Hant` → `zh` (not shipped) → invariant | no cross-fallback to Simplified |
| `nb` | `nb` → (parent) → invariant | Norwegian Bokmål file is the only Norwegian |
| `en` | invariant base `Strings.resources` | English |

Because all 29 files have full 552-key parity, the neutral-parent fallbacks only matter for newly
added keys between translation passes. `Strings.Get(key)` returns the **key itself** when no resource
matches, so a missing key renders as `Builds_Title` rather than throwing.

Region handling is an explicit override, not general CLDR matching:

- Chinese resolves by script: any `Hant` → Traditional; any `Hans` → Simplified; otherwise
  `zh-TW`/`zh-HK`/`zh-MO` → Traditional, all other `zh-*` → Simplified.
- Spanish: everything `es-*` except `es-ES` is treated as Latin America and maps to `es-419`;
  `es-ES` and bare `es` map to `es`.
- Portuguese: exact `pt-BR`/`pt-PT` win; bare `pt` and every other `pt-*` map to `pt-PT`.
- Norwegian: `nb`, `nn`, `no` all map to `nb`.
- Every other region-qualified locale strips to its two-letter parent if that parent is shipped
  (`fr-FR`→`fr`, `de-DE`→`de`, `ja-JP`→`ja`, …); otherwise English.

### 4.6 How XAML and C# consume strings

- Each XAML file declares `xmlns:res="clr-namespace:LuaToolsGui.Resources"` (MainWindow + all 13
  views/dialogs).
- Static use: `Text="{x:Static res:Strings.Settings_Title}"`.
- `Strings` exposes one static property per key:
  `public static string Key => Get(nameof(Key));`, and
  `Get(key) => ResourceManager.GetString(key, CultureInfo.CurrentUICulture) ?? key`.
- `x:Static` is evaluated when the XAML is parsed, once. WPF does not re-evaluate it on culture
  change. That is the entire reason `SettingsViewModel.OnSelectedLanguageChanged` writes
  `Settings.Language` and raises `RequestRestartPrompt` instead of trying to update live.
- The prompt is a persistent `Snackbar` action toast: title `Lang_Changed_Title`, body
  `Lang_Changed_Body`, action `Lang_Changed_Restart` → `RelaunchApp`.
- If the user ignores the toast, the old language stays until the next launch; `Program` reads the
  saved value before WPF starts.
- There is no `DynamicResource` usage for localized text.

### 4.7 `scripts/check-i18n.py` — what it enforces

Run from the repository root: `python scripts/check-i18n.py`.

Constants:

- `RES_DIR = src/LuaToolsGui/Resources`
- `ENGLISH = Strings.resx`
- `DATA_RE = '<data name="([^"]+)" xml:space="preserve"><value>(.*?)</value></data>'` with `re.S`
- `PLACEHOLDER_RE = r'\{(\d+)\}'`
- `PENDING_TRANSLATION: set[str] = set()` — currently empty.

Algorithm:

1. If English is missing, print `ERROR: ... (run from repo root)` and return 1.
2. `parse(path)`:
   - read as UTF-8;
   - `ET.fromstring(text)` for well-formedness (raises `ET.ParseError` on malformed XML);
   - return `dict(DATA_RE.findall(text))`. The regex, not the XML tree, extracts keys and values.
     Duplicate keys silently collapse (last wins), and any `<data>` whose opening tag/value are not
     contiguous or that has extra children is silently omitted. `re.S` lets values span newlines.
3. For each `Strings.*.resx` (29 files; the English base is **not** matched by the glob):
   - parse; on `ET.ParseError` record `"<file>: INVALID XML. <error>"` and continue;
   - `missing = base_keys - translated_keys - PENDING_TRANSLATION`;
   - `extra = translated_keys - base_keys`;
   - report `missing N key(s): first 10 sorted` and `unknown key(s)` (first 10);
   - for keys present in both, compare `set(re.findall(r'\{(\d+)\}', value))`; any difference is
     reported as `key '<key>' placeholder mismatch (en=[...] vs [...])`.
4. If any problems: print `i18n check FAILED (N problem(s) across 29 language files):` and each
   problem prefixed `"  - "`, return 1.
5. Success: print
   `i18n check OK: 29 language files, 552 keys each, XML valid, placeholders consistent.`
6. If `PENDING_TRANSLATION` is non-empty, print a NOTE listing keys that are English-only by design;
   also print stale entries that no longer exist in English.
7. Return 0.

What it does **not** enforce:

- no duplicate-key detection;
- no check that a translation is non-empty or actually translated;
- no check of XML comments or ordering;
- no check of placeholder **format specifiers**: only plain `{0}`/`{1}` are recognized; `{0:N0}` or `{0,-5}` would be treated as having no placeholder and reported as a mismatch;
- no check of product-name/technical-token rules from the README (human review only);
- no check of file encoding beyond UTF-8 readability;
- no check for untranslated `Builds`/`Launch` keys despite stale comments in the README and
  `Strings.Designer.cs` (see §4.9).

### 4.8 CI workflow `.github/workflows/i18n-check.yml`

Name: `i18n check`.

Triggers:

- `pull_request` on changes to `src/LuaToolsGui/Resources/Strings*.resx` or
  `scripts/check-i18n.py`.
- `push` to branches `master` or `main`, on changes to
  `src/LuaToolsGui/Resources/Strings*.resx` only (the script path is **not** in the push filter).

Job `check`:

- `runs-on: ubuntu-latest`
- `actions/checkout@v4`
- `actions/setup-python@v5` with `python-version: '3.x'`
- `run: python scripts/check-i18n.py` from the repo root.

No caching, no matrix, no Rust build, no test run, no artifact upload. The repository has no other
workflow file.

### 4.9 Translator rules and known stale comments

From `Resources/README.md`:

- Do not add/remove keys; every language carries the exact English key set.
- Keep placeholders exactly; reordering is allowed; dropping one crashes `string.Format` at runtime.
- Do not translate product names `SteamTools`, `OpenSteamTools`, `CloudRedirect`, `Steam`, `Discord`,
  `Hubcap`; technical tokens `App ID`, `DLC`, path `steam\config\stplug-in`, extensions
  `.lua`/`.manifest`/`.zip`.
- Filter/sort dropdown stored values stay English because code compares them; only the display is
  localized by `FilterOptionDisplayConverter`.
- `Drop_Picker_Filter` must keep the exact filter format:
  `Lua / manifest / zip|*.lua;*.manifest;*.zip|All files|*.*` — only the human labels may change.
- `FilterOptionDisplayConverter` maps stored English values `"Any"`, `"Free"`, `"Paid"`,
  `"Hide adult"`, `"Adult only"`, `"Recently added"`, `"Name (A–Z)"`,
  `"Release date (newest)"`, `"Metacritic"`, `"Most reviewed"`, `"All"`; any other value is shown
  as-is.
- Keep the brand pun `"Let's get Luing™!"` playful and keep the `™`.
- Escape XML.
- RTL languages (Arabic) currently ship **LTR-only**; the UI is not mirrored. Do not assume Arabic
  looks native.
- Adding a language: copy `Strings.resx` to `Strings.<tag>.resx`, translate, run the checker, then
  register the tag in `Program.SupportedLanguages` **and** `SettingsViewModel.LanguageOptions` with
  its native endonym. The `.csproj` glob and the Designer accessors need no change (the accessors
  already exist for every key).

Stale-comment discrepancy: the README says "Currently pending: the Builds page (added 2026-07-31)",
and `Strings.Designer.cs` says the Builds and Launch sections are "NOT YET TRANSLATED", but
`PENDING_TRANSLATION` is empty, all 29 files contain all 552 keys, and sampled Builds/Launch values
are translated (e.g. German `Builds_Subtitle` = "Mehrere Versionen der lua eines Spiels aufbewahren
…", Japanese `Launch_Title` = "起動オプション"). Treat the code/script as authoritative, not those
comments.

---

## 5. Release, CI and build expectations

### 5.1 Project configuration (`LuaToolsGui.csproj`)

| Property | Value |
|---|---|
| SDK | `Microsoft.NET.Sdk` |
| `OutputType` | `WinExe` |
| `TargetFramework` | `net8.0-windows` |
| `Nullable` | `enable` |
| `ImplicitUsings` | `enable` |
| `UseWPF` | `true` |
| `UseWindowsForms` | `true` (tray `NotifyIcon`) |
| `AssemblyName` | `LuaTools` |
| `AssemblyTitle` | `LuaTools` |
| `Product` | `LuaTools` |
| `Version` | `1.1.3` |
| `ApplicationIcon` | `icon.ico` |
| `StartupObject` | `LuaToolsGui.Program` |

Special items:

- `<Using Remove="System.Windows.Forms" />` and `<Using Remove="System.Drawing" />` — WinForms is
  referenced fully qualified so its implicit usings do not collide with WPF types.
- `<InternalsVisibleTo Include="LuaToolsGui.Tests" />` — lets tests call `Program.MatchOsLanguage`
  and other internal members.
- `App.xaml` removed from `ApplicationDefinition` and added as `Page`.
- `icon.ico` embedded as `Resource` so `Window Icon="icon.ico"` and the tray `pack://` URI work.
- `<EmbeddedResource Update="Resources\Strings*.resx"><Generator></Generator></EmbeddedResource>` —
  the SDK designer is disabled; `Strings.Designer.cs` is hand-maintained. The glob picks up any new
  `Strings.<tag>.resx` and the SDK builds satellite assemblies.
- No `app.manifest`, no `appsettings.json`, no runtimeconfig template, no signing configuration.

Package references:

| Package | Version | Use |
|---|---|---|
| `CommunityToolkit.Mvvm` | 8.4.2 | `[ObservableProperty]`, `[RelayCommand]` |
| `Markdig.Wpf` | 0.5.0.1 | Markdown rendering |
| `Microsoft.Extensions.Hosting` | 10.0.9 | DI + hosted services + logging |
| `System.Security.Cryptography.ProtectedData` | 10.0.9 | DPAPI for `auth.dat` |
| `Velopack` | 1.2.0 | installer/updater hooks and runtime API |
| `VirtualizingWrapPanel` | 2.5.1 | virtualized grids |
| `WPF-UI` | 4.3.0 | FluentWindow, NavigationView, Snackbar, themes |

`AssemblyInfo.cs` only declares `[assembly: ThemeInfo(ResourceDictionaryLocation.None,
ResourceDictionaryLocation.SourceAssembly)]`.

### 5.2 Build and test expectations (README / CONTRIBUTING)

- Requirements: Windows 10/11, .NET 8 SDK (building from source needs the full SDK; the released
  installer bundles a check for the .NET 8 **Desktop Runtime** and installs it if missing).
- `dotnet build LuaToolsGui.sln`
- `dotnet run --project src/LuaToolsGui/LuaToolsGui.csproj`
- `dotnet watch --project src/LuaToolsGui/LuaToolsGui.csproj`
- `dotnet test`
- `dotnet publish -c Release` is enough for a local test build.
- Solution `LuaToolsGui.sln` contains `src/LuaToolsGui/LuaToolsGui.csproj` and
  `tests/LuaToolsGui.Tests/LuaToolsGui.Tests.csproj`, with Debug/Release for Any CPU/x64/x86.
- Test project: `net8.0-windows`, `UseWPF=true`, `IsPackable=false`, `IsTestProject=true`,
  `Microsoft.NET.Test.Sdk` 17.11.1, `xunit` 2.9.2, `xunit.runner.visualstudio` 2.8.2, project
  reference to the app. Tests only touch non-UI logic; the WPF TFM is required to reference the
  WinExe assembly.

### 5.3 Versioning

- The only version literal in the source is `<Version>1.1.3</Version>` in the csproj.
- `MainViewModel.VersionLabel` reads `AssemblyInformationalVersionAttribute`; if absent, falls back to
  `Assembly.GetName().Version` or `"?"`; strips everything from the first `+` (commit metadata) and
  prefixes `v`, e.g. `v1.1.3`.
- Velopack compares versions and only updates to a **strictly higher** one; `App.xaml.cs` comments
  that every release must bump `--packVersion`.
- No `Directory.Build.props`, no MinVer/GitVersion, no CHANGELOG.
- **Version mismatch in the snapshot:** git tags `v1.3.2` and `v1.3.3` both point at the single
  imported commit (`9461259`, "Add Cloudflare DNS fallback", 2026-09-25), while the csproj still says
  `1.1.3`. The release version is therefore supplied externally (the `--packVersion` comment), not
  derived from the csproj. Do not use the csproj value as the release version without confirming.

### 5.4 Velopack packaging and artifact names

What is in the repo:

- `Velopack` 1.2.0 is referenced; `VelopackApp.Build().Run()` is the first statement in `Main`.
- `UpdateService` uses `GithubSource(repo, accessToken: null, prerelease: false,
  downloader: ProxiedFileDownloader)` for each repo in `AppConfig.GithubReleasesRepos`.
- The primary repo is `https://github.com/madoiscool/LuaTools`; the backup is
  `https://github.com/mendy-tools/LuaTools` (manual re-upload only if the primary is gone).
- `ApplyAndRestart` passes `--minimized --tray-locked`; `ApplyOnExit` uses
  `WaitExitThenApplyUpdates(silent: true, restart: false)`.
- `SteamAutoCrackService` uses `Velopack.Windows.Runtimes` to install the .NET 10 Desktop Runtime on
  demand (despite the app targeting .NET 8), because the SteamAutoCrack release is framework-dependent
  `net10.0-windows`.

What is **not** in the repo:

- no `vpk` command, no packId, packVersion, packChannel, packTitle or icon arguments;
- no `RELEASES`/`releases.*.json` assets, no `.nupkg`, no delta configuration;
- no release workflow, no signing step, no upload step.

Artifact names are visible only in the README download badges:

- `LuaTools-win-Setup.exe`
- `LuaTools-win-Portable.zip`

These match Velopack's default naming for `--packId LuaTools` and a channel/name segment `win`
(`<packId>-<channel>-Setup.exe` / `-Portable.zip`). Treat that inference as unverified; the actual
channel string and packId are not in the repo.

README release statements:

- "Official builds are packaged and signed by the maintainers, so the release tooling isn't part of
  this repo."
- Released builds are framework-dependent (~10 MB); setup auto-installs the .NET 8 Desktop Runtime on
  a clean machine; the app then self-updates through Velopack.
- `.gitignore` excludes `build-release.ps1`, `tools/`, `Releases/`, `publish/` and `docs/`, confirming
  that release tooling/notes are maintained outside the public tree.

### 5.5 CI summary

The repository's only automation is the i18n check in §4.8. There is no build/test workflow, no
release workflow, no dependency automation, no code signing, and no artifact retention in the repo.
The Rust port will need to add build/test/release workflows from scratch; do not assume the C#
release pipeline can be copied.

### 5.6 `.gitignore`

Exact entries:

```
bin/
obj/
publish/
Releases/
*.user
.vs/
*_wpftmp.csproj
.claude/
CLAUDE.md
AGENTS.md
.cursor/
.cursorrules
.github/copilot-instructions.md
.idea/
*.suo
TestResults/
OpenSteamTool-releases/
SteamTools-releases/
tools/
docs/
build-release.ps1
*.lnk
*.log
.env
.env.*
*.pfx
*.p12
*.pem
*.snk
```

Notes for the Rust repo: `docs/` is ignored in the C# repo (this analysis lives in the new repo, so
it is unaffected), `target/` is not listed and must be added, and `Cargo.lock` policy must be
decided (binary app → commit it).

### 5.7 Contribution rules (`CONTRIBUTING.md`) and repository layout

Three non-negotiable rules:

1. **Every user-facing string must be localized.** Add the key to `Strings.resx`, add the accessor to
   the hand-maintained `Strings.Designer.cs` (`public static string Key => Get(nameof(Key));`), add
   the same key with a real translation to all `Strings.<tag>.resx`, then reference it
   (`{x:Static res:Strings.Key}` in XAML, `Resources.Strings.Key` / `string.Format(...)` in C#). Never
   hardcode a visible string in a view, toast, `MessageBox` or empty-state label. CI enforces key
   parity, not the "no hardcoded strings" rule; the one known violation is the plugin-update toast in
   `RunUpdateFlowAsync` ("LuaTools", "Updating plugin. Steam will restart.").
2. **Every GitHub request goes through `GithubProxy`.** Never call `github.com`/`api.github.com`
   directly. `GithubProxy.SendAsync(url, ct)` for API/metadata and
   `GithubProxy.DownloadAsync(url, dest, progress, ct)` for release assets. It tries the direct URL,
   then capability-matched mirrors. `UpdateService` passes a `ProxiedFileDownloader` so the Velopack
   feed and `.nupkg` are mirror-resilient too.
3. **Store-page plugin calls go through `callServerMethod`.** The injected script runs on an HTTPS
   Steam page and cannot raw-fetch a localhost backend (mixed-content policy silently kills it). Any
   new store-page → backend call must use
   `Millennium.callServerMethod("luatools", "<Name>", args)` and add a matching entry to
   `CefInjectorService.CallBackendMethod`'s `methodMap`; an unmapped name silently no-ops.

`Millennium.callServerMethod` resolves either to Millennium's own object when installed or to the
queue-based polyfill that `CefInjectorService` injects ahead of the plugin script. The actual HTTP
request is made outside the browser, which is what avoids the mixed-content block.

Repository layout (README):

| Path | Contents |
|---|---|
| `src/LuaToolsGui/` | app: `Views/` XAML, `ViewModels/`, `Services/`, `Models/`, `Resources/` |
| `src/LuaToolsGui/AppConfig.cs` | all compiled-in endpoints, mirrors and public client values |
| `tests/LuaToolsGui.Tests/` | xUnit tests |
| `scripts/check-i18n.py` | translation validator run by CI |
| `LuaToolsGui.sln` | app + test projects |

README credits/adjacent software that define feature scope: Millennium (plugin framework whose
injection API is polyfilled), Velopack (installer/updater), DepotDownloaderMod (depot downloads,
self-hosted re-pack), SteamAutoCrack (GUI launched from Downloads), Steamless (SteamStub removal) and
CloudRedirect (Steam Cloud revival add-on). Each of the last four is a DRM/unlocker-adjacent feature
that the Rust port may drop.

---

## 6. Test matrix

Framework: xUnit 2.9.2 (`[Fact]`, `[Theory]`, `[InlineData]`), test project
`tests/LuaToolsGui.Tests/LuaToolsGui.Tests.csproj`, TFM `net8.0-windows`, no mocking library. Tests
use real temp directories (`Path.GetTempPath()` + GUID), injectable clocks (`UtcNow` init-only
properties), stub `HttpMessageHandler`s and injected Steam stop/start delegates. No test starts or
kills Steam, launches the UI, or touches the network.

Summary (theory rows expanded; 281 cases total):

| File | Cases | Unit under test | Port decision |
|---|---:|---|---|
| `AppInfoCodecTests.cs` | 10 | binary VDF/appinfo v29 codec | keep if launch-options/appinfo editing is kept; otherwise drop |
| `BuildsActiveHashTests.cs` | 6 | `BuildsViewModel.ResolveActiveHash` | **drop** (lua build switcher) |
| `DepotCacheMigrationTests.cs` | 15 | `DepotCacheMigrationService.Migrate` | **drop** with depot/lua features; keep only if manifest rescue is kept |
| `DohResolverTests.cs` | 31 | DNS-over-HTTPS parser/resolver | keep |
| `DownloadRateTests.cs` | 6 | `DownloadItem` rate/ETA window | keep |
| `FileHashTests.cs` | 10 | `FileHash.Sha256`/`Matches` | keep the hashing helper; test's original consumer (fix revert) is DRM |
| `LanguageDetectionTests.cs` | 55 | `Program.MatchOsLanguage` | keep |
| `LaunchModStoreTests.cs` | 9 | launch-option snapshot store | keep if launch options kept |
| `LaunchOptionsServiceTests.cs` | 11 | read/stage/apply/restore/drift | keep if launch options kept |
| `LaunchOptionsViewModelTests.cs` | 4 | `CanSave` enablement | keep if launch options kept |
| `LuaBuildTests.cs` | 13 | `LuaFileParser` pins + build-id filename | **drop** (lua unlocking) |
| `LuaEditorTests.cs` | 13 | `LuaEditor` per-depot lock/enable | **drop** (lua unlocking) |
| `LuaVaultTests.cs` | 33 | lua variant vault | **drop** (lua unlocking) |
| `ModeMigrationTests.cs` | 24 | 4-mode → 3-mode migration | **drop** (unlocker) |
| `SteamAppInfoCacheTests.cs` | 12 | appdetails response matching | keep (Steam metadata) |
| `SteamDepotInfoCacheTests.cs` | 5 | depot info cache/TTL | keep if depot metadata is kept; otherwise drop |
| `SteamLinkParserTests.cs` | 24 | dropped-link appid extraction | keep |
| **Total** | **281** | | |

### 6.1 `AppInfoCodecTests.cs` (10 facts)

Unit: `LuaToolsGui.Services.AppInfo.AppInfoFile`, `BinaryVdf`, `LaunchOption`.

Fixture: a synthetic **v29** `appinfo.vdf` in a temp dir, containing one app `603750`:

- Header: magic `AppInfoFile.Magic29` = `0x07564429`, a uint32 `1`, then an 8-byte offset to the
  string table.
- App record: int32 appid `603750`, int32 size (`60 + body`), 60-byte meta, VDF body, int32 `0`
  terminator.
- Meta layout (60 bytes): offset 0 `infoState` = 1 (uint32), 4 `lastUpdated` = 1234 (uint32),
  8 `picsToken` = 99 (uint64), 16 `sha1_text` = ASCII `"TEXTHASH-DO-NOT-TOUCH"` padded to 20 bytes,
  36 `changeNumber` = 4242 (uint32), 40 `sha1_binary` = SHA-1 of the body.
- String table: int32 count 15, then NUL-terminated strings: `appinfo`, `appid`, `common`, `name`,
  `config`, `launch`, `0`, `executable`, `arguments`, `oslist`, `type`, `description`, `legacy`,
  `dup`, plus the deliberately invalid-UTF-8 bytes `62 61 64 FD 6B` (`"bad\xFDk"`).
- Body contains `appinfo.appid = 603750`, `common.name = "Test Game"`, `common.legacy` as raw invalid
  bytes `[0x53,0x74,0xFD,0xFE,0x72]`, two `common.dup` entries (`"first"`, `"second"`), a property
  whose key is the invalid string, and `config.launch.0` with `executable = "Game.exe"`,
  `type = "default"`, `config.oslist = "windows"`.

Assertions:

1. `RoundTrip_IsByteIdentical` — `new AppInfoFile(src).SaveAs(dst, empty edits)` produces a file
   byte-for-byte equal to the source. No-edit rebuilds must not reorder, re-encode or drop anything.
2. `DuplicateKeysInOneObject_BothSurvive` — `common` has exactly two `dup` properties, in order
   `"first"`, `"second"`. A dictionary-backed model would collapse them.
3. `NonUtf8String_SurvivesAnEditToAnotherField` — after changing `launch.0.arguments` to `-windowed`,
   the re-read `common.legacy` value is still the exact byte array `[53,74,FD,FE,72]`. Invalid bytes
   must never be decoded/re-encoded through U+FFFD.
4. `Edit_RestampsBinaryHash_AndPreservesTextHashAndChangeNumber` — after changing arguments to
   `-novid`: meta[40..60] equals `SHA1(body)`; it differs from the original meta[40..60]; meta[16..36]
   (sha1_text) is unchanged; `entry.ChangeNumber == 4242` (not bumped).
5. `LaunchOptions_ReadWriteRoundTrip` — reads one option with `Executable = "Game.exe"` and
   `OsList = "windows"`. Adds a second option with `Index = LaunchOption.NextIndex(...)` (must be
   `"1"`), `Executable = "ModLoader.exe"`, `Arguments = "--profile dev"`,
   `WorkingDir = "mods"`, `Description = "Play with mods"`, `Type = "option1"`,
   `OsList = "windows"`; after write/re-read all six fields match and `after[1].Index == "1"`.
6. `EmptyOptionalFields_AreOmittedNotWrittenBlank` — `new LaunchOption { Executable = "run.sh",
   OsList = "linux" }.ToTable()` has `executable` present, `arguments`/`workingdir`/`description`
   absent, and `config.oslist == "linux"`.
7. `UnmodelledKeys_SurviveAnEdit` — a table with `executable = "hl.exe"` and
   `vacmodulefilename = "resource\\sourceinit.dat"` round-trips through `FromTable` → set
   `Arguments = "-steam"` → `ToTable(existing)`; the unmodelled key remains and arguments is
   `"-steam"`.
8. `Reorder_RenumbersKeys_AndCarriesEachEntrysUnmodelledKeysWithIt` — entries `0/hl.exe/a.dat` and
   `1/hl2.exe/b.dat`; reverse the list + `Renumber` + `WriteAll`. After re-read indexes are `["0",
   "1"]`, executables are `hl2.exe`, `hl.exe`, and the extras follow: key `"0"` has `b.dat`, key `"1"`
   has `a.dat`. (Matching the old table by new index instead of `SourceIndex` would swap them.)
9. `NextIndex_AppendsPastTheHighestAndIgnoresGaps` — options at `0`, `2`, `3` → `"4"`; empty list →
   `"0"`.
10. `UnknownMagic_IsRejected` — a file starting `0x07564430` (hypothetical v30) throws
    `AppInfoFormatException`.

Codec facts used by the port: v29 uses an indexed string table for table keys; v28 has a 40-byte
meta, v29 a 60-byte meta; magic 27/28/29 are accepted; save must copy unedited apps byte-for-byte and
recompute only the edited app's binary SHA-1.

### 6.2 `BuildsActiveHashTests.cs` (6 facts) — drop with the lua build switcher

Unit: `BuildsViewModel.ResolveActiveHash(string? liveHash, IReadOnlyList<LuaVariant> stored,
string? editBase)`.

Fixture: `LuaVariant` build `"aaa"` (Kind `build`) and default `"bbb"` (Kind `default`).

Algorithm asserted:

1. `LiveMatchingAStoredVariant_IsThatVariant` — live `"aaa"` → `"aaa"`.
2. `LiveMatchingNothing_ResolvesToTheVariantBeingEdited` — live `"edited"`, editBase `"aaa"` →
   `"aaa"`.
3. `LiveMatchingNothing_WithNoEditBase_LeavesNothingActive` — live `"edited"`, editBase null →
   returns `"edited"`, and no stored variant has that hash (UI shows no active row).
4. `LiveMatchingNothing_WithAStaleEditBase_LeavesNothingActive` — editBase `"gone"` → `"edited"`.
5. `NoLiveFile_IsNull` — live null → null, even with a valid editBase.
6. `AStaleEditBase_DoesNotOverrideAnExactLiveMatch` — live `"bbb"`, editBase `"aaa"` → `"bbb"`.

Exact implementation: null → null; exact stored match → live; else editBase only if it is a stored
hash; else live.

### 6.3 `DepotCacheMigrationTests.cs` (10 facts + 1 theory × 5 = 15)

Unit: `DepotCacheMigrationService.Migrate(legacyDir, realDir, ILogger, ct)`.

Fixture: temp root with `config/depotcache` (legacy) and `depotcache` (real). A valid manifest is
built as:

- 8-byte header: uint32 LE metadata magic `0x1F4812BE`, uint32 LE metadata length;
- protobuf metadata: field 1 (`0x08`) varint depot id, field 2 (`0x10`) varint gid.

This is exactly what `ManifestFile.TryRead` validates; no binary fixture is committed.

Assertions:

1. `StrandedManifest_IsMovedIntoTheRealFolder` — `4889481_4624935753769628021.manifest` → `Moved = 1`,
   `Failed = 0`, file exists in real with identical bytes.
2. `EmptiedLegacyFolder_IsRemoved` — after moving `1231561_6402543383554775381.manifest`, the legacy
   folder no longer exists.
3. `SecondRun_IsANoOp` — first run moves `632361_5715419509320521739.manifest`; second run
   `IsEmpty == true`, file still in real.
4. `MissingLegacyFolder_IsANoOp` — `IsEmpty`, and the real folder is **not created**.
5. `ManifestAlreadyInTheRealFolder_IsNotOverwritten` — real has `2587891_452405881105540002.manifest`
   with valid bytes; legacy has the same name with `[DE,AD,BE,EF]`. Result `Moved = 0`,
   `AlreadyPresent = 1`, real bytes remain the valid ones.
6. `CorruptFile_IsNotMoved` — `702350_7446681591222407330.manifest` contains `[00,01,02,03]` →
   `Moved = 0`, `Rejected = 1`, file remains in legacy.
7. `ManifestWhoseContentContradictsItsName_IsNotMoved` — name `991352_48810978170646456.manifest` but
   content depot `883719`, gid `2758490342177349494` → `Rejected = 1`, `Moved = 0`.
8. `UnrecognisedName_IsNotMoved` (5 cases): `notamanifest.manifest` (no `_`), `_123.manifest`
   (empty depot), `883719_.manifest` (empty gid), `abc_123.manifest` (non-numeric depot),
   `883719_notagid.manifest` (non-numeric gid) → each `Moved = 0`, `Rejected = 1`, file remains.
9. `UnrelatedFiles_AreLeftAlone` — only `notes.txt` → `IsEmpty`, content `"keep me"`, legacy folder
   remains (not empty).
10. `SameFolderForBoth_IsRefused` — `Migrate(Real, Real, ...)` → `IsEmpty`, file still exists.
11. `LeftoversKeepTheLegacyFolder` — corrupt `...00.manifest` rejected + valid
    `991352_48810978170646456.manifest` moved → `Moved = 1`, `Rejected = 1`, legacy folder still
    exists, corrupt file still there.

Filename grammar: split on the **first** `_`; both sides non-empty; depot parses as `long`; gid parses
as `ulong`; extension must be `.manifest` (the directory enumeration filters `*.manifest`).

### 6.4 `DohResolverTests.cs` (11 facts + 3 theories = 31)

Unit: `DohResolver.Parse`, `ShouldBypass`, `ResolveAsync`.

Constants: endpoint `https://1.1.1.1/dns-query` (IP literal so TLS validates without DNS), timeout
5 s, `MinTtl = 30 s`, `MaxTtl = 5 min`, cache keyed case-insensitively.

Parse assertions:

1. `Parse_ReadsARecordsAndTheLowestTtl` — two A records `104.21.0.1` TTL 300 and `172.67.0.2` TTL
   120 → addresses in wire order, TTL `120 s` (lowest wins).
2. `Parse_HandlesARealCloudflareResponse` — verbatim response with `TC/RD/RA/AD/CD/Question` extra
   fields → `104.21.52.64`, `172.67.196.53`, TTL 300. Extra top-level fields ignored.
3. `Parse_SkipsCnameRecords` — type 5 CNAME plus a type 1 A → only the A (`104.21.0.1`).
4. `Parse_ReadsAaaaRecords` — type 28 `2606:4700::1` → parsed as an address.
5. `Parse_TreatsNonZeroStatusAsNoAnswer` — `{"Status":3,"Answer":[]}` → empty.
6. `Parse_YieldsNothingForUnusableBodies` (7 cases):
   `{"Status":0}`, `{"Status":0,"Answer":[]}`,
   `{"Status":0,"Answer":[{"type":1,"data":"not-an-ip"}]}`,
   `{"Status":0,"Answer":[{"type":1}]}`, `not json at all`, `[]`, `""` → all empty.

Bypass assertions:

7. `ShouldBypass_IsTrueForLocalAndLiteralHosts` (9): `127.0.0.1`, `::1`, `167.235.229.108`,
   `localhost`, `LOCALHOST`, `printer.local`, `myhost`, `""`, null.
8. `ShouldBypass_IsFalseForRealHosts` (4): `lua.tools`, `db.lua.tools`, `api.github.com`,
   `hubcapmanifest.com`.
   Rules: null/whitespace, any IP literal, case-insensitive `localhost`, suffix `.local` or
   `.localhost`, and any single-label host (no `.`) → bypass.

Resolve assertions:

9. `ResolveAsync_ReturnsEmptyForABypassedHostWithoutAskingTheNetwork` — `localhost` → empty and
   handler `Calls == 0`.
10. `ResolveAsync_CachesUntilTheTtlExpires` — TTL 120, two lookups → 1 HTTP call; advance 121 s →
    third lookup makes a second call and returns one address.
11. `ResolveAsync_AppliesTheMinimumTtlFloor` — TTL 0, lookup then advance 25 s and lookup again →
    still 1 HTTP call (30 s floor).
12. `ResolveAsync_ReturnsEmptyWhenTheEndpointIsUnreachable` — handler throws `HttpRequestException`
    → empty, no throw.
13. `ResolveAsync_ReturnsEmptyOnAnErrorStatusCode` — 503 → empty.
14. `ResolveAsync_DoesNotCacheAFailure` — `{"Status":2}` twice → 2 HTTP calls (failures are not
    cached).

### 6.5 `DownloadRateTests.cs` (6 facts)

Unit: `DownloadItem.ApplySample`, `ResetMetrics`, `BytesPerSecond`, `Eta`.

Fixture clock starts `2026-01-01T00:00:00Z`; `DownloadJob(DownloadKind.Depot, "test:1", 1, "Test",
"Depot files", null, fileCb, finishCb)`; `UtcNow` is injected and advanced by the test. `1 MB =
1024*1024`.

Window constants: `RateWindow = 3 s`, `MinSpan = 0.5 s`, `MaxSamples = 64`.

1. `LongGapBetweenFiles_ReportsTheRealAverage` — total `22_580 MB`; sample `(0, total)` at t0; advance
   166 s; sample `(8_315 MB, total)` → rate in `[45 MB/s, 55 MB/s]` (8.3 GB / 166 s ≈ 50 MB/s). This
   is the original bug: the age trim must keep at least two samples and must not freeze a stale rate.
2. `AfterAFastStart_ALongSlowFileUpdatesTheReading` — 30 iterations of advance 0.1 s and +10 MB
   (~100 MB/s) → rate in `[90, 110] MB/s`; then advance 166 s and +8,315 MB → rate is **not equal**
   to the fast reading and is in `[40, 60] MB/s`.
3. `BurstOfCompletions_DoesNotSpike` — after the 166 s/8,315 MB sample, 8 iterations of advance 0.01 s
   and +1 MB → rate in `[1 MB/s, 200 MB/s]` (pre-fix it read GB/s because it divided by ~10 ms).
4. `DenseSteadyStream_MatchesTheActualRate` — 100 iterations of advance 0.1 s and +2 MB, total
   `3_000 MB` → `[18, 22] MB/s` (actual 20).
5. `Eta_FollowsTheMeasuredRate` — total `1_000 MB`; sample 0 at t0; advance 10 s; sample 500 MB →
   rate 50 MB/s; `Eta` non-null and total seconds in `[8, 12]`.
6. `ResetMetrics_ClearsTheWindow` — after a valid rate, reset → `BytesPerSecond == 0`,
   `BytesRead == 0`, `Eta == null`; advance 1 s and apply `(0, total)` → rate still `0` (the cleared
   window must not resurrect old samples as a bogus first reading).

### 6.6 `FileHashTests.cs` (8 facts + 1 theory × 2 = 10)

Unit: `FileHash.Sha256(path)`, `FileHash.Matches(path, expected)`.

1. `Sha256_MatchesTheKnownVectorForAbc` — bytes `"abc"` → lowercase hex
   `ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad`.
2. `Sha256_IsNullForAMissingFile`.
3. `Sha256_IsNullForNullOrEmptyPath` — null and `"  "`.
4. `Sha256_IsReadableWhileAnotherHandleIsOpen` — another `FileStream` open with
   `FileShare.ReadWrite`; hashing still succeeds (the helper opens with share-read/write semantics).
5. `Matches_IsTrueForTheSameContent`.
6. `Matches_IsFalseOnceTheContentChanges` — hash taken before overwriting `"before"` → `"after"`.
7. `Matches_IgnoresHexCase` — uppercase of the correct digest still matches.
8. `Matches_TreatsAnAbsentHashAsNothingToCheck` (2) — expected null or `""` → **true**. This is the
   compatibility rule for pre-hash fix records: absent hash means "cannot verify", never "failed".
9. `Matches_IsFalseWhenTheFileIsGoneButAHashWasExpected` — missing file + real expected hash →
   false.

### 6.7 `LanguageDetectionTests.cs` (9 theories = 55)

Unit: `Program.MatchOsLanguage(new CultureInfo(os))`.

Exact cases:

| Group | Inputs → expected |
|---|---|
| Region-qualified strips to neutral (22) | `es-ES`→`es`, `fr-FR`→`fr`, `de-DE`→`de`, `it-IT`→`it`, `ja-JP`→`ja`, `ko-KR`→`ko`, `ru-RU`→`ru`, `pl-PL`→`pl`, `nl-NL`→`nl`, `tr-TR`→`tr`, `uk-UA`→`uk`, `cs-CZ`→`cs`, `hu-HU`→`hu`, `ro-RO`→`ro`, `el-GR`→`el`, `bg-BG`→`bg`, `th-TH`→`th`, `vi-VN`→`vi`, `id-ID`→`id`, `da-DK`→`da`, `fi-FI`→`fi`, `sv-SE`→`sv` |
| Exact regional tags (3) | `pt-BR`→`pt-BR`, `pt-PT`→`pt-PT`, `es-419`→`es-419` |
| Neutral tags (4) | `en`→`en`, `es`→`es`, `fr`→`fr`, `de`→`de` |
| Chinese by script (7) | `zh-CN`→`zh-Hans`, `zh-SG`→`zh-Hans`, `zh-Hans`→`zh-Hans`, `zh-TW`→`zh-Hant`, `zh-HK`→`zh-Hant`, `zh-MO`→`zh-Hant`, `zh-Hant`→`zh-Hant` |
| Spanish (5) | `es-ES`→`es`, `es-MX`→`es-419`, `es-AR`→`es-419`, `es-CO`→`es-419`, `es-CL`→`es-419` |
| Portuguese (3) | `pt-PT`→`pt-PT`, `pt-BR`→`pt-BR`, `pt`→`pt-PT` |
| Norwegian (4) | `nb-NO`→`nb`, `nn-NO`→`nb`, `nb`→`nb`, `nn`→`nb` |
| Arabic (3) | `ar-SA`→`ar`, `ar-EG`→`ar`, `ar`→`ar` |
| Unsupported (4) | `he-IL`, `hi-IN`, `af-ZA`, `""` → `en` |

This is the regression guard for the original bug where `es-ES` fell through to English instead of
mapping to `es`.

### 6.8 `LaunchModStoreTests.cs` (9 facts)

Unit: `LaunchModStore` (`%AppData%\LuaToolsGui\launchmods.json`, injected temp path in tests).

Fixture helper: `Options(params string[] executables)` creates `LaunchOption` entries with
`Index = i`, `Type = "default"`, `OsList = "windows"`.

1. `Save_CapturesTheSnapshotAndDesiredState` — `Save(603750, 42, ["Game.exe"], ["Game.exe",
   "ModLoader.exe"])` → `ChangeNumber == 42`, `Original.Count == 1`, `Desired.Count == 2`,
   `Desired[1].Executable == "ModLoader.exe"`.
2. `Save_DoesNotOverwriteTheSnapshotOnASecondEdit` — second `Save` with original
   `["Game.exe","Mod1.exe"]` and desired `["Game.exe","Mod2.exe"]` → `Original.Count == 1`,
   `Original[0].Executable == "Game.exe"` (Steam's, not our first edit), `Desired[1] == "Mod2.exe"`.
3. `Store_PersistsAcrossInstances` — reopen → `IsEmpty == false`, `Desired.Count == 2`.
4. `Remove_ForgetsTheGameAndEmptiesTheFile` — remove → `IsEmpty`, `Get(603750) == null`, file absent,
   reopened store empty.
5. `Rebase_RefreshesTheSnapshotWhenSteamUpdatedTheApp` — `Rebase(603750, 99, ["Game64.exe",
   "Extra.exe"])` → true, `ChangeNumber == 99`, `Original.Count == 2`,
   `Original[0].Executable == "Game64.exe"`, `Desired.Count == 2` unchanged.
6. `Rebase_DoesNothingWhenTheChangeNumberMatches` — same change number 42 → false, original still
   `"Game.exe"`.
7. `Rebase_IgnoresUnknownApps` — app 1 → false.
8. `Differs_DetectsEveryManagedField` — identical → false; differing executable → true; extra entry →
   true; `Arguments = "-novid"` → true; `BetaKey = "beta"` → true; `Index = "7"` → true.
9. `CorruptFile_IsPreservedNotSilentlyReset` — file content `{ this is not json` → store `IsEmpty`,
   and `launchmods.json.corrupt` exists (moved aside, original not overwritten).

### 6.9 `LaunchOptionsServiceTests.cs` (11 facts)

Unit: `LaunchOptionsService` with injected `appInfoPath`, `steamRunning`, `stopSteam` delegate,
`startSteam` delegate, `LaunchModStore`, backup dir. Steam process control is **never** real.

Fixture appinfo: v29, one app `603750`, `common.name = "Test Game"`, `config.installdir =
"Test Game"`, launches `("0","Game.exe","windows")` and `("1","nw","linux")`, meta changeNumber 7777,
sha1 of body. String table contains the keys used.

1. `Read_ReturnsEntriesInstallDirAndChangeNumber` — 2 entries, `Current[0].Executable == "Game.exe"`,
   `Current[1].OsList == "linux"`, `InstallDir == "Test Game"`, `ChangeNumber == 7777`,
   `IsModded == false`.
2. `Read_ReturnsNullForAnAppSteamDoesntKnow` — app 999999 → null.
3. `StageThenApply_WritesTheEditIntoAppInfo` — stage a desired list adding `ModLoader.exe` with
   `Arguments = "--profile dev"`, `Type = "default"`, `OsList = "windows"`. Staging alone leaves
   `Read(...).Current.Count == 2` but `IsModded == true`; `Apply({603750: desired}, restartSteam:
   true)` returns `Ok`, then count 3, `after[2].Executable == "ModLoader.exe"`,
   `after[2].Arguments == "--profile dev"`.
4. `Apply_ClosesAndRestartsSteamOnlyWhenItWasRunning` — steam not running → 0 stop/0 start; running →
   `SteamWasRunning == true`, 1 stop/1 start; `restartSteam: false` → 2 stops, still 1 start.
5. `Restore_PutsBackTheSnapshotAndForgetsTheMod` — stage a single `"Replaced.exe"` entry, apply,
   `Read` count 1; `StageRestore(603750)` returns the original; apply it + `store.Remove`; `Read`
   count 2, `Game.exe`, `nw`, `IsModded == false`.
6. `Apply_BacksUpAppInfoFirst` — exactly one `appinfo-*.vdf` in the injected backup dir.
7. `FindDrifted_IsEmptyWhenNothingIsStaged`.
8. `FindDrifted_SpotsSteamOverwritingAnAppliedEdit` — stage/edit/apply `-novid` → drift empty
   (in sync); rewrite appinfo from scratch (Steam PICS regeneration) → `[603750]`.
9. `Reapply_WritesStagedEditsBack_AndSkipsAppsNoLongerModded` — after the rewrite, `Reapply([603750,
   999999], restartSteam: false)` returns Ok; `Read(603750).Current[0].Arguments == "-novid"`;
   drift empty. Unknown 999999 is skipped, not fatal.
10. `Reapply_WithNothingStaged_IsANoOp` — Ok, 0 stop calls (never close Steam for an empty batch).
11. `Read_RebasesTheSnapshotAfterSteamUpdatesTheApp` — stage with changeNumber 7777; rewrite appinfo
    with `Game64.exe` and changeNumber 8888; a subsequent `Read` rebases store to `ChangeNumber ==
    8888`, `Original[0].Executable == "Game64.exe"`.

### 6.10 `LaunchOptionsViewModelTests.cs` (4 facts)

Unit: `LaunchOptionsViewModel` (`CanSave`, commands), real `LaunchOptionsService` with injected
appinfo and `ToastService`, app `648800` "Raft", launches `("0","Raft.exe")` and
`("2","Raft_legacy.exe")` (deliberately non-contiguous).

1. `Save_IsDisabledOnOpen_EvenWhenIndicesHaveGaps` — after `LoadAsync(648800, "Raft")`: `Error ==
   null`, `Entries.Count == 2`, `CanSave == false`. Opening a file with gaps must not look dirty
   merely because saving would compact the keys.
2. `Save_EnablesOnAFieldEdit_AndDisablesAgainWhenItsUndone` — set `Entries[0].Arguments = "-novid"`
   → true; set it back to `""` → false.
3. `Save_EnablesOnAddDeleteAndReorder` — `AddEntryCommand` → true; `DeleteEntryCommand` → false;
   select `Entries[1]`, `MoveUpCommand` → true; `MoveDownCommand` → false.
4. `Save_StaysDisabledWhenAFailedReadLeavesNoState` — load 999999 "Nothing" → `Error != null`,
   `CanSave == false`.

`CanSave` means "saving would actually change something", not "a game is loaded".

### 6.11 `LuaBuildTests.cs` (5 facts + 2 theories × 8 = 13) — drop with lua unlocking

Unit: `LuaFileParser.Parse(path, appId)`, `LuaInstaller.BuildIdFromFileName`,
`LuaInstaller.AppIdFromFileName`. All parse tests write a temp `.lua` and parse it with default
appid `386940`.

Pin semantics:

1. `ActivePin_IsReportedAsPinned` — file
   `addappid(386940)`, `addappid(228983, 1, "deadbeef")`,
   `setManifestid(228983, "9931278451102938", 0)` → `HasActivePins == true`, the 228983 entry has
   `ManifestId == "9931278451102938"`, `CommentedManifestId == null`.
2. `CommentedOutPin_IsNotReportedAsPinned` — same but `-- setManifestid(...)` →
   `HasActivePins == false`, `ActivePins` empty, `ManifestId == null`,
   `CommentedManifestId == "9931278451102938"`, `CommentedPins[228983] == "9931278451102938"`.
3. `MixedPins_AreBucketedIndependently` — 228983 has active `"111"`, 228985 has commented `"222"`
   → active pins contains 228983 `"111"`, not 228985; commented pins contains 228985 `"222"`.
4. `CommentedOutAddAppId_IsIgnored` — `-- addappid(999999, 1, "cc")` never appears in `Entries`.
5. `BuildIdFromFileName_ReadsTheBuild` (3) — `386940_18234567.lua` → `"18234567"`;
   `C:\downloads\386940_18234567.lua` → `"18234567"`; `1623730_9.lua` → `"9"`. Regex:
   `^\s*\d+_(\d+)\s*$` on the filename stem.
6. `BuildIdFromFileName_ReturnsNullWithoutABuild` (5) — `386940.lua`, `386940 (1).lua`,
   `Steamtools.lua`, `386940_.lua`, `386940_beta.lua` → null.
7. `AppIdFromFileName_StillWorksForBuildNamedFiles` — `386940_18234567.lua` and `386940.lua` both
   → 386940. Regex: leading digits `^\s*(\d+)`.

### 6.12 `LuaEditorTests.cs` (13 facts) — drop with lua unlocking

Unit: `LuaEditor.SetDepotLocked`, `SetDepotEnabled`, `LuaFileParser` interaction.

Base fixture:

```
addappid(386940, 1, "basekey")
addappid(228983,0,"aabb")
setManifestid(228983,"111111111")
addappid(228985,0,"ccdd")
--setManifestid(228985,"222222222")
```

1. `Unlock_CommentsOutTheActivePin` — `SetDepotLocked(base, 228983, locked:false)` contains
   `--setManifestid(228983,"111111111")`, no line starting `setManifestid(228983`, and the other
   commented pin is untouched.
2. `Lock_UncommentsAPin` — `SetDepotLocked(base, 228985, locked:true)` contains
   `setManifestid(228985,"222222222")` and no `--setManifestid(228985`.
3. `Lock_IsANoOpWhenAlreadyInThatState` — exact original string for
   `SetDepotLocked(base, 228983, true)` and `SetDepotLocked(base, 228985, false)`.
4. `Lock_RoundTripsExactly` — lock off then on returns the exact original text (content hash must not
   drift).
5. `Disable_CommentsOutTheDecryptionKeyLine` — `SetDepotEnabled(base, 228983, false)` contains
   `--addappid(228983,0,"aabb")`; the 228985 line remains active; the result starts with
   `addappid(386940, 1, "basekey")`.
6. `Enable_UncommentsTheKeyLine` — disable then enable returns the exact original.
7. `Disable_CommentsOutEveryLineForThatId` — for a file with `addappid(386940)`, `addappid(228983)`,
   `addappid(228983,0,"aabb")`, disabling 228983 comments **both** 228983 lines and leaves 386940.
8. `Toggles_DoNotMatchIdsBySubstring` — lines for 2289830, 22898, 228983: the first two unchanged,
   only `--addappid(228983,0,"cc")` commented.
9. `Toggles_PreserveIndentation` — `    addappid(228983,0,"aa")` → `    --addappid(228983,0,"aa")`,
   and enabling returns the indented original.
10. `Toggles_PreserveCrlfLineEndings` — CRLF input → output exact
    `addappid(386940)\r\n--addappid(228983,0,"aa")\r\n`; re-enable returns the CRLF original.
11. `Toggles_HandleSpacedCommentStyle` — `-- addappid(228983,0,"aa")` enabling removes `-- ` →
    `addappid(228983,0,"aa")`.
12. `Parser_KeepsTheTrailingNameOfADisabledDepot` — real-style file:
    `addappid(2784470) -- 9 Kings`, `addappid(2784471, 1, "aa") -- Depot 2784471`,
    `addappid(2784472, 1, "bb") -- Depot 2784472`; disable 2784471; parse with base 2784470 →
    disabled entry for 2784471 has `Comment == "Depot 2784471"`; 2784472 still active with
    `Comment == "Depot 2784472"`.
13. `Parser_ReportsDisabledDepotsSeparatelyFromActiveOnes` — after disabling 228983: not in
    `Entries`; present in `DisabledEntries`; 228985 and 386940 remain active.

Editor algorithm: regex anchored at line start with optional indent then optional `--\s*`; match
`setManifestid\s*\(\s*<id>\s*[,)]` or `addappid\s*\(\s*<id>\s*[,)]` case-insensitively; comment =
insert `--` after indent; uncomment = drop the captured `--\s*` run; split on `\n` only and join on
`\n`, so `\r` rides along and CRLF is preserved byte-for-byte.

### 6.13 `LuaVaultTests.cs` (33 facts) — drop with lua unlocking

Unit: `LuaVault` (temp `stplug-in` and vault dirs), `LuaFileParser`, `LuaEditor`.

Fixtures:

- AppId `386940`.
- `AutoUpdatingLua`: `addappid(386940)`, `addappid(228983,0,"aabb")`,
  `--setManifestid(228983,"111111111")` (no active pins).
- `PinnedBuildLua`: same but active `setManifestid(228983,"222222222")`.
- Build id `18234567`, source `Ryuu` in one capture test.
- Hashes are SHA-256 lowercase hex of bytes (`LuaVault.HashText`/`HashFile`).

Upgrade path:

1. `SyncDefaultFromLive_AdoptsAPreExistingLua` — one variant, Kind `default`, `BuildId == null`,
   `IsPinned == false`, variant hash == `GetActiveHash`.
2. `SyncDefaultFromLive_IsIdempotent` — call twice → one variant.
3. `SyncDefaultFromLive_NoOpsWithNoLiveFile` — no variants.

Loose build files (`<appid>_<buildid>.lua`):

4. `AdoptLooseBuildLuas_PicksUpBuildNamedFiles` — one variant Kind `build`, BuildId `18234567`,
   `IsPinned == true`, `ManifestIds["228983"] == "222222222"`.
5. `AdoptLooseBuildLuas_IgnoresOtherApps` — `999999_18234567.lua` → no variants for 386940.
6. `ApplyBuildIfNothingLive_ActivatesADroppedInBuild` — after adopting a build with no live
   `<appid>.lua`, `ApplyBuildIfNothingLive` returns the build, live file exists and equals
   `PinnedBuildLua`, active variant hash == build hash.
7. `ApplyBuildIfNothingLive_LeavesAnExistingLiveLuaAlone` — live AutoUpdating + default + loose build
   → returns null; live bytes unchanged.
8. `ApplyBuildIfNothingLive_NoOpsWithNoBuilds` — null, no live file.

Tracking probe:

9. `HasVariants_IsFalseForAnUntrackedGameEvenWithALiveLua` — live file exists but no index:
   `HasVariants == false`, `GetActiveHash != null`, `GetActiveVariant == null`; after sync, true.

Apply:

10. `Apply_WritesTheStoredBytesVerbatimAndKeepsPins` — applying build writes `PinnedBuildLua` exactly,
    active hash/variant == build hash, and the live text still contains an active
    `setManifestid(228983,"222222222")` (Apply must not run the Auto-Update transform).
11. `Apply_CanSwitchBackToDefault` — apply build, then default → live == `AutoUpdatingLua`, active
    variant == default.

Content addressing:

12. `Capture_DedupesIdenticalBytes` — two captures of identical text as `Preset` → one variant.
13. `Capture_BackfillsBuildIdOntoAlreadyStoredBytes` — a default captured first, then the same bytes
    captured as `Build` with id `18234567`/source `Ryuu` → still one variant, now BuildId
    `18234567`, Kind `build`.

External edits and the single Default slot:

14. `ExternalEdit_IsUnmatchedUntilSynced_ThenBecomesTheDefault` — edit live to AutoUpdating + `"\n--
    hand edited"`; before sync active hash is non-null but active variant null; after sync one
    Default whose hash equals that active hash (it replaced the old default).
15. `SyncDefaultFromLive_ReplacesTheDefaultInsteadOfAppendingASecondOne` — re-add from another
    generator (`"-- generated by something else\n" + AutoUpdatingLua`); one variant, Kind default,
    hash == `LuaVault.HashText(second)`; the first hash's `<hash>.lua` file is gone.
16. `SyncDefaultFromLive_CollapsesTwoDefaultsKeepingTheLiveOne` — two defaults created by the old
    append behavior, the **older** one live; after sync one variant with the older (live) hash, and
    active hash equals it.
17. `SyncDefaultFromLive_LeavesTheDefaultAloneWhileASavedBuildIsLive` — default + build 777 captured
    and applied; sync → 2 variants, active hash == build, default hash still present.
18. `SyncDefaultFromLive_LeavesThePendingEditAlone` — build applied, `SetEditBase(build.Hash)`, live
    diverged; sync → 2 variants (no third), default present, edit base still build hash.
19. `Apply_CapturesUnsavedLiveBytesBeforeOverwritingThem` — unsynced working copy live; apply build
    → a Default variant with hash `LuaVault.HashText(working)`; applying that default restores the
    working bytes exactly.
20. `SaveText_AdoptsTheEditedLiveFileAsAPreset` — `SaveText(app, edited, "my mix")` → preset label
    and DisplayLabel `"my mix"`, active variant hash == preset hash.
21. `WriteLive_UpdatesTheFileSteamReads` — `WriteLive(PinnedBuildLua)` → file equals it.

In-place update:

22. `UpdateVariant_KeepsIdentityAndReplacesTheBytes` — build 18234567 renamed "pre-nerf", then edited
    via `SetDepotEnabled(..., false)`; new hash differs, BuildId `18234567`, Label `pre-nerf`, Kind
    `build`; exactly one variant, `ReadText` equals the edited text.
23. `UpdateVariant_RemovesTheSupersededFile` — old `<hash>.lua` gone, new one exists.
24. `UpdateVariant_ReturnsNullWhenTheBaseIsGone` — base hash `"0"` ×64 → null.
25. `UpdateVariant_MergesWhenTheEditMatchesAnExistingVariant` — editing Default until byte-identical
    to the build merges to one variant with BuildId `18234567`.

Edit base:

26. `EditBase_PersistsAcrossVaultInstances` — reopened vault returns the same hash.
27. `EditBase_IsClearedByApplyAndByUpdate` — update clears it (live matches updated); apply clears it.
28. `EditBase_IsClearedBySavingANewPreset` — SaveText clears edit base; 2 variants (base kept, fork
    added).

Rename/delete:

29. `Rename_SetsTheLabelWithoutLosingTheBuildId` — initial DisplayLabel `"Build 18234567"`; rename
    "pre-nerf patch" → DisplayLabel that, BuildId still `18234567`.
30. `Rename_ToBlankRestoresTheGeneratedLabel` — rename "temp" then `"   "` → `"Build 18234567"`.
31. `Delete_RefusesTheActiveVariant` — returns false, one variant remains.
32. `Delete_RemovesAnInactiveVariant` — returns true, only the Default remains.

Persistence:

33. `Variants_SurviveAcrossVaultInstances` — reopen sees 2 variants, one with DisplayLabel `"keeper"`
    and BuildId `18234567`; `AppsWithVariants()` contains 386940.

### 6.14 `ModeMigrationTests.cs` (2 facts + 6 theories × 22 = 24) — drop with unlocker

Unit: `ModeMigration.Migrate(string? stored)` → `(string? Mode, bool ResetOnboarding)`, and
`ModeMigration.Apply(settings)`.

1. `BetterSteamToolsBrandedModes_BecomeBst_WithoutReonboarding` (2) — `"OpenSteamTools"`,
   `"OpenSteamToolsNightly"` → `"Bst"`, reset false. These were the old UI's "BetterSteamTools"
   entries, so BST is what those users believe they chose.
2. `RetiredModes_AreClearedAndTriggerOnboarding` (2) — `"SteamTools"`, `"CloudRedirect"` → null,
   reset true.
3. `CurrentModes_AreUntouched` (3) — `Ost`, `Bst`, `Custom` → same string, reset false.
4. `NoLegacyValueParsesAsACurrentMode` (4) — `Enum.TryParse<UnlockerMode>` is false for every legacy
   string.
5. `MigrationIsIdempotent` (8) — for all four legacy values, all three current values, and null:
   `Migrate(Migrate(x).Mode)` equals `Migrate(x).Mode`; second reset always false.
6. `NoStoredMode_IsNotTreatedAsARetiredMode` (3) — null, `""`, `"   "` → null, reset false.
7. `UnrecognisedGarbage_IsClearedRatherThanGuessed` — `"SomethingHandEditedByAUser"` → null, true.
8. `LowercaseVariants_AreTreatedAsUnknown` — `"ost"` → null, true (case-sensitive enum parse).

Exact rules: whitespace/null → `(null,false)`; `Enum.TryParse<UnlockerMode>` (case-sensitive) →
canonical `.ToString()`, false; `OpenSteamTools`/`OpenSteamToolsNightly` → `Bst`, false; everything
else → null, true.

### 6.15 `SteamAppInfoCacheTests.cs` (6 facts + 1 theory × 6 = 12)

Unit: `SteamAppInfoCache.FindAppDetailsEntry(JsonElement root, long appid)`.

Matching algorithm:

1. Direct key `root[appid]`. Accept it if it has no `data` object (e.g. `success:false`) OR its
   `data.steam_appid` is a **number** equal to `appid`. A direct key whose data names another app is
   refused.
2. Otherwise scan all properties for one whose `data.steam_appid` is a number equal to `appid`.
3. Otherwise null.

Cases:

1. `ChildKeyedResponse_IsMatchedByInnerSteamAppid` — `{"1660740":{"success":true,"data":
   {"steam_appid":500,"name":"Left 4 Dead"}}}`, requested 500 → found, name `"Left 4 Dead"`.
2. `SameKeyedResponse_StillResolves` — `{"10":{... "steam_appid":10,"name":"Counter-Strike"}}`,
   requested 10 → found, `"Counter-Strike"`.
3. `FailedLookup_IsStillFoundByItsKey` — `{"999999999":{"success":false}}`, requested 999999999 →
   found, `success == false`.
4. `EntryForAnotherApp_IsRefusedRatherThanGuessed` — only entry `1660740` with
   `steam_appid:1660740`, requested 500 → null.
5. `UnprovablePayloads_AreRefused` (6) — `{}`, `[]`, data without `steam_appid`, `steam_appid` as a
   string `"500"`, `success:true` with no data, entry that is a string `"nonsense"` → null for 500.
6. `DirectKey_WinsOverTheScan` — both `"500"` (Left 4 Dead) and `"1660740"` (Uncensored, steam_appid
   500) → the direct `"500"` name wins.
7. `DirectKeyDescribingAnotherApp_IsRefused` — `"500"` with `steam_appid:1660740` → null.

This was a real regression: Steam keyed appdetails by a child app id (DLC/soundtrack) while
`data.steam_appid` still echoed the requested id; `GetProperty(appid)` threw `KeyNotFoundException`
into a bare catch, so popular games silently failed forever.

### 6.16 `SteamDepotInfoCacheTests.cs` (5 facts)

Unit: `SteamDepotInfo` with injected `HttpMessageHandler` and `UtcNow`; app 386940, depot 228983.

Canned success body:

```
{"data":{
  "386940":{"depots":{"228983":{"manifests":{"public":{"gid":"999"}}}}},
  "999999":{"depots":{"228983":{"manifests":{"public":{"gid":"999"}}}}}}}
```

1. `Success_IsCachedForTheSession` — three `GetAsync(386940)` → one HTTP call.
2. `Invalidate_ForcesTheNextLookupToRefetch` — one call, `Invalidate(386940)`, next get → one more
   (2 total).
3. `Invalidate_OnlyDropsTheAppItNames` — get 386940 then 999999 (2 calls); invalidate 386940; get
   999999 again → still 2 calls.
4. `Failure_IsRetriedOnceItsTtlHasPassed` — server 500, clock `2026-08-01T12:00:00Z`: null, null, one
   call; advance 61 s, server now OK → non-null, second call. `FailureTtl = 60 s`.
5. `Success_IsNotExpiredByTheFailureTtl` — success cached, advance 1 h → non-null, still one call.
   Successes live for the session; only failures expire.

### 6.17 `SteamLinkParserTests.cs` (3 facts + 2 theories × 21 = 24)

Unit: `SteamLinkParser.AppIdFrom(string? text)`.

Accepted (11) → appid:

- SteamDB, any sub-page: `https://steamdb.info/app/648800/`, `.../depots/`, `.../patchnotes/` →
  648800; `http://steamdb.info/app/1/` → 1.
- Steam store: `https://store.steampowered.com/app/648800/Raft/`,
  `https://store.steampowered.com/app/648800`,
  `https://store.steampowered.com/agecheck/app/648800/`,
  `https://store.steampowered.com/app/648800/Raft/?snr=1_7_7_230_150_1` → 648800.
- Community: `https://steamcommunity.com/app/648800/discussions/` → 648800.
- `steam://store/648800`, `steam://install/648800` → 648800.

Rejected (10) → null:

- `https://steamdb.info/depot/648801/` (depot id, not appid)
- `https://steamdb.info/sub/12345/` (package id)
- `648800` and `app 648800` (bare numbers/text are not links)
- `steam://rungameid/13548193523847888896` (composite shortcut/mod id)
- `https://example.com/app/648800/` (wrong host)
- `https://steamdb.info/` (no id)
- `""`, `"   "`, null

Additional:

- `TextWithSeveralLinks_TakesTheFirst` — two links → 648800 (first).
- `MatchingIsCaseInsensitive` — `HTTPS://STEAMDB.INFO/APP/648800/` → 648800.
- `FragmentsAndTrailingTextAreIgnored` — `https://steamdb.info/app/648800/#section` → 648800.

### 6.18 DRM/unlocker tests to drop

If the Rust port deliberately excludes Steam depot unlocking, lua generation/pinning, unlocker modes
and Denuvo fixes, drop these test files entirely and the modules they cover:

| Test file / cases | Why it is DRM/unlocker-specific |
|---|---|
| `ModeMigrationTests.cs` (24) | Migrates unlocker backends `Ost`/`Bst`/`Custom` and retired `SteamTools`/`CloudRedirect` |
| `LuaEditorTests.cs` (13) | Comments `addappid(depot,1,"<decryption key>")` and `setManifestid` pins — the core lua unlocking mechanism |
| `LuaBuildTests.cs` (13) | Manifest pins and `<appid>_<buildid>.lua` build identity for pinned manifests |
| `LuaVaultTests.cs` (33) | Stores/switches lua variants, including pinned builds |
| `BuildsActiveHashTests.cs` (6) | Selects the active lua variant in the Builds page |
| `DepotCacheMigrationTests.cs` (15) | Rescues `.manifest` files into Steam's `depotcache` so pinned depots resolve; part of the depot/manifest feature set |
| `FileHashTests.cs` (10) | The helper is generic, but the tests were written for Denuvo fix revert. Keep the SHA-256 helper and re-home the compatibility rule (`null expected` → true) if any hash-verified file operation remains; otherwise drop |
| `SteamDepotInfoCacheTests.cs` (5) | Depot manifest metadata cache used by the Builds/Depots page. Drop only if depot metadata is out of scope |

`AppInfoCodecTests`, `LaunchModStoreTests`, `LaunchOptionsServiceTests` and
`LaunchOptionsViewModelTests` are **not** DRM: they edit Steam's launch options (`config.launch`),
a benign configuration feature. Keep them if launch-option editing is in scope; they require a
binary-VDF/appinfo writer and a backup/drift model. If launch-option editing is dropped, all 34 of
those cases go with it.

`DohResolverTests`, `DownloadRateTests`, `LanguageDetectionTests`, `SteamAppInfoCacheTests` and
`SteamLinkParserTests` are independent of DRM and should always be ported.

---

## 7. Recommended Rust workspace layout

The workspace at `/home/mohamed/Projects/luatools-rs` already has the right seven-crate split. Keep
it; add one `xtask` and one optional `luatools-platform` only if Windows code grows beyond the app
crate.

```
luatools-rs/
  Cargo.toml                 # workspace, resolver 3, edition 2024, rust 1.95
  Cargo.lock                 # commit (binary application)
  crates/
    luatools-core/           # no UI, no network, no Windows-only code
      src/lib.rs
      src/config.rs          # AppConfig constants (URLs, repos, ports, limits, user agents)
      src/paths.rs           # %AppData%\LuaToolsGui, %TEMP%\LuaTools\downloads, log paths
      src/settings.rs        # AppSettings + atomic JSON load/save + corrupt/bak handling
      src/cache.rs           # CacheData + locked read-modify-write + history cap
      src/auth_store.rs      # StoredAuth JSON shape; DPAPI behind a trait
      src/language.rs        # supported tags, MatchOsLanguage, fallback chain
      src/models/
        mod.rs
        api.rs               # Steam/Hubcap/Supabase DTOs
        modes.rs             # only if unlocker mode is retained
        launch.rs            # LaunchOption/LaunchMod shapes
        fixes.rs             # only if Denuvo fixes are retained (recommended: omit)
      src/error.rs
      src/commands.rs        # UI <-> service command/event enums (or keep in app)
    luatools-i18n/           # no UI/no network
      build.rs               # parse locales/Strings*.resx -> generated maps
      src/lib.rs             # Language selection, t! lookup, positional formatter
      locales/               # copied RESX tree (or symlink; keep exact keys)
    luatools-net/            # network only; depends on core
      src/http.rs            # shared reqwest Client + DNS Resolve impl
      src/doh.rs             # 1.1.1.1 JSON parser, TTL floor/cap, bypass rules
      src/github.rs          # direct-then-mirror candidate selection, asset verification
      src/api.rs             # lua.tools/Steam/Hubcap clients
      src/auth.rs            # Supabase OAuth, refresh, DPAPI-backed store
      src/update.rs          # release check, semver compare, download/stage/apply
      src/server.rs          # axum 127.0.0.1:6767 API (if the plugin backend is kept)
    luatools-steam/          # file formats and Steam discovery; no network
      src/lib.rs
      src/discovery.rs       # registry + libraryfolders.vdf + path validation
      src/vdf.rs             # binary VDF (types, duplicate keys, raw bytes) + text VDF
      src/appinfo.rs         # magic 27/28/29, meta layouts, save/rebuild
      src/launch_options.rs  # read/stage/apply/restore/drift, backup naming
      src/depotcache.rs      # migration only if depot manifests are kept
      src/manifest.rs        # .manifest name/content validation only if retained
    luatools-downloads/      # scheduler, jobs, metrics; depends on core (+ net if needed)
      src/lib.rs
      src/queue.rs           # pump, dedupe by key, serialized install gate
      src/job.rs             # DownloadJob, DownloadKind, JobResult
      src/item.rs            # statuses, rate window, ETA, reset
      src/history.rs         # records, cap 100
      src/byte_format.rs     # Size/Rate/Duration formatting
    luatools-ui/             # eframe/egui only; no network/file mutation
      src/lib.rs
      src/app.rs             # eframe::App, viewport, page routing
      src/theme.rs
      src/pages/             # home, add, downloads, manage, builds, mode, fixes, plugin, settings
      src/widgets/           # depot table, drop zone, toast, confirm overlay
      src/onboarding.rs
      src/i18n.rs            # re-export t! and language setting hook
    luatools-app/            # the only crate with Windows/tray/registry/runtime ownership
      src/main.rs            # #![cfg_attr(windows, windows_subsystem = "windows")]
      src/bootstrap.rs       # exact startup order
      src/single_instance.rs # named mutex/events + pending URL handoff
      src/protocol.rs        # register HKCU, parse, pending temp file
      src/lifecycle.rs       # startup tasks, host supervisor, shutdown
      src/tray.rs            # tray-icon/muda, menu ids, balloon/notification
      src/update_flow.rs     # app-before-plugin gate + restart args
      src/platform/windows.rs # registry, DPAPI, mutex/events, known folders
      src/wiring.rs          # AppServices struct, callback wiring analogous to App.OnStartup
  xtask/
    src/main.rs              # subcommands: i18n-check, dist, icon, update-feed
  scripts/check-i18n.py      # keep for translators/CI or port to xtask
  .github/workflows/         # build/test, i18n, release
```

### 7.1 Module boundaries and dependency direction

```
luatools-app ──> luatools-ui ──> luatools-i18n
     │                │
     ├──> luatools-net ──> luatools-core
     ├──> luatools-steam ─> luatools-core
     └──> luatools-downloads ─> luatools-core (and optionally luatools-net)
```

Rules:

- `luatools-core` must remain sync and pure except for atomic file persistence. It owns the JSON
  shapes and the language-matching algorithm, so it is testable without a runtime or Windows.
- `luatools-i18n` owns generated string tables and formatting. No UI dependency; returns `&'static
  str` for unformatted keys and `String` for formatted ones.
- `luatools-net` owns every HTTP client and the DoH resolver. All services receive one shared
  `reqwest::Client` (or a clone) so DNS policy and pooling are global, mirroring `AppHttp`.
- `luatools-steam` is sync file-format code. It must never open a network connection; Steam API
  calls belong in `luatools-net` or the app.
- `luatools-downloads` owns scheduling, statuses, byte/rate accounting and history. It should not
  know about Steam paths or UI.
- `luatools-ui` is a rendering layer. It emits `Command` values and consumes `Event`/snapshot
  values; it does not mutate files or block on network. All services live behind handles in
  `AppServices`.
- `luatools-app` is the composition root and the only place with `cfg(windows)` platform code,
  `unsafe` blocks, tray/registry access, and runtime ownership.

Threading model:

- Create one Tokio multi-thread runtime in `main`; keep a `Handle` in `AppServices`. eframe runs on
  the main thread. Network/file work runs on Tokio; UI updates are sent over a channel
  (`tokio::sync::mpsc` or `crossbeam-channel`) and drained in `eframe::App::update`.
- Long CPU work (appinfo indexing, hashing 373 MB, depot scans) uses `tokio::task::spawn_blocking`,
  never the UI thread. The C# code already does this for `FindDrifted`/`Reapply`.
- Download queue state can be actor-owned: a single task owns the queue and receives `Enqueue`,
  `Cancel`, `Pause`, `Resume`, `Remove` commands; the UI gets immutable snapshots per frame. This
  avoids the C# "everything on the WPF dispatcher" constraint while preserving ordering.

### 7.2 Lifecycle mapping from C# to Rust

| C# location | Rust location | Behavior to preserve |
|---|---|---|
| `Program.Main` | `luatools-app/src/main.rs` | Velopack-equivalent hook (if any) / update bootstrap first, then language, protocol, args, single instance, eframe |
| `App.xaml.cs` constructor | `wiring.rs::AppServices::new` | explicit service construction order; no hidden DI container |
| `App.OnStartup` | `bootstrap.rs::run` | exact ordering from §1.7; mode migration before unlocker read; background tasks after UI |
| `App.OnExit` | `lifecycle.rs::shutdown` | apply staged update on exit, stop hosted tasks in reverse order, dispose handles |
| `Program` statics/events | `single_instance.rs` | same names/semantics (`LuaToolsGui.SingleInstance`, `ShowWindow`, `EnableTrayLock`, `RecheckUpdates`) |
| `ProtocolService` | `app::protocol` (pure parse/pending in `core` if desired) | parse/pending are testable without Windows; registry write behind a `Platform` trait |
| `UpdateService` + `RunUpdateFlowAsync` | `net::update` + `app::update_flow` | app-before-plugin, `--minimized --tray-locked` restart args, single-flight |
| `MainWindow` tray | `app::tray` | same menu labels/behavior, 5 s balloon, hide-to-tray on close |
| `SettingsService`/`CacheService` | `core::settings`/`core::cache` | atomic write + `.bak` (settings) / no `.bak` (cache) + corrupt preservation |
| `MainWindow.Loaded` auth restore | `ui::app` startup event → `auth::restore` | offline proceeds as guest |
| `OnboardingViewModel` | `ui::onboarding` + `wiring` | same defaults, `OnboardingComplete` rules, Bst + plugin install with one Steam stop/start |
| `AppHttp` + `DohResolver` | `net::http` + `net::doh` | shared client, per-connection mode provider, bypass rules, TTL floor/cap |

Important eframe differences:

- There is no `x:Static`, so language switching does not need a relaunch technically. Decide
  whether to preserve the C# UX (write setting + prompt restart) or switch live. If live, keep the
  same setting and `Lang_Changed_*` semantics optional; document the deviation.
- There is no `ShutdownMode`; `eframe` exits when the event loop returns. Implement a `quitting`
  flag and `ViewportCommand::Close` explicitly, and keep the tray thread alive until the UI has
  stopped.
- Window geometry is not persisted in C#. `NativeOptions.viewport` should set inner size 1080×720,
  min 760×560, centered. Do not enable eframe persistence unless adding a new feature.

### 7.3 External crates

Already in the workspace (keep unless noted):

| Crate | Use | Why / notes |
|---|---|---|
| `serde`, `serde_json` | all DTOs, settings/cache/auth, launchmods/index | exact key names via `#[serde(rename = "...")]`; do **not** use `deny_unknown_fields` (the C# reader ignores unknown fields) |
| `thiserror` | typed errors (appinfo format, protocol parse, settings I/O) | library crates should not use `anyhow` in public APIs |
| `anyhow` | app/service boundary | fine in `luatools-app` and top-level service methods |
| `tokio` | runtime, tasks, timers, process, fs, sync | current features are enough; add `net` for the local server |
| `reqwest` | HTTP | current `rustls-tls`, `json`, `gzip`, `stream`, `http2`; implement `reqwest::dns::Resolve` for DoH and a separate client for DoH itself |
| `url` | protocol/link parsing | `Url::parse`, scheme/authority/path handling |
| `futures-util`, `tokio-util` | streaming, cancellation | download progress / graceful shutdown |
| `tracing`, `tracing-subscriber` | structured logs | add `tracing-appender` for a file log |
| `axum` | local 127.0.0.1:6767 API | only if the plugin backend is ported; `tower-http` for CORS/preflight |
| `chrono` | timestamps, history, TTLs | keep one time library; serde feature already enabled |
| `directories` | `%AppData%` base | Windows config dir; for `Downloads` use `known-folders`/Win32 |
| `parking_lot` | locks in cache/auth/downloads | avoid poisoning; keep critical sections short |
| `indexmap` | ordered maps for UI/API where needed | **not** for appinfo/VDF duplicate keys; use `Vec<(String, Value)>` there |
| `sha2`, `hex` | SHA-256 for covers/assets/vault | appinfo also needs SHA-1 (see below) |
| `image` | cover/capsule decoding | add `ico` for the app/tray icon or use the `ico` crate |
| `eframe`, `egui` | GUI | existing 0.37; Windows backend; keep `x11`/`wayland` only for Linux dev builds |
| `tempfile` | atomic writes/staging tests | use `NamedTempFile::persist` for settings/cache |

Additions, with justification:

| Crate | Use | Justification |
|---|---|---|
| `sha1` | appinfo `sha1_binary`/`sha1_text` | the C# codec recomputes SHA-1 over the app body; `sha2` cannot do it |
| `quick-xml` (build-dependency) | parse RESX in `build.rs`/xtask | robust XML vs the C# regex; preserve key set, detect malformed files |
| `phf` (or generated match) | static locale/key maps | 552 keys × 30 locales; perfect-hash lookup avoids runtime maps and startup cost |
| `unic-langid` | parse/normalize BCP-47 tags | correctly handles `zh-Hans`, `es-419`, `pt-BR`; `LanguageIdentifier` has script/region fields |
| `sys-locale` | OS display language | closest equivalent of `CultureInfo.InstalledUICulture` on Windows |
| `winreg` | HKCU protocol handler, Run key | small, safe wrapper over the registry; exact paths from §2.4/§2.2 |
| `windows` | named mutex/events, DPAPI, known folders, process control | `CreateMutexW`, `CreateEventW`, `WaitForMultipleObjects`, `CryptProtectData`, `SHGetKnownFolderPath`; scope `unsafe` to `platform/windows.rs` |
| `tray-icon` + `muda` | system tray and native menu | eframe has no tray; `tray-icon` is the maintained cross-platform tray, `muda` provides menu items |
| `rfd` | file/folder pickers | replaces WinForms `OpenFileDialog`/`FolderBrowserDialog` |
| `arboard` | clipboard | copy appid / paths |
| `open` or `webbrowser` | open URLs, reveal in Explorer | `open` handles `explorer /select,` style reveal better; keep a small platform helper |
| `semver` (or custom) | update version comparison | Velopack versions may be 3- or 4-part; test a custom parser against real tags before relying on `semver` |
| `zip` | release/tool archives | `zip` crate; `flate2` comes transitively |
| `self-replace` | replace the running exe on Windows | updater cannot overwrite a running image; `self-replace` schedules the swap; pair with a small relaunch helper |
| `atomic-write-file` | settings/cache/index writes | write-temp-then-rename with Windows semantics; `tempfile::persist` is an alternative |
| `tracing-appender` | rolling file log | fills the C# gap; log to `%AppData%\LuaToolsGui\logs\` |
| `egui_extras` | tables, image loaders, date picker | the C# UI uses virtualized grids and date/sort controls |
| `tower-http` | CORS for local API | the C# server answers `OPTIONS` 204 and sets CORS on every response |
| `bytes` | streaming downloads | chunk buffers for reqwest streams |
| `known-folders` | `Downloads`/local app data | `directories` does not expose the Windows Downloads known folder |
| `clap` (or `pico-args`) | CLI flags | only 3 flags; `pico-args` is enough; `clap` if `--help`/future flags matter |
| `formatx` (or custom 50-line formatter) | positional `{0}` substitution | Rust `format!` requires literals; translations carry runtime `{0}`/`{1}`/`{2}`. A custom formatter is safest and can reject malformed placeholders |
| `rstest` | table-driven tests | mirrors xUnit `[Theory]`/`[InlineData]` with named cases |
| `wiremock` or `httpmock` | HTTP stubs | replaces the C# `HttpMessageHandler` stubs for DoH/API/download tests |
| `assert_fs` | filesystem assertions | temp trees for appinfo/settings/depotcache tests |
| `insta` | snapshot tests | appinfo round-trip/JSON shape snapshots; reviewable diffs |
| `proptest` | property tests | duplicate-key/raw-byte VDF invariants; fuzzy protocol/link parsing |
| `cargo-nextest` (dev tool) | faster test runs | not a dependency; recommended CI runner |
| `ico` | app/tray icon | `image` does not decode `.ico` without a feature; `ico` is focused |
| `single-instance` or `interprocess` (optional) | portable single-instance/pipe | `windows` named mutex/events is the exact port; use `interprocess` if Linux dev builds need it |

Do **not** add:

- a full i18n framework (`fluent`, `rust-i18n`) unless the translation format is being redesigned:
  the current messages have no plural/gender syntax and use positional placeholders; a generated
  static map is smaller and exactly matches the 552-key contract.
- an ORM/database: the C# app uses JSON files only.
- a WPF-like MVVM framework: eframe immediate-mode rendering does not need it; use plain state +
  command enums.
- `serde_yaml`/`toml` for settings: settings/cache/auth are JSON; only the unlocker `latest.toml`
  uses TOML, and that feature may be dropped.

### 7.4 Cargo feature/lint considerations

- The workspace currently denies `unsafe_code`. Win32 named mutex/events, DPAPI, `SHGetKnownFolderPath`
  and `self-replace` require `unsafe`. Options:
  1. put all `unsafe` in `luatools-app/src/platform/windows.rs` and add a module-level
     `#[allow(unsafe_code)]` with a documented safety comment; or
  2. relax the workspace lint to `warn` and require `// SAFETY:` comments.
  Do not let the lint force a safe-but-wrong design (e.g. an in-process-only "single instance").
- Gate Windows-only dependencies under `[target.'cfg(windows)'.dependencies]`; keep Linux dev builds
  compiling with stub `Platform` implementations (tray/registry/single-instance no-ops).
- `eframe`'s `x11`/`wayland` features are Linux-only; keep them in dev but do not ship them on
  Windows.
- `release` profile already has `lto = "fat"`, `codegen-units = 1`, `strip = true`; add
  `panic = "abort"` only if the updater/panic hook design does not need unwinding.

### 7.5 Porting the tests

| C# test file | Rust location | Notes |
|---|---|---|
| `AppInfoCodecTests` | `luatools-steam/tests/appinfo_codec.rs` | use `proptest` for duplicate keys/raw bytes; keep exact v29 fixture |
| `LaunchModStoreTests` | `luatools-steam/tests/launch_mod_store.rs` or `core` if store is core | tempdir + JSON |
| `LaunchOptionsServiceTests` | `luatools-steam/tests/launch_options_service.rs` | inject `SteamControl` trait; no real Steam |
| `LaunchOptionsViewModelTests` | `luatools-app`/`luatools-ui` unit tests | test a pure `can_save` function rather than egui widgets |
| `DohResolverTests` | `luatools-net/tests/doh.rs` | `wiremock` or a custom `tower::Service`; injectable clock |
| `DownloadRateTests` | `luatools-downloads/tests/rate.rs` | injectable `Instant`/`DateTime`; expand rstest cases |
| `FileHashTests` | `luatools-core/tests/hash.rs` | keep known SHA-256 vector and null-expected rule |
| `LanguageDetectionTests` | `luatools-core/tests/language.rs` | port every 55 case as `#[rstest]` |
| `SteamAppInfoCacheTests` | `luatools-net/tests/appdetails_match.rs` | pure JSON matching |
| `SteamDepotInfoCacheTests` | `luatools-net/tests/depot_info_cache.rs` | stub transport + clock |
| `SteamLinkParserTests` | `luatools-core/tests/steam_link.rs` | pure `Url` parsing; keep rejection cases |
| Lua/depot/unlocker test files | omit | see §6.18 |

Aim to keep the same test **names** (snake_case transliteration) so future audits can diff the
matrices, and keep the same boundary values (166 s, 8,315 MB, 22,580 MB, 60 s TTL, 55 language
cases) rather than inventing new ones.

---

## 8. Ambiguities and decisions needed

1. **Release pipeline is not in the source repo.** There is no release workflow, no `vpk` command,
   no packId/channel, no signing configuration, and no update-feed asset names. The only artifact
   names are the README badges `LuaTools-win-Setup.exe` / `LuaTools-win-Portable.zip`. Confirm the
   actual packId, channel (`win`?), installer technology, code-signing identity and feed layout
   before designing the Rust updater.
2. **Velopack upgrade compatibility.** Existing users are on Velopack-managed installs. A Rust
   binary cannot simply replace the exe and keep the Velopack update chain. Decide whether to
   (a) keep packaging the Rust binary with Velopack and implement the Velopack install/update hooks,
   (b) ship a one-time migration installer, or (c) accept that existing installs must manually
   reinstall. This is the largest release risk.
3. **No window geometry persistence exists.** The prompt asks about "window state persistence", but
   the C# app persists only `MinimizeToTray` and the `StartWithWindows` Run key. Confirm that
   geometry persistence is out of scope (the Rust workspace's eframe `persistence` feature would
   otherwise silently add it).
4. **No global unhandled-exception handling.** There is no `DispatcherUnhandledException`/
   `AppDomain.UnhandledException`/`TaskScheduler.UnobservedTaskException` hook and no general file
   log. Should the Rust port add a panic hook + rolling file log (recommended), or mirror the silent
   behavior?
5. **`SettingsService.Save` omits `DnsMode` from the "empty" check.** A settings file whose only
   change is DNS mode is deleted on save. Fix in Rust, but confirm whether a migration should
   preserve/repair existing installs.
6. **Cache has no backup, settings does.** `settings.json` keeps `.bak` and moves corrupt files to
   `.corrupt`; `cache.json` has only `.tmp` and silently resets to empty on parse failure, which can
   wipe the permanent `DonatedAppIds`. Decide whether the Rust cache should gain a `.bak`/corrupt
   preservation (recommended) or match exactly.
7. **Logging destination.** `Host.CreateDefaultBuilder` logs to debugger/EventLog/console providers on
   Windows, not a file; only `plugin-backend.log` is a real file. Decide the Rust log path/rotation
   and whether `PluginLog`'s exact format/path must be preserved for support workflows.
8. **`--minimized` semantics.** It is used both for "start in tray on sign-in" (Run key) and for the
   loader's duplicate launches. A `--minimized` launch with no protocol URL starts tray-only; a
   `--minimized` second instance does not signal the window; a cold `--minimized` run does not
   auto-exit. Confirm all three are intended and whether `StartWithWindows` should also pass
   `--tray-locked` (currently it does not).
9. **Silent install auto-exit edge case.** Auto-exit requires a cold `StartupUrl`, no `MinimizeToTray`,
   and `DownloadQueue.ActiveCount == 0` after a 6 s delay. If another download is queued, the process
   never auto-exits and can linger in the tray indefinitely. Confirm the desired policy for
   queued/background work.
10. **Language casing/canonicalization.** `IsSupported` is case-insensitive but returns the original
    `os.Name`/saved string; a hand-edited `PT-br` is accepted but not canonicalized. Decide whether
    the Rust port should normalize to the canonical tag on load/save.
11. **Stale translation comments.** `Resources/README.md` says the Builds page is pending and
    `Strings.Designer.cs` says Builds/Launch are untranslated, but `PENDING_TRANSLATION` is empty and
    all 552 keys exist in all 29 files, with sampled Builds/Launch translations present. Confirm
    the comments are stale and parity is truly enforced.
12. **The i18n checker's regex is format-sensitive.** It only sees `<data name="..."
    xml:space="preserve"><value>...</value></data>` exactly; `<comment>` children, line breaks
    between the data open tag and `<value>`, or extra attributes make a key invisible. The Rust
    `xtask i18n-check` should either preserve this constraint or intentionally use a real XML parse
    (recommended) and reject comments/format drift explicitly.
13. **`SteamSearchResult`, `StandardUsage` and `StoredAuth` have no `JsonPropertyName`.** Their
    serialized shape is PascalCase, unlike the API DTOs. `StoredAuth` is on disk (DPAPI), so a Rust
    port must match `RefreshToken`, `AccessToken`, `ExpiresAt`, etc. exactly to read existing
    sessions. Confirm whether to migrate the auth file or force a re-login.
14. **`DenuvoFixRecord`/`AppliedFixIndex` are DRM contracts.** The comments say the on-disk JSON
    names must not change, but the features should be dropped. Confirm that no migration/read of
    existing `.luatools-fix` records is required, and whether leftover files should be ignored or
    cleaned up.
15. **`ModeDefinition.HiddenUnlessFile` is never set** by the current three definitions, and
    `FixedTag` is always null; `ModeKind.Manual` is only Custom. Confirm these are dead model fields
    (drop) or reserved for future modes.
16. **`ModeDefinition` and `UpdateManifest` are records with mixed persistence.** `Kinds`/`Statuses`
    are enum names if serialized, while `ModeDefinition` includes a `string[] PlaceFiles`; TOML
    parsing is line-based, not a real TOML parser. Confirm the unlocker feature is out of scope
    (recommended) so this can be dropped wholesale.
17. **`LaunchOption` on-disk shape is PascalCase** (`Index`, `SourceIndex`, `Executable`, ...) because
    it has no JSON attributes and is stored in `launchmods.json`. If launch options are ported, keep
    this exact shape; if not, drop the whole appinfo/launch-options stack.
18. **`AppInfoCodec` supports magic 27/28/29** with 40-byte meta for ≤27 and 60-byte meta for ≥28;
    only v29 is tested. Confirm whether the Rust codec must read/rewrite v27/v28 or can reject them.
    Also confirm whether all unedited apps must be copied byte-for-byte (the test asserts a
    single-app file round-trips identically).
19. **Reference repos are placeholder/dual.** `AppConfig.GithubReleasesRepos` has a primary and a
    backup, but the comment still contains a TODO to "set the real primary repo before the first
    `vpk upload`". Confirm the primary is live and whether the backup should exist in the Rust
    updater.
20. **`UpdateReady` event is never subscribed.** It is raised but unused. The Rust updater can omit
    it or use it for a UI badge; confirm no hidden consumer.
21. **Backend endpoints are HTTP, not HTTPS.** `AppConfig.ManifestBackendUrl` is
   `http://167.235.229.108` and `HttpServerService`'s `ManifestBackendUrl` is the same host. Confirm
   these are still correct and whether the Rust port should keep plain HTTP for that backend.
22. **Hardcoded English strings remain in C#.** Examples: the plugin-update toast in
   `RunUpdateFlowAsync` ("LuaTools", "Updating plugin. Steam will restart."), the tray text
   "LuaTools", and some log messages. The port should localize user-visible ones; confirm whether
   exact parity requires keeping them.
23. **The Rust workspace's `unsafe_code = "deny"` conflicts with Win32 requirements.** Decide the
   lint policy (scoped `allow` in a platform module vs. relaxing the workspace lint).
24. **`Cargo.toml` currently has no Windows dependencies, no `xtask`, and no `target/` ignore.**
   These are additions, not existing decisions; confirm they belong in this port phase.
25. **`Strings.resx` base file is not counted by the i18n script's glob.** The script reports 29
    "language files" even though there are 30 RESX files; the README says 29 languages. Keep the
    terminology clear in the Rust port (29 translations + 1 source).
26. **Source version vs release tags disagree.** The csproj says `1.1.3`, while git tags `v1.3.2`
    and `v1.3.3` point at the same imported commit. Confirm which value is the real current release
    and whether the Rust port should derive its version from Cargo (`0.1.0` today), a CI variable, or
    a tag. Existing Velopack users will not receive a Rust build unless its version is strictly
    higher than the installed one.
27. **The bot-account re-link banner is hardcoded off.** `AuthService.IsBotProvisioned` detects
    emails ending in `@bot.lua.tools`, and `AppConfig.BotAccountEmailDomain` says the app should
    prompt the user to re-link, but `SettingsViewModel.ShowBotLinkBanner => false` always returns
    false, so the `Settings_BotLink_*` keys are dead UI. Confirm whether the Rust port should
    implement the banner or omit the detection.

---

## Appendix A — full English key inventory (552)

Generated from `Strings.resx`; keys are listed in file order within each prefix group. This is the
exact key set every `Strings.<tag>.resx` must carry.

Total keys: **552**. Groups: 23.

**Nav (13):** `Nav_Home`, `Nav_Add`, `Nav_Manage`, `Nav_Mode`, `Nav_Fixes`, `Nav_RestartSteam`, `Nav_Settings`, `Nav_SignInDiscord`, `Nav_WaitingForBrowser`, `Nav_Footer_Guest`, `Nav_Footer_LoggedIn`, `Nav_Builds`, `Nav_Downloads`

**Main (1):** `Main_RestartSteam_Ask`

**Settings (61):** `Settings_Title`, `Settings_Section_Account`, `Settings_Section_General`, `Settings_Section_Steam`, `Settings_Section_Install`, `Settings_Section_Community`, `Settings_SignOut`, `Settings_BrowsingAsGuest`, `Settings_GuestHint`, `Settings_SignInDiscord`, `Settings_BotCode_Hint`, `Settings_BotCode_Placeholder`, `Settings_BotCode_Redeem`, `Settings_BotCode_Expired`, `Settings_BotCode_Invalid`, `Settings_BotCode_ServerError`, `Settings_BotLink_Title`, `Settings_BotLink_Body`, `Settings_BotLink_Dismiss`, `Settings_LoginRequired`, `Settings_ManageKeyHintPrefix`, `Settings_Section_Hubcap`, `Settings_HubcapKey`, `Settings_HubcapKey_Hint`, `Settings_HubcapKeyPlaceholder`, `Settings_HubcapValidate`, `Settings_HubcapClear`, `Settings_HubcapGetKey`, `Settings_HubcapKeyOk`, `Settings_HubcapKeyOkExpiry`, `Settings_HubcapKeyBad`, `Settings_HubcapKeyError`, `Settings_HubcapActive`, `Settings_HubcapRefresh`, `Settings_Language`, `Settings_Language_Hint`, `Settings_Language_SystemDefault`, `Settings_Dns`, `Settings_Dns_Hint`, `Settings_Dns_Auto`, `Settings_Dns_Always`, `Settings_Dns_Never`, `Settings_SteamLocation`, `Settings_Change`, `Settings_Open`, `Settings_ResetToAuto`, `Settings_AutoUpdateApps`, `Settings_AutoUpdateApps_Hint`, `Settings_DonateKeys`, `Settings_DonateKeys_Hint`, `Settings_Section_Startup`, `Settings_StartWithWindows`, `Settings_StartWithWindows_Hint`, `Settings_MinimizeToTray`, `Settings_MinimizeToTray_Hint`, `Settings_SteamNotFound`, `Settings_SteamSource_Custom`, `Settings_SteamSource_NotFound`, `Settings_SteamSource_Auto`, `Settings_SteamWarning_NoExe`, `Settings_ChooseSteamFolder`

**Tray (2):** `Tray_Open`, `Tray_Exit`

**Lang (3):** `Lang_Changed_Title`, `Lang_Changed_Body`, `Lang_Changed_Restart`

**Home (14):** `Home_Welcome`, `Home_LuasOnBoard`, `Home_LastAdded`, `Home_PluginStatus`, `Home_RecentlyAdded`, `Home_ViewDetails`, `Home_CheckingSteam`, `Home_SteamDetected`, `Home_SteamNotFound`, `Home_BrowsingAsGuest`, `Home_SignedIn`, `Home_SignedInAs`, `Home_NoModeSelected`, `Home_ModeIs`

**Mode (45):** `Mode_Title`, `Mode_Subtitle`, `Mode_CheckForUpdates`, `Mode_Active`, `Mode_Recommended`, `Mode_Confirm_Body`, `Mode_Cancel`, `Mode_CloseSteamContinue`, `Mode_Desc_Bst`, `Mode_CloudRedirect_Manage`, `Mode_CloudRedirect_LaunchFailed`, `Mode_Desc_Ost`, `Mode_Name_Custom`, `Mode_Desc_Custom`, `Mode_UserManaged`, `Mode_Experimental`, `Mode_CloudRedirectSupport`, `Mode_CloudRedirect_AddonDesc`, `Mode_CloudRedirect_Locked`, `Mode_CloudRedirect_Enable`, `Mode_CloudRedirect_Disable`, `Mode_CloudRedirect_Status_Enabled`, `Mode_CloudRedirect_Status_Disabled`, `Mode_CloudRedirect_Status_NotInstalled`, `Mode_CloudRedirect_Status_UpdateAvailable`, `Mode_CloudRedirect_Toast_Enabled`, `Mode_CloudRedirect_Toast_Disabled`, `Mode_CloudRedirect_Toast_Updated`, `Mode_Checking`, `Mode_StatusUnavailable`, `Mode_NotActive`, `Mode_NotInstalled`, `Mode_UpToDate`, `Mode_UpdateAvailable`, `Mode_Btn_Reinstall`, `Mode_Btn_Update`, `Mode_Btn_Install`, `Mode_Btn_Switch`, `Mode_Confirm_Reinstall`, `Mode_Confirm_Switch`, `Mode_Toast_Updated`, `Mode_Toast_Updated_Restarting`, `Mode_Toast_Updated_Start`, `Mode_Toast_InstallFailed`, `Mode_Toast_InstallFailed_Body`

**Onboarding (11):** `Onboarding_Title`, `Onboarding_SignIn_Blurb`, `Onboarding_SignedIn`, `Onboarding_Recommended_Title`, `Onboarding_Recommended_Detail`, `Onboarding_Plugin_Title`, `Onboarding_Plugin_Detail`, `Onboarding_Yes`, `Onboarding_No`, `Onboarding_Go`, `Onboarding_Applying`

**Drop (13):** `Drop_Title`, `Drop_LinkHint`, `Drop_BrowseFiles`, `Drop_Picker_Title`, `Drop_Picker_Filter`, `Drop_Nothing`, `Drop_Confirm_Replace`, `Drop_Confirm_NoChanges`, `Drop_NothingInstalled`, `Drop_Count_Luas`, `Drop_Count_Manifests`, `Drop_Result_Installed`, `Drop_Result_Failed`

**Common (6):** `Common_SearchPlaceholder`, `Common_Loading`, `Common_AppId`, `Common_AppFallback`, `Common_CopyAppId`, `Common_ShowInFolder`

**Manage (77):** `Manage_Title`, `Manage_Refresh`, `Manage_Filters`, `Manage_Filter_Type`, `Manage_Filter_Genre`, `Manage_Filter_Year`, `Manage_Filter_Price`, `Manage_Filter_Content`, `Manage_Content_HideAdult`, `Manage_Content_AdultOnly`, `Manage_Filter_SortBy`, `Manage_ClearFilters`, `Manage_Select`, `Manage_DeleteThisLua`, `Manage_ResultsPerPage`, `Manage_Action_ViewDetails`, `Manage_Action_Update`, `Manage_Action_OpenStore`, `Manage_Action_OpenInSteam`, `Manage_Action_Reveal`, `Manage_Action_CopyAppId`, `Manage_Action_RemoveDrm`, `Manage_Action_ManageBuild`, `Manage_AboutHeader`, `Manage_Info_Developer`, `Manage_Info_Publisher`, `Manage_ActionsHeader`, `Manage_Steamless_Confirm_Title`, `Manage_Steamless_Confirm_Body`, `Manage_Steamless_Working`, `Manage_Toast_Steamless_Done`, `Manage_Steamless_NoInstall`, `Manage_Steamless_Failed`, `Manage_CopyAppIdTip`, `Manage_LoadingDepotInfo`, `Manage_MissingHint`, `Manage_UnknownHint`, `Manage_Clear`, `Manage_DeleteSelected`, `Manage_LoadingGames`, `Manage_Opt_Any`, `Manage_Opt_Free`, `Manage_Opt_Paid`, `Manage_Sort_RecentlyAdded`, `Manage_Sort_NameAZ`, `Manage_Sort_ReleaseNewest`, `Manage_Sort_Metacritic`, `Manage_Sort_MostReviewed`, `Manage_PageSize_All`, `Manage_PageLabel`, `Manage_SelectionLabel`, `Manage_FetchingDetails`, `Manage_Empty_NoMatch`, `Manage_Empty_NoLuas`, `Manage_Empty_NoSteam`, `Manage_Toast_NotFound_Title`, `Manage_Toast_NotFound_Body`, `Manage_Toast_Refreshed_Title`, `Manage_Toast_Refreshed_Body`, `Manage_DepotError`, `Manage_Toggle_InLua`, `Manage_Toggle_Missing`, `Manage_Toggle_Unknown`, `Manage_Depot`, `Manage_SharedDepot`, `Manage_DlcName`, `Manage_Delete_Title`, `Manage_Delete_Body`, `Manage_DeleteMany_Title`, `Manage_DeleteMany_Body`, `Manage_RemoveFailed_Title`, `Manage_RemoveFailed_File`, `Manage_RemoveFailed_Named`, `Manage_RemoveFailed_Count`, `Manage_RestartSteam_Title`, `Manage_RestartSteam_Failed`, `Manage_Action_LaunchOptions`

**Fixes (42):** `Fixes_Title`, `Fixes_Loading`, `Fixes_Build`, `Fixes_Manifest`, `Fixes_Fix`, `Fixes_Count`, `Fixes_Err_Load`, `Fixes_Empty_None`, `Fixes_SignIn`, `Fixes_SignInRequired`, `Fixes_Toast_DownloadFailed`, `Fixes_Toast_DownloadFailed_Body`, `Fixes_Toast_InstallFailed`, `Fixes_Toast_InstallFailed_Body`, `Fixes_Toast_FixInstalled`, `Fixes_Toast_FixInstalled_Body`, `Fixes_Toast_GameNotFound`, `Fixes_Toast_GameNotFound_Body`, `Fixes_Toast_PartiallyApplied`, `Fixes_Toast_PartiallyApplied_Body`, `Fixes_Toast_FixApplied`, `Fixes_Toast_FixApplied_Body`, `Fixes_Toast_CouldntApply`, `Fixes_Toast_Refreshed_Title`, `Fixes_Toast_Refreshed_Body`, `Fixes_Revert`, `Fixes_Revert_Cancel`, `Fixes_Revert_Confirm_Title`, `Fixes_Revert_Confirm_Body`, `Fixes_Revert_Done`, `Fixes_Revert_Done_Body`, `Fixes_Revert_Partial`, `Fixes_Revert_Partial_Body`, `Fixes_Revert_Failed`, `Fixes_Revert_NoRecord`, `Fixes_Revert_Conflict`, `Fixes_Revert_Conflict_Body`, `Fixes_Applied_Hint`, `Fixes_MyGames`, `Fixes_MyGames_Count`, `Fixes_MyGames_NotInstalled`, `Fixes_NotInstalled_Hint`

**Add (40):** `Add_Title`, `Add_Subtitle`, `Add_SearchPlaceholder`, `Add_Featured_TopSellers`, `Add_Featured_NewReleases`, `Add_Released`, `Add_Fetch`, `Add_Checking`, `Add_Reveal`, `Add_Supporter`, `Add_Unlimited`, `Add_RequiresOwnKey`, `Add_Discord`, `Add_JoinDiscord`, `Add_Download`, `Add_NeedsHubcapKey`, `Add_NeedsHubcapKey_Tip`, `Add_Depots`, `Add_Available`, `Add_Missing`, `Add_NoKey`, `Add_DownloadLua`, `Add_Generating`, `Add_DlcHint`, `Add_AlreadyHaveFiles`, `Add_SignIn_Dlc`, `Add_SignIn_Download`, `Add_Err_BaseGame`, `Add_Err_Generic`, `Add_Err_Download`, `Add_Confirm_Replace`, `Add_Confirm_NoChanges`, `Add_Status_Cancelled`, `Add_Status_InstallFailed`, `Add_Status_AddedManifests`, `Add_Status_AddedFetch`, `Add_FastFetch`, `Add_FastFetch_Hint`, `Add_FastFetch_NoSource`, `Add_FastFetch_Via`

**Confirm (7):** `Confirm_OpenSteamDb`, `Confirm_Shared`, `Confirm_ReviewChanges`, `Confirm_Adding`, `Confirm_Removing`, `Confirm_Skip`, `Confirm_Replace`

**Plugin (42):** `Plugin_Title`, `Plugin_Subtitle`, `Plugin_CardTitle`, `Plugin_Badge_UpdateAvailable`, `Plugin_Row_InstalledVersion`, `Plugin_Row_LatestVersion`, `Plugin_Row_Frontend`, `Plugin_Row_Loader`, `Plugin_Millennium_Coexisting`, `Plugin_Footer`, `Plugin_Btn_Install`, `Plugin_Btn_Update`, `Plugin_Btn_Reinstall`, `Plugin_Btn_Uninstall`, `Plugin_Btn_CheckForUpdates`, `Plugin_Checking`, `Plugin_Status_Installed`, `Plugin_Status_NotInstalled`, `Plugin_Status_UpToDate`, `Plugin_Status_OutOfDate`, `Plugin_Version_Unknown`, `Plugin_Version_Offline`, `Plugin_Status_OfflineCheck`, `Plugin_Status_Port8080Busy`, `Plugin_Confirm_RestartBody`, `Plugin_Confirm_RestartCaption`, `Plugin_Toast_Title`, `Plugin_Toast_Installed`, `Plugin_Toast_InstallFailed`, `Plugin_Toast_Removed`, `Plugin_Toast_UninstallFailed`, `Plugin_Toast_UpdateAvailable`, `Plugin_Toast_UpToDate`, `Plugin_Err_SteamNotFound`, `Plugin_Err_GithubUnreachable`, `Plugin_Err_MissingAssets`, `Plugin_Err_VerifyFailed`, `Plugin_Err_NoLuatoolsJs`, `Plugin_About_Header`, `Plugin_Feature_StoreButton`, `Plugin_Feature_AutoUpdate`, `Plugin_Feature_Millennium`

**Builds (55):** `Builds_Title`, `Builds_Subtitle`, `Builds_NoSelection`, `Builds_ActiveBuild`, `Builds_Active`, `Builds_Variant_Default`, `Builds_Variant_Build`, `Builds_Variant_Preset`, `Builds_Pinned`, `Builds_AutoUpdating`, `Builds_LatestBuild`, `Builds_Manifest`, `Builds_ManifestLatest`, `Builds_PinDisabled`, `Builds_Meta_Depots`, `Builds_Meta_Unsaved`, `Builds_Source`, `Builds_Action_Apply`, `Builds_Action_Rename`, `Builds_Action_Edit`, `Builds_Action_Delete`, `Builds_Action_Save`, `Builds_Action_SaveAsPreset`, `Builds_Action_Cancel`, `Builds_RenamePlaceholder`, `Builds_Editor_Title`, `Builds_Delete_Title`, `Builds_Delete_Body`, `Builds_Delete_ActiveTip`, `Builds_Apply_Done`, `Builds_Apply_Failed`, `Builds_Save_Done`, `Builds_Save_Failed`, `Builds_Preset_Saved`, `Builds_Action_SaveTo`, `Builds_Action_SaveAsNew`, `Builds_Save_Updated`, `Builds_Depot_Lock`, `Builds_Depot_LockTip`, `Builds_Depot_EnableTip`, `Builds_Depot_Search`, `Builds_Depot_NoMatch`, `Builds_Depot_InactiveNote`, `Builds_Action_Download`, `Builds_Select_Title`, `Builds_Select_Confirm`, `Builds_Select_NoManifest`, `Builds_Select_All`, `Builds_Select_None`, `Builds_Select_ChooseFolder`, `Builds_Select_SaveTo`, `Builds_Select_Space`, `Builds_Select_SignIn`, `Builds_Select_NoKey`, `Builds_Select_SharedHint`

**Launch (33):** `Launch_Title`, `Launch_Loading`, `Launch_NotFound`, `Launch_UnsupportedFormat`, `Launch_ReadFailed`, `Launch_ExecutableRequired`, `Launch_Add`, `Launch_Delete`, `Launch_Field_Executable`, `Launch_Field_Arguments`, `Launch_Field_WorkingDir`, `Launch_Field_Description`, `Launch_Field_Os`, `Launch_Field_Type`, `Launch_Field_BetaKey`, `Launch_Field_BetaKey_Tip`, `Launch_Field_OwnsDlc`, `Launch_InstallPath`, `Launch_SteamRestartNote`, `Launch_Restore`, `Launch_Restore_Tip`, `Launch_Restore_Title`, `Launch_Restore_Body`, `Launch_NothingToRestore`, `Launch_ApplyNow_Title`, `Launch_ApplyNow_Body`, `Launch_Staged`, `Launch_Applied`, `Launch_Applied_Restarted`, `Launch_ApplyFailed`, `Launch_Drift_Title`, `Launch_Drift_Body`, `Launch_Drift_Action`

**Err (16):** `Err_SteamNotFound`, `Err_GithubUnreachable`, `Err_UpdateServerUnreachable`, `Err_Cancelled`, `Err_CancelledByUser`, `Err_ReleaseMissingDownload`, `Err_ReleaseMissingFile`, `Err_VerifyFailed`, `Err_VerifyFailedFile`, `Err_DownloadMissingFiles`, `Err_WriteFailedCount`, `Err_WriteFailedFile`, `Err_OpenDownloadFailed`, `Err_NoSourceAvailable`, `Err_ClipboardBusy`, `Err_PathMissing`

**Auth (6):** `Auth_Err_Timeout`, `Auth_Err_TokenExchangeFailed`, `Auth_Err_TokenExchangeEmpty`, `Auth_Err_NotSignedIn`, `Auth_Err_RefreshFailed`, `Auth_Err_RefreshEmpty`

**Api (4):** `Api_Err_EmptyDownloadLink`, `Api_Err_DownloadFailed`, `Api_Err_RequestFailed`, `Api_Err_SessionExpired`

**Hubcap (4):** `Hubcap_Err_InvalidKey`, `Hubcap_Err_LimitReached`, `Hubcap_Err_NoManifest`, `Hubcap_Err_DownloadFailed`

**Downloads (45):** `Downloads_Title`, `Downloads_Empty`, `Downloads_Section_Active`, `Downloads_Section_History`, `Downloads_Status_Queued`, `Downloads_Status_Downloading`, `Downloads_Status_AwaitingConfirm`, `Downloads_Status_Installing`, `Downloads_Status_Completed`, `Downloads_Status_Failed`, `Downloads_Status_Cancelled`, `Downloads_Action_Cancel`, `Downloads_Action_Retry`, `Downloads_Action_Remove`, `Downloads_Action_MoveUp`, `Downloads_Action_MoveDown`, `Downloads_Action_ClearHistory`, `Downloads_Action_Review`, `Downloads_Of`, `Downloads_Eta`, `Downloads_ActionRequired`, `Downloads_Err_Interrupted`, `Downloads_Kind_Dlc`, `Downloads_Status_Paused`, `Downloads_Status_Verifying`, `Downloads_Action_Pause`, `Downloads_Action_Resume`, `Downloads_Kind_Depot`, `Downloads_Depots_Progress`, `Downloads_Depot_FetchingManifest`, `Downloads_Depots_Preparing`, `Downloads_Depots_GettingTool`, `Downloads_Kind_Tool`, `Downloads_SteamAutoCrack`, `Downloads_SAC_GettingRuntime`, `Downloads_SAC_GettingTool`, `Downloads_SAC_Launched`, `Downloads_SAC_Err_Runtime`, `Downloads_SAC_Err_Restart`, `Downloads_SAC_Err_Tool`, `Downloads_SAC_Err_Launch`, `Downloads_SAC_Updated`, `Downloads_Depot_PreAllocating`, `Downloads_Depot_Validating`, `Downloads_ClearHistory_Confirm`

**Depot (12):** `Depot_Err_Tool`, `Depot_Err_NoKeys`, `Depot_Err_Failed`, `Depot_Status_Done`, `Depot_Err_NoSpace`, `Depot_Err_SignIn`, `Depot_Err_NoManifest`, `Depot_Cancel_Title`, `Depot_Cancel_Body`, `Depot_Cancel_DeleteFailed`, `Depot_Err_BadKey`, `Depot_Err_NoKeyFor`
