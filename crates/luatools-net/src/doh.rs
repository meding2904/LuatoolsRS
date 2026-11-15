//! DNS-over-HTTPS resolution with a small TTL cache.
//!
//! Users whose ISP DNS blocks or sinkholes the project's hosts can ask a public resolver over
//! HTTPS instead. The endpoint is addressed by IP literal on purpose: resolving a host name in
//! order to resolve host names would need the very DNS that is broken.
//!
//! Failures never surface as errors. A resolver that cannot answer returns an empty list so the
//! caller (and the reqwest resolver shim in this module) can fall back to the system resolver.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Default Cloudflare dns-json endpoint (an IP literal, not a host name).
pub const DEFAULT_ENDPOINT: &str = "https://1.1.1.1/dns-query";

/// Shortest time an answer is cached.
pub const MIN_TTL: Duration = Duration::from_secs(30);

/// Longest time an answer is cached.
pub const MAX_TTL: Duration = Duration::from_secs(5 * 60);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

struct CacheEntry {
    addresses: Vec<IpAddr>,
    expires: Instant,
}

type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Resolves host names through a dns-json HTTP endpoint and caches answers by TTL.
///
/// Cloning is cheap: all instances share the same cache and client. The clock is injectable so
/// expiry can be tested without sleeping.
#[derive(Clone)]
pub struct DohResolver {
    endpoint: Arc<str>,
    client: Option<reqwest::Client>,
    cache: Arc<Mutex<HashMap<String, CacheEntry>>>,
    now: ClockFn,
}

