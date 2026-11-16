//! Steam store-page CEF injection over the Chrome DevTools Protocol.
//!
//! Steam exposes a CDP endpoint on `127.0.0.1:8080` while the plugin's
//! `.cef-enable-remote-debugging` marker is installed. [`CefInjector`] polls the target list,
//! keeps one WebSocket per store tab, injects `luatools.js` into every store page whose JS context
//! is not live, and drains the plugin's queued
//! `window.Millennium.callServerMethod("luatools", ...)` requests through [`CefBridge`].
//!
//! Everything here is best-effort: a missing Steam client, a closed debugging port, or a crashed
//! tab costs a debug log line, never the application. A missing or outdated `luatools.js` simply
//! means nothing is injected until the file appears.
//!
//! The transport is tungstenite's blocking API (re-exported by `tokio-tungstenite`), driven from a
//! Tokio blocking task. The app crate has no direct `futures-util` dependency, so the async
//! `WebSocketStream` extension helpers are not available; socket read timeouts keep the loop
//! responsive and let it observe the shutdown token between reads.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use luatools_core::{AppPaths, Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{self, Message, http::Uri};
use tokio_util::sync::CancellationToken;

/// Steam's DevTools HTTP endpoint. The port is hardcoded by Steam itself.
const CDP_HOST: &str = "127.0.0.1";
const CDP_PORT: u16 = 8080;
const CDP_PATH: &str = "/json";

/// Fast cadence used to drain queued bridge requests.
const TICK_INTERVAL: Duration = Duration::from_millis(150);
/// Slow cadence used to rediscover tabs, check liveness, and re-inject after navigation.
const DISCOVERY_INTERVAL: Duration = Duration::from_millis(1000);
/// How long one `Runtime.evaluate` may wait for its matching CDP response.
const EVALUATE_TIMEOUT: Duration = Duration::from_millis(800);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_millis(400);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_READ_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_HTTP_BYTES: usize = 8 * 1024 * 1024;

/// Set as the final statement of `luatools.js`; absent after a navigation wipes the JS context.
const LIVENESS_EXPRESSION: &str = "String(window.__LuaToolsReady === true)";

/// Reads the queued RPCs, with their argument objects stringified exactly like the C# service.
const PENDING_EXPRESSION: &str = "JSON.stringify(Object.values(window.Millennium._pending||{}).map(function(r){return{m:r.method,a:JSON.stringify(r.args)}}))";

const DRAIN_PREFIX: &str = "(function(r){var ks=Object.keys(window.Millennium._pending);for(var i=0;i<ks.length&&i<r.length;i++){var id=ks[i];window.Millennium._readyResponses[id]=r[i];delete window.Millennium._pending[id];}})(";
const DRAIN_SUFFIX: &str = ");";

/// Polyfill for `window.Millennium` installed alongside `luatools.js`.
///
/// A real Millennium object, if the page has one, is preserved: only `"luatools"` calls are
/// intercepted, every other plugin name is forwarded to the original `callServerMethod`. When no
/// Millennium exists, a minimal stand-in is created. The `_pending` / `_readyResponses` queues are
/// attached to whichever object ends up as `window.Millennium`.
const POLYFILL_JS: &str = r#"
(function(){
var real=window.Millennium;
var pending={},ready={},reqId=0;
function ltCall(p,m,a){
  var i='_ltr_'+(++reqId);
  pending[i]={method:m,args:a,ts:Date.now()};
  return new Promise(function(rv,rj){
    var mx=100;
    function ck(){
      var r=ready[i];
      if(r!==undefined){delete ready[i];if(r.e){rj(new Error(r.e))}else{rv(r.v)}return}
      if(--mx>0){setTimeout(ck,50)}else{delete pending[i];rv({success:true})}
    }
    ck();
  });
}
if(real&&typeof real.callServerMethod==='function'){
  var realCall=real.callServerMethod.bind(real);
  real.callServerMethod=function(p,m,a){return p==='luatools'?ltCall(p,m,a):realCall(p,m,a)};
  real._pending=pending;
  real._readyResponses=ready;
  window.Millennium=real;
}else{
  window.Millennium={_pending:pending,_readyResponses:ready,callServerMethod:function(p,m,a){return ltCall(p,m,a)}};
}
})();
"#;

