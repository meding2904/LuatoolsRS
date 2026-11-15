//! Persistent user settings.
//!
//! The file lives in [`crate::AppPaths::config_dir`] as `settings.json`. Fields are additive:
//! missing keys keep their default and unknown keys are ignored so a newer build can be
//! rolled back without losing the rest of the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::locale::Locale;

/// Which colour theme to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemePreference {
    /// Follow the operating system.
    #[default]
    System,
    /// Always dark.
    Dark,
    /// Always light.
    Light,
}

impl ThemePreference {
    /// All values in menu order.
    pub const ALL: &'static [Self] = &[Self::System, Self::Dark, Self::Light];
}

/// How the window behaves when closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloseBehavior {
    /// Exit the process.
    #[default]
    Exit,
    /// Hide the window and keep running in the tray.
    ToTray,
}

/// Download scheduling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DownloadScheduling {
    /// Run jobs one after another.
    #[default]
    Sequential,
    /// Run up to `max_concurrent_downloads` jobs at once.
    Parallel,
}

/// Everything persisted between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    // Appearance and language.
    /// Interface language.
    pub language: Locale,
    /// Colour theme.
    pub theme: ThemePreference,

    // Window and tray.
    /// Remember the last window geometry.
    pub remember_window: bool,
    /// Window size in logical pixels.
    pub window_size: [f32; 2],
    /// Last active page id, for restoring the session.
    pub last_page: Option<String>,
    /// What the close button does.
    pub close_behavior: CloseBehavior,
    /// Start hidden in the tray.
    pub start_minimized: bool,

    // Downloads.
    /// Default destination for new downloads.
    pub download_dir: PathBuf,
    /// Queue scheduling policy.
    pub scheduling: DownloadScheduling,
    /// Worker count used when `scheduling` is parallel.
    pub max_concurrent_downloads: u8,
    /// Optional global speed limit, in bytes per second.
    pub speed_limit_bytes: Option<u64>,
    /// Verify files after transferring them.
    pub verify_after_download: bool,

    // Paths and network.
    /// Override for the Steam root; empty means auto-detect.
    pub steam_path: Option<PathBuf>,
    /// Extra Steam library folders to scan.
    pub extra_library_paths: Vec<PathBuf>,
    /// Optional HTTP(S) proxy for API and CDN requests.
    pub proxy_url: Option<String>,
    /// Resolve DNS over HTTPS instead of the system resolver.
    pub use_doh: bool,
    /// Port for the local API server; zero picks a free port.
    pub local_api_port: u16,

    // Integrations.
    /// Install the companion Steam store plugin.
    pub plugin_enabled: bool,
    /// Check for application updates on launch.
    pub check_updates: bool,
    /// Allow anonymous usage analytics.
    pub analytics_enabled: bool,
    /// Whether the onboarding flow finished.
    pub onboarding_completed: bool,
    /// Stable anonymous installation id.
    pub installation_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Locale::default(),
            theme: ThemePreference::default(),
            remember_window: true,
            window_size: [1280.0, 800.0],
            last_page: None,
            close_behavior: CloseBehavior::default(),
            start_minimized: false,
            download_dir: default_download_dir(),
            scheduling: DownloadScheduling::default(),
            max_concurrent_downloads: 3,
            speed_limit_bytes: None,
            verify_after_download: true,
            steam_path: None,
            extra_library_paths: Vec::new(),
            proxy_url: None,
            use_doh: false,
            local_api_port: 0,
            plugin_enabled: false,
            check_updates: true,
            analytics_enabled: false,
            onboarding_completed: false,
            installation_id: String::new(),
        }
    }
}

impl Settings {
    /// Rejects values the rest of the app cannot honour, clamping where sensible.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] for a malformed proxy URL, a zero-width window, or a
    /// download directory that is not absolute.
    pub fn validate(&self) -> Result<()> {
        if let Some(proxy) = self.proxy_url.as_deref().filter(|p| !p.is_empty()) {
            url::Url::parse(proxy).map_err(|e| Error::InvalidInput(format!("proxy url: {e}")))?;
        }
        if !self.download_dir.is_absolute() {
            return Err(Error::InvalidInput(
                "download directory must be an absolute path".to_owned(),
            ));
        }
        if self.window_size[0] < 320.0 || self.window_size[1] < 240.0 {
            return Err(Error::InvalidInput("window size is too small".to_owned()));
        }
        Ok(())
    }

    /// Number of simultaneous jobs the queue may run.
    #[must_use]
    pub fn effective_concurrency(&self) -> usize {
        match self.scheduling {
            DownloadScheduling::Sequential => 1,
            DownloadScheduling::Parallel => usize::from(self.max_concurrent_downloads.clamp(1, 16)),
        }
    }

    /// Returns the settings with out-of-range numbers clamped into range.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.max_concurrent_downloads = self.max_concurrent_downloads.clamp(1, 16);
        self.window_size[0] = self.window_size[0].clamp(640.0, 7680.0);
        self.window_size[1] = self.window_size[1].clamp(480.0, 4320.0);
        if self.download_dir.as_os_str().is_empty() {
            self.download_dir = default_download_dir();
        }
        self
    }
}

fn default_download_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("Downloads"), PathBuf::from)
        .join("LuaTools")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_sequential() {
        let settings = Settings::default();
        assert_eq!(settings.effective_concurrency(), 1);
        assert!(settings.download_dir.is_absolute());
        assert_eq!(
            settings.validate().map_err(|e| e.to_string()),
            Ok(()),
            "default settings must validate"
        );
    }

    #[test]
    fn parallel_concurrency_is_clamped() {
        let settings = Settings {
            scheduling: DownloadScheduling::Parallel,
            max_concurrent_downloads: 200,
            ..Settings::default()
        };
        assert_eq!(settings.effective_concurrency(), 16);
        let zero = Settings {
            max_concurrent_downloads: 0,
            ..settings
        };
        assert_eq!(zero.effective_concurrency(), 1);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let parsed: Settings = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(parsed.theme, ThemePreference::Light);
        assert_eq!(parsed.max_concurrent_downloads, 3);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let parsed: Settings =
            serde_json::from_str(r#"{"theme":"dark","fromTheFuture":42}"#).unwrap();
        assert_eq!(parsed.theme, ThemePreference::Dark);
    }

    #[test]
    fn malformed_proxy_is_rejected() {
        let settings = Settings {
            proxy_url: Some("not a url".to_owned()),
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }
}
