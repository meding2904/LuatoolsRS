//! The loopback HTTP API used by the Steam store plugin.
//!
//! The listener is bound to `127.0.0.1` only. When a token is configured every request must carry
//! it in the `X-LuaTools-Token` header; the browser-facing CORS headers allow exactly that header
//! so a page served from the Steam store origin can call in.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use luatools_core::{AppId, Error, Result};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Header carrying the shared token.
pub const TOKEN_HEADER: &str = "x-luatools-token";

/// Settings for [`serve`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    /// Requested port; zero lets the operating system choose one.
    pub port: u16,
    /// Optional shared secret required on every API request.
    pub token: Option<String>,
}

impl ServerConfig {
    /// Builds a configuration.
    #[must_use]
    pub fn new(port: u16, token: Option<String>) -> Self {
        Self { port, token }
    }
}

/// The state the local API exposes.
pub trait PluginApi: Send + Sync {
    /// Application version shown to the plugin.
    fn version(&self) -> String;

    /// Whether an app is installed through the plugin.
    fn has_app(&self, app: AppId) -> bool;

    /// Apps the plugin has added recently, newest first.
    fn loaded_apps(&self) -> Vec<(AppId, String)>;
}

/// Binds the local API on `127.0.0.1`, spawns it, and returns the actual port.
///
/// The returned future completes once the listener is bound; the server keeps running on the
/// current Tokio runtime until `shutdown` is cancelled, then stops accepting new connections and
/// drains in-flight requests.
///
/// # Errors
///
/// Returns [`Error::Io`] when the loopback address cannot be bound or inspected.
pub async fn serve(
    config: ServerConfig,
    api: Arc<dyn PluginApi>,
    shutdown: CancellationToken,
) -> Result<u16> {
    let ServerConfig { port, token } = config;
    let app = routes(api, token);
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = TcpListener::bind(address)
        .await
        .map_err(|error| Error::io(format!("binding {address}"), error))?;
    let port = listener
        .local_addr()
        .map_err(|error| Error::io("reading local api address", error))?
        .port();

    tokio::spawn(async move {
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, "local plugin API server stopped");
        }
    });

    Ok(port)
}

/// Builds the local API router.
///
/// Exposed separately from [`serve`] so the routes can be mounted elsewhere or driven in tests
/// without binding a socket. An empty token is treated as no token.
pub fn routes(api: Arc<dyn PluginApi>, token: Option<String>) -> Router {
    let state = AppState {
        api,
        token: token
            .filter(|token| !token.is_empty())
            .map(Arc::<str>::from),
    };

    Router::new()
        .route("/version", get(version))
        .route("/has/{appid}", get(has_app))
        .route("/loaded-apps", get(loaded_apps))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(middleware::from_fn(cors))
        .with_state(state)
}

#[derive(Clone)]
struct AppState {
    api: Arc<dyn PluginApi>,
    token: Option<Arc<str>>,
}

#[derive(Serialize)]
struct VersionBody {
    success: bool,
    version: String,
}

#[derive(Serialize)]
struct HasAppBody {
    success: bool,
    exists: bool,
}

#[derive(Serialize)]
struct LoadedAppBody {
    appid: u32,
    name: String,
}

#[derive(Serialize)]
struct LoadedAppsBody {
    success: bool,
    apps: Vec<LoadedAppBody>,
}

#[derive(Serialize)]
struct ErrorBody {
    success: bool,
    error: String,
}

impl ErrorBody {
    fn new(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error: message.into(),
        }
    }
}

async fn version(State(state): State<AppState>) -> Json<VersionBody> {
    Json(VersionBody {
        success: true,
        version: state.api.version(),
    })
}

async fn has_app(State(state): State<AppState>, Path(appid): Path<String>) -> Response {
    match appid.parse::<u32>() {
        Ok(raw) => Json(HasAppBody {
            success: true,
            exists: state.api.has_app(AppId::from(raw)),
        })
        .into_response(),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody::new("invalid appid")),
        )
            .into_response(),
    }
}