/// Host callbacks used to service requests coming from a Steam store page.
///
/// Implementations run on the injector's polling task and should return quickly. The methods are
/// the only way the store page can affect the application; the injector never talks to the local
/// HTTP API itself.
pub trait CefBridge: Send + Sync {
    /// Returns true when LuaTools is installed for `app`.
    fn has_app(&self, app: u32) -> bool;

    /// Opens the LuaTools settings page.
    fn open_settings(&self);

    /// Restarts Steam.
    fn restart_steam(&self);

    /// Opens a validated `http(s)` URL in the user's browser.
    fn open_external_url(&self, url: &str);

    /// Starts a check for application and plugin updates.
    fn check_updates(&self);

    /// Apps added since the last Steam restart, as `(appid, name)`.
    fn loaded_apps(&self) -> Vec<(u32, String)>;

    /// Clears the "apps added since the last restart" notification list.
    fn dismiss_loaded_apps(&self);

    /// Writes one message from the store page to the application log.
    fn log(&self, message: &str);
}

/// Injects `luatools.js` into Steam store tabs and services their queued bridge requests.
pub struct CefInjector {
    api_port: u16,
    bridge: Arc<dyn CefBridge>,
    shutdown: CancellationToken,
    scripts: ScriptStore,
    sockets: HashMap<String, CdpSocket>,
    next_id: u64,
}

impl CefInjector {
    /// Starts the injector on a Tokio blocking task.
    ///
    /// The task runs until `shutdown` is cancelled. It is safe to leave running while Steam is
    /// closed and does nothing until `luatools.js` is installed.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, because `spawn_blocking` requires one. Call it
    /// from the backend runtime.
    #[must_use]
    pub fn start(
        paths: AppPaths,
        api_port: u16,
        bridge: Arc<dyn CefBridge>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::task::spawn_blocking(move || {
            let injector = Self {
                api_port,
                bridge,
                shutdown,
                scripts: ScriptStore::new(&paths),
                sockets: HashMap::new(),
                next_id: 1,
            };
            injector.run();
        })
    }

    /// Polls discovery on the slow cadence and bridge requests on the fast cadence.
    fn run(mut self) {
        tracing::debug!(api_port = self.api_port, "CEF injector started");
        let mut tabs: Vec<StoreTab> = Vec::new();
        let mut next_discovery = Instant::now();

        loop {
            if self.shutdown.is_cancelled() {
                break;
            }

            let now = Instant::now();
            if now >= next_discovery {
                next_discovery = now + DISCOVERY_INTERVAL;
                self.scripts.refresh();
                match fetch_store_tabs(&self.shutdown) {
                    Ok(found) => {
                        tabs = found;
                        self.inject_missing(&tabs);
                        self.evict_absent(&tabs);
                    }
                    Err(Error::Cancelled) => break,
                    Err(error) => tracing::debug!(%error, "CEF target discovery failed"),
                }
            }

            for tab in &tabs {
                if self.shutdown.is_cancelled() {
                    break;
                }
                self.drain_tab(tab);
            }

            sleep_interruptible(&self.shutdown, TICK_INTERVAL);
        }

        self.sockets.clear();
        tracing::debug!("CEF injector stopped");
    }

    /// Injects the polyfill and script into every store tab whose JS context is not live.
    fn inject_missing(&mut self, tabs: &[StoreTab]) {
        let Some(injection) = self.scripts.injection().map(str::to_owned) else {
            return;
        };
        for tab in tabs {
            if self.shutdown.is_cancelled() {
                return;
            }
            let alive = self.evaluate(tab, LIVENESS_EXPRESSION);
            if alive.as_deref() != Some("true") {
                tracing::debug!(tab = %tab.id, url = %tab.url, "injecting luatools.js");
                let _ = self.evaluate(tab, &injection);
            }
        }
    }

