//! LuaTools application entry point and service wiring.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod backend;
mod logging;
mod platform;
mod settings_store;
mod single_instance;

use eframe::egui;
use luatools_core::{AppPaths, Settings};
use luatools_ui::LuaToolsApp;

use crate::backend::BackendConfig;
use crate::settings_store::SettingsStore;

fn main() -> eframe::Result {
    let paths = match AppPaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("LuaTools: cannot resolve application directories: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = paths.ensure() {
        eprintln!("LuaTools: cannot create application directories: {error}");
        std::process::exit(1);
    }
    if let Err(error) = logging::init(&paths.log_dir()) {
        eprintln!("LuaTools: cannot start logging: {error}");
    }
    backend::clean_stale_temp(&paths);

    let store = SettingsStore::new(paths.settings_file());
    let settings = store.load();
    let startup = single_instance::parse_startup_args(std::env::args().skip(1));

    let lock_path = paths.data_dir().join("instance.lock");
    let lock = match single_instance::InstanceLock::acquire(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let pending = backend::pending_url_path(&paths);
            if let Some(url) = startup.url.as_deref()
                && let Err(error) = single_instance::write_pending(&pending, url)
            {
                eprintln!("LuaTools: cannot hand off URL: {error}");
            }
            return Ok(());
        }
        Err(error) => {
            eprintln!("LuaTools: cannot take the instance lock: {error}");
            std::process::exit(1);
        }
    };
    tracing::info!(lock = %lock.path().display(), "starting LuaTools");

    let pending = backend::pending_url_path(&paths);
    let startup_url = startup
        .url
        .clone()
        .or_else(|| single_instance::take_pending(&pending));

    if !startup.minimized
        && let Err(error) = run_window(paths.clone(), settings.clone(), startup_url.clone())
    {
        tracing::error!(%error, "the window could not be created");
        eprintln!("LuaTools: {error}");
    }

    // Keep the lock alive until the process actually exits.
    drop(lock);
    let _ = pending;
    Ok(())
}

fn run_window(paths: AppPaths, settings: Settings, startup_url: Option<String>) -> eframe::Result {
    let (handle, events) = match backend::spawn(BackendConfig {
        paths,
        settings: settings.clone(),
        startup_url,
    }) {
        Ok(channels) => channels,
        Err(error) => {
            eprintln!("LuaTools: cannot start the backend: {error}");
            return Ok(());
        }
    };

    let size = settings.window_size;
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LuaTools")
        .with_inner_size([size[0], size[1]])
        .with_min_inner_size([760.0, 560.0])
        .with_app_id("luatools");
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        // OpenGL is a compatibility fallback for machines without a working Vulkan/Metal device.
        renderer: if std::env::var_os("LUATOOLS_GL").is_some() {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        ..Default::default()
    };

    eframe::run_native(
        "LuaTools",
        options,
        Box::new(move |cc| {
            let app = LuaToolsApp::new(cc, handle, events, settings);
            Ok(Box::new(app))
        }),
    )
}

/// Decodes the bundled window icon.
///
/// Returns `None` when the asset is missing or not decodable; the window then uses the platform
/// default icon instead of failing startup.
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../../../assets/icon.ico");
    let image = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

/// Returns the default log file name, used by the about screen.
#[must_use]
pub fn log_file(paths: &AppPaths) -> std::path::PathBuf {
    paths.log_dir().join("luatools.log")
}
