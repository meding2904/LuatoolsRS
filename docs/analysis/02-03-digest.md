# Infrastructure and UI analysis — digest

The full analyses of `SettingsService`/`CacheService`/`AppHttp`/`DohResolver`/`GithubProxy`/
`AuthService`/`UpdateService`/`PluginInstallerService`/`CefInjectorService`/`HttpServerService`
and of every WPF view/view model were returned inline by the analysis subagents. The actionable
contracts were folded into [`../port-spec.md`](../port-spec.md). This file keeps the facts that
matter to a maintainer and are not obvious from the Rust code.

## Persistence

- Original settings: `%AppData%\LuaToolsGui\settings.json`, PascalCase JSON, every field saved,
  temp file + rename, `.bak` backup, corrupt primary preserved as `.corrupt`.
  Known bug: the "nothing worth saving" predicate omitted the DNS mode, so a DNS-only settings
  file was deleted. The Rust store has no delete-when-empty behaviour.
- Cache: `cache.json` in the same directory, one write lock, no backup, silently resets on parse
  failure. The Rust port splits cache (disposable, TTL) from history (durable, atomic).
- Secrets: the original stored the Hubcap key in plaintext settings and the auth session in a
  DPAPI blob. `luatools-net::auth::FileSecretStore` uses 0600 JSON on Unix; a Windows DPAPI
  backend is a follow-up behind the same trait.

## Network

- Shared `SocketsHttpHandler`, 2-minute pooled connection lifetime, per-connection DNS mode
  callback (`Auto`/`Always`/`Never`).
- DoH: `https://1.1.1.1/dns-query`, 5 s timeout, `Accept: application/dns-json`, TTL clamped
  30 s–5 min, bypass for IP literals/`localhost`/`*.local`/single-label hosts, no failure caching.
- GitHub downloads: direct URL first, then mirror prefixes; no retry/backoff; per-candidate
  partial-file cleanup; digest verified by the caller.
- Local plugin server: fixed `127.0.0.1:6767`, wildcard CORS, no auth in the original. The Rust
  server binds loopback only and accepts an optional shared token header.

## Download queue

- Original: WPF-dispatcher-owned, unlimited parallel downloads, serialized installs, progress
  throttled to one UI post per 100 ms, history cap 100, pause only for depot jobs.
- Known race: resume cleared the pause flag while the old run loop was unwinding, so the old
  loop could settle the resumed job as cancelled. The Rust actor serializes transitions.
- The Rust queue is generic (`Transfer` trait) and does not implement depot acquisition.

## UI

- Fixed dark theme, 1080×720 default, 760×560 minimum, 48 px custom title bar, 190 px nav rail,
  no router (page selection is state), no page-transition animations, no keyboard shortcuts.
- Palette (exact): window/card surfaces are 3–8 % white over `#202020`; dialogs `#1C1C22`;
  primary text `#FFFFFF`; secondary `#9CA3AF`; tertiary `#6B7280`; brand violet `#A78BFA`;
  error `#F87171`; success `#4ADE80`; warning `#FBBF24`; scrim `#99000000`.
- Type: Segoe UI 14 default; title 22 bold; section 15 semibold; card title 13.5–14.5 semibold;
  meta 11–11.5. Radii: 4 controls, 8/10 cards, 12 dialogs, 14 onboarding. Spacing scale
  2/4/6/8/10/12/14/16/18/20/24/32.
- Pages: Home (greeting, 2 stat tiles, status rows, drop zone, recent apps), Add (search,
  featured strips, details, sources, DLC, drop zone), Downloads (queue + history), Manage
  (virtualized grid, filters, pager, detail flyout), Depots (two-column picker + depot table),
  Fixes, Launch options modal, Mode, Onboarding overlay, Plugin, Settings.
- Localization: 552 keys, 30 resx files, 29 translations, `{n}` placeholders, key parity
  enforced in CI. English is the fallback.
- Confirmations default: depot cancel defaults to destructive *Yes*; clear history defaults to
  safe *No*. The Rust confirm dialog keeps the labels but no longer defaults silently.

## Not ported

Mode (unlocker backends), Fixes (Denuvo patches), Add manifest-source installation, Depots
pin/key editing and depot acquisition, Manage "Remove Steam DRM", Downloads SteamAutoCrack,
Settings Hubcap/DonateKeys, DropZone file installation. See the exclusion table in
[`../port-spec.md`](../port-spec.md).