    /// Drains the tab's queued requests and writes the responses back into the page.
    fn drain_tab(&mut self, tab: &StoreTab) {
        let Some(raw) = self.evaluate(tab, PENDING_EXPRESSION) else {
            return;
        };
        if raw.trim().is_empty() || raw == "[]" || raw == "null" {
            return;
        }
        let requests = match parse_pending(&raw) {
            Ok(requests) => requests,
            Err(error) => {
                tracing::debug!(%error, tab = %tab.id, "could not parse queued LuaTools requests");
                return;
            }
        };
        if requests.is_empty() {
            return;
        }

        let responses: Vec<Value> = requests
            .iter()
            .map(|request| response_envelope(self.bridge.as_ref(), request))
            .collect();
        let payload = match serde_json::to_string(&responses) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(%error, "could not encode bridge responses");
                return;
            }
        };
        let expression = format!("{DRAIN_PREFIX}{payload}{DRAIN_SUFFIX}");
        let _ = self.evaluate(tab, &expression);
    }

    /// Runs one `Runtime.evaluate` and returns its string result.
    ///
    /// A socket is opened lazily per tab and evicted on any error, so the next call reconnects.
    fn evaluate(&mut self, tab: &StoreTab, expression: &str) -> Option<String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let command = json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": { "expression": expression, "returnByValue": true },
        });
        let payload = match serde_json::to_string(&command) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(%error, "could not encode a CDP command");
                return None;
            }
        };

        let stale = self
            .sockets
            .get(&tab.id)
            .is_none_or(|socket| socket.url != tab.web_socket_debugger_url);
        if stale {
            self.sockets.remove(&tab.id);
            match connect_socket(&tab.web_socket_debugger_url) {
                Ok(socket) => {
                    self.sockets.insert(
                        tab.id.clone(),
                        CdpSocket {
                            url: tab.web_socket_debugger_url.clone(),
                            socket,
                        },
                    );
                }
                Err(error) => {
                    tracing::debug!(%error, tab = %tab.id, "CDP connection failed");
                    return None;
                }
            }
        }

        let entry = self.sockets.get_mut(&tab.id)?;
        let outcome = match entry.socket.send(Message::text(payload)) {
            Ok(()) => {
                read_matching_response(&mut entry.socket, id, EVALUATE_TIMEOUT, &self.shutdown)
            }
            Err(error) => Err(ws_error("sending a CDP command", error)),
        };

        match outcome {
            Ok(Some(value)) => Some(value),
            Ok(None) => {
                tracing::debug!(tab = %tab.id, "CDP command timed out");
                None
            }
            Err(error) => {
                tracing::debug!(%error, tab = %tab.id, "CDP command failed");
                self.sockets.remove(&tab.id);
                None
            }
        }
    }

    /// Drops sockets for tabs Steam no longer reports.
    fn evict_absent(&mut self, tabs: &[StoreTab]) {
        let live: HashSet<&str> = tabs.iter().map(|tab| tab.id.as_str()).collect();
        self.sockets.retain(|id, _| live.contains(id.as_str()));
    }
}

/// One persistent CDP WebSocket and the URL it was opened for.
struct CdpSocket {
    url: String,
    socket: tungstenite::WebSocket<TcpStream>,
}

/// Disk-backed `luatools.js`, reloaded when its modification time changes.
struct ScriptStore {
    candidates: Vec<PathBuf>,
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
    injection: Option<String>,
}

impl ScriptStore {
    fn new(paths: &AppPaths) -> Self {
        let mut store = Self {
            candidates: script_candidates(paths),
            path: None,
            modified: None,
            injection: None,
        };
        store.refresh();
        if store.injection.is_none() {
            tracing::debug!("luatools.js not found; store pages stay untouched until it appears");
        }
        store
    }

    /// Re-resolves a missing file and reloads it when the modification time changed.
    fn refresh(&mut self) {
        if self.path.is_none() {
            self.path = self
                .candidates
                .iter()
                .find(|candidate| candidate.is_file())
                .cloned();
        }
        let Some(path) = self.path.clone() else {
            self.injection = None;
            self.modified = None;
            return;
        };

        let modified = match std::fs::metadata(&path).and_then(|meta| meta.modified()) {
            Ok(modified) => Some(modified),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "luatools.js disappeared");
                self.path = None;
                self.modified = None;
                self.injection = None;
                return;
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "luatools.js metadata unavailable");
                None
            }
        };
        if let (Some(current), Some(cached)) = (modified, self.modified)
            && current == cached
        {
            return;
        }

        match load_script(&path) {
            Ok(script) => {
                self.modified = modified;
                if script.trim().is_empty() {
                    self.injection = None;
                } else {
                    self.injection = Some(format!("{POLYFILL_JS}\n{script}"));
                    tracing::info!(bytes = script.len(), path = %path.display(), "loaded luatools.js");
                }
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "luatools.js could not be read");
            }
        }
    }

    fn injection(&self) -> Option<&str> {
        self.injection.as_deref()
    }
}

