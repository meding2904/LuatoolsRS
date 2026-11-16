//! Loopback plugin API state and server startup.

use std::collections::HashMap;
use std::sync::Arc;

use luatools_core::{AppId, Settings};
use luatools_net::server::{PluginApi, ServerConfig, serve};
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

/// Installed-app snapshot served to the Steam store plugin.
#[derive(Debug, Default)]
pub struct PluginState {
    /// Installed app ids and names.
    apps: HashMap<u32, String>,
    /// Apps to show in the plugin's "recently loaded" popup.
    loaded: Vec<AppId>,
}

impl PluginState {
    /// Replaces the snapshot.
    pub fn replace(&mut self, apps: impl IntoIterator<Item = (AppId, String)>) {
        self.apps.clear();
        for (app, name) in apps {
            self.apps.insert(app.get(), name);
        }
        self.loaded = self.apps.keys().copied().map(AppId::from).collect();
    }

    /// Clears the recently-loaded popup.
    pub fn dismiss_loaded(&mut self) {
        self.loaded.clear();
    }

    /// Returns true when the app is installed.
    #[must_use]
    pub fn has_app(&self, app: AppId) -> bool {
        self.apps.contains_key(&app.get())
    }

    /// App ids and names, sorted by id.
    #[must_use]
    pub fn loaded(&self) -> Vec<(AppId, String)> {
        let mut apps: Vec<_> = self
            .loaded
            .iter()
            .filter_map(|app| self.apps.get(&app.get()).map(|name| (*app, name.clone())))
            .collect();
        apps.sort_by_key(|(app, _)| app.get());
        apps
    }
}

/// Shared API implementation handed to the Axum server.
pub struct PluginApiHandle {
    state: Arc<RwLock<PluginState>>,
    version: String,
}

impl PluginApi for PluginApiHandle {
    fn version(&self) -> String {
        self.version.clone()
    }

    fn has_app(&self, app: AppId) -> bool {
        self.state.read().has_app(app)
    }

    fn loaded_apps(&self) -> Vec<(AppId, String)> {
        self.state.read().loaded()
    }
}

/// Starts the loopback server, returning the bound port and the state handle.
///
/// The server is always loopback-only. A port of zero lets the operating system choose one.
/// Failures are logged and reported as `None`; they never block the rest of startup.
pub async fn start(
    settings: &Settings,
    state: Arc<RwLock<PluginState>>,
    shutdown: CancellationToken,
) -> Option<u16> {
    let api: Arc<dyn PluginApi> = Arc::new(PluginApiHandle {
        state,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    match serve(
        ServerConfig::new(settings.local_api_port, None),
        api,
        shutdown,
    )
    .await
    {
        Ok(port) => {
            tracing::info!(port, "local plugin API listening");
            Some(port)
        }
        Err(error) => {
            tracing::warn!(%error, "local plugin API could not start");
            None
        }
    }
}
