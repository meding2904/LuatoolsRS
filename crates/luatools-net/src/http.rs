//! HTTP client construction shared by every outbound request.
//!
//! The client is built once and cloned (a [`reqwest::Client`] is an `Arc` internally); requests
//! reuse the same connection pool. When [`HttpClientConfig::doh`] is enabled the client resolves
//! names through [`crate::doh::DohResolver`], which itself uses an ordinary client so resolution
//! cannot recurse.

use std::sync::Arc;
use std::time::Duration;

use luatools_core::{Error, Result};

use crate::doh::DohResolver;

/// The user agent sent with every LuaTools request.
pub const USER_AGENT: &str = concat!("LuaTools/", env!("CARGO_PKG_VERSION"));

/// Connection settings for an [`reqwest::Client`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientConfig {
    /// Total request timeout, including the body.
    pub timeout: Duration,
    /// `User-Agent` header value.
    pub user_agent: String,
    /// Optional HTTP(S) proxy URL.
    pub proxy: Option<String>,
    /// Resolve host names through DNS-over-HTTPS instead of the system resolver.
    pub doh: bool,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            user_agent: USER_AGENT.to_owned(),
            proxy: None,
            doh: false,
        }
    }
}

impl HttpClientConfig {
    /// Returns the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Builds a client from `config`.
///
/// An empty proxy string is treated as "no proxy". A malformed proxy is rejected rather than
/// silently ignored so a typo in settings is visible.
///
/// # Errors
///
/// Returns [`Error::InvalidInput`] for a malformed proxy URL and [`Error::Network`] when the
/// underlying TLS or client configuration cannot be initialised.
pub fn build(config: &HttpClientConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(config.timeout)
        .user_agent(config.user_agent.as_str());

    if let Some(proxy) = config
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|proxy| !proxy.is_empty())
    {
        let proxy = reqwest::Proxy::all(proxy)
            .map_err(|e| Error::InvalidInput(format!("proxy url: {e}")))?;
        builder = builder.proxy(proxy);
    }

    if config.doh {
        builder = builder.dns_resolver(Arc::new(DohResolver::new()));
    }

    builder.build().map_err(|e| Error::Network {
        context: "building http client".to_owned(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_builds() {
        let client = build(&HttpClientConfig::default());
        assert!(client.is_ok(), "{client:?}");
    }

    #[test]
    fn empty_proxy_is_ignored() {
        let config = HttpClientConfig {
            proxy: Some("   ".to_owned()),
            ..HttpClientConfig::default()
        };
        assert!(build(&config).is_ok());
    }

    #[test]
    fn malformed_proxy_is_rejected() {
        let config = HttpClientConfig {
            proxy: Some("not a proxy".to_owned()),
            ..HttpClientConfig::default()
        };
        let err = build(&config).expect_err("proxy must be rejected");
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn doh_config_builds_without_network() {
        let config = HttpClientConfig {
            doh: true,
            ..HttpClientConfig::default()
        };
        assert!(build(&config).is_ok());
    }

    #[test]
    fn default_user_agent_carries_the_crate_version() {
        let config = HttpClientConfig::default();
        assert!(config.user_agent.starts_with("LuaTools/"));
    }
}