/// Ordered `luatools.js` locations, primary installation first.
fn script_candidates(paths: &AppPaths) -> Vec<PathBuf> {
    let mut candidates = vec![
        paths
            .data_dir()
            .join("plugin")
            .join("public")
            .join("luatools.js"),
        paths.data_dir().join("plugin").join("luatools.js"),
    ];
    for root in legacy_roots() {
        candidates.push(root.join("plugin").join("public").join("luatools.js"));
        candidates.push(root.join("plugin").join("luatools.js"));
    }
    candidates
}

/// Legacy C# locations: `%AppData%\LuaToolsGui` (Windows) or `~/.config/LuaToolsGui`.
fn legacy_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("LuaToolsGui"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".config").join("LuaToolsGui"));
    }
    roots
}

fn load_script(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|error| {
        Error::io(format!("reading {}", path.display()), error).context("loading luatools.js")
    })
}

/// A Steam store tab worth injecting into.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoreTab {
    id: String,
    url: String,
    web_socket_debugger_url: String,
}

/// One entry of Chrome's `/json` target list.
#[derive(Debug, Clone, Deserialize)]
struct RawTab {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: Option<String>,
}

/// Keeps store pages that expose a usable debugger socket, ignoring every other target.
fn parse_store_tabs(json: &str) -> Result<Vec<StoreTab>> {
    let raw: Vec<Value> = serde_json::from_str(json)
        .map_err(|error| Error::parse("cdp targets", error.to_string()))?;
    Ok(raw
        .into_iter()
        .filter_map(|value| {
            let tab: RawTab = serde_json::from_value(value).ok()?;
            let id = tab.id?;
            let url = tab.url?;
            let web_socket_debugger_url = tab.web_socket_debugger_url?;
            if id.is_empty()
                || web_socket_debugger_url.is_empty()
                || !url.to_ascii_lowercase().contains("store.steampowered.com")
            {
                return None;
            }
            Some(StoreTab {
                id,
                url,
                web_socket_debugger_url,
            })
        })
        .collect())
}

/// One queued request as serialized by [`PENDING_EXPRESSION`].
#[derive(Debug, Clone, Deserialize)]
struct RawPending {
    #[serde(default)]
    m: Option<String>,
    #[serde(default)]
    a: Option<Value>,
}

/// A parsed bridge request; `args` is the plugin's argument object.
#[derive(Debug, Clone, PartialEq)]
struct BridgeRequest {
    method: String,
    args: Value,
}

fn parse_pending(json: &str) -> Result<Vec<BridgeRequest>> {
    let raw: Vec<RawPending> = serde_json::from_str(json)
        .map_err(|error| Error::parse("luatools pending requests", error.to_string()))?;
    Ok(raw
        .into_iter()
        .map(|request| {
            let method = request.m.unwrap_or_default();
            let args = match request.a {
                Some(Value::String(text)) => serde_json::from_str(&text).unwrap_or(Value::Null),
                Some(value) => value,
                None => Value::Null,
            };
            BridgeRequest { method, args }
        })
        .collect())
}

/// Looks up `name` in the argument object, falling back to `args` itself for bare values.
fn arg_value<'a>(args: &'a Value, name: &str) -> Option<&'a Value> {
    args.get(name)
}