async fn loaded_apps(State(state): State<AppState>) -> Json<LoadedAppsBody> {
    let apps = state
        .api
        .loaded_apps()
        .into_iter()
        .map(|(app, name)| LoadedAppBody {
            appid: app.get(),
            name,
        })
        .collect();
    Json(LoadedAppsBody {
        success: true,
        apps,
    })
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(ErrorBody::new("not found")))
}

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = state.token.as_deref() {
        let supplied = request
            .headers()
            .get(TOKEN_HEADER)
            .and_then(|value| value.to_str().ok());
        if supplied != Some(expected) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorBody::new("unauthorized")),
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn cors(request: Request, next: Next) -> Response {
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        set_cors_headers(response.headers_mut());
        return response;
    }

    let mut response = next.run(request).await;
    set_cors_headers(response.headers_mut());
    response
}

fn set_cors_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, X-LuaTools-Token"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use serde_json::Value;
    use tower::ServiceExt as _;

    struct MockApi;

    impl PluginApi for MockApi {
        fn version(&self) -> String {
            "0.1.0".to_owned()
        }

        fn has_app(&self, app: AppId) -> bool {
            app == AppId::from(730)
        }

        fn loaded_apps(&self) -> Vec<(AppId, String)> {
            vec![
                (AppId::from(730), "Counter-Strike 2".to_owned()),
                (AppId::from(570), "Dota 2".to_owned()),
            ]
        }
    }

    fn api() -> Arc<dyn PluginApi> {
        Arc::new(MockApi)
    }

    async fn call(app: Router, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let response = app.oneshot(request).await.expect("route response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body bytes");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("json body")
        };
        (status, value)
    }

    fn get(uri: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn version_route_returns_the_api_version() {
        let (status, body) = call(routes(api(), None), get("/version")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], Value::Bool(true));
        assert_eq!(body["version"], Value::String("0.1.0".to_owned()));
    }

    #[tokio::test]
    async fn has_route_reports_known_and_unknown_apps() {
        let app = routes(api(), None);
        let (status, body) = call(app.clone(), get("/has/730")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["exists"], Value::Bool(true));

        let (status, body) = call(app, get("/has/999")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["exists"], Value::Bool(false));
    }

    #[tokio::test]
    async fn has_route_rejects_a_non_numeric_appid() {
        let (status, body) = call(routes(api(), None), get("/has/not-a-number")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["success"], Value::Bool(false));
        assert_eq!(body["error"], Value::String("invalid appid".to_owned()));
    }

    #[tokio::test]
    async fn loaded_apps_route_preserves_order_and_names() {
        let (status, body) = call(routes(api(), None), get("/loaded-apps")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["apps"],
            serde_json::json!([
                {"appid": 730, "name": "Counter-Strike 2"},
                {"appid": 570, "name": "Dota 2"}
            ])
        );
    }

    #[tokio::test]
    async fn unknown_route_is_json_404() {
        let (status, body) = call(routes(api(), None), get("/nope")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["success"], Value::Bool(false));
    }

    #[tokio::test]
    async fn token_is_required_when_configured() {
        let app = routes(api(), Some("secret".to_owned()));

        let (status, body) = call(app.clone(), get("/version")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], Value::String("unauthorized".to_owned()));

        let wrong = HttpRequest::builder()
            .uri("/version")
            .header(TOKEN_HEADER, "wrong")
            .body(Body::empty())
            .expect("request");
        let (status, _) = call(app.clone(), wrong).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let right = HttpRequest::builder()
            .uri("/version")
            .header(TOKEN_HEADER, "secret")
            .body(Body::empty())
            .expect("request");
        let (status, body) = call(app, right).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["version"], Value::String("0.1.0".to_owned()));
    }

    #[tokio::test]
    async fn empty_token_means_no_token() {
        let (status, _) = call(routes(api(), Some(String::new())), get("/version")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn preflight_is_answered_without_a_token() {
        let app = routes(api(), Some("secret".to_owned()));
        let request = HttpRequest::builder()
            .method(Method::OPTIONS)
            .uri("/has/730")
            .header("origin", "https://store.steampowered.com")
            .header("access-control-request-method", "GET")
            .body(Body::empty())
            .expect("request");

        let response = app.oneshot(request).await.expect("preflight response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|value| value.to_str().ok()),
            Some("*")
        );
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().contains(TOKEN_HEADER))
        );
    }
}