impl fmt::Debug for DohResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DohResolver")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Default for DohResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl DohResolver {
    /// Builds a resolver against [`DEFAULT_ENDPOINT`] using the system clock.
    ///
    /// If the HTTP client cannot be initialised the resolver quietly answers nothing, which
    /// leaves the caller on the system resolver.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok();
        Self {
            endpoint: Arc::from(DEFAULT_ENDPOINT),
            client,
            cache: Arc::new(Mutex::new(HashMap::new())),
            now: Arc::new(Instant::now),
        }
    }

    /// Builds a resolver with a caller-supplied clock, for deterministic cache-expiry tests.
    #[must_use]
    pub fn with_clock(clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        let mut resolver = Self::new();
        resolver.now = Arc::new(clock);
        resolver
    }

    /// Replaces the dns-json endpoint.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Arc::from(endpoint.into());
        self
    }

    /// Addresses for `host`, or an empty vector when DoH cannot answer.
    ///
    /// Names rejected by [`Self::should_bypass`] return an empty vector without a request.
    /// Transport, TLS, status, and parse failures are swallowed so callers can fall back.
    pub async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        if Self::should_bypass(host) {
            return Vec::new();
        }
        if let Some(hit) = self.cached(host) {
            return hit;
        }

        let encoded: String = url::form_urlencoded::byte_serialize(host.as_bytes()).collect();
        let url = format!("{}?name={encoded}&type=A", self.endpoint);
        let Some(client) = self.client.as_ref() else {
            return Vec::new();
        };

        let response = match client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/dns-json")
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(host, %error, "DoH request failed");
                return Vec::new();
            }
        };
        if !response.status().is_success() {
            tracing::debug!(host, status = %response.status(), "DoH answered an error status");
            return Vec::new();
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(host, %error, "DoH body could not be read");
                return Vec::new();
            }
        };

        let (addresses, ttl) = Self::parse(&body);
        if addresses.is_empty() {
            return addresses;
        }
        self.remember(host, &addresses, ttl);
        addresses
    }

    /// Returns a live cached answer, dropping an expired entry on the way out.
    #[must_use]
    pub fn cached(&self, host: &str) -> Option<Vec<IpAddr>> {
        let key = cache_key(host);
        let now = (self.now)();
        let mut cache = self.lock_cache();
        match cache.get(&key) {
            Some(entry) if entry.expires > now => Some(entry.addresses.clone()),
            Some(_) => {
                cache.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Stores a pre-resolved answer, clamping its lifetime to [`MIN_TTL`]..=[`MAX_TTL`].
    ///
    /// Exposed so a caller that resolved through another channel can warm the cache, and so
    /// cache expiry is testable without a network.
    pub fn remember(&self, host: &str, addresses: &[IpAddr], ttl: Duration) {
        if addresses.is_empty() {
            return;
        }
        let ttl = ttl.clamp(MIN_TTL, MAX_TTL);
        let expires = (self.now)() + ttl;
        self.lock_cache().insert(
            cache_key(host),
            CacheEntry {
                addresses: addresses.to_vec(),
                expires,
            },
        );
    }

    /// Names that must never go to DoH.
    ///
    /// IP literals need no resolution. Loopback, `.local`, `.localhost`, and single-label names
    /// belong to the machine or the local network; a public resolver has no useful answer and
    /// asking would leak the name.
    #[must_use]
    pub fn should_bypass(host: &str) -> bool {
        let host = host.trim().trim_end_matches('.');
        if host.is_empty() || host.parse::<IpAddr>().is_ok() {
            return true;
        }
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        let lower = host.to_ascii_lowercase();
        if lower.ends_with(".local") || lower.ends_with(".localhost") {
            return true;
        }
        !host.contains('.')
    }

    /// Extracts A/AAAA answers and the lowest record TTL from a dns-json body.
    ///
    /// Returns empty addresses and a zero TTL for a non-object body, a non-zero `Status`
    /// (NXDOMAIN is 3), a missing `Answer` array, or no usable record. CNAME records are steps in
    /// the chain, not answers, and are skipped. The returned TTL is clamped to
    /// [`MIN_TTL`]..=[`MAX_TTL`]; a missing TTL behaves like zero and therefore clamps to
    /// [`MIN_TTL`].
    #[must_use]
    pub fn parse(json: &str) -> (Vec<IpAddr>, Duration) {
        let Ok(root) = serde_json::from_str::<Value>(json) else {
            return (Vec::new(), Duration::ZERO);
        };
        if !root.is_object() {
            return (Vec::new(), Duration::ZERO);
        }
        let status = root
            .get("Status")
            .or_else(|| root.get("status"))
            .and_then(Value::as_i64);
        if status != Some(0) {
            return (Vec::new(), Duration::ZERO);
        }
        let Some(answer) = root
            .get("Answer")
            .or_else(|| root.get("answer"))
            .and_then(Value::as_array)
        else {
            return (Vec::new(), Duration::ZERO);
        };

        let mut addresses = Vec::with_capacity(answer.len());
        let mut min_ttl: Option<u64> = None;

        for entry in answer {
            let Some(kind) = entry.get("type").and_then(Value::as_i64) else {
                continue;
            };
            // 1 = A, 28 = AAAA. Everything else (notably 5 = CNAME) is not an answer.
            if kind != 1 && kind != 28 {
                continue;
            }
            let Some(data) = entry.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(address) = data.parse::<IpAddr>() else {
                continue;
            };
            if !addresses.contains(&address) {
                addresses.push(address);
            }
            if let Some(ttl) = entry
                .get("TTL")
                .or_else(|| entry.get("ttl"))
                .and_then(Value::as_i64)
            {
                let ttl = u64::try_from(ttl).unwrap_or(0);
                min_ttl = Some(min_ttl.map_or(ttl, |current| current.min(ttl)));
            }
        }

        if addresses.is_empty() {
            return (Vec::new(), Duration::ZERO);
        }
        let seconds = min_ttl
            .unwrap_or(0)
            .clamp(MIN_TTL.as_secs(), MAX_TTL.as_secs());
        (addresses, Duration::from_secs(seconds))
    }

    fn lock_cache(&self) -> MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn cache_key(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Adapts [`DohResolver`] to reqwest's resolver interface.
///
/// DoH answers win. When DoH yields nothing the system resolver is consulted, so enabling DoH
/// cannot make a reachable host unreachable. This is the only place the two resolvers are
/// combined; [`DohResolver::resolve`] itself stays empty-on-failure.
impl reqwest::dns::Resolve for DohResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let resolver = self.clone();
        Box::pin(async move {
            let mut addresses: Vec<SocketAddr> = resolver
                .resolve(&host)
                .await
                .into_iter()
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();

            if addresses.is_empty() {
                match tokio::net::lookup_host((host.as_str(), 0)).await {
                    Ok(found) => addresses = found.collect(),
                    Err(error) => return Err(format!("dns {host}: {error}").into()),
                }
            }
            if addresses.is_empty() {
                return Err(format!("dns {host}: no addresses").into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test vector must be an IP")
    }

    fn body(answer: &str) -> String {
        format!(r#"{{"Status":0,"Answer":[{answer}]}}"#)
    }

    #[test]
    fn lowest_ttl_wins() {
        let json = body(
            r#"{"name":"example.com.","type":1,"TTL":300,"data":"93.184.216.34"},
               {"name":"example.com.","type":1,"TTL":120,"data":"93.184.216.35"}"#,
        );
        let (addresses, ttl) = DohResolver::parse(&json);
        assert_eq!(addresses, vec![ip("93.184.216.34"), ip("93.184.216.35")]);
        assert_eq!(ttl, Duration::from_secs(120));
    }

    #[test]
    fn cname_is_skipped() {
        let json = body(
            r#"{"name":"www.example.com.","type":5,"TTL":5,"data":"example.com."},
               {"name":"example.com.","type":1,"TTL":200,"data":"93.184.216.34"}"#,
        );
        let (addresses, ttl) = DohResolver::parse(&json);
        assert_eq!(addresses, vec![ip("93.184.216.34")]);
        assert_eq!(ttl, Duration::from_secs(200));
    }

    #[test]
    fn aaaa_records_are_accepted() {
        let json = body(r#"{"name":"example.com.","type":28,"TTL":90,"data":"2001:db8::1"}"#);
        let (addresses, ttl) = DohResolver::parse(&json);
        assert_eq!(
            addresses,
            vec![IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))]
        );
        assert_eq!(ttl, Duration::from_secs(90));
    }

    #[test]
    fn nxdomain_returns_empty() {
        let (addresses, ttl) = DohResolver::parse(r#"{"Status":3,"Answer":[]}"#);
        assert!(addresses.is_empty());
        assert_eq!(ttl, Duration::ZERO);
    }

    #[test]
    fn malformed_json_returns_empty() {
        for json in [
            "",
            "not json",
            "[]",
            "null",
            r#"{"Status":"0"}"#,
            r#"{"Status":0}"#,
        ] {
            let (addresses, ttl) = DohResolver::parse(json);
            assert!(addresses.is_empty(), "{json}");
            assert_eq!(ttl, Duration::ZERO, "{json}");
        }
    }

    #[test]
    fn ttl_is_clamped_between_30_seconds_and_5_minutes() {
        let (_, low) = DohResolver::parse(&body(r#"{"type":1,"TTL":5,"data":"93.184.216.34"}"#));
        assert_eq!(low, Duration::from_secs(30));

        let (_, high) =
            DohResolver::parse(&body(r#"{"type":1,"TTL":100000,"data":"93.184.216.34"}"#));
        assert_eq!(high, Duration::from_secs(300));

        let (_, missing) = DohResolver::parse(&body(r#"{"type":1,"data":"93.184.216.34"}"#));
        assert_eq!(missing, Duration::from_secs(30));
    }

    #[test]
    fn ipv4_answers_without_ttl_are_still_answers() {
        let (addresses, _) = DohResolver::parse(&body(r#"{"type":1,"data":"127.0.0.1"}"#));
        assert_eq!(addresses, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    #[test]
    fn bypass_rules() {
        for host in [
            "",
            "   ",
            "127.0.0.1",
            "8.8.8.8",
            "::1",
            "2001:db8::1",
            "localhost",
            "LOCALHOST",
            "localhost.",
            "printer.local",
            "app.localhost",
            "intranet",
            "trayhost",
        ] {
            assert!(DohResolver::should_bypass(host), "{host:?} must bypass");
        }

        for host in [
            "example.com",
            "api.github.com",
            "sub.local.example.com",
            "1.1.1.1.example.com",
            "lua.tools",
        ] {
            assert!(!DohResolver::should_bypass(host), "{host:?} must resolve");
        }
    }

    #[test]
    fn cache_expires_with_the_injected_clock() {
        let clock = Arc::new(Mutex::new(Instant::now()));
        let resolver = DohResolver::with_clock({
            let clock = Arc::clone(&clock);
            move || *clock.lock().expect("test clock lock")
        });
        let address = ip("93.184.216.34");

        resolver.remember("example.com", &[address], Duration::from_secs(60));
        assert_eq!(resolver.cached("EXAMPLE.COM."), Some(vec![address]));

        *clock.lock().expect("test clock lock") += Duration::from_secs(59);
        assert!(resolver.cached("example.com").is_some());

        *clock.lock().expect("test clock lock") += Duration::from_secs(2);
        assert_eq!(resolver.cached("example.com"), None);
    }

    #[test]
    fn remembered_ttl_is_clamped() {
        let clock = Arc::new(Mutex::new(Instant::now()));
        let resolver = DohResolver::with_clock({
            let clock = Arc::clone(&clock);
            move || *clock.lock().expect("test clock lock")
        });
        let address = ip("93.184.216.34");

        resolver.remember("example.com", &[address], Duration::from_secs(1));
        *clock.lock().expect("test clock lock") += Duration::from_secs(2);
        assert!(resolver.cached("example.com").is_some());
        *clock.lock().expect("test clock lock") += Duration::from_secs(29);
        assert!(resolver.cached("example.com").is_none());
    }

    #[test]
    fn empty_answer_is_not_cached() {
        let resolver = DohResolver::new();
        resolver.remember("example.com", &[], Duration::from_secs(300));
        assert_eq!(resolver.cached("example.com"), None);
    }
}