fn arg_u32(args: &Value, name: &str) -> Option<u32> {
    let value = arg_value(args, name).unwrap_or(args);
    match value {
        Value::Number(number) => number.as_u64().and_then(|raw| u32::try_from(raw).ok()),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn arg_string(args: &Value, name: &str) -> Option<String> {
    let value = arg_value(args, name).unwrap_or(args);
    match value {
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// Extracts a log line from the shapes the store frontend is known to send.
fn logger_message(args: &Value) -> String {
    for key in ["message", "msg", "text"] {
        if let Some(text) = arg_string(args, key) {
            return text;
        }
    }
    match args {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Maps one plugin request onto [`CefBridge`].
///
/// Methods LuaTools does not know are acknowledged with `{"success":true}` so an older or newer
/// frontend can still resolve its promise. Missing required arguments are reported as `{e}`
/// responses by [`response_envelope`].
fn dispatch(bridge: &dyn CefBridge, request: &BridgeRequest) -> std::result::Result<Value, String> {
    match request.method.as_str() {
        "HasLuaToolsForApp" => {
            let app = arg_u32(&request.args, "appid")
                .ok_or_else(|| "HasLuaToolsForApp requires an appid".to_owned())?;
            Ok(json!({ "success": true, "exists": bridge.has_app(app) }))
        }
        "OpenSettings" => {
            bridge.open_settings();
            Ok(json!({ "success": true }))
        }
        "RestartSteam" => {
            bridge.restart_steam();
            Ok(json!({ "success": true }))
        }
        "OpenExternalUrl" => {
            let url = arg_string(&request.args, "url")
                .ok_or_else(|| "OpenExternalUrl requires a url".to_owned())?;
            let lowered = url.to_ascii_lowercase();
            if !(lowered.starts_with("http://") || lowered.starts_with("https://")) {
                return Err("OpenExternalUrl only accepts http(s) URLs".to_owned());
            }
            bridge.open_external_url(&url);
            Ok(json!({ "success": true }))
        }
        "CheckForUpdatesNow" => {
            bridge.check_updates();
            Ok(json!({ "success": true }))
        }
        "ReadLoadedApps" => {
            let apps: Vec<Value> = bridge
                .loaded_apps()
                .into_iter()
                .map(|(appid, name)| json!({ "appid": appid, "name": name }))
                .collect();
            Ok(json!({ "success": true, "apps": apps }))
        }
        "DismissLoadedApps" => {
            bridge.dismiss_loaded_apps();
            Ok(json!({ "success": true }))
        }
        "Logger" => {
            bridge.log(&logger_message(&request.args));
            Ok(json!({ "success": true }))
        }
        _ => Ok(json!({ "success": true })),
    }
}

/// Wraps a bridge result in the `{v}` / `{e}` shape the polyfill resolves.
fn response_envelope(bridge: &dyn CefBridge, request: &BridgeRequest) -> Value {
    match dispatch(bridge, request) {
        Ok(value) => json!({ "v": value }),
        Err(message) => json!({ "e": message }),
    }
}

/// Converts a tungstenite failure into the workspace error type.
fn ws_error(context: &str, error: tungstenite::Error) -> Error {
    Error::Network {
        context: context.to_owned(),
        message: error.to_string(),
    }
}

/// Opens a CDP socket with bounded connect and read times.
fn connect_socket(ws_url: &str) -> Result<tungstenite::WebSocket<TcpStream>> {
    let uri: Uri = ws_url
        .parse()
        .map_err(|error| Error::parse("websocket url", format!("{ws_url}: {error}")))?;
    match uri.scheme_str() {
        Some(scheme) if scheme.eq_ignore_ascii_case("ws") => {}
        _ => {
            return Err(Error::parse(
                "websocket url",
                format!("{ws_url} is not a ws:// URL"),
            ));
        }
    }
    let host = uri
        .host()
        .ok_or_else(|| Error::parse("websocket url", format!("{ws_url} has no host")))?;
    let port = uri.port_u16().unwrap_or(80);
    let address = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|error| Error::io(format!("resolving {host}:{port}"), error))?
        .next()
        .ok_or_else(|| Error::parse("websocket url", format!("{host}:{port} did not resolve")))?;

    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|error| Error::io(format!("connecting to {address}"), error))?;
    stream
        .set_read_timeout(Some(SOCKET_READ_TIMEOUT))
        .map_err(|error| Error::io("configuring the CDP socket", error))?;
    stream
        .set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))
        .map_err(|error| Error::io("configuring the CDP socket", error))?;
    let _ = stream.set_nodelay(true);

    tungstenite::client::client(ws_url, stream)
        .map(|(socket, _response)| socket)
        .map_err(|error| Error::Network {
            context: "opening the CDP websocket".to_owned(),
            message: error.to_string(),
        })
}

/// Result of correlating one frame with the command that is waiting for a response.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CdpReply {
    /// An event frame, another command's response, or unparseable data.
    Ignored,
    /// Our response, but without a plain string result (for example an injection eval).
    MissingValue,
    /// Our response with a string result.
    Text(String),
}

fn parse_cdp_reply(raw: &str, id: u64) -> CdpReply {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return CdpReply::Ignored;
    };
    if value.get("id").and_then(Value::as_u64) != Some(id) {
        return CdpReply::Ignored;
    }
    match value
        .pointer("/result/result/value")
        .and_then(Value::as_str)
    {
        Some(text) => CdpReply::Text(text.to_owned()),
        None => CdpReply::MissingValue,
    }
}

/// Reads frames until the response for `id` arrives, skipping events and other ids.
///
/// `Ok(None)` means the deadline expired or shutdown was requested; the caller keeps or evicts the
/// socket as appropriate.
fn read_matching_response(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    id: u64,
    timeout: Duration,
    shutdown: &CancellationToken,
) -> Result<Option<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        if shutdown.is_cancelled() || Instant::now() >= deadline {
            return Ok(None);
        }
        match socket.read() {
            Ok(Message::Text(text)) => match parse_cdp_reply(text.as_str(), id) {
                CdpReply::Ignored => {}
                CdpReply::MissingValue => return Ok(Some(String::new())),
                CdpReply::Text(value) => return Ok(Some(value)),
            },
            Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Close(frame)) => {
                let detail = frame.map(|frame| format!(": {frame}")).unwrap_or_default();
                return Err(Error::Network {
                    context: "reading a CDP response".to_owned(),
                    message: format!("the tab closed the debugging socket{detail}"),
                });
            }
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(ws_error("reading a CDP response", error)),
        }
    }
}

/// Parsed HTTP response, used only for Steam's local `/json` endpoint.
#[derive(Debug, PartialEq, Eq)]
struct HttpResponse {
    status: u16,
    body: String,
}

/// Parses a complete-enough HTTP response. `None` means the body is still incomplete.
fn parse_http_response(bytes: &[u8]) -> Result<Option<HttpResponse>> {
    let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| Error::parse("http", format!("the headers are not UTF-8: {error}")))?;
    let mut lines = header.split("\r\n");
    let Some(status_line) = lines.next() else {
        return Ok(None);
    };
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| Error::parse("http", "the response has no status code"))?
        .parse::<u16>()
        .map_err(|error| Error::parse("http", format!("invalid status code: {error}")))?;

    let mut content_length = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }

    let available = &bytes[header_end + 4..];
    let body = match content_length {
        Some(length) if available.len() < length => return Ok(None),
        Some(length) => &available[..length],
        None => available,
    };
    let body = std::str::from_utf8(body)
        .map_err(|error| Error::parse("http", format!("the body is not UTF-8: {error}")))?
        .to_owned();
    Ok(Some(HttpResponse { status, body }))
}

/// Fetches `/json` from Steam's loopback DevTools server.
fn fetch_store_tabs(shutdown: &CancellationToken) -> Result<Vec<StoreTab>> {
    let body = fetch_cdp_target_list(shutdown)?;
    parse_store_tabs(&body)
}

fn fetch_cdp_target_list(shutdown: &CancellationToken) -> Result<String> {
    let address = SocketAddr::from(([127, 0, 0, 1], CDP_PORT));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|error| Error::io(format!("connecting to {address}"), error))?;
    stream
        .set_read_timeout(Some(HTTP_READ_TIMEOUT))
        .map_err(|error| Error::io("configuring the CEF target socket", error))?;
    stream
        .set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))
        .map_err(|error| Error::io("configuring the CEF target socket", error))?;

    let request = format!(
        "GET {CDP_PATH} HTTP/1.1\r\nHost: {CDP_HOST}:{CDP_PORT}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| Error::io("requesting the CEF target list", error))?;

    let deadline = Instant::now() + HTTP_TIMEOUT;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        if shutdown.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                response.extend_from_slice(&buffer[..count]);
                if response.len() > MAX_HTTP_BYTES {
                    return Err(Error::parse("cdp targets", "the target list is too large"));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(Error::io("reading the CEF target list", error)),
        }
    }

    let parsed = parse_http_response(&response)?
        .ok_or_else(|| Error::parse("http", "the CEF target response was incomplete"))?;
    if parsed.status != 200 {
        return Err(Error::Http {
            status: parsed.status,
            url: format!("http://{CDP_HOST}:{CDP_PORT}{CDP_PATH}"),
        });
    }
    Ok(parsed.body)
}

/// Sleeps in small slices so cancellation is observed promptly.
fn sleep_interruptible(shutdown: &CancellationToken, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !shutdown.is_cancelled() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use parking_lot::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeBridge {
        has: AtomicBool,
        settings: AtomicUsize,
        restarts: AtomicUsize,
        updates: AtomicUsize,
        dismissed: AtomicUsize,
        seen_apps: Mutex<Vec<u32>>,
        urls: Mutex<Vec<String>>,
        logs: Mutex<Vec<String>>,
        apps: Mutex<Vec<(u32, String)>>,
    }

    impl CefBridge for FakeBridge {
        fn has_app(&self, app: u32) -> bool {
            self.seen_apps.lock().push(app);
            self.has.load(Ordering::Relaxed)
        }

        fn open_settings(&self) {
            self.settings.fetch_add(1, Ordering::Relaxed);
        }

        fn restart_steam(&self) {
            self.restarts.fetch_add(1, Ordering::Relaxed);
        }

        fn open_external_url(&self, url: &str) {
            self.urls.lock().push(url.to_owned());
        }

        fn check_updates(&self) {
            self.updates.fetch_add(1, Ordering::Relaxed);
        }

        fn loaded_apps(&self) -> Vec<(u32, String)> {
            self.apps.lock().clone()
        }

        fn dismiss_loaded_apps(&self) {
            self.dismissed.fetch_add(1, Ordering::Relaxed);
        }

        fn log(&self, message: &str) {
            self.logs.lock().push(message.to_owned());
        }
    }

    fn request(method: &str, args: Value) -> BridgeRequest {
        BridgeRequest {
            method: method.to_owned(),
            args,
        }
    }

    #[test]
    fn store_tabs_are_filtered_and_parsed() {
        let json = r#"[
            {
                "id": "A",
                "url": "https://store.steampowered.com/app/620/Portal_2/",
                "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/A",
                "title": "Portal 2"
            },
            {
                "id": "B",
                "url": "https://STORE.STEAMPOWERED.COM/app/620",
                "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/B"
            },
            {
                "id": "C",
                "url": "https://store.steampowered.com/app/620",
                "webSocketDebuggerUrl": ""
            },
            {
                "url": "https://store.steampowered.com/app/620",
                "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/D"
            },
            {
                "id": "E",
                "webSocketDebuggerUrl": "ws://127.0.0.1:8080/devtools/page/E"
            },
            "junk"
        ]"#;

        let tabs = parse_store_tabs(json).unwrap();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].id, "A");
        assert_eq!(tabs[1].id, "B");
        assert_eq!(
            tabs[0].web_socket_debugger_url,
            "ws://127.0.0.1:8080/devtools/page/A"
        );
    }

    #[test]
    fn non_array_target_lists_are_rejected() {
        assert!(parse_store_tabs("{}").is_err());
    }

    #[test]
    fn pending_requests_and_arguments_are_parsed() {
        let json = r#"[
            {"m":"HasLuaToolsForApp","a":"{\"appid\":620}"},
            {"m":"Logger","a":"\"hello\""},
            {"m":"ReadLoadedApps"}
        ]"#;

        let requests = parse_pending(json).unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "HasLuaToolsForApp");
        assert_eq!(arg_u32(&requests[0].args, "appid"), Some(620));
        assert_eq!(requests[1].args, Value::String("hello".to_owned()));
        assert_eq!(requests[2].args, Value::Null);
    }

    #[test]
    fn polyfill_intercepts_only_luatools_and_keeps_real_millennium() {
        assert!(POLYFILL_JS.contains("p==='luatools'"));
        assert!(POLYFILL_JS.contains("realCall(p,m,a)"));
        assert!(POLYFILL_JS.contains("_pending"));
        assert!(POLYFILL_JS.contains("_readyResponses"));
        assert!(POLYFILL_JS.contains("window.Millennium=real"));
    }

    #[test]
    fn argument_extraction_handles_numbers_strings_and_bare_values() {
        assert_eq!(arg_u32(&json!({"appid": 620}), "appid"), Some(620));
        assert_eq!(arg_u32(&json!({"appid": "620"}), "appid"), Some(620));
        assert_eq!(arg_u32(&json!(620), "appid"), Some(620));
        assert_eq!(arg_u32(&json!({}), "appid"), None);
        assert_eq!(
            arg_string(&json!({"url": "https://example.com"}), "url").as_deref(),
            Some("https://example.com")
        );
        assert_eq!(logger_message(&json!({"message": "hi"})), "hi");
        assert_eq!(logger_message(&json!("bare")), "bare");
    }

    #[test]
    fn cdp_replies_are_correlated_by_id() {
        let text = r#"{"id":7,"result":{"result":{"type":"string","value":"true"}}}"#;
        assert_eq!(parse_cdp_reply(text, 7), CdpReply::Text("true".to_owned()));

        let boolean = r#"{"id":7,"result":{"result":{"type":"boolean","value":true}}}"#;
        assert_eq!(parse_cdp_reply(boolean, 7), CdpReply::MissingValue);

        let event = r#"{"method":"Page.frameNavigated","params":{}}"#;
        assert_eq!(parse_cdp_reply(event, 7), CdpReply::Ignored);
        assert_eq!(parse_cdp_reply(text, 8), CdpReply::Ignored);
        assert_eq!(parse_cdp_reply("not json", 7), CdpReply::Ignored);
    }

    #[test]
    fn dispatch_maps_methods_to_the_bridge() {
        let bridge = FakeBridge::default();
        bridge.has.store(true, Ordering::Relaxed);
        *bridge.apps.lock() = vec![(620, "Portal 2".to_owned())];

        let reply = response_envelope(
            &bridge,
            &request("HasLuaToolsForApp", json!({"appid": 620})),
        );
        assert_eq!(reply, json!({"v": {"success": true, "exists": true}}));
        assert_eq!(*bridge.seen_apps.lock(), vec![620]);

        assert_eq!(
            response_envelope(&bridge, &request("OpenSettings", Value::Null)),
            json!({"v": {"success": true}})
        );
        assert_eq!(
            response_envelope(&bridge, &request("RestartSteam", Value::Null)),
            json!({"v": {"success": true}})
        );
        assert_eq!(
            response_envelope(&bridge, &request("CheckForUpdatesNow", Value::Null)),
            json!({"v": {"success": true}})
        );
        assert_eq!(
            response_envelope(&bridge, &request("DismissLoadedApps", Value::Null)),
            json!({"v": {"success": true}})
        );
        assert_eq!(bridge.settings.load(Ordering::Relaxed), 1);
        assert_eq!(bridge.restarts.load(Ordering::Relaxed), 1);
        assert_eq!(bridge.updates.load(Ordering::Relaxed), 1);
        assert_eq!(bridge.dismissed.load(Ordering::Relaxed), 1);

        assert_eq!(
            response_envelope(&bridge, &request("ReadLoadedApps", Value::Null)),
            json!({"v": {"success": true, "apps": [{"appid": 620, "name": "Portal 2"}]}})
        );

        assert_eq!(
            response_envelope(
                &bridge,
                &request("OpenExternalUrl", json!({"url": "https://example.com/x"}))
            ),
            json!({"v": {"success": true}})
        );
        assert_eq!(
            *bridge.urls.lock(),
            vec!["https://example.com/x".to_owned()]
        );

        let rejected = response_envelope(
            &bridge,
            &request("OpenExternalUrl", json!({"url": "file:///etc/passwd"})),
        );
        assert!(rejected.get("e").is_some());
        assert_eq!(bridge.urls.lock().len(), 1);

        assert_eq!(
            response_envelope(&bridge, &request("Logger", json!({"message": "store log"}))),
            json!({"v": {"success": true}})
        );
        assert_eq!(*bridge.logs.lock(), vec!["store log".to_owned()]);
    }

    #[test]
    fn unsupported_methods_and_missing_arguments_stay_best_effort() {
        let bridge = FakeBridge::default();
        assert_eq!(
            response_envelope(&bridge, &request("SomethingNewer", json!({}))),
            json!({"v": {"success": true}})
        );
        let missing = response_envelope(&bridge, &request("HasLuaToolsForApp", json!({})));
        assert!(missing.get("e").is_some());
    }

    #[test]
    fn http_responses_are_parsed_by_content_length() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(
            parse_http_response(raw).unwrap(),
            Some(HttpResponse {
                status: 200,
                body: "[]".to_owned()
            })
        );

        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n[]";
        assert_eq!(parse_http_response(partial).unwrap(), None);

        let body_to_eof = b"HTTP/1.1 200 OK\r\n\r\n[]";
        let parsed = parse_http_response(body_to_eof).unwrap().unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, "[]");
    }

    #[test]
    fn script_candidates_prefer_the_installed_plugin_layout() {
        let paths = AppPaths::new("/tmp/luatools-cef-test");
        let candidates = script_candidates(&paths);
        assert_eq!(
            candidates[0],
            paths.data_dir().join("plugin/public/luatools.js")
        );
        assert_eq!(candidates[1], paths.data_dir().join("plugin/luatools.js"));
    }
}
